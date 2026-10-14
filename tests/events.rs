use chrono::Utc;
use crabgate::inventory::{self, Selection};
use crabgate::{
    admin_orders,
    crypto::MasterKey,
    events::{self, EventInput, TicketBoxState, TicketTypeInput},
};
use loco_rs::db;
use migration::OrganizerMigrator;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};

async fn database() -> (tempfile::TempDir, sea_orm::DatabaseConnection) {
    let dir = tempfile::tempdir().unwrap();
    let connection = sea_orm::Database::connect(format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("event.db").display()
    ))
    .await
    .unwrap();
    db::migrate::<OrganizerMigrator>(&connection).await.unwrap();
    (dir, connection)
}

fn input() -> EventInput {
    EventInput {
        title: "Summer Music Night".to_owned(),
        date: "2027-06-15".to_owned(),
        start_time: "19:00".to_owned(),
        end_time: "21:00".to_owned(),
        timezone: "Europe/Dublin".to_owned(),
        venue: "Harbour Arts Centre".to_owned(),
        address: "1 Harbour Road".to_owned(),
        description: "Local music".to_owned(),
        event_capacity: Some(50),
        max_per_order: 10,
        attendee_name_required: true,
    }
}

#[tokio::test]
async fn publishing_requires_a_ticket_type_and_return_to_draft_requires_no_sales() {
    let (_dir, db) = database().await;
    let event = events::create(&db, &input()).await.unwrap();
    assert_eq!(event.status, "Draft");
    assert_eq!(event.starts_at, 1813082400);
    assert_eq!(
        events::publish(&db, &event.id, Utc::now().timestamp())
            .await
            .unwrap_err()
            .to_string(),
        "Add a ticket type before you publish."
    );
    let ticket_type = events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "General admission".to_owned(),
            price: "15.50".to_owned(),
            capacity: 20,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(ticket_type.price_cents, 1550);
    events::publish(&db, &event.id, Utc::now().timestamp())
        .await
        .unwrap();
    assert_eq!(
        events::get(&db, &event.id).await.unwrap().unwrap().status,
        "Published"
    );
    events::return_to_draft(&db, &event.id).await.unwrap();
    assert_eq!(
        events::get(&db, &event.id).await.unwrap().unwrap().status,
        "Draft"
    );
}

#[tokio::test]
async fn ticket_box_states_and_ticket_type_edit_have_literal_results() {
    let (_dir, db) = database().await;
    let event = events::create(&db, &input()).await.unwrap();
    let ticket_type = events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "Early".to_owned(),
            price: "0".to_owned(),
            capacity: 20,
            sales_open: Some("2027-06-01T10:00".to_owned()),
            sales_close: Some("2027-06-10T10:00".to_owned()),
        },
    )
    .await
    .unwrap();
    events::publish(&db, &event.id, Utc::now().timestamp())
        .await
        .unwrap();
    let event = events::get(&db, &event.id).await.unwrap().unwrap();
    assert_eq!(
        events::ticket_box_state(&event, &ticket_type, 0, 0, 0, 0, 1800000000),
        TicketBoxState::ComingSoon
    );
    assert_eq!(
        events::ticket_box_state(&event, &ticket_type, 0, 0, 0, 0, 1812000000),
        TicketBoxState::OnSale
    );
    assert_eq!(
        events::ticket_box_state(&event, &ticket_type, 0, 0, 0, 0, 1813000000),
        TicketBoxState::SalesEnded
    );
    assert_eq!(
        events::ticket_box_state(&event, &ticket_type, 20, 0, 20, 0, 1812000000),
        TicketBoxState::SoldOut
    );
    events::update_ticket_type(
        &db,
        &event,
        &ticket_type.id,
        &TicketTypeInput {
            name: "Standard".to_owned(),
            price: "12.50".to_owned(),
            capacity: 30,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    let saved = events::list_ticket_types(&db, &event.id).await.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].name, "Standard");
    assert_eq!(saved[0].price_cents, 1250);
    assert_eq!(saved[0].capacity, 30);
}

