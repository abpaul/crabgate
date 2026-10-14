use crabgate::{
    crypto::{MasterKey, OrganizerCipher},
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, Selection},
    orders::{self, OrderError, PaymentOutcome},
    settings,
};
use loco_rs::db;
use migration::OrganizerMigrator;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

#[derive(Clone, Copy, Debug)]
enum Terms {
    Disabled,
    Optional,
    Required,
    LegacyNull,
}

async fn set_terms(db: &DatabaseConnection, terms: Terms) {
    let (url, required) = match terms {
        Terms::Disabled | Terms::LegacyNull => ("", false),
        Terms::Optional => ("https://example.com/optional", false),
        Terms::Required => ("https://example.com/required", true),
    };
    settings::set_booking_terms(db, url, required)
        .await
        .unwrap();
}

async fn setup(
    initial: Terms,
    paid: bool,
) -> (DatabaseConnection, OrganizerCipher, orders::HoldDetails) {
    let path = tempfile::tempdir().unwrap().keep().join("terms.db");
    println!("case initial={initial:?} paid={paid} db={}", path.display());
    let db = sea_orm::Database::connect(format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap();
    db::migrate::<OrganizerMigrator>(&db).await.unwrap();
    set_terms(&db, initial).await;
    let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .unwrap()
        .generate_organizer_key("terms-service")
        .unwrap()
        .1;
    let event = events::create(
        &db,
        &EventInput {
            title: "Terms service".into(),
            date: "2027-06-15".into(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Hall".into(),
            address: "Street".into(),
            description: String::new(),
            event_capacity: Some(2),
            max_per_order: 2,
            attendee_name_required: true,
        },
    )
    .await
    .unwrap();
    let kind = events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "Standard".into(),
            price: if paid { "10" } else { "0" }.into(),
            capacity: 2,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    let now = orders::now();
    events::publish(&db, &event.id, now).await.unwrap();
    let hold = inventory::reserve(
        &db,
        &event.id,
        &[Selection {
            ticket_type_id: kind.id,
            quantity: 1,
        }],
        now,
    )
    .await
    .unwrap();
    if matches!(initial, Terms::LegacyNull) {
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE holds SET booking_terms_snapshot = NULL WHERE id = ?",
            [hold.id.clone().into()],
        ))
        .await
        .unwrap();
    }
    let details = orders::hold_details(&db, &hold.id).await.unwrap();
    (db, cipher, details)
}

async fn facts(
    db: &DatabaseConnection,
    id: &str,
) -> (Option<String>, Option<String>, Option<i64>, Option<i64>) {
    let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT booking_terms_snapshot, booking_terms_token, booking_terms_submitted_at, booking_terms_accepted_at FROM holds WHERE id = ?", [id.into()]
    )).await.unwrap().unwrap();
    (
        row.try_get("", "booking_terms_snapshot").unwrap(),
        row.try_get("", "booking_terms_token").unwrap(),
        row.try_get("", "booking_terms_submitted_at").unwrap(),
        row.try_get("", "booking_terms_accepted_at").unwrap(),
    )
}

async fn save(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    hold: &orders::HoldDetails,
    now: i64,
) -> Result<(), OrderError> {
    orders::save_details(
        db,
        cipher,
        hold,
        "buyer@example.com",
        &["Buyer".into()],
        true,
        now,
    )
    .await
}

async fn assert_no_details_or_tickets(db: &DatabaseConnection, hold: &orders::HoldDetails) {
    let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT buyer_email_encrypted, (SELECT COUNT(*) FROM hold_attendees WHERE hold_id = ?) AS attendees, (SELECT COUNT(*) FROM orders) AS orders, (SELECT COUNT(*) FROM tickets) AS tickets FROM holds WHERE id = ?",
        [hold.id.clone().into(), hold.id.clone().into()]
    )).await.unwrap().unwrap();
    assert_eq!(
        row.try_get::<Option<String>>("", "buyer_email_encrypted")
            .unwrap(),
        None
    );
    assert_eq!(row.try_get::<i64>("", "attendees").unwrap(), 0);
    assert_eq!(row.try_get::<i64>("", "orders").unwrap(), 0);
    assert_eq!(row.try_get::<i64>("", "tickets").unwrap(), 0);
    assert_eq!(orders::order_id_for_hold(db, &hold.id).await.unwrap(), None);
}

