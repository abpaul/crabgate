use chrono::Utc;
use crabgate::{
    app::App,
    crypto::OrganizerCipher,
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, Selection},
    members::{self, Role},
    orders::{self, PaymentOutcome},
};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

fn registry(
    ctx: &loco_rs::app::AppContext,
) -> std::sync::Arc<crabgate::organizers::OrganizerRegistry> {
    ctx.shared_store
        .get::<std::sync::Arc<crabgate::initializers::organizers::OrganizerRuntime>>()
        .unwrap()
        .registry()
}

async fn make_event(db: &DatabaseConnection, title: &str) -> events::Event {
    events::create(
        db,
        &EventInput {
            title: title.into(),
            date: "2027-06-15".into(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Harbour Hall".into(),
            address: "1 Main Street".into(),
            description: String::new(),
            event_capacity: Some(100),
            max_per_order: 10,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap()
}

async fn make_order(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event: &events::Event,
) -> orders::Order {
    let now = Utc::now().timestamp();
    let mut selections = Vec::new();
    for name in ["General admission", "Balcony"] {
        let kind = events::add_ticket_type(
            db,
            event,
            &TicketTypeInput {
                name: name.into(),
                price: "0".into(),
                capacity: 50,
                sales_open: None,
                sales_close: None,
            },
        )
        .await
        .unwrap();
        selections.push(Selection {
            ticket_type_id: kind.id,
            quantity: 1,
        });
    }
    events::publish(db, &event.id, now).await.unwrap();
    let hold = inventory::reserve(db, &event.id, &selections, now)
        .await
        .unwrap();
    let details = orders::hold_details(db, &hold.id).await.unwrap();
    orders::save_details(
        db,
        cipher,
        &details,
        "buyer@example.com",
        &["Niamh Byrne".into(), "Niamh Byrne".into()],
        false,
        now,
    )
    .await
    .unwrap();
    let session = format!("free_{}", hold.id);
    orders::attach_session(db, &hold.id, &session, now)
        .await
        .unwrap();
    let PaymentOutcome::Paid(id) = orders::complete_payment(db, &session, "", 0, now)
        .await
        .unwrap()
    else {
        panic!("Expected paid free order")
    };
    orders::get_order(db, cipher, &id).await.unwrap()
}

#[tokio::test]
#[serial]
async fn cg013_magic_link_lands_staff_in_permitted_event_list() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = registry(&ctx).resolve("localhost").await.unwrap();
        let event = make_event(&scope.db, "Door Night").await;
        make_order(
            &scope.db,
            &registry(&ctx).cipher(&scope.organizer).unwrap(),
            &event,
        )
        .await;
        members::add_member(
            &ctx.db,
            &scope.organizer.id,
            "door@example.com",
            Role::DoorStaff,
        )
        .await
        .unwrap();
        let token = members::request_magic_link(
            &ctx.db,
            &scope.organizer.id,
            "door@example.com",
            Utc::now().timestamp(),
        )
        .await
        .unwrap()
        .unwrap();
        let claimed = request.get(&format!("/office/magic-link/{token}")).await;
        assert_eq!(claimed.status_code(), 303);
        let location = claimed.header("location").to_str().unwrap().to_owned();
        let cookie = claimed
            .header("set-cookie")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let landing = request.get(&location).add_header("cookie", cookie).await;
        println!(
            "Staff magic link -> {location} -> {}",
            landing.status_code()
        );
        assert_eq!(location, "/door");
        assert_eq!(landing.status_code(), 200);
        assert!(landing.text().contains("Door Night"));
        assert!(landing.text().contains(&format!("/door/{}", event.id)));
        assert!(landing.text().contains("Harbour Hall"));
        assert!(!landing.text().contains("Revenue"));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg013_lookup_shows_order_and_admission_context() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = registry(&ctx).resolve("localhost").await.unwrap();
        let cipher = registry(&ctx).cipher(&scope.organizer).unwrap();
        let event = make_event(&scope.db, "Lookup Night").await;
        let order = make_order(&scope.db, &cipher, &event).await;
        let link = crabgate::door::create_link(
            &scope.db,
            &cipher,
            &event.id,
            "Front",
            Utc::now().timestamp(),
        )
        .await
        .unwrap();
        let found = request
            .get(&format!(
                "/door/{}/lookup?link={}&q=Niamh",
                event.id, link.token
            ))
            .await;
        assert_eq!(found.status_code(), 200);
        assert!(found.text().contains("Niamh Byrne"));
        assert!(found.text().contains("General admission"));
        assert!(found.text().contains("Balcony"));
        assert!(found.text().contains(&format!("Order {}", order.number)));
        assert!(found.text().contains("Order status: Paid"));
        assert!(found.text().contains("Ticket status: Valid"));
        assert!(found.text().contains("Ready to check in"));
    })
    .await;
}

