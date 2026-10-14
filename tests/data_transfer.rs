use chrono::Utc;
use crabgate::{
    admin_orders,
    crypto::MasterKey,
    data_transfer,
    events::{self, EventInput, TicketTypeInput},
};
use loco_rs::db;
use migration::OrganizerMigrator;
use sea_orm::DatabaseConnection;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

async fn database(path: &std::path::Path) -> DatabaseConnection {
    let db = sea_orm::Database::connect(format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap();
    db::migrate::<OrganizerMigrator>(&db).await.unwrap();
    db
}

#[tokio::test]
async fn export_import_reencrypts_personal_data_and_preserves_order() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.db");
    let new_path = dir.path().join("new.db");
    let export_path = dir.path().join("export.db");
    let old_db = database(&old_path).await;
    let master = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();
    let old_cipher = master.generate_organizer_key("old-org").unwrap().1;
    let (wrapped, new_cipher) = master.generate_organizer_key("new-org").unwrap();
    let event = events::create(
        &old_db,
        &EventInput {
            title: "Autumn Night".into(),
            date: "2027-06-15".into(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Hall".into(),
            address: "Main Street".into(),
            description: String::new(),
            event_capacity: Some(4),
            max_per_order: 10,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let kind = events::add_ticket_type(
        &old_db,
        &event,
        &TicketTypeInput {
            name: "Standard".into(),
            price: "0".into(),
            capacity: 4,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(&old_db, &event.id, Utc::now().timestamp())
        .await
        .unwrap();
    let id = admin_orders::complimentary(
        &old_db,
        &old_cipher,
        &event.id,
        &kind.id,
        "buyer@example.com",
        "Niamh",
        Utc::now().timestamp(),
    )
    .await
    .unwrap();
    let old_order = admin_orders::order(&old_db, &old_cipher, &id)
        .await
        .unwrap();
    let bytes = data_transfer::export(&old_db, &old_cipher).await.unwrap();
    assert!(bytes.starts_with(b"SQLite format 3"));
    assert!(bytes
        .windows(b"buyer@example.com".len())
        .any(|part| part == b"buyer@example.com"));
    std::fs::write(&export_path, &bytes).unwrap();
    let exported =
        sea_orm::Database::connect(format!("sqlite://{}?mode=rw", export_path.display()))
            .await
            .unwrap();
    let login_table = exported
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT name FROM sqlite_master WHERE type='table' AND name='members'".to_owned(),
        ))
        .await
        .unwrap();
    assert!(login_table.is_none());
    exported.close().await.unwrap();
    let fresh = database(&new_path).await;
    fresh.close().await.unwrap();
    let control_path = dir.path().join("control.db");
    let control =
        sea_orm::Database::connect(format!("sqlite://{}?mode=rwc", control_path.display()))
            .await
            .unwrap();
    db::migrate::<migration::Migrator>(&control).await.unwrap();
    control.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO organizers (id, name, subdomain, db_path, data_key) VALUES (?, 'New Organizer', 'local', ?, ?)",
        ["new-org".into(), new_path.to_string_lossy().into_owned().into(), wrapped.into()]
    )).await.unwrap();
    control.close().await.unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_crabgate-cli"))
        .arg("import")
        .arg(&export_path)
        .env(
            "DATABASE_URL",
            format!("sqlite://{}?mode=rw", control_path.display()),
        )
        .env(
            "CRABGATE_MASTER_KEY",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Import complete"));
    let imported = sea_orm::Database::connect(format!("sqlite://{}?mode=rw", new_path.display()))
        .await
        .unwrap();
    let order = admin_orders::order(&imported, &new_cipher, &id)
        .await
        .unwrap();
    assert_eq!(order.number, old_order.number);
    assert_eq!(order.buyer_email, "buyer@example.com");
    assert_eq!(order.tickets.len(), 1);
    assert_eq!(order.tickets[0].code, old_order.tickets[0].code);
    assert_eq!(order.tickets[0].attendee_name.as_deref(), Some("Niamh"));
    imported.close().await.unwrap();
    assert!(
        data_transfer::import_fresh(&export_path, &new_path, &new_cipher)
            .await
            .is_err()
    );
}
