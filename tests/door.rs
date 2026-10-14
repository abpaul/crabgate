use chrono::Utc;
use crabgate::{
    admin_orders,
    crypto::{MasterKey, OrganizerCipher},
    door::{self, CheckinResult},
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, Selection},
    orders::{self, PaymentOutcome},
};
use loco_rs::db;
use migration::OrganizerMigrator;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

async fn setup() -> (
    tempfile::TempDir,
    sea_orm::DatabaseConnection,
    OrganizerCipher,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = sea_orm::Database::connect(format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("door.db").display()
    ))
    .await
    .unwrap();
    db::migrate::<OrganizerMigrator>(&db).await.unwrap();
    let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .unwrap()
        .generate_organizer_key("test")
        .unwrap()
        .1;
    (dir, db, cipher)
}

async fn event_with_ticket(
    db: &sea_orm::DatabaseConnection,
    cipher: &OrganizerCipher,
    title: &str,
    name: &str,
) -> (String, String, String) {
    let now = Utc::now().timestamp();
    let event = events::create(
        db,
        &EventInput {
            title: title.to_owned(),
            date: "2027-06-15".to_owned(),
            start_time: "19:00".to_owned(),
            end_time: "21:00".to_owned(),
            timezone: "Europe/Dublin".to_owned(),
            venue: "Hall".to_owned(),
            address: "1 Main Street".to_owned(),
            description: String::new(),
            event_capacity: Some(10),
            max_per_order: 10,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let ticket_type = events::add_ticket_type(
        db,
        &event,
        &TicketTypeInput {
            name: "Standard".to_owned(),
            price: "0".to_owned(),
            capacity: 10,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(db, &event.id, now).await.unwrap();
    let hold = inventory::reserve(
        db,
        &event.id,
        &[Selection {
            ticket_type_id: ticket_type.id,
            quantity: 1,
        }],
        now,
    )
    .await
    .unwrap();
    let details = orders::hold_details(db, &hold.id).await.unwrap();
    orders::save_details(
        db,
        cipher,
        &details,
        "buyer@example.com",
        &[name.to_owned()],
        false,
        now,
    )
    .await
    .unwrap();
    let session_id = format!("free_{}", hold.id);
    orders::attach_session(db, &hold.id, &session_id, now)
        .await
        .unwrap();
    let order_id = match orders::complete_payment(db, &session_id, "", 0, now)
        .await
        .unwrap()
    {
        PaymentOutcome::Paid(id) => id,
        _ => panic!("Order not paid"),
    };
    let order = orders::get_order(db, cipher, &order_id).await.unwrap();
    (
        event.id,
        order.tickets[0].id.clone(),
        order.tickets[0].code.clone(),
    )
}

#[tokio::test]
async fn door_link_expires_at_event_end_and_revoke_blocks_access() {
    let (_dir, db, cipher) = setup().await;
    let (event_id, _, _) = event_with_ticket(&db, &cipher, "Music Night", "Niamh Byrne").await;
    let now = Utc::now().timestamp();
    let link = door::create_link(&db, &cipher, &event_id, "Front door", now)
        .await
        .unwrap();
    assert_eq!(
        door::verify_link(&db, &event_id, &link.token, now)
            .await
            .unwrap(),
        Some(link.id.clone())
    );
    assert_eq!(
        door::verify_link(&db, &event_id, &link.token, 1813082400 + 7201)
            .await
            .unwrap(),
        None
    );
    door::revoke_link(&db, &link.id, now + 1).await.unwrap();
    assert_eq!(
        door::verify_link(&db, &event_id, &link.token, now + 2)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        door::list_links(&db, &cipher).await.unwrap()[0].label,
        "Front door"
    );
}

#[tokio::test]
async fn every_result_state_and_undo_have_literal_counts() {
    let (_dir, db, cipher) = setup().await;
    let (event_id, ticket_id, code) =
        event_with_ticket(&db, &cipher, "Music Night", "Niamh Byrne").await;
    let (other_id, other_ticket_id, other_code) =
        event_with_ticket(&db, &cipher, "Open Day", "Cian Walsh").await;
    let now = Utc::now().timestamp();
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE tickets SET code = lower(code) WHERE id = ?",
        [ticket_id.clone().into()],
    ))
    .await
    .unwrap();
    assert_eq!(door::count(&db, &event_id).await.unwrap(), (0, 1));
    match door::check_in(&db, &cipher, &event_id, &code, "Door Link", "front", now)
        .await
        .unwrap()
    {
        CheckinResult::CheckedIn(ticket) => {
            assert_eq!(ticket.attendee_name.as_deref(), Some("Niamh Byrne"))
        }
        _ => panic!("Ticket was not checked in"),
    }
    assert!(matches!(
        door::check_in(&db, &cipher, &event_id, &code, "Member", "member", now + 1)
            .await
            .unwrap(),
        CheckinResult::AlreadyCheckedIn(_)
    ));
    assert!(matches!(
        door::check_in(
            &db,
            &cipher,
            &event_id,
            &other_code,
            "Member",
            "member",
            now + 1
        )
        .await
        .unwrap(),
        CheckinResult::WrongEvent
    ));
    assert!(matches!(
        door::check_in(
            &db,
            &cipher,
            &event_id,
            "unknown",
            "Member",
            "member",
            now + 1
        )
        .await
        .unwrap(),
        CheckinResult::Unknown
    ));
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE tickets SET status = 'Void' WHERE id = ?",
        [other_ticket_id.into()],
    ))
    .await
    .unwrap();
    assert!(matches!(
        door::check_in(
            &db,
            &cipher,
            &other_id,
            &other_code,
            "Member",
            "member",
            now + 1
        )
        .await
        .unwrap(),
        CheckinResult::VoidTicket(_)
    ));
    assert_eq!(door::count(&db, &event_id).await.unwrap(), (1, 1));
    assert_eq!(
        door::search(&db, &cipher, &event_id, "niamh")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        door::search(&db, &cipher, &other_id, "niamh")
            .await
            .unwrap()
            .len(),
        0
    );
    door::undo(&db, &event_id, &ticket_id, "member", now + 2)
        .await
        .unwrap();
    assert_eq!(door::count(&db, &event_id).await.unwrap(), (0, 1));
    assert!(matches!(
        door::check_in(&db, &cipher, &event_id, &code, "Member", "member", now + 3)
            .await
            .unwrap(),
        CheckinResult::CheckedIn(_)
    ));
    assert_eq!(door::count(&db, &event_id).await.unwrap(), (1, 1));
}

