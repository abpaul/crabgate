use std::collections::HashMap;

use crabgate::{
    admin_orders::{self, Report},
    crypto::{MasterKey, OrganizerCipher},
    door::{self, CheckinResult},
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, Selection},
    orders::{self, PaymentOutcome},
    questions,
};
use migration::{MigrationTrait, Migrator, MigratorTrait, OrganizerMigrator};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

const NOW: i64 = 1_792_000_000;
const INDEX_MIGRATION: &str = "m20261011_000030_read_indexes";
const INDEXES: &[(&str, &str)] = &[
    (
        "checkins_ticket_time",
        "CREATE INDEX checkins_ticket_time ON checkins(ticket_id, checked_at)",
    ),
    (
        "holds_status_expiry",
        "CREATE INDEX holds_status_expiry ON holds(status, expires_at)",
    ),
    (
        "tickets_code_nocase",
        "CREATE INDEX tickets_code_nocase ON tickets(code COLLATE NOCASE)",
    ),
    (
        "tickets_order",
        "CREATE INDEX tickets_order ON tickets(order_id)",
    ),
    (
        "tickets_type_status",
        "CREATE INDEX tickets_type_status ON tickets(ticket_type_id, status)",
    ),
];

struct PreIndexOrganizerMigrator;

impl MigratorTrait for PreIndexOrganizerMigrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        let mut migrations = OrganizerMigrator::migrations();
        let position = migrations
            .iter()
            .position(|migration| migration.name() == INDEX_MIGRATION)
            .unwrap();
        migrations.truncate(position);
        migrations
    }
}

fn preceding_migrations() -> u32 {
    u32::try_from(PreIndexOrganizerMigrator::migrations().len()).unwrap()
}

async fn connect(path: &std::path::Path) -> DatabaseConnection {
    sea_orm::Database::connect(format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap()
}

async fn schema(db: &DatabaseConnection, kind: &str) -> Vec<(String, String)> {
    db.query_all_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT name, sql FROM sqlite_schema WHERE type = ? AND sql IS NOT NULL ORDER BY name",
        [kind.into()],
    ))
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        (
            row.try_get("", "name").unwrap(),
            row.try_get("", "sql").unwrap(),
        )
    })
    .collect()
}

async fn index_migration_receipts(db: &DatabaseConnection) -> i64 {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Sqlite,
        "SELECT COUNT(*) AS count FROM seaql_migrations WHERE version = 'm20261011_000030_read_indexes'",
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get("", "count")
    .unwrap()
}

#[tokio::test]
async fn failed_index_upgrade_and_rollback_leave_schema_and_receipt_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let db = connect(&dir.path().join("failure.db")).await;
    let preceding = preceding_migrations();
    OrganizerMigrator::up(&db, Some(preceding)).await.unwrap();
    db.execute_unprepared(
        "CREATE TABLE migration_sentinel (value TEXT NOT NULL);
         INSERT INTO migration_sentinel VALUES ('Keep');
         CREATE INDEX holds_status_expiry ON holds(created_at);",
    )
    .await
    .unwrap();
    let before = schema(&db, "index").await;
    assert!(OrganizerMigrator::up(&db, None).await.is_err());
    assert_eq!(schema(&db, "index").await, before);
    assert_eq!(index_migration_receipts(&db).await, 0);
    db.execute_unprepared("DROP INDEX holds_status_expiry")
        .await
        .unwrap();
    OrganizerMigrator::up(&db, None).await.unwrap();
    assert_eq!(index_migration_receipts(&db).await, 1);

    db.execute_unprepared("DROP INDEX checkins_ticket_time")
        .await
        .unwrap();
    let before = schema(&db, "index").await;
    assert!(OrganizerMigrator::down(&db, Some(1)).await.is_err());
    assert_eq!(schema(&db, "index").await, before);
    assert_eq!(index_migration_receipts(&db).await, 1);
    db.execute_unprepared(INDEXES[0].1).await.unwrap();
    OrganizerMigrator::down(&db, Some(1)).await.unwrap();
    assert_eq!(index_migration_receipts(&db).await, 0);
    assert_eq!(
        db.query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT value FROM migration_sentinel"
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "value")
        .unwrap(),
        "Keep"
    );
}