#[tokio::test]
async fn daylight_saving_and_timezone_changes_are_explicit() {
    let (_dir, db) = database().await;
    let mut proposed = input();
    proposed.date = "2027-03-28".to_owned();
    proposed.start_time = "01:30".to_owned();
    assert!(events::create(&db, &proposed)
        .await
        .unwrap_err()
        .to_string()
        .contains("does not exist"));
    proposed.date = "2027-10-31".to_owned();
    assert!(events::create(&db, &proposed)
        .await
        .unwrap_err()
        .to_string()
        .contains("occurs twice"));
    let event = events::create(&db, &input()).await.unwrap();
    events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "General".to_owned(),
            price: "5".to_owned(),
            capacity: 10,
            sales_open: Some("2027-06-01T10:00".to_owned()),
            sales_close: None,
        },
    )
    .await
    .unwrap();
    let mut changed = input();
    changed.timezone = "Europe/Bucharest".to_owned();
    assert!(events::update(&db, &event.id, &changed)
        .await
        .unwrap_err()
        .to_string()
        .contains("Clear saved sales windows"));
    assert_eq!(
        events::get(&db, &event.id).await.unwrap().unwrap().timezone,
        "Europe/Dublin"
    );
}

#[tokio::test]
async fn capacity_cannot_drop_below_open_holds() {
    let (_dir, db) = database().await;
    let event = events::create(&db, &input()).await.unwrap();
    let ticket_type = events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "General".to_owned(),
            price: "10".to_owned(),
            capacity: 5,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(&db, &event.id, Utc::now().timestamp())
        .await
        .unwrap();
    inventory::reserve(
        &db,
        &event.id,
        &[Selection {
            ticket_type_id: ticket_type.id.clone(),
            quantity: 2,
        }],
        Utc::now().timestamp(),
    )
    .await
    .unwrap();
    let mut reduced = input();
    reduced.event_capacity = Some(1);
    assert_eq!(
        events::update(&db, &event.id, &reduced)
            .await
            .unwrap_err()
            .to_string(),
        "Event capacity cannot be less than sold tickets and open holds"
    );
    let current = events::get(&db, &event.id).await.unwrap().unwrap();
    assert_eq!(current.event_capacity, Some(50));
    assert_eq!(
        events::update_ticket_type(
            &db,
            &current,
            &ticket_type.id,
            &TicketTypeInput {
                name: "General".to_owned(),
                price: "10".to_owned(),
                capacity: 1,
                sales_open: None,
                sales_close: None,
            },
        )
        .await
        .unwrap_err()
        .to_string(),
        "Ticket type capacity cannot be less than sold tickets and open holds"
    );
    assert_eq!(
        events::list_ticket_types(&db, &event.id).await.unwrap()[0].capacity,
        5
    );
}

#[tokio::test]
async fn paid_event_changes_queue_one_notice_per_order_and_edit() {
    let (_dir, db) = database().await;
    let event = events::create(&db, &input()).await.unwrap();
    let ticket_type = events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "General".to_owned(),
            price: "10".to_owned(),
            capacity: 5,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .unwrap()
        .generate_organizer_key("test")
        .unwrap()
        .1;
    admin_orders::complimentary(
        &db,
        &cipher,
        &event.id,
        &ticket_type.id,
        "buyer@example.com",
        "Alex",
        Utc::now().timestamp(),
    )
    .await
    .unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE events SET first_sale_at = ? WHERE id = ?",
        [Utc::now().timestamp().into(), event.id.clone().into()],
    ))
    .await
    .unwrap();
    let mut changed = input();
    changed.venue = "New Hall".to_owned();
    events::update(&db, &event.id, &changed).await.unwrap();
    let notices = db.query_all_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT n.old_place, n.new_place, r.sent_at FROM event_change_notices n JOIN event_change_recipients r ON r.notice_id = n.id".to_owned()
    )).await.unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].try_get::<String>("", "old_place").unwrap(),
        "Harbour Arts Centre, 1 Harbour Road"
    );
    assert_eq!(
        notices[0].try_get::<String>("", "new_place").unwrap(),
        "New Hall, 1 Harbour Road"
    );
    assert_eq!(
        notices[0].try_get::<Option<i64>>("", "sent_at").unwrap(),
        None
    );
    events::update(&db, &event.id, &changed).await.unwrap();
    let count = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM event_change_notices".to_owned(),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(count.try_get::<i64>("", "count").unwrap(), 1);
}

