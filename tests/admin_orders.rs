use chrono::Utc;
use crabgate::{
    admin_orders,
    crypto::{MasterKey, OrganizerCipher},
    door::{self, CheckinResult},
    events::{self, EventInput, TicketTypeInput},
};
use loco_rs::db;
use migration::OrganizerMigrator;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

async fn setup() -> (
    tempfile::TempDir,
    DatabaseConnection,
    OrganizerCipher,
    String,
    String,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = sea_orm::Database::connect(format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("admin.db").display()
    ))
    .await
    .unwrap();
    db::migrate::<OrganizerMigrator>(&db).await.unwrap();
    let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .unwrap()
        .generate_organizer_key("test")
        .unwrap()
        .1;
    let event = events::create(
        &db,
        &EventInput {
            title: "Music Night".into(),
            date: "2027-06-15".into(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Hall".into(),
            address: "Main Street".into(),
            description: String::new(),
            event_capacity: Some(2),
            max_per_order: 10,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let kind = events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "Standard".into(),
            price: "20".into(),
            capacity: 2,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(&db, &event.id, Utc::now().timestamp())
        .await
        .unwrap();
    (dir, db, cipher, event.id, kind.id)
}

#[tokio::test]
async fn complimentary_counts_against_capacity_and_refund_voids_all_tickets() {
    let (_dir, db, cipher, event, kind) = setup().await;
    let now = Utc::now().timestamp();
    let first =
        admin_orders::complimentary(&db, &cipher, &event, &kind, "one@example.com", "Niamh", now)
            .await
            .unwrap();
    let second =
        admin_orders::complimentary(&db, &cipher, &event, &kind, "two@example.com", "Cian", now)
            .await
            .unwrap();
    assert!(admin_orders::complimentary(
        &db,
        &cipher,
        &event,
        &kind,
        "three@example.com",
        "Orla",
        now
    )
    .await
    .is_err());
    let found = admin_orders::list(&db, &cipher, Some(&event), "one@example.com")
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].ticket_count, 1);
    assert!(found[0].complimentary);
    assert_eq!(
        admin_orders::list(&db, &cipher, Some(&event), "Niamh")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        admin_orders::list(&db, &cipher, None, "Niamh")
            .await
            .unwrap()
            .len(),
        0
    );
    let csv = admin_orders::report(&db, &cipher, &event, admin_orders::Report::Tickets)
        .await
        .unwrap();
    assert!(csv.starts_with(
        "Order,Ticket code,Ticket Type,Seat,Attendee,Buyer email,Status,Checked in at\r\n"
    ));
    assert!(csv.contains(",Standard,,Niamh,one@example.com,Valid,"));
    admin_orders::refund(&db, &first, None, now + 1)
        .await
        .unwrap();
    admin_orders::refund(&db, &first, None, now + 2)
        .await
        .unwrap();
    let order = admin_orders::order(&db, &cipher, &first).await.unwrap();
    assert_eq!(order.status, "Refunded");
    assert_eq!(order.tickets[0].status, "Void");
    assert!(matches!(
        door::check_in(
            &db,
            &cipher,
            &event,
            &order.tickets[0].code,
            "Member",
            "admin",
            now + 3
        )
        .await
        .unwrap(),
        CheckinResult::VoidTicket(_)
    ));
    let other = admin_orders::order(&db, &cipher, &second).await.unwrap();
    assert!(matches!(
        door::check_in(
            &db,
            &cipher,
            &event,
            &other.tickets[0].code,
            "Member",
            "admin",
            now + 3
        )
        .await
        .unwrap(),
        CheckinResult::CheckedIn(_)
    ));
    admin_orders::refund(&db, &second, None, now + 4)
        .await
        .unwrap();
    let other = admin_orders::order(&db, &cipher, &second).await.unwrap();
    assert_eq!(other.tickets[0].status, "Void");
    let count = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM checkins WHERE ticket_id = ? AND undone_at IS NULL",
            [other.tickets[0].id.clone().into()],
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(count.try_get::<i64>("", "count").unwrap(), 1);
    let checkins = admin_orders::report(&db, &cipher, &event, admin_orders::Report::Checkins)
        .await
        .unwrap();
    assert!(checkins.contains(",Cian,"));
    admin_orders::anonymize(&db, &cipher, &second)
        .await
        .unwrap();
    let other = admin_orders::order(&db, &cipher, &second).await.unwrap();
    assert_eq!(other.buyer_email, "");
    assert_eq!(other.tickets[0].attendee_name, None);
}