#[tokio::test]
async fn organizer_indexes_are_additive_reversible_and_leave_control_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let db = connect(&dir.path().join("organizer.db")).await;
    let preceding = preceding_migrations();
    OrganizerMigrator::up(&db, Some(preceding)).await.unwrap();
    let tables_before = schema(&db, "table").await;
    let indexes_before = schema(&db, "index").await;
    assert_eq!(indexes_before.len(), 18);

    OrganizerMigrator::up(&db, None).await.unwrap();
    assert_eq!(schema(&db, "table").await, tables_before);
    let added = schema(&db, "index")
        .await
        .into_iter()
        .filter(|index| !indexes_before.contains(index))
        .collect::<Vec<_>>();
    assert_eq!(
        added,
        INDEXES
            .iter()
            .map(|(name, sql)| ((*name).to_owned(), (*sql).to_owned()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        db.query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA index_xinfo(tickets_code_nocase)"
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "coll")
        .unwrap(),
        "NOCASE"
    );
    OrganizerMigrator::down(&db, Some(1)).await.unwrap();
    assert_eq!(schema(&db, "index").await, indexes_before);
    assert_eq!(schema(&db, "table").await, tables_before);
    OrganizerMigrator::up(&db, None).await.unwrap();
    assert_eq!(schema(&db, "index").await.len(), indexes_before.len() + 5);

    let control = connect(&dir.path().join("control.db")).await;
    Migrator::up(&control, None).await.unwrap();
    assert!(schema(&control, "index")
        .await
        .iter()
        .all(|(name, _)| !INDEXES.iter().any(|(index, _)| name == index)));
    assert!(schema(&control, "table")
        .await
        .iter()
        .all(|(name, _)| name != "tickets" && name != "holds" && name != "checkins"));
}

