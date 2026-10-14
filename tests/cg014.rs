use chrono::Utc;
use crabgate::{
    admin_orders,
    app::App,
    crypto::OrganizerCipher,
    door, event_notices,
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, Selection},
    members::{self, Role},
    orders::{self, PaymentOutcome},
};
use loco_rs::{app::AppContext, testing::prelude::*};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use std::sync::{Arc, Mutex};

fn registry(ctx: &AppContext) -> Arc<crabgate::organizers::OrganizerRegistry> {
    ctx.shared_store
        .get::<Arc<crabgate::initializers::organizers::OrganizerRuntime>>()
        .unwrap()
        .registry()
}

fn input() -> EventInput {
    EventInput {
        title: "Notice Night".into(),
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
    }
}

async fn event(db: &DatabaseConnection) -> (events::Event, String, String) {
    let event = events::create(db, &input()).await.unwrap();
    let mut kinds = Vec::new();
    for price in ["10", "0"] {
        kinds.push(
            events::add_ticket_type(
                db,
                &event,
                &TicketTypeInput {
                    name: format!("Admission {price}"),
                    price: price.into(),
                    capacity: 100,
                    sales_open: None,
                    sales_close: None,
                },
            )
            .await
            .unwrap()
            .id,
        );
    }
    events::publish(db, &event.id, Utc::now().timestamp())
        .await
        .unwrap();
    (event, kinds.remove(0), kinds.remove(0))
}

async fn comp(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event: &str,
    kind: &str,
    email: &str,
) -> String {
    admin_orders::complimentary(
        db,
        cipher,
        event,
        kind,
        email,
        "Guest",
        Utc::now().timestamp(),
    )
    .await
    .unwrap()
}

async fn purchase(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event: &str,
    kind: &str,
    email: &str,
    cents: i64,
    quantity: i64,
) -> String {
    let now = Utc::now().timestamp();
    let hold = inventory::reserve(
        db,
        event,
        &[Selection {
            ticket_type_id: kind.into(),
            quantity,
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
        email,
        &vec![String::new(); quantity as usize],
        false,
        now,
    )
    .await
    .unwrap();
    let session = format!("cg014_{}", hold.id);
    orders::attach_session(db, &hold.id, &session, now)
        .await
        .unwrap();
    let intent = if cents == 0 {
        String::new()
    } else {
        format!("pi_synthetic_{}", hold.id)
    };
    let PaymentOutcome::Paid(id) = orders::complete_payment(db, &session, &intent, cents, now)
        .await
        .unwrap()
    else {
        panic!("Expected issued Order")
    };
    id
}

async fn counts(db: &DatabaseConnection) -> (i64, i64, i64) {
    let row = db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT (SELECT COUNT(*) FROM event_change_notices) AS notices, (SELECT COUNT(*) FROM event_change_recipients) AS recipients, (SELECT COUNT(*) FROM event_change_recipients WHERE sent_at IS NOT NULL) AS sent".to_owned())).await.unwrap().unwrap();
    (
        row.try_get("", "notices").unwrap(),
        row.try_get("", "recipients").unwrap(),
        row.try_get("", "sent").unwrap(),
    )
}

async fn sql(db: &DatabaseConnection, statement: &str, id: &str) {
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        statement,
        [id.into()],
    ))
    .await
    .unwrap();
}

fn assert_notice(message: &str, email: &str, number: &str) {
    for expected in [
        format!("To: {email}"),
        format!("order {number}"),
        "Previous time: 2027-06-15 19:00 to 21:00 (Europe/Dublin)".into(),
        "New time: 2027-06-16 20:00 to 22:00 (Europe/Dublin)".into(),
        "Previous venue: Harbour Hall, 1 Main Street".into(),
        "New venue: New Hall, 2 Main Street".into(),
    ] {
        assert!(
            message.contains(&expected),
            "Missing literal {expected:?}: {message}"
        );
    }
}

fn changed() -> EventInput {
    EventInput {
        date: "2027-06-16".into(),
        start_time: "20:00".into(),
        end_time: "22:00".into(),
        venue: "New Hall".into(),
        address: "2 Main Street".into(),
        ..input()
    }
}