async fn session(
    ctx: &loco_rs::app::AppContext,
    organizer: &str,
    email: &str,
    role: Role,
) -> String {
    members::add_member(&ctx.db, organizer, email, role)
        .await
        .unwrap();
    let now = Utc::now().timestamp();
    let token = members::request_magic_link(&ctx.db, organizer, email, now)
        .await
        .unwrap()
        .unwrap();
    let value = members::claim_magic_link(&ctx.db, organizer, &token, now)
        .await
        .unwrap()
        .unwrap();
    format!("cg_session={value}")
}

#[tokio::test]
#[serial]
async fn cg013_admin_destination_and_staff_finance_boundaries() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = registry(&ctx).resolve("localhost").await.unwrap();
        let event = make_event(&scope.db, "Boundary Night").await;
        let cipher = registry(&ctx).cipher(&scope.organizer).unwrap();
        let order = make_order(&scope.db, &cipher, &event).await;
        members::add_member(
            &ctx.db,
            &scope.organizer.id,
            "admin@example.com",
            Role::Admin,
        )
        .await
        .unwrap();
        let token = members::request_magic_link(
            &ctx.db,
            &scope.organizer.id,
            "admin@example.com",
            Utc::now().timestamp(),
        )
        .await
        .unwrap()
        .unwrap();
        let claimed = request.get(&format!("/office/magic-link/{token}")).await;
        assert_eq!(claimed.status_code(), 303);
        assert_eq!(
            claimed.header("location").to_str().unwrap(),
            "/office/events"
        );
        let admin = claimed
            .header("set-cookie")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let office = request
            .get("/office/events")
            .add_header("cookie", &admin)
            .await;
        assert_eq!(office.status_code(), 200);
        assert!(office.text().contains("Signed in as Admin"));
        assert_eq!(
            request
                .get("/door")
                .add_header("cookie", &admin)
                .await
                .status_code(),
            200
        );
        let staff = session(
            &ctx,
            &scope.organizer.id,
            "door@example.com",
            Role::DoorStaff,
        )
        .await;
        for path in [
            "/office/events".to_owned(),
            "/office/orders".to_owned(),
            format!("/office/orders/{}", order.id),
            "/office/door".to_owned(),
            "/office/setup".to_owned(),
            "/office/data/export".to_owned(),
            format!("/office/events/{}/export/orders.csv", event.id),
            format!("/office/events/{}/export/tickets.csv", event.id),
            format!("/office/events/{}/export/checkins.csv", event.id),
        ] {
            assert_eq!(
                request
                    .get(&path)
                    .add_header("cookie", &staff)
                    .await
                    .status_code(),
                403,
                "{path}"
            );
        }
        assert_eq!(
            request
                .post(&format!("/office/orders/{}/refund", order.id))
                .add_header("cookie", &staff)
                .await
                .status_code(),
            403
        );
        assert_eq!(
            request
                .post("/office/door/links")
                .add_header("cookie", &staff)
                .form(&[("label", "Forbidden"), ("event_id", event.id.as_str())])
                .await
                .status_code(),
            403
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg013_expired_reused_and_removed_membership_links_do_not_sign_in() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = registry(&ctx).resolve("localhost").await.unwrap();
        let now = Utc::now().timestamp();
        members::add_member(
            &ctx.db,
            &scope.organizer.id,
            "expired@example.com",
            Role::DoorStaff,
        )
        .await
        .unwrap();
        let expired = members::request_magic_link(
            &ctx.db,
            &scope.organizer.id,
            "expired@example.com",
            now - 901,
        )
        .await
        .unwrap()
        .unwrap();
        let response = request.get(&format!("/office/magic-link/{expired}")).await;
        assert_eq!(response.status_code(), 401);
        assert!(response.text().contains("expired or was already used"));
        assert!(!response.headers().contains_key("set-cookie"));
        let member = members::add_member(
            &ctx.db,
            &scope.organizer.id,
            "removed@example.com",
            Role::DoorStaff,
        )
        .await
        .unwrap();
        let removed =
            members::request_magic_link(&ctx.db, &scope.organizer.id, "removed@example.com", now)
                .await
                .unwrap()
                .unwrap();
        assert!(
            members::remove_member(&ctx.db, &scope.organizer.id, &member)
                .await
                .unwrap()
        );
        let response = request.get(&format!("/office/magic-link/{removed}")).await;
        assert_eq!(response.status_code(), 401);
        assert!(!response.headers().contains_key("set-cookie"));
        members::add_member(
            &ctx.db,
            &scope.organizer.id,
            "reuse@example.com",
            Role::DoorStaff,
        )
        .await
        .unwrap();
        let token =
            members::request_magic_link(&ctx.db, &scope.organizer.id, "reuse@example.com", now)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            request
                .get(&format!("/office/magic-link/{token}"))
                .await
                .status_code(),
            303
        );
        let response = request.get(&format!("/office/magic-link/{token}")).await;
        assert_eq!(response.status_code(), 401);
        assert!(response.text().contains("expired or was already used"));
        assert!(!response.headers().contains_key("set-cookie"));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg013_event_list_validates_current_membership_and_session() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = registry(&ctx).resolve("localhost").await.unwrap();
        let event = make_event(&scope.db, "Session Night").await;
        make_order(
            &scope.db,
            &registry(&ctx).cipher(&scope.organizer).unwrap(),
            &event,
        )
        .await;
        assert_eq!(request.get("/door").await.status_code(), 303);
        let staff = session(
            &ctx,
            &scope.organizer.id,
            "change@example.com",
            Role::DoorStaff,
        )
        .await;
        assert!(request
            .get("/door")
            .add_header("cookie", &staff)
            .await
            .text()
            .contains("Signed in as Door Staff"));
        members::add_member(
            &ctx.db,
            &scope.organizer.id,
            "change@example.com",
            Role::Admin,
        )
        .await
        .unwrap();
        assert_eq!(
            request
                .get("/office/events")
                .add_header("cookie", &staff)
                .await
                .status_code(),
            200
        );
        members::add_member(
            &ctx.db,
            &scope.organizer.id,
            "otheradmin@example.com",
            Role::Admin,
        )
        .await
        .unwrap();
        let id = members::add_member(
            &ctx.db,
            &scope.organizer.id,
            "change@example.com",
            Role::DoorStaff,
        )
        .await
        .unwrap();
        assert_eq!(
            request
                .get("/office/events")
                .add_header("cookie", &staff)
                .await
                .status_code(),
            403
        );
        assert!(request
            .get("/door")
            .add_header("cookie", &staff)
            .await
            .text()
            .contains("Signed in as Door Staff"));
        assert!(members::remove_member(&ctx.db, &scope.organizer.id, &id)
            .await
            .unwrap());
        assert_eq!(
            request
                .get("/door")
                .add_header("cookie", &staff)
                .await
                .status_code(),
            303
        );
        assert_eq!(
            request
                .get(&format!("/door/{}", event.id))
                .add_header("cookie", &staff)
                .await
                .status_code(),
            403
        );
        let revoked = session(
            &ctx,
            &scope.organizer.id,
            "revoked@example.com",
            Role::DoorStaff,
        )
        .await;
        members::revoke_session(
            &ctx.db,
            &scope.organizer.id,
            revoked.strip_prefix("cg_session=").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            request
                .get("/door")
                .add_header("cookie", &revoked)
                .await
                .status_code(),
            303
        );
        let expired = session(
            &ctx,
            &scope.organizer.id,
            "sessionexpired@example.com",
            Role::DoorStaff,
        )
        .await;
        ctx.db
            .execute_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "UPDATE member_sessions SET expires_at = 1",
            ))
            .await
            .unwrap();
        assert_eq!(
            request
                .get("/door")
                .add_header("cookie", &expired)
                .await
                .status_code(),
            303
        );
        assert_eq!(
            request
                .get(&format!("/door/{}/lookup?q=Niamh", event.id))
                .add_header("cookie", &expired)
                .await
                .status_code(),
            403
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg013_empty_list_excludes_draft_cancelled_and_ended_events() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = registry(&ctx).resolve("localhost").await.unwrap();
        let staff = session(
            &ctx,
            &scope.organizer.id,
            "door@example.com",
            Role::DoorStaff,
        )
        .await;
        let empty = request.get("/door").add_header("cookie", &staff).await;
        assert_eq!(empty.status_code(), 200);
        assert!(empty.text().contains("No events are ready for check-in"));
        for (title, status, end) in [
            ("Hidden draft", "Draft", 1900000000_i64),
            ("Hidden cancelled", "Cancelled", 1900000000),
            ("Hidden ended", "Published", 1),
        ] {
            let event = make_event(&scope.db, title).await;
            scope
                .db
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "UPDATE events SET status = ?, ends_at = ? WHERE id = ?",
                    [status.into(), end.into(), event.id.into()],
                ))
                .await
                .unwrap();
        }
        let empty = request.get("/door").add_header("cookie", &staff).await;
        assert_eq!(empty.status_code(), 200);
        assert!(empty.text().contains("No events are ready for check-in"));
        assert!(!empty.text().contains("Hidden"));
        assert!(empty.text().contains("Sign out"));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg013_lookup_status_and_two_devices_share_atomic_admission_and_undo() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = registry(&ctx).resolve("localhost").await.unwrap();
        let cipher = registry(&ctx).cipher(&scope.organizer).unwrap();
        let event = make_event(&scope.db, "Two devices").await;
        let order = make_order(&scope.db, &cipher, &event).await;
        let a = session(
            &ctx,
            &scope.organizer.id,
            "devicea@example.com",
            Role::DoorStaff,
        )
        .await;
        let b = session(
            &ctx,
            &scope.organizer.id,
            "deviceb@example.com",
            Role::DoorStaff,
        )
        .await;
        let lookup = format!("/door/{}/lookup", event.id);
        let found = request
            .get(&format!("{lookup}?q=buyer%40example.com"))
            .add_header("cookie", &a)
            .await;
        assert_eq!(found.status_code(), 200);
        assert!(found.text().contains("2 tickets match"));
        assert!(found.text().contains("Buyer: buyer@example.com"));
        assert!(!found.text().contains("total_cents"));
        assert!(!found.text().contains("€"));
        let missing = request
            .get(&format!("{lookup}?q=Nobody"))
            .add_header("cookie", &a)
            .await;
        assert!(missing.text().contains("No tickets found"));
        let short = request
            .get(&format!("{lookup}?q=N"))
            .add_header("cookie", &a)
            .await;
        assert!(short.text().contains("Enter at least 2 characters"));
        assert!(!short.text().contains("No tickets found"));
        let ticket = &order.tickets[0];
        let path = format!("/door/{}/check", event.id);
        let (first, second) = tokio::join!(
            request
                .post(&path)
                .add_header("cookie", &a)
                .form(&[("code", &ticket.code)]),
            request
                .post(&path)
                .add_header("cookie", &b)
                .form(&[("code", &ticket.code)])
        );
        assert_eq!(first.status_code(), 200);
        assert_eq!(second.status_code(), 200);
        let titles = [first.text(), second.text()];
        assert_eq!(
            titles
                .iter()
                .filter(|text| text.contains("<p class=\"big\">Checked in</p>"))
                .count(),
            1
        );
        assert_eq!(
            titles
                .iter()
                .filter(|text| text.contains("<p class=\"big\">Already checked in</p>"))
                .count(),
            1
        );
        let active = scope
            .db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM checkins WHERE ticket_id = ? AND undone_at IS NULL",
                [ticket.id.clone().into()],
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(active.try_get::<i64>("", "count").unwrap(), 1);
        let found = request
            .get(&format!("{lookup}?q={}", ticket.code))
            .add_header("cookie", &b)
            .await;
        assert!(found.text().contains("Already checked in"));
        assert!(found.text().contains("Checked in at"));
        assert!(found.text().contains(">Undo</button>"));
        assert!(!found.text().contains(">Check in</button>"));
        crabgate::admin_orders::mark_refunded(&scope.db, &order.id, Utc::now().timestamp())
            .await
            .unwrap();
        let found = request
            .get(&format!("{lookup}?q={}", ticket.code))
            .add_header("cookie", &a)
            .await;
        assert!(found.text().contains("Order status: Refunded"));
        assert!(found.text().contains("Ticket status: Void"));
        assert!(found.text().contains("Cannot admit"));
        let undone = request
            .post(&format!("/door/{}/undo/{}", event.id, ticket.id))
            .add_header("cookie", &b)
            .await;
        assert_eq!(undone.status_code(), 303);
        let feedback = request
            .get(undone.header("location").to_str().unwrap())
            .add_header("cookie", &b)
            .await;
        assert!(feedback
            .text()
            .contains("Check-in undone. Ticket and order status still apply."));
        let found = request
            .get(&format!("{lookup}?q={}", ticket.code))
            .add_header("cookie", &b)
            .await;
        assert!(found.text().contains("Cannot admit"));
        assert!(!found.text().contains(">Check in</button>"));
        let denied = request
            .post(&path)
            .add_header("cookie", &a)
            .form(&[("code", &ticket.code)])
            .await;
        assert!(denied.text().contains("<p class=\"big\">Void Ticket</p>"));
        assert_eq!(
            crabgate::door::count(&scope.db, &event.id).await.unwrap(),
            (0, 0)
        );
        let unknown = request
            .post(&path)
            .add_header("cookie", &a)
            .form(&[("code", "UNKNOWN-CODE")])
            .await;
        assert!(unknown.text().contains("<p class=\"big\">Unknown code</p>"));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg013_cancelled_event_lookup_never_offers_admission() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = registry(&ctx).resolve("localhost").await.unwrap();
        let event = make_event(&scope.db, "Cancelled Night").await;
        let order = make_order(
            &scope.db,
            &registry(&ctx).cipher(&scope.organizer).unwrap(),
            &event,
        )
        .await;
        let staff = session(
            &ctx,
            &scope.organizer.id,
            "door@example.com",
            Role::DoorStaff,
        )
        .await;
        crabgate::admin_orders::cancel_event(&scope.db, &event.id, Utc::now().timestamp())
            .await
            .unwrap();
        let found = request
            .get(&format!("/door/{}/lookup?q=Niamh", event.id))
            .add_header("cookie", &staff)
            .await;
        assert_eq!(found.status_code(), 200);
        assert!(found.text().contains("Event cancelled"));
        assert!(!found.text().contains(">Check in</button>"));
        let response = request
            .post(&format!("/door/{}/check", event.id))
            .add_header("cookie", &staff)
            .form(&[("code", &order.tickets[0].code)])
            .await;
        assert!(response.text().contains("<p class=\"big\">Void Ticket</p>"));
        assert_eq!(
            crabgate::door::count(&scope.db, &event.id).await.unwrap().0,
            0
        );
    })
    .await;
}