#[tokio::test]
async fn cg012_event_edits_revalidate_saved_windows_without_moving_them() {
    let (_dir, db) = database().await;
    let original = input();
    let event = events::create(&db, &original).await.unwrap();
    let kind = events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "Early".into(),
            price: "0".into(),
            capacity: 10,
            sales_open: Some("2027-06-10T10:00".into()),
            sales_close: Some("2027-06-15T18:00".into()),
        },
    )
    .await
    .unwrap();
    let mut changed = original.clone();
    changed.start_time = "17:00".into();
    assert_eq!(events::update(&db, &event.id, &changed).await.unwrap_err().to_string(),
        "Adjust the saved sales window for Early before changing the event time: Sales window must close before the event starts");
    assert_eq!(
        events::get(&db, &event.id)
            .await
            .unwrap()
            .unwrap()
            .start_time,
        "19:00"
    );
    changed.start_time = "18:00".into();
    let equal = events::update(&db, &event.id, &changed).await.unwrap();
    assert_eq!(equal.starts_at, 1813078800);
    changed.date = "2027-06-16".into();
    changed.end_time = "02:00".into();
    let later = events::update(&db, &event.id, &changed).await.unwrap();
    assert_eq!(later.starts_at, 1813165200);
    assert_eq!(later.ends_at, 1813194000);
    let saved = events::list_ticket_types(&db, &event.id)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(saved.sales_open, Some(1812618000));
    assert_eq!(saved.sales_close, Some(1813078800));
    events::publish(&db, &event.id, 1812617000).await.unwrap();
    let selection = [Selection {
        ticket_type_id: kind.id.clone(),
        quantity: 1,
    }];
    assert_eq!(
        inventory::reserve(&db, &event.id, &selection, 1812617999)
            .await
            .unwrap_err()
            .to_string(),
        "This ticket isn't on sale."
    );
    let opening_hold = inventory::reserve(&db, &event.id, &selection, 1812618000)
        .await
        .unwrap();
    assert_eq!(opening_hold.expires_at, 1812619860);
    assert_eq!(opening_hold.total_cents, 0);
    let closing_hold = inventory::reserve(&db, &event.id, &selection, 1813078799)
        .await
        .unwrap();
    assert_eq!(closing_hold.expires_at, 1813080659);
    assert_eq!(
        inventory::type_counts(&db, &kind.id, 1813078799)
            .await
            .unwrap(),
        (0, 1)
    );
    assert_eq!(
        inventory::reserve(&db, &event.id, &selection, 1813078800)
            .await
            .unwrap_err()
            .to_string(),
        "This ticket isn't on sale."
    );
    changed.timezone = "Europe/London".into();
    assert_eq!(events::update(&db, &event.id, &changed).await.unwrap_err().to_string(),
        "Clear saved sales windows before changing the event time zone. Then set the windows again in the new zone.");
    events::update_ticket_type(
        &db,
        &later,
        &kind.id,
        &TicketTypeInput {
            name: "Early".into(),
            price: "0".into(),
            capacity: 10,
            sales_open: Some("2027-06-10T10:00".into()),
            sales_close: None,
        },
    )
    .await
    .unwrap();
    changed.timezone = original.timezone.clone();
    changed.date = "2027-06-10".into();
    changed.start_time = "10:00".into();
    assert_eq!(events::update(&db, &event.id, &changed).await.unwrap_err().to_string(),
        "Adjust the saved sales window for Early before changing the event time: Sales window opening must be before closing");
    changed.start_time = "10:01".into();
    let moved = events::update(&db, &event.id, &changed).await.unwrap();
    assert_eq!(moved.starts_at, 1812618060);
    assert_eq!(
        events::list_ticket_types(&db, &event.id).await.unwrap()[0].sales_close,
        None
    );
}

#[tokio::test]
async fn cg012_default_close_tracks_date_zone_and_dst_edits() {
    let (_dir, db) = database().await;
    let event = events::create(&db, &input()).await.unwrap();
    events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "Standard".into(),
            price: "0".into(),
            capacity: 10,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    let mut changed = input();
    changed.date = "2027-03-28".into();
    changed.start_time = "01:30".into();
    assert!(events::update(&db, &event.id, &changed)
        .await
        .unwrap_err()
        .to_string()
        .contains("does not exist"));
    changed.date = "2027-10-31".into();
    assert!(events::update(&db, &event.id, &changed)
        .await
        .unwrap_err()
        .to_string()
        .contains("occurs twice"));
    assert_eq!(
        events::get(&db, &event.id).await.unwrap().unwrap().date,
        "2027-06-15"
    );
    changed.date = "2027-03-28".into();
    changed.start_time = "02:30".into();
    changed.end_time = "03:30".into();
    changed.timezone = "Europe/London".into();
    let saved = events::update(&db, &event.id, &changed).await.unwrap();
    assert_eq!(saved.starts_at, 1806197400);
    assert_eq!(saved.ends_at, 1806201000);
    events::publish(&db, &event.id, 1806190000).await.unwrap();
    let selection = [Selection {
        ticket_type_id: events::list_ticket_types(&db, &event.id).await.unwrap()[0]
            .id
            .clone(),
        quantity: 1,
    }];
    assert!(inventory::reserve(&db, &event.id, &selection, 1806197399)
        .await
        .is_ok());
    assert_eq!(
        inventory::reserve(&db, &event.id, &selection, 1806197400)
            .await
            .unwrap_err()
            .to_string(),
        "This ticket isn't on sale."
    );
}

