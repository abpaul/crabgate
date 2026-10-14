use crabgate::{
    crypto::MasterKey,
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, Selection},
    orders::{self, PaymentOutcome},
    settings,
};
use loco_rs::db;
use migration::OrganizerMigrator;
use sea_orm::ConnectionTrait;

#[tokio::test]
async fn stripe_and_webhook_secrets_are_encrypted_for_one_organizer() {
    let dir = tempfile::tempdir().unwrap();
    let db = sea_orm::Database::connect(format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("settings.db").display()
    ))
    .await
    .unwrap();
    db::migrate::<OrganizerMigrator>(&db).await.unwrap();
    let master = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();
    let first = master.generate_organizer_key("first").unwrap().1;
    let second = master.generate_organizer_key("second").unwrap().1;
    settings::set(&db, &first, "stripe_secret_key", "sk_test_example")
        .await
        .unwrap();
    settings::set(&db, &first, "webhook_secret", "whsec_example")
        .await
        .unwrap();
    assert_eq!(
        settings::get(&db, &first, "stripe_secret_key")
            .await
            .unwrap()
            .as_deref(),
        Some("sk_test_example")
    );
    assert_eq!(
        settings::get(&db, &first, "webhook_secret")
            .await
            .unwrap()
            .as_deref(),
        Some("whsec_example")
    );
    assert!(settings::get(&db, &second, "stripe_secret_key")
        .await
        .is_err());
    let raw = db
        .query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "SELECT value FROM organizer_settings WHERE key = 'stripe_secret_key'".to_owned(),
        ))
        .await
        .unwrap()
        .unwrap();
    let value: String = raw.try_get("", "value").unwrap();
    assert!(!value.contains("sk_test_example"));
}

#[tokio::test]
async fn currency_is_fixed_after_the_first_checkout_hold() {
    let dir = tempfile::tempdir().unwrap();
    let db = sea_orm::Database::connect(format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("currency.db").display()
    ))
    .await
    .unwrap();
    db::migrate::<OrganizerMigrator>(&db).await.unwrap();
    assert_eq!(settings::currency(&db).await.unwrap(), "EUR");
    settings::set_currency(&db, "GBP").await.unwrap();
    assert_eq!(settings::currency(&db).await.unwrap(), "GBP");
    let event = events::create(
        &db,
        &EventInput {
            title: "Currency show".to_owned(),
            date: "2027-06-15".to_owned(),
            start_time: "19:00".to_owned(),
            end_time: "21:00".to_owned(),
            timezone: "Europe/Dublin".to_owned(),
            venue: "Hall".to_owned(),
            address: "1 Street".to_owned(),
            description: String::new(),
            event_capacity: None,
            max_per_order: 1,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let kind = events::add_ticket_type(
        &db,
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
    let now = chrono::Utc::now().timestamp();
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
    assert!(matches!(
        settings::set_currency(&db, "USD").await,
        Err(settings::SettingsError::CurrencyLocked)
    ));
    assert_eq!(events::format_price(1050, "GBP"), "£10.50");
    let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .unwrap()
        .generate_organizer_key("currency")
        .unwrap()
        .1;
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
    orders::attach_session(&db, &hold.id, "cs_currency", now)
        .await
        .unwrap();
    assert!(matches!(
        orders::complete_payment_in_currency(&db, "cs_currency", "pi_currency", 1000, "USD", now)
            .await
            .unwrap(),
        PaymentOutcome::RefundNeeded
    ));
}

#[tokio::test]
async fn contact_links_and_reply_to_reject_unsafe_values() {
    let dir = tempfile::tempdir().unwrap();
    let db = sea_orm::Database::connect(format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("contact.db").display()
    ))
    .await
    .unwrap();
    db::migrate::<OrganizerMigrator>(&db).await.unwrap();
    assert!(
        settings::set_contact(&db, "javascript:alert(1)", "", "hello@example.com")
            .await
            .is_err()
    );
    assert!(settings::set_contact(
        &db,
        "https://example.com/imprint",
        "",
        "hello@example.com\r\nBcc: victim@example.com"
    )
    .await
    .is_err());
    settings::set_contact(
        &db,
        "https://example.com/imprint",
        "https://example.com/privacy",
        "hello@example.com",
    )
    .await
    .unwrap();
    let contact = settings::contact(&db).await.unwrap();
    assert_eq!(contact.imprint_url, "https://example.com/imprint");
    assert_eq!(contact.privacy_url, "https://example.com/privacy");
    assert_eq!(contact.reply_to, "hello@example.com");
}
