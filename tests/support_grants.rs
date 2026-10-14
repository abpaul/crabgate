use crabgate::{
    admin_orders,
    app::App,
    crypto::MasterKey,
    events::{self, EventInput, TicketTypeInput},
    members::{self, Role},
    operator, settings,
};
use loco_rs::{app::AppContext, testing::prelude::*};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;

const SYNTHETIC_SECRET: &str = "sk_test_support_boundary_not_real";

struct Fixture {
    organizer_id: String,
    member_id: String,
    event_id: String,
    order_id: String,
    admin_cookie: String,
    operator_cookie: String,
}

async fn fixture(ctx: &AppContext) -> Fixture {
    let row = ctx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT id, db_path, data_key FROM organizers WHERE subdomain = 'local'",
        ))
        .await
        .unwrap()
        .unwrap();
    let organizer_id: String = row.try_get("", "id").unwrap();
    let path: String = row.try_get("", "db_path").unwrap();
    let wrapped: String = row.try_get("", "data_key").unwrap();
    let db = sea_orm::Database::connect(format!("sqlite://{path}?mode=rw"))
        .await
        .unwrap();
    let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .unwrap()
        .open_organizer_key(&organizer_id, &wrapped)
        .unwrap();
    settings::set(&db, &cipher, "stripe_secret_key", SYNTHETIC_SECRET)
        .await
        .unwrap();
    let event = events::create(
        &db,
        &EventInput {
            title: "Support boundary workshop".to_owned(),
            date: "2027-06-15".to_owned(),
            start_time: "19:00".to_owned(),
            end_time: "21:00".to_owned(),
            timezone: "Europe/Dublin".to_owned(),
            venue: "Hall".to_owned(),
            address: "1 Street".to_owned(),
            description: String::new(),
            event_capacity: Some(10),
            max_per_order: 4,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let ticket_type = events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "Workshop place".to_owned(),
            price: "0".to_owned(),
            capacity: 10,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    let now = crabgate::orders::now();
    events::publish(&db, &event.id, now).await.unwrap();
    let order_id = admin_orders::complimentary(
        &db,
        &cipher,
        &event.id,
        &ticket_type.id,
        "buyer@example.com",
        "Workshop guest",
        now,
    )
    .await
    .unwrap();
    let member_id = members::add_member(&ctx.db, &organizer_id, "admin@example.com", Role::Admin)
        .await
        .unwrap();
    let token = members::request_magic_link(&ctx.db, &organizer_id, "admin@example.com", now)
        .await
        .unwrap()
        .unwrap();
    let admin_session = members::claim_magic_link(&ctx.db, &organizer_id, &token, now)
        .await
        .unwrap()
        .unwrap();
    ctx.db
        .execute_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "INSERT INTO operators (id, email) VALUES ('support', 'support@example.com')",
        ))
        .await
        .unwrap();
    let token = operator::request_link(&ctx.db, "support@example.com", now)
        .await
        .unwrap()
        .unwrap();
    let operator_session = operator::claim_link(&ctx.db, &token, now)
        .await
        .unwrap()
        .unwrap();
    Fixture {
        organizer_id,
        member_id,
        event_id: event.id,
        order_id,
        admin_cookie: format!("cg_session={admin_session}"),
        operator_cookie: format!("cg_operator_session={operator_session}"),
    }
}

