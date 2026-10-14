use std::collections::HashMap;

use chrono::Utc;
use crabgate::{
    admin_orders,
    crypto::MasterKey,
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, Selection},
    orders::{self, PaymentOutcome},
    questions::{self, QuestionError},
};
use loco_rs::db;
use migration::OrganizerMigrator;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

#[tokio::test]
async fn questions_validate_encrypt_snapshot_and_anonymize_answers() {
    let dir = tempfile::tempdir().unwrap();
    let db = sea_orm::Database::connect(format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("questions.db").display()
    ))
    .await
    .unwrap();
    db::migrate::<OrganizerMigrator>(&db).await.unwrap();
    let event = events::create(
        &db,
        &EventInput {
            title: "Questions test".to_owned(),
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
            price: "0".to_owned(),
            capacity: 10,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    let choice_id = questions::add(
        &db,
        &event.id,
        "Order",
        "Choice",
        "Meal choice",
        &["Vegetarian".to_owned(), "Standard".to_owned()],
        true,
    )
    .await
    .unwrap();
    let text_id = questions::add(
        &db,
        &event.id,
        "Attendee",
        "Text",
        "Accessibility need",
        &[],
        false,
    )
    .await
    .unwrap();
    assert!(matches!(
        questions::add(
            &db,
            &event.id,
            "Order",
            "Choice",
            "Bad",
            &["Same".to_owned(), "Same".to_owned()],
            true
        )
        .await,
        Err(QuestionError::InvalidOptions)
    ));
    events::publish(&db, &event.id, Utc::now().timestamp())
        .await
        .unwrap();
    let now = Utc::now().timestamp();
    let hold = inventory::reserve(
        &db,
        &event.id,
        &[Selection {
            ticket_type_id: kind.id.clone(),
            quantity: 1,
        }],
        now,
    )
    .await
    .unwrap();
    let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .unwrap()
        .generate_organizer_key("test")
        .unwrap()
        .1;
    let configured = questions::list(&db, &event.id).await.unwrap();
    let mut form = HashMap::new();
    form.insert(format!("question_{choice_id}"), "Unknown".to_owned());
    assert!(matches!(
        questions::save_answers(&db, &cipher, &hold.id, &configured, 1, &form).await,
        Err(QuestionError::InvalidAnswer)
    ));
    form.insert(format!("question_{choice_id}"), "Vegetarian".to_owned());
    form.insert(
        format!("question_{text_id}_0"),
        "Wheelchair space".to_owned(),
    );
    questions::save_answers(&db, &cipher, &hold.id, &configured, 1, &form)
        .await
        .unwrap();
    let encrypted = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT answer_encrypted FROM hold_answers WHERE hold_id = ? AND question_id = ?",
            [hold.id.clone().into(), text_id.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "answer_encrypted")
        .unwrap();
    assert!(!encrypted.contains("Wheelchair"));
    let details = orders::hold_details(&db, &hold.id).await.unwrap();
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
    orders::attach_session(&db, &hold.id, "free_questions", now)
        .await
        .unwrap();
    let order_id = match orders::complete_payment(&db, "free_questions", "", 0, now)
        .await
        .unwrap()
    {
        PaymentOutcome::Paid(id) => id,
        _ => panic!("free Order was not created"),
    };
    let missing = inventory::reserve(
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
    let missing_details = orders::hold_details(&db, &missing.id).await.unwrap();
    orders::save_details(
        &db,
        &cipher,
        &missing_details,
        "other@example.com",
        &[String::new()],
        false,
        now,
    )
    .await
    .unwrap();
    orders::attach_session(&db, &missing.id, "free_missing_answer", now)
        .await
        .unwrap();
    assert!(matches!(
        orders::complete_payment(&db, "free_missing_answer", "", 0, now)
            .await
            .unwrap(),
        PaymentOutcome::RefundNeeded
    ));
    questions::delete(&db, &event.id, &choice_id).await.unwrap();
    let answers = questions::order_answers(&db, &cipher, &order_id)
        .await
        .unwrap();
    assert_eq!(answers.len(), 2);
    assert!(answers.iter().any(|answer| answer.label == "Meal choice"
        && answer.answer == "Vegetarian"
        && answer.attendee_number.is_none()));
    assert!(answers
        .iter()
        .any(|answer| answer.label == "Accessibility need"
            && answer.answer == "Wheelchair space"
            && answer.attendee_number == Some(1)));
    admin_orders::anonymize(&db, &cipher, &order_id)
        .await
        .unwrap();
    assert!(questions::order_answers(&db, &cipher, &order_id)
        .await
        .unwrap()
        .is_empty());
}