#[tokio::test]
async fn new_no_terms_save_rejects_current_required_and_rolls_back_free_and_paid() {
    for initial in [
        Terms::Disabled,
        Terms::Optional,
        Terms::LegacyNull,
        Terms::Required,
    ] {
        for paid in [false, true] {
            let (db, cipher, hold) = setup(initial, paid).await;
            let before = facts(&db, &hold.id).await;
            set_terms(&db, Terms::Required).await;
            let now = orders::now();
            assert!(
                matches!(
                    save(&db, &cipher, &hold, now).await,
                    Err(OrderError::BookingTerms(
                        crabgate::booking_terms::BookingTermsError::AcknowledgementRequired
                    ))
                ),
                "initial={initial:?} paid={paid}"
            );
            assert_eq!(facts(&db, &hold.id).await, before);
            assert_no_details_or_tickets(&db, &hold).await;
            orders::attach_session(&db, &hold.id, "cs_rejected", now)
                .await
                .unwrap();
            assert!(matches!(
                orders::complete_payment(&db, "cs_rejected", "pi_synthetic", hold.total_cents, now)
                    .await
                    .unwrap(),
                PaymentOutcome::RefundNeeded
            ));
            assert_no_details_or_tickets(&db, &hold).await;
            db.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn disabled_optional_and_old_holds_record_submission_without_inventing_acknowledgement() {
    for initial in [Terms::Disabled, Terms::Optional, Terms::LegacyNull] {
        for current in [Terms::Disabled, Terms::Optional] {
            for paid in [false, true] {
                let (db, cipher, hold) = setup(initial, paid).await;
                set_terms(&db, current).await;
                let before = facts(&db, &hold.id).await;
                let now = orders::now();
                save(&db, &cipher, &hold, now).await.unwrap();
                let submitted = facts(&db, &hold.id).await;
                assert_eq!(submitted, (before.0.clone(), before.1, Some(now), None));
                set_terms(&db, Terms::Required).await;
                let displayed = crabgate::booking_terms::display(&db, &hold.id, now + 1)
                    .await
                    .unwrap();
                assert!(!displayed.terms.required);
                assert_eq!(
                    displayed.terms.url,
                    if matches!(initial, Terms::Optional) {
                        "https://example.com/optional"
                    } else {
                        ""
                    }
                );
                assert_eq!(facts(&db, &hold.id).await, submitted);
                save(&db, &cipher, &hold, now + 1).await.unwrap();
                assert_eq!(facts(&db, &hold.id).await, submitted);
                orders::attach_session(&db, &hold.id, "cs_submitted", now)
                    .await
                    .unwrap();
                let PaymentOutcome::Paid(id) = orders::complete_payment(
                    &db,
                    "cs_submitted",
                    "pi_synthetic",
                    hold.total_cents,
                    now,
                )
                .await
                .unwrap() else {
                    panic!("submitted payment must succeed")
                };
                let order = orders::get_order(&db, &cipher, &id).await.unwrap();
                assert_eq!(order.buyer_email, "buyer@example.com");
                assert_eq!(order.tickets.len(), 1);
                assert_eq!(order.tickets[0].attendee_name.as_deref(), Some("Buyer"));
                let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                    "SELECT booking_terms_snapshot, booking_terms_accepted_at FROM orders WHERE id = ?", [id.clone().into()]
                )).await.unwrap().unwrap();
                assert_eq!(
                    row.try_get::<Option<String>>("", "booking_terms_snapshot")
                        .unwrap(),
                    before.0
                );
                assert_eq!(
                    row.try_get::<Option<i64>>("", "booking_terms_accepted_at")
                        .unwrap(),
                    None
                );
                set_terms(&db, Terms::Optional).await;
                assert!(
                    matches!(orders::complete_payment(&db, "cs_submitted", "pi_synthetic", hold.total_cents, now + 2).await.unwrap(), PaymentOutcome::AlreadyProcessed(existing) if existing == id)
                );
                assert_eq!(facts(&db, &hold.id).await, submitted);
                assert_eq!(
                    orders::get_order(&db, &cipher, &id)
                        .await
                        .unwrap()
                        .tickets
                        .len(),
                    1
                );
                db.close().await.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn unsubmitted_required_snapshot_still_needs_terms_input_when_current_is_not_required() {
    for current in [Terms::Disabled, Terms::Optional] {
        for paid in [false, true] {
            let (db, cipher, hold) = setup(Terms::Required, paid).await;
            let before = facts(&db, &hold.id).await;
            set_terms(&db, current).await;
            assert!(matches!(
                save(&db, &cipher, &hold, orders::now()).await,
                Err(OrderError::BookingTerms(
                    crabgate::booking_terms::BookingTermsError::AcknowledgementRequired
                ))
            ));
            assert_eq!(facts(&db, &hold.id).await, before);
            assert_no_details_or_tickets(&db, &hold).await;
            db.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn migration_era_saved_payment_callbacks_keep_null_evidence_under_current_required() {
    for paid in [false, true] {
        let (db, cipher, hold) = setup(Terms::LegacyNull, paid).await;
        let now = orders::now();
        save(&db, &cipher, &hold, now).await.unwrap();
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE holds SET booking_terms_submitted_at = NULL WHERE id = ?",
            [hold.id.clone().into()],
        ))
        .await
        .unwrap();
        assert_eq!(facts(&db, &hold.id).await, (None, None, None, None));
        orders::attach_session(&db, &hold.id, "cs_legacy", now)
            .await
            .unwrap();
        set_terms(&db, Terms::Required).await;
        let PaymentOutcome::Paid(id) =
            orders::complete_payment(&db, "cs_legacy", "pi_synthetic", hold.total_cents, now)
                .await
                .unwrap()
        else {
            panic!("legacy callback must succeed")
        };
        assert_eq!(
            orders::get_order(&db, &cipher, &id)
                .await
                .unwrap()
                .tickets
                .len(),
            1
        );
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT booking_terms_snapshot, booking_terms_accepted_at FROM orders WHERE id = ?",
                [id.clone().into()],
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.try_get::<Option<String>>("", "booking_terms_snapshot")
                .unwrap(),
            None
        );
        assert_eq!(
            row.try_get::<Option<i64>>("", "booking_terms_accepted_at")
                .unwrap(),
            None
        );
        assert!(
            matches!(orders::complete_payment(&db, "cs_legacy", "pi_synthetic", hold.total_cents, now + 1).await.unwrap(), PaymentOutcome::AlreadyProcessed(existing) if existing == id)
        );
        assert_eq!(facts(&db, &hold.id).await, (None, None, None, None));
        db.close().await.unwrap();
    }
}
