use chrono::Utc;
use crabgate::{
    crypto::MasterKey,
    discounts::{self, DiscountError},
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, InventoryError, Selection},
    orders::{self, PaymentOutcome},
};
use loco_rs::db;
use migration::OrganizerMigrator;

#[tokio::test]
async fn discount_limit_reserves_with_hold_and_paid_total_is_verified() {
    let dir = tempfile::tempdir().unwrap();
    let db = sea_orm::Database::connect(format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("discounts.db").display()
    ))
    .await
    .unwrap();
    db::migrate::<OrganizerMigrator>(&db).await.unwrap();
    let event = events::create(
        &db,
        &EventInput {
            title: "Discount test".to_owned(),
            date: "2027-06-15".to_owned(),
            start_time: "19:00".to_owned(),
            end_time: "21:00".to_owned(),
            timezone: "Europe/Dublin".to_owned(),
            venue: "Hall".to_owned(),
            address: "Street".to_owned(),
            description: String::new(),
            event_capacity: None,
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
            name: "General".to_owned(),
            price: "10.00".to_owned(),
            capacity: 10,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    let now = Utc::now().timestamp();
    let second_kind = events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "Balcony".to_owned(),
            price: "5.00".to_owned(),
            capacity: 10,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    discounts::create(
        &db,
        discounts::NewDiscount {
            event_id: &event.id,
            code: "SAVE20",
            kind: "Percent",
            value: 20,
            starts_at: None,
            ends_at: None,
            max_redemptions: Some(1),
        },
        now,
    )
    .await
    .unwrap();
    discounts::create(
        &db,
        discounts::NewDiscount {
            event_id: &event.id,
            code: "FREE",
            kind: "Percent",
            value: 100,
            starts_at: None,
            ends_at: None,
            max_redemptions: None,
        },
        now,
    )
    .await
    .unwrap();
    discounts::create(
        &db,
        discounts::NewDiscount {
            event_id: &event.id,
            code: "FIVE",
            kind: "Fixed",
            value: 500,
            starts_at: Some(now + 1),
            ends_at: Some(now + 100),
            max_redemptions: None,
        },
        now,
    )
    .await
    .unwrap();
    events::publish(&db, &event.id, now).await.unwrap();
    let selection = [Selection {
        ticket_type_id: kind.id.clone(),
        quantity: 1,
    }];
    let (left, right) = tokio::join!(
        inventory::reserve_with_discount(&db, &event.id, &selection, Some("save20"), now),
        inventory::reserve_with_discount(&db, &event.id, &selection, Some("SAVE20"), now)
    );
    let first = match (left, right) {
        (Ok(hold), Err(InventoryError::Discount(DiscountError::Unavailable)))
        | (Err(InventoryError::Discount(DiscountError::Unavailable)), Ok(hold)) => hold,
        _ => panic!("last discount redemption was not reserved once"),
    };
    assert_eq!(first.total_cents, 800);
    let details = orders::hold_details(&db, &first.id).await.unwrap();
    assert_eq!(details.subtotal_cents, 1000);
    assert_eq!(details.discount_cents, 200);
    assert_eq!(details.discount_code.as_deref(), Some("SAVE20"));
    orders::expire_hold(&db, &first.id).await.unwrap();
    let second = inventory::reserve_with_discount(&db, &event.id, &selection, Some("SAVE20"), now)
        .await
        .unwrap();
    assert_eq!(second.total_cents, 800);
    let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .unwrap()
        .generate_organizer_key("test")
        .unwrap()
        .1;
    let details = orders::hold_details(&db, &second.id).await.unwrap();
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
    orders::attach_session(&db, &second.id, "cs_discount", now)
        .await
        .unwrap();
    assert!(matches!(
        orders::complete_payment(&db, "cs_discount", "pi_discount", 1000, now)
            .await
            .unwrap(),
        PaymentOutcome::RefundNeeded
    ));
    let correct = inventory::reserve_with_discount(&db, &event.id, &selection, Some("SAVE20"), now)
        .await
        .unwrap();
    let details = orders::hold_details(&db, &correct.id).await.unwrap();
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
    orders::attach_session(&db, &correct.id, "cs_discount_correct", now)
        .await
        .unwrap();
    let id =
        match orders::complete_payment(&db, "cs_discount_correct", "pi_discount_correct", 800, now)
            .await
            .unwrap()
        {
            PaymentOutcome::Paid(id) => id,
            _ => panic!("discounted payment did not create an Order"),
        };
    let order = orders::get_order(&db, &cipher, &id).await.unwrap();
    assert_eq!(order.total_cents, 800);
    assert_eq!(
        discounts::order_summary(&db, &id).await.unwrap(),
        Some(("SAVE20".to_owned(), 1000, 200))
    );
    assert!(matches!(
        inventory::reserve_with_discount(&db, &event.id, &selection, Some("SAVE20"), now).await,
        Err(InventoryError::Discount(DiscountError::Unavailable))
    ));
    let free = inventory::reserve_with_discount(&db, &event.id, &selection, Some("FREE"), now)
        .await
        .unwrap();
    assert_eq!(free.total_cents, 0);
    let free_details = orders::hold_details(&db, &free.id).await.unwrap();
    orders::save_details(
        &db,
        &cipher,
        &free_details,
        "free@example.com",
        &[String::new()],
        false,
        now,
    )
    .await
    .unwrap();
    orders::attach_session(&db, &free.id, "free_discount", now)
        .await
        .unwrap();
    assert!(matches!(
        orders::complete_payment(&db, "free_discount", "", 0, now)
            .await
            .unwrap(),
        PaymentOutcome::Paid(_)
    ));
    assert!(matches!(
        inventory::reserve_with_discount(&db, &event.id, &selection, Some("FIVE"), now).await,
        Err(InventoryError::Discount(DiscountError::Unavailable))
    ));
    let mixed = inventory::reserve_with_discount(
        &db,
        &event.id,
        &[
            Selection {
                ticket_type_id: kind.id.clone(),
                quantity: 1,
            },
            Selection {
                ticket_type_id: second_kind.id,
                quantity: 1,
            },
        ],
        Some("FIVE"),
        now + 2,
    )
    .await
    .unwrap();
    assert_eq!(mixed.total_cents, 1000);
    assert_eq!(
        orders::hold_details(&db, &mixed.id)
            .await
            .unwrap()
            .subtotal_cents,
        1500
    );
    let five = discounts::list(&db, &event.id)
        .await
        .unwrap()
        .into_iter()
        .find(|code| code.code == "FIVE")
        .unwrap();
    discounts::deactivate(&db, &event.id, &five.id)
        .await
        .unwrap();
    assert!(matches!(
        inventory::reserve_with_discount(&db, &event.id, &selection, Some("FIVE"), now + 3).await,
        Err(InventoryError::Discount(DiscountError::Unavailable))
    ));
}