#[tokio::test]
#[serial]
async fn granted_support_cannot_download_the_database_or_payment_credentials() {
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        operator::grant(
            &ctx.db,
            &f.organizer_id,
            &f.member_id,
            1,
            crabgate::orders::now(),
        )
        .await
        .unwrap();
        let export = request
            .get("/office/data/export")
            .add_header("cookie", &f.operator_cookie)
            .await;
        let leaked_secret = export
            .as_bytes()
            .windows(SYNTHETIC_SECRET.len())
            .any(|window| window == SYNTHETIC_SECRET.as_bytes());
        println!(
            "Support database export: HTTP {}, decrypted synthetic secret present: {leaked_secret}",
            export.status_code()
        );
        assert_eq!(export.status_code(), 403);
        assert!(!leaked_secret);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn granted_support_can_read_orders_and_events_but_not_exports_or_credentials() {
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        operator::grant(
            &ctx.db,
            &f.organizer_id,
            &f.member_id,
            1,
            crabgate::orders::now(),
        )
        .await
        .unwrap();
        let event = request
            .get(&format!("/office/events/{}", f.event_id))
            .add_header("cookie", &f.operator_cookie)
            .await;
        assert_eq!(event.status_code(), 200);
        assert!(event.text().contains("Support boundary workshop"));
        let order = request
            .get(&format!("/office/orders/{}", f.order_id))
            .add_header("cookie", &f.operator_cookie)
            .await;
        assert_eq!(order.status_code(), 200);
        assert!(order.text().contains("buyer@example.com"));
        for filename in ["orders.csv", "tickets.csv", "checkins.csv"] {
            let export = request
                .get(&format!("/office/events/{}/export/{filename}", f.event_id))
                .add_header("cookie", &f.operator_cookie)
                .await;
            assert_eq!(export.status_code(), 403, "{filename}");
            assert!(!export.text().contains("buyer@example.com"));
        }
        for path in [
            "/office/setup",
            "/office/setup/master-key-status",
            "/office/access-grants",
        ] {
            let response = request
                .get(path)
                .add_header("cookie", &f.operator_cookie)
                .await;
            assert_eq!(response.status_code(), 403, "{path}");
            assert!(!response.text().contains(SYNTHETIC_SECRET));
        }
        assert_eq!(
            request
                .post(&format!("/office/events/{}/draft", f.event_id))
                .add_header("cookie", &f.operator_cookie)
                .await
                .status_code(),
            403
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn organizer_admin_still_receives_database_and_all_csv_exports() {
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        let export = request
            .get("/office/data/export")
            .add_header("cookie", &f.admin_cookie)
            .await;
        assert_eq!(export.status_code(), 200);
        assert_eq!(export.header("content-type"), "application/vnd.sqlite3");
        assert!(export.as_bytes().starts_with(b"SQLite format 3\0"));
        assert!(export
            .as_bytes()
            .windows(SYNTHETIC_SECRET.len())
            .any(|w| w == SYNTHETIC_SECRET.as_bytes()));
        for filename in ["orders.csv", "tickets.csv", "checkins.csv"] {
            let export = request
                .get(&format!("/office/events/{}/export/{filename}", f.event_id))
                .add_header("cookie", &f.admin_cookie)
                .await;
            assert_eq!(export.status_code(), 200, "{filename}");
            assert_eq!(export.header("content-type"), "text/csv; charset=utf-8");
            assert!(!export.text().is_empty());
        }
        let both = format!("{}; {}", f.admin_cookie, f.operator_cookie);
        assert_eq!(
            request
                .get("/office/data/export")
                .add_header("cookie", both)
                .await
                .status_code(),
            200
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn absent_expired_and_revoked_grants_deny_support_access() {
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        let paths = [
            format!("/office/events/{}", f.event_id),
            format!("/office/orders/{}", f.order_id),
            "/office/data/export".to_owned(),
            format!("/office/events/{}/export/orders.csv", f.event_id),
        ];
        for path in &paths {
            assert_eq!(
                request
                    .get(path)
                    .add_header("cookie", &f.operator_cookie)
                    .await
                    .status_code(),
                403,
                "no grant: {path}"
            );
        }
        let now = crabgate::orders::now();
        operator::grant(&ctx.db, &f.organizer_id, &f.member_id, 1, now - 3600)
            .await
            .unwrap();
        for path in &paths {
            assert_eq!(
                request
                    .get(path)
                    .add_header("cookie", &f.operator_cookie)
                    .await
                    .status_code(),
                403,
                "expired grant: {path}"
            );
        }
        let grant = operator::grant(&ctx.db, &f.organizer_id, &f.member_id, 1, now)
            .await
            .unwrap();
        assert_eq!(
            request
                .get(&paths[0])
                .add_header("cookie", &f.operator_cookie)
                .await
                .status_code(),
            200
        );
        operator::revoke(&ctx.db, &f.organizer_id, &grant, now)
            .await
            .unwrap();
        for path in &paths {
            assert_eq!(
                request
                    .get(path)
                    .add_header("cookie", &f.operator_cookie)
                    .await
                    .status_code(),
                403,
                "revoked grant: {path}"
            );
        }
    })
    .await;
}