struct EditionEnvironment(Option<String>);

impl EditionEnvironment {
    fn hosted() -> Self {
        let prior = std::env::var("CRABGATE_EDITION").ok();
        std::env::set_var("CRABGATE_EDITION", "hosted");
        Self(prior)
    }
}

impl Drop for EditionEnvironment {
    fn drop(&mut self) {
        match &self.0 {
            Some(value) => std::env::set_var("CRABGATE_EDITION", value),
            None => std::env::remove_var("CRABGATE_EDITION"),
        }
    }
}

#[tokio::test]
#[serial]
async fn cg013_hosted_staff_link_session_and_lookup_stay_in_organizer() {
    let _edition = EditionEnvironment::hosted();
    request::<App, _, _>(|request, ctx| async move {
        let registry = registry(&ctx);
        let blue = registry.create("Blue Arts", "blue-door").await.unwrap();
        let red = registry.create("Red Arts", "red-door").await.unwrap();
        let blue_scope = registry.resolve("blue-door.crabgate.com").await.unwrap();
        let red_scope = registry.resolve("red-door.crabgate.com").await.unwrap();
        let blue_event = make_event(&blue_scope.db, "Blue Night").await;
        make_order(
            &blue_scope.db,
            &registry.cipher(&blue).unwrap(),
            &blue_event,
        )
        .await;
        let red_event = make_event(&red_scope.db, "Red Night").await;
        make_order(&red_scope.db, &registry.cipher(&red).unwrap(), &red_event).await;
        members::add_member(&ctx.db, &blue.id, "blue@example.com", Role::DoorStaff)
            .await
            .unwrap();
        let token = members::request_magic_link(
            &ctx.db,
            &blue.id,
            "blue@example.com",
            Utc::now().timestamp(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            request
                .get(&format!("/office/magic-link/{token}"))
                .add_header("host", "red-door.crabgate.com")
                .await
                .status_code(),
            401
        );
        let claimed = request
            .get(&format!("/office/magic-link/{token}"))
            .add_header("host", "blue-door.crabgate.com")
            .await;
        assert_eq!(claimed.status_code(), 303);
        assert_eq!(claimed.header("location").to_str().unwrap(), "/door");
        let cookie = claimed
            .header("set-cookie")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let blue_list = request
            .get("/door")
            .add_header("host", "blue-door.crabgate.com")
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(blue_list.status_code(), 200);
        assert!(blue_list.text().contains("Blue Night"));
        assert!(!blue_list.text().contains("Red Night"));
        let cross_list = request
            .get("/door")
            .add_header("host", "red-door.crabgate.com")
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(cross_list.status_code(), 303);
        assert_eq!(
            cross_list.header("location").to_str().unwrap(),
            "/office/sign-in"
        );
        let cross_lookup = request
            .get(&format!("/door/{}/lookup?q=Niamh", red_event.id))
            .add_header("host", "red-door.crabgate.com")
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(cross_lookup.status_code(), 403);
        assert!(!cross_lookup.text().contains("buyer@example.com"));
        assert_eq!(
            request
                .get("/office/data/export")
                .add_header("host", "blue-door.crabgate.com")
                .add_header("cookie", &cookie)
                .await
                .status_code(),
            403
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg013_operator_grant_does_not_become_door_membership() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = registry(&ctx).resolve("localhost").await.unwrap();
        let event = make_event(&scope.db, "Operator boundary").await;
        make_order(
            &scope.db,
            &registry(&ctx).cipher(&scope.organizer).unwrap(),
            &event,
        )
        .await;
        let member = members::add_member(
            &ctx.db,
            &scope.organizer.id,
            "admin@example.com",
            Role::Admin,
        )
        .await
        .unwrap();
        ctx.db
            .execute_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "INSERT INTO operators (id, email) VALUES ('support', 'support@example.com')",
            ))
            .await
            .unwrap();
        let now = Utc::now().timestamp();
        let token = crabgate::operator::request_link(&ctx.db, "support@example.com", now)
            .await
            .unwrap()
            .unwrap();
        let session = crabgate::operator::claim_link(&ctx.db, &token, now)
            .await
            .unwrap()
            .unwrap();
        let cookie = format!("cg_operator_session={session}");
        crabgate::operator::grant(&ctx.db, &scope.organizer.id, &member, 1, now)
            .await
            .unwrap();
        assert_eq!(
            request
                .get("/office/events")
                .add_header("cookie", &cookie)
                .await
                .status_code(),
            200
        );
        assert_eq!(
            request
                .get("/door")
                .add_header("cookie", &cookie)
                .await
                .status_code(),
            303
        );
        assert_eq!(
            request
                .get(&format!("/door/{}/lookup?q=Niamh", event.id))
                .add_header("cookie", &cookie)
                .await
                .status_code(),
            403
        );
    })
    .await;
}