#[tokio::test]
async fn cancelled_event_stops_sales_and_refunds_all_orders() {
    let (_dir, db, cipher, event, kind) = setup().await;
    let now = Utc::now().timestamp();
    let first =
        admin_orders::complimentary(&db, &cipher, &event, &kind, "one@example.com", "Niamh", now)
            .await
            .unwrap();
    let second =
        admin_orders::complimentary(&db, &cipher, &event, &kind, "two@example.com", "Cian", now)
            .await
            .unwrap();
    let ids = admin_orders::cancel_event(&db, &event, now + 1)
        .await
        .unwrap();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&first) && ids.contains(&second));
    let ticket = admin_orders::order(&db, &cipher, &first)
        .await
        .unwrap()
        .tickets
        .remove(0);
    assert!(matches!(
        door::check_in(
            &db,
            &cipher,
            &event,
            &ticket.code,
            "Member",
            "admin",
            now + 1
        )
        .await
        .unwrap(),
        CheckinResult::VoidTicket(_)
    ));
    for id in [&first, &second] {
        admin_orders::refund(&db, id, None, now + 2).await.unwrap();
    }
    assert_eq!(
        events::get(&db, &event).await.unwrap().unwrap().status,
        "Cancelled"
    );
    assert_eq!(
        admin_orders::order(&db, &cipher, &first)
            .await
            .unwrap()
            .tickets[0]
            .status,
        "Void"
    );
    assert_eq!(
        admin_orders::order(&db, &cipher, &second)
            .await
            .unwrap()
            .status,
        "Refunded"
    );
    assert_eq!(
        admin_orders::cancel_event(&db, &event, now + 3)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn durable_work_survives_reopen_and_notice_claims_exclude_concurrent_retries() {
    let (dir, db, cipher, event, kind) = setup().await;
    let now = Utc::now().timestamp();
    let id = admin_orders::complimentary(
        &db,
        &cipher,
        &event,
        &kind,
        "claim@example.com",
        "Claim",
        now,
    )
    .await
    .unwrap();
    let reopened = sea_orm::Database::connect(format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("admin.db").display()
    ))
    .await
    .unwrap();
    let (first_cancel, second_cancel) = tokio::join!(
        admin_orders::cancel_event(&db, &event, now),
        admin_orders::cancel_event(&reopened, &event, now)
    );
    first_cancel.unwrap();
    second_cancel.unwrap();
    let progress = admin_orders::work_progress(&reopened, &id).await.unwrap();
    assert_eq!(progress.refund, "Pending");
    assert_eq!(progress.cancellation_notice, "Pending");
    let (first, second) = tokio::join!(
        admin_orders::claim_notice(&db, &id, true),
        admin_orders::claim_notice(&reopened, &id, true)
    );
    assert_eq!(
        usize::from(first.unwrap()) + usize::from(second.unwrap()),
        1
    );
    assert_eq!(
        admin_orders::work_progress(&db, &id)
            .await
            .unwrap()
            .cancellation_notice,
        "Sending"
    );
    assert!(!admin_orders::notice_sent(&db, &id, true).await.unwrap());
    admin_orders::record_notice_error(&db, &id, true, false)
        .await
        .unwrap();
    assert_eq!(
        admin_orders::work_progress(&db, &id)
            .await
            .unwrap()
            .cancellation_notice,
        "Uncertain"
    );
    assert!(!admin_orders::claim_notice(&reopened, &id, true)
        .await
        .unwrap());
    let (first, second) = tokio::join!(
        admin_orders::refund(&db, &id, None, now),
        admin_orders::refund(&reopened, &id, None, now)
    );
    first.unwrap();
    second.unwrap();
    assert_eq!(
        admin_orders::order(&db, &cipher, &id).await.unwrap().status,
        "Refunded"
    );
    assert_eq!(
        admin_orders::order(&db, &cipher, &id)
            .await
            .unwrap()
            .tickets
            .len(),
        1
    );
    assert_eq!(
        admin_orders::order(&db, &cipher, &id)
            .await
            .unwrap()
            .tickets[0]
            .status,
        "Void"
    );
    assert_eq!(
        admin_orders::work_progress(&db, &id).await.unwrap().refund,
        "Completed"
    );
    admin_orders::record_refund_failure(&db, &id).await.unwrap();
    admin_orders::record_refund_provider_status(&db, &id, "re_late", false)
        .await
        .unwrap();
    assert_eq!(
        admin_orders::work_progress(&db, &id).await.unwrap().refund,
        "Completed"
    );
}

#[tokio::test]
async fn interrupted_refund_claim_recovers_after_lease_without_issuing_tickets() {
    let (_dir, db, cipher, event, kind) = setup().await;
    let now = Utc::now().timestamp();
    let id = admin_orders::complimentary(
        &db,
        &cipher,
        &event,
        &kind,
        "crash@example.com",
        "Crash",
        now,
    )
    .await
    .unwrap();
    admin_orders::cancel_event(&db, &event, now).await.unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE orders SET status = 'RefundPending', refund_claim_until = ? WHERE id = ?",
        [(now + 300).into(), id.clone().into()],
    ))
    .await
    .unwrap();
    admin_orders::refund(&db, &id, None, now + 299)
        .await
        .unwrap();
    assert_eq!(
        admin_orders::order(&db, &cipher, &id).await.unwrap().status,
        "RefundPending"
    );
    admin_orders::refund(&db, &id, None, now + 300)
        .await
        .unwrap();
    assert_eq!(
        admin_orders::order(&db, &cipher, &id).await.unwrap().status,
        "Refunded"
    );
    assert_eq!(
        admin_orders::order(&db, &cipher, &id)
            .await
            .unwrap()
            .tickets
            .len(),
        1
    );
    assert_eq!(
        admin_orders::order(&db, &cipher, &id)
            .await
            .unwrap()
            .tickets[0]
            .status,
        "Void"
    );
}