fn cg012_ticket(close: Option<&str>) -> TicketTypeInput {
    TicketTypeInput {
        name: "Review type".into(),
        price: "0".into(),
        capacity: 100,
        sales_open: None,
        sales_close: close.map(str::to_owned),
    }
}
#[tokio::test]
async fn cg012_stale_ticket_add_must_reject_close_after_saved_start() {
    let (_dir, db) = database().await;
    let stale = events::create(&db, &input()).await.unwrap();
    let mut moved = input();
    moved.start_time = "17:00".into();
    events::update(&db, &stale.id, &moved).await.unwrap();
    let result =
        events::add_ticket_type(&db, &stale, &cg012_ticket(Some("2027-06-15T18:00"))).await;
    let saved = events::get(&db, &stale.id).await.unwrap().unwrap();
    let types = events::list_ticket_types(&db, &stale.id).await.unwrap();
    println!(
        "add result={result:?}; saved start={}; close={:?}",
        saved.starts_at,
        types.first().and_then(|t| t.sales_close)
    );
    assert_eq!(saved.starts_at, 1813075200);
    assert_eq!(
        result.unwrap_err().to_string(),
        "Sales window must close before the event starts"
    );
    assert!(types.is_empty());
}
#[tokio::test]
async fn cg012_stale_ticket_update_must_reject_close_after_saved_start() {
    let (_dir, db) = database().await;
    let stale = events::create(&db, &input()).await.unwrap();
    let kind = events::add_ticket_type(&db, &stale, &cg012_ticket(None))
        .await
        .unwrap();
    let mut moved = input();
    moved.start_time = "17:00".into();
    events::update(&db, &stale.id, &moved).await.unwrap();
    let result = events::update_ticket_type(
        &db,
        &stale,
        &kind.id,
        &cg012_ticket(Some("2027-06-15T18:00")),
    )
    .await;
    let saved = events::get(&db, &stale.id).await.unwrap().unwrap();
    let types = events::list_ticket_types(&db, &stale.id).await.unwrap();
    println!(
        "update result={result:?}; saved start={}; close={:?}",
        saved.starts_at, types[0].sales_close
    );
    assert_eq!(saved.starts_at, 1813075200);
    assert_eq!(
        result.unwrap_err().to_string(),
        "Sales window must close before the event starts"
    );
    assert_eq!(types[0].sales_close, None);
}
#[tokio::test]
async fn cg012_concurrent_event_writer_must_revalidate_against_locked_current_start() {
    let (dir, db) = database().await;
    let reader = sea_orm::Database::connect(format!(
        "sqlite://{}?mode=rw",
        dir.path().join("event.db").display()
    ))
    .await
    .unwrap();
    let event = events::create(&db, &input()).await.unwrap();
    let tx = db.begin().await.unwrap();
    tx.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE events SET updated_at=updated_at WHERE id=?",
        [event.id.clone().into()],
    ))
    .await
    .unwrap();
    let original = input();
    let pending = events::update(&reader, &event.id, &original);
    tokio::pin!(pending);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(250), &mut pending)
            .await
            .is_err(),
        "update must wait for the writer lock"
    );
    tx.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE events SET start_time='20:00', starts_at=1813086000 WHERE id=?",
        [event.id.clone().into()],
    ))
    .await
    .unwrap();
    let later = events::get(&tx, &event.id).await.unwrap().unwrap();
    events::add_ticket_type(&tx, &later, &cg012_ticket(Some("2027-06-15T19:30")))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let result = pending.await;
    let saved = events::get(&db, &event.id).await.unwrap().unwrap();
    let types = events::list_ticket_types(&db, &event.id).await.unwrap();
    println!(
        "pending result={result:?}; saved start={}; close={:?}",
        saved.starts_at, types[0].sales_close
    );
    assert_eq!(
        result.unwrap_err().to_string(),
        "Adjust the saved sales window for Review type before changing the event time: Sales window must close before the event starts"
    );
    assert_eq!(saved.starts_at, 1813086000);
    assert_eq!(types[0].sales_close, Some(1813084200));
}

