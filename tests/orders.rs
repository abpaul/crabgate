use chrono::Utc;
use crabgate::{
    admin_orders,
    crypto::MasterKey,
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, Selection},
    orders::{self, PaymentOutcome},
};
use loco_rs::db;
use migration::OrganizerMigrator;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};

async fn setup() -> (
    tempfile::TempDir,
    sea_orm::DatabaseConnection,
    String,
    String,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut options = sea_orm::ConnectOptions::new(format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("orders.db").display()
    ));
    options.max_connections(2).map_sqlx_sqlite_opts(|opts| {
        opts.shared_cache(false)
            .journal_mode(sea_orm::sqlx::sqlite::SqliteJournalMode::Wal)
    });
    let connection = sea_orm::Database::connect(options).await.unwrap();
    db::migrate::<OrganizerMigrator>(&connection).await.unwrap();
    let event = events::create(
        &connection,
        &EventInput {
            title: "Last seat".to_owned(),
            date: "2027-06-15".to_owned(),
            start_time: "19:00".to_owned(),
            end_time: "21:00".to_owned(),
            timezone: "Europe/Dublin".to_owned(),
            venue: "Hall".to_owned(),
            address: "1 Main Street".to_owned(),
            description: String::new(),
            event_capacity: Some(1),
            max_per_order: 10,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let ticket_type = events::add_ticket_type(
        &connection,
        &event,
        &TicketTypeInput {
            name: "Standard".to_owned(),
            price: "10".to_owned(),
            capacity: 1,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(&connection, &event.id, Utc::now().timestamp())
        .await
        .unwrap();
    (dir, connection, event.id, ticket_type.id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payment_waits_for_an_existing_writer_and_issues_one_order() {
    let (_dir, db, event_id, ticket_type_id) = setup().await;
    db.execute_unprepared("PRAGMA journal_mode = WAL")
        .await
        .unwrap();
    let now = Utc::now().timestamp();
    let hold = inventory::reserve(
        &db,
        &event_id,
        &[Selection {
            ticket_type_id,
            quantity: 1,
        }],
        now,
    )
    .await
    .unwrap();
    let details = orders::hold_details(&db, &hold.id).await.unwrap();
    orders::save_details(
        &db,
        &cipher(),
        &details,
        "buyer@example.com",
        &["Niamh".into()],
        false,
        now,
    )
    .await
    .unwrap();
    orders::attach_session(&db, &hold.id, "cs_contended", now)
        .await
        .unwrap();
    let writer = db.begin().await.unwrap();
    writer
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET updated_at = updated_at + 1 WHERE id = ?",
            [event_id.clone().into()],
        ))
        .await
        .unwrap();
    let payment = orders::complete_payment(&db, "cs_contended", "pi_contended", 1000, now);
    let release = async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        writer.commit().await.unwrap();
    };
    let (result, ()) = tokio::join!(payment, release);
    let order_id = match result.unwrap() {
        PaymentOutcome::Paid(id) => id,
        _ => panic!("Expected a paid order after the writer committed"),
    };
    match orders::complete_payment(&db, "cs_contended", "pi_contended", 1000, now)
        .await
        .unwrap()
    {
        PaymentOutcome::AlreadyProcessed(id) => assert_eq!(id, order_id),
        _ => panic!("Repeated payment must return the original order"),
    }
    assert_eq!(
        inventory::open_counts(&db, &event_id, now).await.unwrap(),
        (1, 0)
    );
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT (SELECT COUNT(*) FROM orders) AS orders, (SELECT COUNT(*) FROM tickets) AS tickets",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<i64>("", "orders").unwrap(), 1);
    assert_eq!(row.try_get::<i64>("", "tickets").unwrap(), 1);
}

#[tokio::test]
async fn payment_after_event_cancellation_requests_refund_without_ticket() {
    let (_dir, db, event_id, ticket_type_id) = setup().await;
    let now = Utc::now().timestamp();
    let hold = inventory::reserve(
        &db,
        &event_id,
        &[Selection {
            ticket_type_id,
            quantity: 1,
        }],
        now,
    )
    .await
    .unwrap();
    let details = orders::hold_details(&db, &hold.id).await.unwrap();
    orders::save_details(
        &db,
        &cipher(),
        &details,
        "buyer@example.com",
        &["Niamh".into()],
        false,
        now,
    )
    .await
    .unwrap();
    orders::attach_session(&db, &hold.id, "cs_cancelled", now)
        .await
        .unwrap();
    assert!(admin_orders::cancel_event(&db, &event_id, now + 1)
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        orders::complete_payment(&db, "cs_cancelled", "pi_cancelled", 1000, now + 2)
            .await
            .unwrap(),
        PaymentOutcome::RefundNeeded
    ));
    assert_eq!(
        inventory::open_counts(&db, &event_id, now + 2)
            .await
            .unwrap(),
        (0, 0)
    );
}

#[tokio::test]
async fn payment_without_buyer_details_requests_refund() {
    let (_dir, db, event_id, ticket_type_id) = setup().await;
    let now = Utc::now().timestamp();
    let hold = inventory::reserve(
        &db,
        &event_id,
        &[Selection {
            ticket_type_id,
            quantity: 1,
        }],
        now,
    )
    .await
    .unwrap();
    orders::attach_session(&db, &hold.id, "cs_missing_details", now)
        .await
        .unwrap();
    assert!(matches!(
        orders::complete_payment(
            &db,
            "cs_missing_details",
            "pi_missing_details",
            1000,
            now + 1
        )
        .await
        .unwrap(),
        PaymentOutcome::RefundNeeded
    ));
    assert!(orders::order_id_for_hold(&db, &hold.id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn refund_event_before_checkout_event_never_issues_a_ticket() {
    let (_dir, db, event_id, ticket_type_id) = setup().await;
    let now = Utc::now().timestamp();
    let hold = inventory::reserve(
        &db,
        &event_id,
        &[Selection {
            ticket_type_id,
            quantity: 1,
        }],
        now,
    )
    .await
    .unwrap();
    let details = orders::hold_details(&db, &hold.id).await.unwrap();
    orders::save_details(
        &db,
        &cipher(),
        &details,
        "buyer@example.com",
        &["Niamh".into()],
        false,
        now,
    )
    .await
    .unwrap();
    orders::attach_session(&db, &hold.id, "cs_refunded_early", now)
        .await
        .unwrap();
    assert_eq!(
        orders::record_external_refund(&db, "pi_refunded_early", now + 1)
            .await
            .unwrap(),
        None
    );
    assert!(matches!(
        orders::complete_payment(&db, "cs_refunded_early", "pi_refunded_early", 1000, now + 2)
            .await
            .unwrap(),
        PaymentOutcome::AlreadyRefunded
    ));
    assert_eq!(
        inventory::open_counts(&db, &event_id, now + 2)
            .await
            .unwrap(),
        (0, 0)
    );
    assert_eq!(
        orders::order_id_for_hold(&db, &hold.id).await.unwrap(),
        None
    );
}

fn cipher() -> crabgate::crypto::OrganizerCipher {
    MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .unwrap()
        .generate_organizer_key("test")
        .unwrap()
        .1
}

#[tokio::test]
async fn buyer_resends_are_limited_to_three_per_hour() {
    let (_dir, db, event_id, ticket_type_id) = setup().await;
    let order_id = admin_orders::complimentary(
        &db,
        &cipher(),
        &event_id,
        &ticket_type_id,
        "buyer@example.com",
        "Niamh",
        3_600,
    )
    .await
    .unwrap();
    for _ in 0..3 {
        assert!(orders::claim_resend(&db, &order_id, 3_600).await.unwrap());
    }
    assert!(!orders::claim_resend(&db, &order_id, 3_600).await.unwrap());
    assert!(orders::claim_resend(&db, &order_id, 7_200).await.unwrap());
}

#[tokio::test]
async fn paid_webhook_retry_issues_exactly_one_ticket() {
    let (_dir, db, event_id, ticket_type_id) = setup().await;
    let now = Utc::now().timestamp();
    let hold = inventory::reserve(
        &db,
        &event_id,
        &[Selection {
            ticket_type_id,
            quantity: 1,
        }],
        now,
    )
    .await
    .unwrap();
    let details = orders::hold_details(&db, &hold.id).await.unwrap();
    let cipher = cipher();
    orders::save_details(
        &db,
        &cipher,
        &details,
        "buyer@example.com",
        &["Niamh".to_owned()],
        false,
        now,
    )
    .await
    .unwrap();
    orders::attach_session(&db, &hold.id, "cs_test_123", now)
        .await
        .unwrap();
    let id = match orders::complete_payment(&db, "cs_test_123", "pi_test_123", 1000, now + 1)
        .await
        .unwrap()
    {
        PaymentOutcome::Paid(id) => id,
        _ => panic!("first webhook did not create an Order"),
    };
    match orders::complete_payment(&db, "cs_test_123", "pi_test_123", 1000, now + 2)
        .await
        .unwrap()
    {
        PaymentOutcome::AlreadyProcessed(existing) => assert_eq!(existing, id),
        _ => panic!("retry did not return the existing Order"),
    };
    let order = orders::get_order(&db, &cipher, &id).await.unwrap();
    assert_eq!(order.buyer_email, "buyer@example.com");
    assert_eq!(order.total_cents, 1000);
    assert_eq!(order.tickets.len(), 1);
    assert_eq!(order.tickets[0].code.len(), 19);
    assert_eq!(order.tickets[0].attendee_name.as_deref(), Some("Niamh"));
    assert_eq!(
        inventory::open_counts(&db, &event_id, now + 2)
            .await
            .unwrap(),
        (1, 0)
    );
}

#[tokio::test]
async fn late_payment_with_no_capacity_requests_one_refund() {
    let (_dir, db, event_id, ticket_type_id) = setup().await;
    let now = Utc::now().timestamp();
    let selection = Selection {
        ticket_type_id,
        quantity: 1,
    };
    let old = inventory::reserve(&db, &event_id, std::slice::from_ref(&selection), now - 2200)
        .await
        .unwrap();
    let cipher = cipher();
    let details = orders::hold_details(&db, &old.id).await.unwrap();
    orders::save_details(
        &db,
        &cipher,
        &details,
        "late@example.com",
        &[String::new()],
        false,
        now - 2200,
    )
    .await
    .unwrap();
    orders::attach_session(&db, &old.id, "cs_test_late", now - 2200)
        .await
        .unwrap();
    inventory::reserve(&db, &event_id, &[selection], now)
        .await
        .unwrap();
    assert!(matches!(
        orders::complete_payment(&db, "cs_test_late", "pi_test_late", 1000, now)
            .await
            .unwrap(),
        PaymentOutcome::RefundNeeded
    ));
    assert!(matches!(
        orders::complete_payment(&db, "cs_test_late", "pi_test_late", 1000, now + 1)
            .await
            .unwrap(),
        PaymentOutcome::RefundNeeded
    ));
    orders::mark_refunded(&db, "cs_test_late").await.unwrap();
    assert!(matches!(
        orders::complete_payment(&db, "cs_test_late", "pi_test_late", 1000, now + 2)
            .await
            .unwrap(),
        PaymentOutcome::AlreadyRefunded
    ));
    assert_eq!(
        inventory::open_counts(&db, &event_id, now).await.unwrap(),
        (0, 1)
    );
}

#[tokio::test]
async fn paid_webhook_after_expiry_still_issues_ticket_when_capacity_is_free() {
    let (_dir, db, event_id, ticket_type_id) = setup().await;
    let now = Utc::now().timestamp();
    let old = inventory::reserve(
        &db,
        &event_id,
        &[Selection {
            ticket_type_id,
            quantity: 1,
        }],
        now - 1900,
    )
    .await
    .unwrap();
    let cipher = cipher();
    let details = orders::hold_details(&db, &old.id).await.unwrap();
    orders::save_details(
        &db,
        &cipher,
        &details,
        "late@example.com",
        &[String::new()],
        false,
        now - 1900,
    )
    .await
    .unwrap();
    orders::attach_session(&db, &old.id, "cs_test_late_free", now - 1900)
        .await
        .unwrap();
    orders::expire_hold(&db, &old.id).await.unwrap();
    assert!(matches!(
        orders::complete_payment(&db, "cs_test_late_free", "pi_test_late_free", 1000, now)
            .await
            .unwrap(),
        PaymentOutcome::Paid(_)
    ));
    assert_eq!(
        inventory::open_counts(&db, &event_id, now).await.unwrap(),
        (1, 0)
    );
}
