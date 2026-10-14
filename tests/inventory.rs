use chrono::Utc;
use crabgate::{
    crypto::MasterKey,
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, Selection},
    orders::{self, PaymentOutcome},
};
use loco_rs::db;
use migration::OrganizerMigrator;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn simultaneous_checkouts_reserve_only_the_last_two_tickets() {
    let dir = tempfile::tempdir().unwrap();
    let connection = sea_orm::Database::connect(format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("inventory.db").display()
    ))
    .await
    .unwrap();
    db::migrate::<OrganizerMigrator>(&connection).await.unwrap();
    let event = events::create(
        &connection,
        &EventInput {
            title: "Last seats".to_owned(),
            date: "2027-06-15".to_owned(),
            start_time: "19:00".to_owned(),
            end_time: "21:00".to_owned(),
            timezone: "Europe/Dublin".to_owned(),
            venue: "Hall".to_owned(),
            address: "1 Main Street".to_owned(),
            description: String::new(),
            event_capacity: Some(2),
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
            capacity: 2,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    let now = Utc::now().timestamp();
    events::publish(&connection, &event.id, now).await.unwrap();
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let db = connection.clone();
        let event_id = event.id.clone();
        let ticket_type_id = ticket_type.id.clone();
        tasks.push(tokio::spawn(async move {
            inventory::reserve(
                &db,
                &event_id,
                &[Selection {
                    ticket_type_id,
                    quantity: 1,
                }],
                now,
            )
            .await
        }));
    }
    let mut holds = Vec::new();
    for task in tasks {
        if let Ok(hold) = task.await.unwrap() {
            holds.push(hold);
        }
    }
    assert_eq!(holds.len(), 2);
    assert_eq!(
        inventory::open_counts(&connection, &event.id, now)
            .await
            .unwrap(),
        (0, 2)
    );
    let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .unwrap()
        .generate_organizer_key("test")
        .unwrap()
        .1;
    for (index, hold) in holds.iter().enumerate() {
        let details = orders::hold_details(&connection, &hold.id).await.unwrap();
        orders::save_details(
            &connection,
            &cipher,
            &details,
            "buyer@example.com",
            &[String::new()],
            false,
            now,
        )
        .await
        .unwrap();
        let session_id = format!("cs_test_{index}");
        orders::attach_session(&connection, &hold.id, &session_id, now)
            .await
            .unwrap();
        assert!(matches!(
            orders::complete_payment(
                &connection,
                &session_id,
                &format!("pi_test_{index}"),
                1000,
                now + 1
            )
            .await
            .unwrap(),
            PaymentOutcome::Paid(_)
        ));
    }
    assert_eq!(
        inventory::open_counts(&connection, &event.id, now + 1)
            .await
            .unwrap(),
        (2, 0)
    );
}