#[tokio::test]
async fn cg012_ticket_writers_wait_and_validate_the_committed_event() {
    for edit in [false, true] {
        let (dir, db) = database().await;
        let reader = sea_orm::Database::connect(format!(
            "sqlite://{}?mode=rw",
            dir.path().join("event.db").display()
        ))
        .await
        .unwrap();
        let stale = events::create(&db, &input()).await.unwrap();
        let kind = events::add_ticket_type(&db, &stale, &cg012_ticket(None))
            .await
            .unwrap();
        let tx = db.begin().await.unwrap();
        tx.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET start_time='17:00', starts_at=1813075200 WHERE id=?",
            [stale.id.clone().into()],
        ))
        .await
        .unwrap();
        let ticket = cg012_ticket(Some("2027-06-15T18:00"));
        let pending = async {
            if edit {
                events::update_ticket_type(&reader, &stale, &kind.id, &ticket).await
            } else {
                events::add_ticket_type(&reader, &stale, &ticket)
                    .await
                    .map(|_| ())
            }
        };
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut pending)
                .await
                .is_err()
        );
        tx.commit().await.unwrap();
        assert_eq!(
            pending.await.unwrap_err().to_string(),
            "Sales window must close before the event starts"
        );
        let saved = events::list_ticket_types(&db, &stale.id).await.unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].sales_close, None);
        assert_eq!(
            events::get(&db, &stale.id)
                .await
                .unwrap()
                .unwrap()
                .starts_at,
            1813075200
        );
    }
}

#[tokio::test]
async fn cg012_stale_ticket_snapshot_uses_current_zone_and_cancel_status() {
    let (_dir, db) = database().await;
    let stale = events::create(&db, &input()).await.unwrap();
    let kind = events::add_ticket_type(&db, &stale, &cg012_ticket(None))
        .await
        .unwrap();
    let mut paris = input();
    paris.timezone = "Europe/Paris".into();
    events::update(&db, &stale.id, &paris).await.unwrap();
    let ticket = cg012_ticket(Some("2027-06-15T18:00"));
    let added = events::add_ticket_type(&db, &stale, &ticket).await.unwrap();
    events::update_ticket_type(&db, &stale, &kind.id, &ticket)
        .await
        .unwrap();
    let saved = events::list_ticket_types(&db, &stale.id).await.unwrap();
    assert_eq!(added.sales_close, Some(1813075200));
    assert_eq!(saved.len(), 2);
    assert!(saved
        .iter()
        .all(|item| item.sales_close == Some(1813075200)));
    assert_eq!(
        events::get(&db, &stale.id)
            .await
            .unwrap()
            .unwrap()
            .starts_at,
        1813078800
    );
    admin_orders::cancel_event(&db, &stale.id, 1800000000)
        .await
        .unwrap();
    assert_eq!(
        events::add_ticket_type(&db, &stale, &cg012_ticket(None))
            .await
            .unwrap_err()
            .to_string(),
        "A Cancelled Event cannot be edited"
    );
    assert_eq!(
        events::update_ticket_type(&db, &stale, &kind.id, &cg012_ticket(None))
            .await
            .unwrap_err()
            .to_string(),
        "A Cancelled Event cannot be edited"
    );
    let saved = events::list_ticket_types(&db, &stale.id).await.unwrap();
    assert_eq!(saved.len(), 2);
    assert!(saved
        .iter()
        .all(|item| item.sales_close == Some(1813075200)));
}