#[tokio::test]
async fn cancellation_refunds_both_purchased_free_and_complimentary_orders() {
    use crabgate::{
        inventory::{self, Selection},
        orders::{self, PaymentOutcome},
    };
    let (_dir, db, cipher, event_id, kind) = setup().await;
    let now = Utc::now().timestamp();
    let event = events::get(&db, &event_id).await.unwrap().unwrap();
    let free_kind = events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "Free".into(),
            price: "0".into(),
            capacity: 2,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    let comp = admin_orders::complimentary(
        &db,
        &cipher,
        &event_id,
        &kind,
        "comp@example.com",
        "Comp",
        now,
    )
    .await
    .unwrap();
    let hold = inventory::reserve(
        &db,
        &event_id,
        &[Selection {
            ticket_type_id: free_kind.id,
            quantity: 1,
        }],
        now,
    )
    .await
    .unwrap();
    let details = orders::hold_details(&db, &hold.id).await.unwrap();
    orders::save_details(
        &db,
        &cipher,
        &details,
        "free@example.com",
        &["Free".into()],
        false,
        now,
    )
    .await
    .unwrap();
    orders::attach_session(&db, &hold.id, "free_cg005", now)
        .await
        .unwrap();
    let paid = match orders::complete_payment(&db, "free_cg005", "", 0, now)
        .await
        .unwrap()
    {
        PaymentOutcome::Paid(id) => id,
        _ => panic!("free purchase did not complete"),
    };
    let rows = admin_orders::list(&db, &cipher, Some(&event_id), "")
        .await
        .unwrap();
    assert!(
        rows.iter()
            .find(|row| row.id == comp)
            .unwrap()
            .complimentary
    );
    assert!(
        !rows
            .iter()
            .find(|row| row.id == paid)
            .unwrap()
            .complimentary
    );
    let ids = admin_orders::cancel_event(&db, &event_id, now)
        .await
        .unwrap();
    assert_eq!(ids.len(), 2);
    for id in [&comp, &paid] {
        admin_orders::refund(&db, id, None, now).await.unwrap();
        assert_eq!(
            admin_orders::work_progress(&db, id).await.unwrap().refund,
            "Completed"
        );
        assert_eq!(
            admin_orders::work_progress(&db, id)
                .await
                .unwrap()
                .cancellation_notice,
            "Pending"
        );
        assert_eq!(
            admin_orders::order(&db, &cipher, id).await.unwrap().tickets[0].status,
            "Void"
        );
    }
    assert_eq!(
        events::get(&db, &event_id).await.unwrap().unwrap().status,
        "Cancelled"
    );
}

#[tokio::test]
async fn migration_recovers_abandoned_cancelled_work_and_preserves_notice_receipts() {
    use migration::MigratorTrait;
    let (_dir, db, cipher, event, kind) = setup().await;
    let now = Utc::now().timestamp();
    let pending = admin_orders::complimentary(
        &db,
        &cipher,
        &event,
        &kind,
        "pending@example.com",
        "Pending",
        now,
    )
    .await
    .unwrap();
    let completed = admin_orders::complimentary(
        &db,
        &cipher,
        &event,
        &kind,
        "completed@example.com",
        "Completed",
        now,
    )
    .await
    .unwrap();
    admin_orders::cancel_event(&db, &event, now).await.unwrap();
    admin_orders::refund(&db, &completed, None, now)
        .await
        .unwrap();
    admin_orders::mark_notice_sent(&db, &completed, true, now)
        .await
        .unwrap();
    admin_orders::mark_notice_sent(&db, &completed, false, now)
        .await
        .unwrap();
    let migrations = OrganizerMigrator::migrations();
    let refund_migration = migrations
        .iter()
        .position(|migration| migration.name() == "m20261007_000020_order_refund_work")
        .expect("refund-work migration is registered");
    let rollback_steps = u32::try_from(migrations.len() - refund_migration).unwrap();
    OrganizerMigrator::down(&db, Some(rollback_steps))
        .await
        .unwrap();
    OrganizerMigrator::up(&db, None).await.unwrap();
    let progress = admin_orders::work_progress(&db, &pending).await.unwrap();
    assert_eq!(progress.refund, "Pending");
    assert_eq!(progress.cancellation_notice, "Pending");
    let progress = admin_orders::work_progress(&db, &completed).await.unwrap();
    assert_eq!(progress.refund, "Completed");
    assert_eq!(progress.cancellation_notice, "Uncertain");
    assert_eq!(progress.refund_notice, "Uncertain");
    assert!(!admin_orders::claim_notice(&db, &completed, true)
        .await
        .unwrap());
    admin_orders::refund(&db, &pending, None, now)
        .await
        .unwrap();
    assert_eq!(
        admin_orders::work_progress(&db, &pending)
            .await
            .unwrap()
            .refund,
        "Completed"
    );
    assert_eq!(
        events::get(&db, &event).await.unwrap().unwrap().status,
        "Cancelled"
    );
}
