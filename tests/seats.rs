use chrono::Utc;
use crabgate::{
    admin_orders::{self, Report},
    crypto::MasterKey,
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, Selection},
    orders::{self, PaymentOutcome},
    seats,
};
use loco_rs::db;
use migration::OrganizerMigrator;

#[tokio::test]
async fn an_expired_seat_hold_cannot_fulfill_after_another_buyer_takes_the_seat() {
    let dir = tempfile::tempdir().unwrap();
    let db = sea_orm::Database::connect(format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("seats.db").display()
    ))
    .await
    .unwrap();
    db::migrate::<OrganizerMigrator>(&db).await.unwrap();
    let event = events::create(
        &db,
        &EventInput {
            title: "Seated show".to_owned(),
            date: "2027-06-15".to_owned(),
            start_time: "19:00".to_owned(),
            end_time: "21:00".to_owned(),
            timezone: "Europe/Dublin".to_owned(),
            venue: "Hall".to_owned(),
            address: "1 Main Street".to_owned(),
            description: String::new(),
            event_capacity: Some(2),
            max_per_order: 2,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let ticket_type = events::add_ticket_type(
        &db,
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
    seats::add_many(
        &db,
        &event.id,
        &ticket_type.id,
        "Main",
        "A",
        &["1".to_owned()],
        now,
    )
    .await
    .unwrap();
    let seat = seats::list(&db, &event.id, now).await.unwrap().remove(0);
    events::publish(&db, &event.id, now).await.unwrap();
    let selection = [Selection {
        ticket_type_id: ticket_type.id,
        quantity: 1,
    }];
    let ids = [seat.id.clone()];
    let first = inventory::reserve_with_options(&db, &event.id, &selection, None, &ids, now)
        .await
        .unwrap();
    assert!(
        inventory::reserve_with_options(&db, &event.id, &selection, None, &ids, now)
            .await
            .is_err()
    );
    let later = first.expires_at + 1;
    let second = inventory::reserve_with_options(&db, &event.id, &selection, None, &ids, later)
        .await
        .unwrap();
    let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .unwrap()
        .generate_organizer_key("test")
        .unwrap()
        .1;
    for (hold, session) in [(&first, "cs_first"), (&second, "cs_second")] {
        let details = orders::hold_details(&db, &hold.id).await.unwrap();
        if hold.id == second.id {
            orders::save_details(
                &db,
                &cipher,
                &details,
                "buyer@example.com",
                &[String::new()],
                false,
                later,
            )
            .await
            .unwrap();
        } else {
            orders::save_details(
                &db,
                &cipher,
                &details,
                "buyer@example.com",
                &[String::new()],
                false,
                now,
            )
            .await
            .unwrap();
        }
        orders::attach_session(&db, &hold.id, session, now)
            .await
            .unwrap();
    }
    assert!(matches!(
        orders::complete_payment(&db, "cs_first", "pi_first", 1000, later)
            .await
            .unwrap(),
        PaymentOutcome::RefundNeeded
    ));
    let paid = orders::complete_payment(&db, "cs_second", "pi_second", 1000, later)
        .await
        .unwrap();
    let PaymentOutcome::Paid(order_id) = paid else {
        panic!("second Buyer did not receive the seat")
    };
    let order = orders::get_order(&db, &cipher, &order_id).await.unwrap();
    assert_eq!(
        order.tickets[0].seat.as_deref(),
        Some("Main, Row A, Seat 1")
    );
    let door = crabgate::door::search(&db, &cipher, &event.id, &order.tickets[0].code)
        .await
        .unwrap();
    assert_eq!(door[0].seat.as_deref(), Some("Main, Row A, Seat 1"));
    let csv = admin_orders::report(&db, &cipher, &event.id, Report::Tickets)
        .await
        .unwrap();
    assert!(csv.contains("Main, Row A, Seat 1"));
    assert!(seats::deactivate(&db, &event.id, &seat.id, later)
        .await
        .is_err());
    seats::add_many(
        &db,
        &event.id,
        &selection[0].ticket_type_id,
        "Main",
        "A",
        &["2".to_owned()],
        later,
    )
    .await
    .unwrap();
    assert!(admin_orders::complimentary(
        &db,
        &cipher,
        &event.id,
        &selection[0].ticket_type_id,
        "guest@example.com",
        "Guest",
        later,
    )
    .await
    .is_err());
    let second_seat = seats::list(&db, &event.id, later)
        .await
        .unwrap()
        .into_iter()
        .find(|seat| seat.label == "2")
        .unwrap();
    let comp_id = admin_orders::complimentary_with_seat(
        &db,
        &cipher,
        &event.id,
        (&selection[0].ticket_type_id, Some(&second_seat.id)),
        "guest@example.com",
        "Guest",
        later,
    )
    .await
    .unwrap();
    let comp = orders::get_order(&db, &cipher, &comp_id).await.unwrap();
    assert_eq!(comp.tickets[0].seat.as_deref(), Some("Main, Row A, Seat 2"));
}