#[tokio::test]
async fn full_refund_then_undo_blocks_reentry_and_preserves_history() {
    for finalize_from_webhook in [false, true] {
        let (_dir, db, cipher) = setup().await;
        let (event_id, ticket_id, code) =
            event_with_ticket(&db, &cipher, "Refund Night", "Niamh Byrne").await;
        let now = Utc::now().timestamp();
        let order_id: String = db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT order_id FROM tickets WHERE id = ?",
                [ticket_id.clone().into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "order_id")
            .unwrap();
        assert!(matches!(
            door::check_in(&db, &cipher, &event_id, &code, "Door Link", "front", now)
                .await
                .unwrap(),
            CheckinResult::CheckedIn(_)
        ));
        if finalize_from_webhook {
            admin_orders::mark_refunded(&db, &order_id, now + 1)
                .await
                .unwrap();
        } else {
            admin_orders::refund(&db, &order_id, None, now + 1)
                .await
                .unwrap();
        }
        match door::check_in(&db, &cipher, &event_id, &code, "Member", "member", now + 2)
            .await
            .unwrap()
        {
            CheckinResult::VoidTicket(ticket) => assert_eq!(ticket.checked_at, Some(now)),
            _ => panic!("Fully refunded Ticket was not void before Undo"),
        }
        door::undo(&db, &event_id, &ticket_id, "member", now + 2)
            .await
            .unwrap();
        for (actor_type, actor_id) in [("Door Link", "front"), ("Member", "member")] {
            assert!(matches!(
                door::check_in(
                    &db,
                    &cipher,
                    &event_id,
                    &code,
                    actor_type,
                    actor_id,
                    now + 3
                )
                .await
                .unwrap(),
                CheckinResult::VoidTicket(_)
            ));
        }
        let order = admin_orders::order(&db, &cipher, &order_id).await.unwrap();
        assert_eq!(order.status, "Refunded");
        assert_eq!(order.tickets[0].status, "Void");
        assert_eq!(door::count(&db, &event_id).await.unwrap(), (0, 0));
        let history = db
            .query_all_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT checked_at, actor_type, actor_id, undone_at, undo_member_id FROM checkins WHERE ticket_id = ?",
                [ticket_id.into()],
            ))
            .await
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].try_get::<i64>("", "checked_at").unwrap(), now);
        assert_eq!(
            history[0].try_get::<String>("", "actor_type").unwrap(),
            "Door Link"
        );
        assert_eq!(
            history[0].try_get::<String>("", "actor_id").unwrap(),
            "front"
        );
        assert_eq!(history[0].try_get::<i64>("", "undone_at").unwrap(), now + 2);
        assert_eq!(
            history[0].try_get::<String>("", "undo_member_id").unwrap(),
            "member"
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn refunded_ticket_is_void_on_shared_qr_and_manual_http_route_after_undo() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().route("/v1/refunds", axum::routing::get(|| async {
            axum::Json(serde_json::json!({"has_more":false,"data":[{"id":"re_cg004_synthetic","payment_intent":"pi_cg004_synthetic","amount":2000,"status":"succeeded"}]}))
        }))).await.unwrap();
    });
    for refund_event in [
        None,
        Some("charge.refunded"),
        Some("refund.created"),
        Some("refund.updated"),
    ] {
        loco_rs::testing::prelude::request::<crabgate::app::App, _, _>(|request, ctx| async move {
        let row = ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT id, db_path, data_key FROM organizers WHERE subdomain = 'local'".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        let organizer_id: String = row.try_get("", "id").unwrap();
        let path: String = row.try_get("", "db_path").unwrap();
        let wrapped: String = row.try_get("", "data_key").unwrap();
        let db = sea_orm::Database::connect(format!("sqlite://{path}?mode=rwc"))
            .await
            .unwrap();
        let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
            .unwrap()
            .open_organizer_key(&organizer_id, &wrapped)
            .unwrap();
        let (event_id, ticket_id, code) =
            event_with_ticket(&db, &cipher, "HTTP Refund Night", "Niamh Byrne").await;
        let now = Utc::now().timestamp();
        let link = door::create_link(&db, &cipher, &event_id, "Front", now)
            .await
            .unwrap();
        crabgate::members::add_member(
            &ctx.db,
            &organizer_id,
            "door@example.com",
            crabgate::members::Role::DoorStaff,
        )
        .await
        .unwrap();
        let token =
            crabgate::members::request_magic_link(&ctx.db, &organizer_id, "door@example.com", now)
                .await
                .unwrap()
                .unwrap();
        let session = crabgate::members::claim_magic_link(&ctx.db, &organizer_id, &token, now)
            .await
            .unwrap()
            .unwrap();
        let cookie = format!("cg_session={session}");
        let check_path = format!("/door/{event_id}/check?link={}", link.token);
        let undo_path = format!("/door/{event_id}/undo/{ticket_id}");
        let form = serde_json::json!({"code": code});
        let admitted = request.post(&check_path).form(&form).await;
        assert_eq!(admitted.status_code(), 200);
        assert!(admitted.text().contains("<p class=\"big\">Checked in</p>"));
        let undone = request
            .post(&undo_path)
            .add_header("cookie", cookie.clone())
            .await;
        assert_eq!(undone.status_code(), 303);
        let readmitted = request.post(&check_path).form(&form).await;
        assert_eq!(readmitted.status_code(), 200);
        assert!(readmitted
            .text()
            .contains("<p class=\"big\">Checked in</p>"));
        let order_id: String = db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT order_id FROM tickets WHERE id = ?",
                [ticket_id.clone().into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "order_id")
            .unwrap();
        if let Some(kind) = refund_event {
            use hmac::{Hmac, Mac};
            crabgate::settings::set(&db, &cipher, "stripe_secret_key", "sk_test_cg004_synthetic").await.unwrap();
            crabgate::settings::set(&db, &cipher, "webhook_secret", "whsec_cg004_synthetic")
                .await
                .unwrap();
            db.execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE orders SET total_cents = 2000, stripe_payment_intent_id = 'pi_cg004_synthetic' WHERE id = ?",
                [order_id.clone().into()],
            ))
            .await
            .unwrap();
            let body = serde_json::json!({
                "type": kind,
                "data": {"object": {
                    "id": "re_cg004_synthetic", "payment_intent": "pi_cg004_synthetic",
                    "refunded": true, "status": "succeeded", "amount": 2000
                }}
            })
            .to_string();
            let timestamp = Utc::now().timestamp();
            let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"whsec_cg004_synthetic").unwrap();
            mac.update(format!("{timestamp}.{body}").as_bytes());
            let signature = format!("t={timestamp},v1={}", hex::encode(mac.finalize().into_bytes()));
            let refunded = request.post("/webhooks/stripe")
                .add_header("Stripe-Signature", signature)
                .text(body)
                .await;
            assert_eq!(refunded.status_code(), 200);
        } else {
            admin_orders::refund(&db, &order_id, None, now + 1)
                .await
                .unwrap();
        }
        let refunded_order = admin_orders::order(&db, &cipher, &order_id).await.unwrap();
        assert_eq!(refunded_order.status, "Refunded");
        assert_eq!(refunded_order.tickets[0].status, "Void");
        let undone = request
            .post(&undo_path)
            .add_header("cookie", cookie.clone())
            .await;
        assert_eq!(undone.status_code(), 303);
        let scanned = request.post(&check_path).form(&form).await;
        assert_eq!(scanned.status_code(), 200);
        assert!(scanned.text().contains("<p class=\"big\">Void Ticket</p>"));
        let manual = request
            .post(&format!("/door/{event_id}/check"))
            .add_header("cookie", cookie.clone())
            .form(&form)
            .await;
        assert_eq!(manual.status_code(), 200);
        assert!(manual.text().contains("<p class=\"big\">Void Ticket</p>"));
        assert_eq!(door::count(&db, &event_id).await.unwrap(), (0, 0));
        let history = db
            .query_all_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT actor_id, undone_at, undo_member_id FROM checkins WHERE ticket_id = ?",
                [ticket_id.into()],
            ))
            .await
            .unwrap();
        assert_eq!(history.len(), 2);
        for row in history {
            assert_eq!(row.try_get::<String>("", "actor_id").unwrap(), link.id);
            assert!(row
                .try_get::<Option<i64>>("", "undone_at")
                .unwrap()
                .is_some());
            assert!(row
                .try_get::<Option<String>>("", "undo_member_id")
                .unwrap()
                .is_some());
        }
    })
    .await;
    }
    server.abort();
    if let Some(value) = prior {
        std::env::set_var("STRIPE_API_BASE", value);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }
}

#[tokio::test]
async fn legacy_valid_ticket_on_refunded_order_cannot_reenter_after_undo() {
    let (_dir, db, cipher) = setup().await;
    let (event_id, ticket_id, code) =
        event_with_ticket(&db, &cipher, "Legacy Refund Night", "Niamh Byrne").await;
    let now = Utc::now().timestamp();
    assert!(matches!(
        door::check_in(&db, &cipher, &event_id, &code, "Member", "member", now)
            .await
            .unwrap(),
        CheckinResult::CheckedIn(_)
    ));
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE orders SET status = 'Refunded', refunded_at = ? WHERE id = (SELECT order_id FROM tickets WHERE id = ?)",
        [now.into(), ticket_id.clone().into()],
    ))
    .await
    .unwrap();
    door::undo(&db, &event_id, &ticket_id, "member", now + 1)
        .await
        .unwrap();
    for actor in ["Door Link", "Member"] {
        assert!(matches!(
            door::check_in(&db, &cipher, &event_id, &code, actor, "front", now + 2)
                .await
                .unwrap(),
            CheckinResult::VoidTicket(_)
        ));
    }
}