#[tokio::test]
#[serial]
async fn complimentary_issuance_and_event_edit_routes_deliver_without_recording_a_sale() {
    request::<App, _, _>(|request, ctx| async move {
        let registry = registry(&ctx);
        let scope = registry.resolve("localhost").await.unwrap();
        let (event, kind, _) = event(&scope.db).await;
        members::add_member(
            &ctx.db,
            &scope.organizer.id,
            "admin@example.com",
            Role::Admin,
        )
        .await
        .unwrap();
        let session = members::create_session_for_email(
            &ctx.db,
            &scope.organizer.id,
            "admin@example.com",
            Utc::now().timestamp(),
        )
        .await
        .unwrap()
        .unwrap();
        let cookie = format!("cg_session={session}");
        let issued = request
            .post("/office/orders/complimentary")
            .add_header("cookie", &cookie)
            .form(&[
                ("ticket_type_id", kind.as_str()),
                ("email", "comp@example.com"),
                ("name", "Guest"),
            ])
            .await;
        assert_eq!(issued.status_code(), 303);
        let location = issued.header("location").to_str().unwrap().to_owned();
        let order = admin_orders::order(
            &scope.db,
            &registry.cipher(&scope.organizer).unwrap(),
            location.strip_prefix("/office/orders/").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(order.total_cents, 0);
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 1);
        assert_eq!(
            events::get(&scope.db, &event.id)
                .await
                .unwrap()
                .unwrap()
                .first_sale_at,
            None
        );
        let form = [
            ("title", "Notice Night"),
            ("date", "2027-06-16"),
            ("start_time", "20:00"),
            ("end_time", "22:00"),
            ("timezone", "Europe/Dublin"),
            ("venue", "New Hall"),
            ("address", "2 Main Street"),
            ("max_per_order", "10"),
            ("event_capacity", "100"),
        ];
        for _ in 0..2 {
            assert_eq!(
                request
                    .post(&format!("/office/events/{}", event.id))
                    .add_header("cookie", &cookie)
                    .form(&form)
                    .await
                    .status_code(),
                303
            );
        }
        assert_eq!(counts(&scope.db).await, (1, 1, 1));
        let deliveries = ctx.mailer.as_ref().unwrap().deliveries();
        assert_eq!(deliveries.count, 2);
        assert_notice(&deliveries.messages[1], "comp@example.com", &order.number);
        assert_eq!(
            events::get(&scope.db, &event.id)
                .await
                .unwrap()
                .unwrap()
                .first_sale_at,
            None
        );
        event_notices::run(&ctx, &registry).await.unwrap();
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 2);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn comp_only_time_and_location_edits_each_deliver_their_own_diff() {
    request::<App, _, _>(|_, ctx| async move {
        let registry = registry(&ctx);
        let scope = registry.resolve("localhost").await.unwrap();
        let db = &scope.db;
        let cipher = registry.cipher(&scope.organizer).unwrap();
        let (event, kind, _) = event(db).await;
        let id = comp(db, &cipher, &event.id, &kind, "buyer@example.com").await;
        let order = orders::get_order(db, &cipher, &id).await.unwrap();
        let mut edit = input();
        edit.start_time = "20:00".into();
        edit.end_time = "22:00".into();
        events::update(db, &event.id, &edit).await.unwrap();
        event_notices::run(&ctx, &registry).await.unwrap();
        assert_eq!(counts(db).await, (1, 1, 1));
        edit.venue = "New Hall".into();
        events::update(db, &event.id, &edit).await.unwrap();
        event_notices::run(&ctx, &registry).await.unwrap();
        assert_eq!(counts(db).await, (2, 2, 2));
        let messages = ctx.mailer.as_ref().unwrap().deliveries();
        assert_eq!(messages.count, 2);
        for message in &messages.messages {
            assert!(message.contains("To: buyer@example.com"));
            assert!(message.contains(&format!("order {}", order.number)));
        }
        for expected in [
            "Previous time: 2027-06-15 19:00 to 21:00 (Europe/Dublin)",
            "New time: 2027-06-15 20:00 to 22:00 (Europe/Dublin)",
            "Previous venue: Harbour Hall, 1 Main Street",
            "New venue: Harbour Hall, 1 Main Street",
        ] {
            assert!(
                messages.messages[0].contains(expected),
                "Missing {expected}"
            );
        }
        for expected in [
            "Previous time: 2027-06-15 20:00 to 22:00 (Europe/Dublin)",
            "New time: 2027-06-15 20:00 to 22:00 (Europe/Dublin)",
            "Previous venue: Harbour Hall, 1 Main Street",
            "New venue: New Hall, 1 Main Street",
        ] {
            assert!(
                messages.messages[1].contains(expected),
                "Missing {expected}"
            );
        }
        assert_eq!(
            events::get(db, &event.id)
                .await
                .unwrap()
                .unwrap()
                .first_sale_at,
            None
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn mixed_orders_use_valid_tickets_and_preserve_distinct_order_messages() {
    request::<App, _, _>(|_, ctx| async move {
        let registry = registry(&ctx);
        let scope = registry.resolve("localhost").await.unwrap();
        let db = &scope.db;
        let cipher = registry.cipher(&scope.organizer).unwrap();
        let (event, paid, free) = event(db).await;
        let paid_order = purchase(db, &cipher, &event.id, &paid, "same@example.com", 2000, 2).await;
        let free_order = purchase(db, &cipher, &event.id, &free, "free@example.com", 0, 1).await;
        let comp_order = comp(db, &cipher, &event.id, &paid, "same@example.com").await;
        let pending = comp(db, &cipher, &event.id, &paid, "pending@example.com").await;
        sql(db, "UPDATE orders SET status = 'RefundPending' WHERE id = ?", &pending).await;
        let refunded = comp(db, &cipher, &event.id, &paid, "refunded@example.com").await;
        admin_orders::refund(db, &refunded, None, Utc::now().timestamp()).await.unwrap();
        let void = comp(db, &cipher, &event.id, &paid, "void@example.com").await;
        sql(db, "UPDATE tickets SET status = 'Void' WHERE order_id = ?", &void).await;
        let ticketless = comp(db, &cipher, &event.id, &paid, "ticketless@example.com").await;
        sql(db, "DELETE FROM tickets WHERE order_id = ?", &ticketless).await;
        db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "UPDATE tickets SET status = 'Void' WHERE id = (SELECT id FROM tickets WHERE order_id = ? ORDER BY id LIMIT 1)", [paid_order.clone().into()])).await.unwrap();
        let ticket = orders::get_order(db, &cipher, &free_order).await.unwrap().tickets.remove(0);
        assert!(matches!(door::check_in(db, &cipher, &event.id, &ticket.code, "Member", "cg014", Utc::now().timestamp()).await.unwrap(), door::CheckinResult::CheckedIn(_)));
        let first_sale = events::get(db, &event.id).await.unwrap().unwrap().first_sale_at;
        assert!(first_sale.is_some());
        events::update(db, &event.id, &changed()).await.unwrap();
        assert_eq!(counts(db).await, (1, 4, 0));
        event_notices::deliver_pending(&ctx, &registry, &scope.organizer, db).await.unwrap();
        assert_eq!(counts(db).await, (1, 4, 4));
        let messages = ctx.mailer.as_ref().unwrap().deliveries();
        assert_eq!(messages.count, 4);
        for (id, email) in [(paid_order, "same@example.com"), (free_order, "free@example.com"), (comp_order, "same@example.com"), (pending, "pending@example.com")] {
            let number = orders::get_order(db, &cipher, &id).await.unwrap().number;
            let matching: Vec<_> = messages.messages.iter().filter(|m| m.contains(&format!("order {number}"))).collect();
            assert_eq!(matching.len(), 1);
            assert_notice(matching[0], email, &number);
        }
        assert_eq!(events::get(db, &event.id).await.unwrap().unwrap().first_sale_at, first_sale);
        event_notices::run(&ctx, &registry).await.unwrap();
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 4);
    }).await;
}

#[tokio::test]
#[serial]
async fn queued_orders_are_rechecked_after_refund_void_or_anonymization() {
    request::<App, _, _>(|_, ctx| async move {
        let registry = registry(&ctx);
        let scope = registry.resolve("localhost").await.unwrap();
        let db = &scope.db;
        let cipher = registry.cipher(&scope.organizer).unwrap();
        let (event, kind, free) = event(db).await;
        let valid = purchase(db, &cipher, &event.id, &free, "valid@example.com", 0, 1).await;
        let refunded = comp(db, &cipher, &event.id, &kind, "refund@example.com").await;
        let void = comp(db, &cipher, &event.id, &kind, "void@example.com").await;
        let anonymous = comp(db, &cipher, &event.id, &kind, "anonymous@example.com").await;
        events::update(db, &event.id, &changed()).await.unwrap();
        assert_eq!(counts(db).await, (1, 4, 0));
        admin_orders::refund(db, &refunded, None, Utc::now().timestamp()).await.unwrap();
        sql(db, "UPDATE tickets SET status = 'Void' WHERE order_id = ?", &void).await;
        admin_orders::anonymize(db, &cipher, &anonymous).await.unwrap();
        event_notices::deliver_pending(&ctx, &registry, &scope.organizer, db).await.unwrap();
        let deliveries = ctx.mailer.as_ref().unwrap().deliveries();
        assert_eq!(deliveries.count, 1);
        assert_notice(&deliveries.messages[0], "valid@example.com", &orders::get_order(db, &cipher, &valid).await.unwrap().number);
        assert_eq!(counts(db).await.2, 1);
        for id in [refunded, void, anonymous] {
            let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS sent FROM event_change_recipients WHERE order_id = ? AND sent_at IS NOT NULL", [id.into()])).await.unwrap().unwrap();
            assert_eq!(row.try_get::<i64>("", "sent").unwrap(), 0);
        }
        event_notices::run(&ctx, &registry).await.unwrap();
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 1);
    }).await;
}

#[tokio::test]
#[serial]
async fn no_tickets_title_only_noop_and_rejected_edits_do_not_queue_notices() {
    request::<App, _, _>(|_, ctx| async move {
        let registry = registry(&ctx);
        let scope = registry.resolve("localhost").await.unwrap();
        let db = &scope.db;
        let (event, kind, _) = event(db).await;
        events::update(db, &event.id, &changed()).await.unwrap();
        assert_eq!(counts(db).await, (0, 0, 0));
        comp(
            db,
            &registry.cipher(&scope.organizer).unwrap(),
            &event.id,
            &kind,
            "buyer@example.com",
        )
        .await;
        let mut title = changed();
        title.title = "New title".into();
        events::update(db, &event.id, &title).await.unwrap();
        events::update(db, &event.id, &title).await.unwrap();
        let mut invalid = title;
        invalid.venue = "Invalid Hall".into();
        invalid.event_capacity = Some(0);
        assert!(events::update(db, &event.id, &invalid).await.is_err());
        assert_eq!(
            events::get(db, &event.id).await.unwrap().unwrap().venue,
            "New Hall"
        );
        assert_eq!(counts(db).await, (0, 0, 0));
        event_notices::run(&ctx, &registry).await.unwrap();
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 0);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cancellation_keeps_its_own_notice_and_suppresses_queued_schedule_mail() {
    request::<App, _, _>(|_, ctx| async move {
        let registry = registry(&ctx);
        let scope = registry.resolve("localhost").await.unwrap();
        let db = &scope.db;
        let (event, kind, _) = event(db).await;
        let id = comp(db, &registry.cipher(&scope.organizer).unwrap(), &event.id, &kind, "buyer@example.com").await;
        events::update(db, &event.id, &changed()).await.unwrap();
        assert_eq!(counts(db).await, (1, 1, 0));
        assert_eq!(admin_orders::cancel_event(db, &event.id, Utc::now().timestamp()).await.unwrap(), vec![id.clone()]);
        let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT o.cancel_notice_state, t.status FROM orders o JOIN tickets t ON t.order_id = o.id WHERE o.id = ?", [id.into()])).await.unwrap().unwrap();
        assert_eq!(row.try_get::<String>("", "cancel_notice_state").unwrap(), "Pending");
        assert_eq!(row.try_get::<String>("", "status").unwrap(), "Valid");
        event_notices::run(&ctx, &registry).await.unwrap();
        assert_eq!(counts(db).await, (1, 1, 0));
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 0);
    }).await;
}

struct SmtpFixture {
    port: u16,
    accepted: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for SmtpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn smtp_refuse_first_message() -> SmtpFixture {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(Mutex::new(Vec::new()));
    let captured = accepted.clone();
    let task = tokio::spawn(async move {
        let mut refuse = true;
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let (read, mut write) = socket.into_split();
            write.write_all(b"220 synthetic CG014\r\n").await.unwrap();
            let mut reader = BufReader::new(read);
            let mut data = false;
            let mut message = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap() == 0 {
                    break;
                }
                if data {
                    if line == ".\r\n" {
                        data = false;
                        if refuse {
                            refuse = false;
                            write
                                .write_all(b"451 synthetic rejection\r\n")
                                .await
                                .unwrap();
                        } else {
                            captured.lock().unwrap().push(message.clone());
                            write.write_all(b"250 accepted\r\n").await.unwrap();
                        }
                        message.clear();
                    } else {
                        message.push_str(&line);
                    }
                } else if line.starts_with("DATA") {
                    data = true;
                    write.write_all(b"354 continue\r\n").await.unwrap();
                } else if line.starts_with("QUIT") {
                    write.write_all(b"221 bye\r\n").await.unwrap();
                    break;
                } else {
                    write.write_all(b"250 OK\r\n").await.unwrap();
                }
            }
        }
    });
    SmtpFixture {
        port,
        accepted,
        task,
    }
}

#[tokio::test]
#[serial]
async fn smtp_rejection_keeps_only_failed_order_pending_with_background_workers() {
    use loco_rs::{app::Hooks, boot::StartMode, config::WorkerMode, environment::Environment};
    let smtp = smtp_refuse_first_message().await;
    let mut config = App::load_config(&Environment::Test).await.unwrap();
    config.workers.mode = WorkerMode::BackgroundAsync;
    let mailer = config.mailer.as_mut().unwrap();
    mailer.stub = false;
    let transport = mailer.smtp.as_mut().unwrap();
    transport.host = "127.0.0.1".into();
    transport.port = smtp.port;
    let boot = App::boot(StartMode::ServerOnly, &Environment::Test, config)
        .await
        .unwrap();
    let ctx = boot.app_context;
    let registry = registry(&ctx);
    let scope = registry.resolve("localhost").await.unwrap();
    let db = &scope.db;
    let cipher = registry.cipher(&scope.organizer).unwrap();
    let (event, kind, free) = event(db).await;
    let mut ids = [
        purchase(db, &cipher, &event.id, &free, "first@example.com", 0, 1).await,
        comp(db, &cipher, &event.id, &kind, "second@example.com").await,
    ];
    ids.sort();
    events::update(db, &event.id, &changed()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        event_notices::deliver_pending(&ctx, &registry, &scope.organizer, db),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(counts(db).await, (1, 2, 1));
    assert_eq!(smtp.accepted.lock().unwrap().len(), 1);
    let rows = db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT order_id, sent_at FROM event_change_recipients ORDER BY order_id".to_owned(),
        ))
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<String>("", "order_id").unwrap(), ids[0]);
    assert_eq!(rows[0].try_get::<Option<i64>>("", "sent_at").unwrap(), None);
    assert!(rows[1]
        .try_get::<Option<i64>>("", "sent_at")
        .unwrap()
        .is_some());
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        event_notices::run(&ctx, &registry),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(counts(db).await, (1, 2, 2));
    let messages = smtp.accepted.lock().unwrap().clone();
    assert_eq!(messages.len(), 2);
    for (index, id) in [ids[1].clone(), ids[0].clone()].iter().enumerate() {
        let order = orders::get_order(db, &cipher, id).await.unwrap();
        assert_notice(&messages[index], &order.buyer_email, &order.number);
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        event_notices::run(&ctx, &registry),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(smtp.accepted.lock().unwrap().len(), 2);
}