#[tokio::test]
async fn cg012_waiting_event_edit_checks_committed_status_and_timezone() {
    for cancelled in [false, true] {
        let (dir, db) = database().await;
        let reader = sea_orm::Database::connect(format!(
            "sqlite://{}?mode=rw",
            dir.path().join("event.db").display()
        ))
        .await
        .unwrap();
        let event = events::create(&db, &input()).await.unwrap();
        let tx = db.begin().await.unwrap();
        tx.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "UPDATE events SET timezone='Europe/Paris', starts_at=1813078800, ends_at=1813086000, status=? WHERE id=?",
            [if cancelled { "Cancelled" } else { "Draft" }.into(), event.id.clone().into()])).await.unwrap();
        let original = input();
        let pending = events::update(&reader, &event.id, &original);
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut pending)
                .await
                .is_err()
        );
        if !cancelled {
            events::add_ticket_type(&tx, &event, &cg012_ticket(Some("2027-06-15T18:00")))
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        let error = pending.await.unwrap_err().to_string();
        assert_eq!(
            error,
            if cancelled {
                "A Cancelled Event cannot be edited"
            } else {
                "Clear saved sales windows before changing the event time zone. Then set the windows again in the new zone."
            }
        );
        let saved = events::get(&db, &event.id).await.unwrap().unwrap();
        assert_eq!(saved.timezone, "Europe/Paris");
        assert_eq!(saved.starts_at, 1813078800);
        assert_eq!(saved.status, if cancelled { "Cancelled" } else { "Draft" });
        let types = events::list_ticket_types(&db, &event.id).await.unwrap();
        assert_eq!(types.len(), if cancelled { 0 } else { 1 });
        if !cancelled {
            assert_eq!(types[0].sales_close, Some(1813075200));
        }
    }
}

#[tokio::test]
async fn cg012_ticket_transaction_rolls_back_and_missing_events_reject_writes() {
    let (_dir, db) = database().await;
    let event = events::create(&db, &input()).await.unwrap();
    let tx = db.begin().await.unwrap();
    let created = events::add_ticket_type(&tx, &event, &cg012_ticket(None))
        .await
        .unwrap();
    assert_eq!(created.name, "Review type");
    assert_eq!(
        events::list_ticket_types(&tx, &event.id)
            .await
            .unwrap()
            .len(),
        1
    );
    tx.rollback().await.unwrap();
    assert!(events::list_ticket_types(&db, &event.id)
        .await
        .unwrap()
        .is_empty());
    let mut missing = event.clone();
    missing.id = "missing-event".into();
    assert!(matches!(
        events::add_ticket_type(&db, &missing, &cg012_ticket(None)).await,
        Err(events::EventError::NotFound)
    ));
    assert!(matches!(
        events::update_ticket_type(&db, &missing, &created.id, &cg012_ticket(None)).await,
        Err(events::EventError::NotFound)
    ));
    assert!(matches!(
        events::update(&db, &missing.id, &input()).await,
        Err(events::EventError::NotFound)
    ));
    let saved = events::add_ticket_type(&db, &event, &cg012_ticket(None))
        .await
        .unwrap();
    assert_eq!(
        events::list_ticket_types(&db, &event.id).await.unwrap()[0].id,
        saved.id
    );
}

#[tokio::test]
async fn cg012_ticket_edit_preserves_assigned_seat_capacity() {
    let (_dir, db) = database().await;
    let event = events::create(&db, &input()).await.unwrap();
    let kind = events::add_ticket_type(&db, &event, &cg012_ticket(None))
        .await
        .unwrap();
    crabgate::seats::add_many(
        &db,
        &event.id,
        &kind.id,
        "Main",
        "A",
        &["1".into(), "2".into()],
        1800000000,
    )
    .await
    .unwrap();
    let mut proposed = cg012_ticket(None);
    proposed.capacity = 1;
    proposed.name = "Rejected".into();
    assert_eq!(
        events::update_ticket_type(&db, &event, &kind.id, &proposed)
            .await
            .unwrap_err()
            .to_string(),
        "Ticket type capacity cannot be less than its assigned seats"
    );
    let saved = events::list_ticket_types(&db, &event.id).await.unwrap();
    assert_eq!(saved[0].name, "Review type");
    assert_eq!(saved[0].capacity, 100);
    proposed.capacity = 2;
    proposed.name = "Two seats".into();
    events::update_ticket_type(&db, &event, &kind.id, &proposed)
        .await
        .unwrap();
    let saved = events::list_ticket_types(&db, &event.id).await.unwrap();
    assert_eq!(saved[0].name, "Two seats");
    assert_eq!(saved[0].capacity, 2);
    assert_eq!(
        crabgate::seats::list(&db, &event.id, 1800000000)
            .await
            .unwrap()
            .len(),
        2
    );
}