#[tokio::test]
async fn older_migrator_rejects_new_receipt_until_index_migration_is_reversed() {
    let dir = tempfile::tempdir().unwrap();
    let db = connect(&dir.path().join("compatibility.db")).await;
    PreIndexOrganizerMigrator::up(&db, None).await.unwrap();
    assert!(
        PreIndexOrganizerMigrator::get_pending_migrations_read_only(&db)
            .await
            .unwrap()
            .is_empty()
    );
    OrganizerMigrator::up(&db, Some(1)).await.unwrap();
    let readiness_error = PreIndexOrganizerMigrator::get_pending_migrations_read_only(&db)
        .await
        .err()
        .unwrap();
    let startup_error = PreIndexOrganizerMigrator::up(&db, None).await.unwrap_err();
    for error in [readiness_error, startup_error] {
        assert!(
            error
                .to_string()
                .contains("Migration file of version 'm20261011_000030_read_indexes' is missing"),
            "unexpected error: {error}"
        );
    }
    OrganizerMigrator::down(&db, Some(1)).await.unwrap();
    PreIndexOrganizerMigrator::up(&db, None).await.unwrap();
    assert!(
        PreIndexOrganizerMigrator::get_pending_migrations_read_only(&db)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn event(db: &DatabaseConnection, title: &str) -> (String, String) {
    let event = events::create(
        db,
        &EventInput {
            title: title.to_owned(),
            date: "2027-06-15".to_owned(),
            start_time: "19:00".to_owned(),
            end_time: "21:00".to_owned(),
            timezone: "Europe/Dublin".to_owned(),
            venue: "Hall".to_owned(),
            address: "Street".to_owned(),
            description: String::new(),
            event_capacity: Some(40),
            max_per_order: 10,
            attendee_name_required: true,
        },
    )
    .await
    .unwrap();
    let kind = events::add_ticket_type(
        db,
        &event,
        &TicketTypeInput {
            name: "General".to_owned(),
            price: "0".to_owned(),
            capacity: 40,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(db, &event.id, NOW).await.unwrap();
    (event.id, kind.id)
}

async fn purchase(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: &str,
    kind: &str,
    names: &[&str],
) -> String {
    let badge = questions::add(db, event_id, "Attendee", "Text", "Badge", &[], false)
        .await
        .unwrap();
    let hold = inventory::reserve(
        db,
        event_id,
        &[Selection {
            ticket_type_id: kind.to_owned(),
            quantity: i64::try_from(names.len()).unwrap(),
        }],
        NOW,
    )
    .await
    .unwrap();
    let asked = questions::list_for_hold(db, &hold.id).await.unwrap();
    let form = names
        .iter()
        .enumerate()
        .map(|(i, name)| (format!("question_{badge}_{i}"), format!("Badge {name}")))
        .collect::<HashMap<_, _>>();
    questions::save_answers(db, cipher, &hold.id, &asked, names.len(), &form)
        .await
        .unwrap();
    let details = orders::hold_details(db, &hold.id).await.unwrap();
    orders::save_details(
        db,
        cipher,
        &details,
        "synthetic@example.test",
        &names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
        false,
        NOW,
    )
    .await
    .unwrap();
    let session = format!("fixture_{}", hold.id);
    orders::attach_session(db, &hold.id, &session, NOW)
        .await
        .unwrap();
    match orders::complete_payment(db, &session, "", 0, NOW)
        .await
        .unwrap()
    {
        PaymentOutcome::Paid(id) => id,
        _ => panic!("expected a complimentary paid Order"),
    }
}

async fn observation(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: &str,
    other: &str,
    kind: &str,
    order_id: &str,
) -> serde_json::Value {
    let csv = admin_orders::report(db, cipher, event_id, Report::Tickets)
        .await
        .unwrap();
    let mut badges = csv
        .lines()
        .skip(1)
        .map(|line| {
            let cells = line.split(',').collect::<Vec<_>>();
            (cells[4].to_owned(), cells[8].to_owned())
        })
        .collect::<Vec<_>>();
    badges.sort();
    assert_eq!(
        badges,
        [
            ("Cian".to_owned(), "Badge Cian".to_owned()),
            ("Niamh".to_owned(), "Badge Niamh".to_owned()),
            ("Orla".to_owned(), "Badge Orla".to_owned())
        ]
    );
    let attendees = admin_orders::attendees(db, cipher, Some(event_id), "")
        .await
        .unwrap();
    assert_eq!(attendees.len(), 3);
    let names = attendees
        .iter()
        .map(|row| row.name.as_deref().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["Niamh", "Cian", "Orla"]);
    assert_eq!(attendees[0].checked_at, Some(NOW + 2));
    assert_eq!(attendees[0].undone_at, Some(NOW + 6));
    assert_eq!(attendees[1].checked_at, Some(NOW + 3));
    assert_eq!(attendees[1].undone_at, None);
    assert_eq!(
        attendees
            .iter()
            .map(|row| row.admission_valid)
            .collect::<Vec<_>>(),
        [true, true, false]
    );
    assert_eq!(
        admin_orders::attendees(db, cipher, None, "")
            .await
            .unwrap()
            .len(),
        4
    );
    let refunded = admin_orders::attendees(db, cipher, Some(other), "")
        .await
        .unwrap();
    assert_eq!(refunded.len(), 1);
    assert_eq!(refunded[0].order_status, "Refunded");
    assert!(!refunded[0].admission_valid);
    assert_eq!(
        admin_orders::attendees(db, cipher, None, "badge")
            .await
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        admin_orders::attendees(db, cipher, None, "CIAN")
            .await
            .unwrap()[0]
            .name
            .as_deref(),
        Some("Cian")
    );
    assert_eq!(inventory::type_counts(db, kind, NOW).await.unwrap(), (2, 2));
    assert_eq!(
        inventory::open_counts(db, event_id, NOW).await.unwrap(),
        (2, 2)
    );
    assert_eq!(door::count(db, event_id).await.unwrap(), (1, 2));
    let activity = admin_orders::ticket_activity(db, order_id).await.unwrap();
    assert_eq!(
        activity
            .iter()
            .map(|row| (row.attendee_number, row.checked_at, row.undone_at))
            .collect::<Vec<_>>(),
        [
            (1, Some(NOW + 2), Some(NOW + 4)),
            (1, Some(NOW + 2), Some(NOW + 6)),
            (2, Some(NOW + 3), None),
            (3, None, None)
        ]
    );
    serde_json::json!({
        "tickets": csv,
        "orders": admin_orders::report(db, cipher, event_id, Report::Orders).await.unwrap(),
        "checkins": admin_orders::report(db, cipher, event_id, Report::Checkins).await.unwrap(),
        "attendees": attendees.iter().map(|row| (&row.ticket_id, &row.code, &row.name, &row.ticket_status, &row.order_id, &row.order_status, row.admission_valid, row.checked_at, row.undone_at)).collect::<Vec<_>>(),
    })
}

#[tokio::test]
async fn index_upgrade_and_rollback_preserve_exports_inventory_and_admission_history() {
    let dir = tempfile::tempdir().unwrap();
    let db = connect(&dir.path().join("data.db")).await;
    let preceding = preceding_migrations();
    OrganizerMigrator::up(&db, Some(preceding)).await.unwrap();
    let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .unwrap()
        .generate_organizer_key("fixture")
        .unwrap()
        .1;
    let (event_id, kind) = event(&db, "Main").await;
    let (other, other_kind) = event(&db, "Other").await;
    let order_id = purchase(&db, &cipher, &event_id, &kind, &["Niamh", "Cian", "Orla"]).await;
    let other_order = purchase(&db, &cipher, &other, &other_kind, &["Pat"]).await;
    admin_orders::refund(&db, &other_order, None, NOW + 1)
        .await
        .unwrap();
    let mut order = orders::get_order(&db, &cipher, &order_id).await.unwrap();
    order
        .tickets
        .sort_by_key(|ticket| match ticket.attendee_name.as_deref() {
            Some("Niamh") => 0,
            Some("Cian") => 1,
            Some("Orla") => 2,
            _ => panic!("unexpected fixture attendee"),
        });
    for undo_at in [NOW + 4, NOW + 6] {
        assert!(matches!(
            door::check_in(
                &db,
                &cipher,
                &event_id,
                &order.tickets[0].code.to_lowercase(),
                "Member",
                "fixture",
                NOW + 2
            )
            .await
            .unwrap(),
            CheckinResult::CheckedIn(_)
        ));
        door::undo(&db, &event_id, &order.tickets[0].id, "fixture", undo_at)
            .await
            .unwrap();
    }
    assert!(matches!(
        door::check_in(
            &db,
            &cipher,
            &event_id,
            &order.tickets[1].code,
            "Member",
            "fixture",
            NOW + 3
        )
        .await
        .unwrap(),
        CheckinResult::CheckedIn(_)
    ));
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE tickets SET status = 'Void' WHERE id = ?",
        [order.tickets[2].id.clone().into()],
    ))
    .await
    .unwrap();
    for at in [NOW, NOW - 2000] {
        inventory::reserve(
            &db,
            &event_id,
            &[Selection {
                ticket_type_id: kind.clone(),
                quantity: 2,
            }],
            at,
        )
        .await
        .unwrap();
    }
    let before = observation(&db, &cipher, &event_id, &other, &kind, &order_id).await;
    for upgrade in [true, false, true] {
        if upgrade {
            OrganizerMigrator::up(&db, None).await.unwrap();
        } else {
            OrganizerMigrator::down(&db, Some(1)).await.unwrap();
        }
        assert_eq!(
            observation(&db, &cipher, &event_id, &other, &kind, &order_id).await,
            before
        );
        assert!(matches!(
            door::check_in(
                &db,
                &cipher,
                &event_id,
                &order.tickets[1].code.to_lowercase(),
                "Member",
                "fixture",
                NOW + 10
            )
            .await
            .unwrap(),
            CheckinResult::AlreadyCheckedIn(_)
        ));
        assert!(matches!(
            door::check_in(
                &db,
                &cipher,
                &event_id,
                &order.tickets[2].code,
                "Member",
                "fixture",
                NOW + 10
            )
            .await
            .unwrap(),
            CheckinResult::VoidTicket(_)
        ));
        let refunded = orders::get_order(&db, &cipher, &other_order).await.unwrap();
        assert!(matches!(
            door::check_in(
                &db,
                &cipher,
                &other,
                &refunded.tickets[0].code,
                "Member",
                "fixture",
                NOW + 10
            )
            .await
            .unwrap(),
            CheckinResult::VoidTicket(_)
        ));
    }
}
