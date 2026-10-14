use crabgate::app::App;
use loco_rs::cli;
use migration::Migrator;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

async fn import_database(path: &str) -> loco_rs::Result<()> {
    if std::env::var("CRABGATE_EDITION").as_deref() == Ok("hosted") {
        return Err(loco_rs::Error::string("Import is Self-hosted only"));
    }
    let url = std::env::var("DATABASE_URL")
        .map_err(|_| loco_rs::Error::string("DATABASE_URL is required for import"))?;
    let encoded = std::env::var("CRABGATE_MASTER_KEY")
        .map_err(|_| loco_rs::Error::string("CRABGATE_MASTER_KEY is required for import"))?;
    let master = crabgate::crypto::MasterKey::from_base64(&encoded)
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let control = sea_orm::Database::connect(url)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let rows = control
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT id, db_path, data_key FROM organizers".to_owned(),
        ))
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    if rows.len() != 1 {
        return Err(loco_rs::Error::string(
            "Import requires one fresh Self-hosted Organizer",
        ));
    }
    let row = &rows[0];
    let id: String = row
        .try_get("", "id")
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let db_path: String = row
        .try_get("", "db_path")
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let wrapped: String = row
        .try_get("", "data_key")
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let cipher = master
        .open_organizer_key(&id, &wrapped)
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    control
        .close()
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    crabgate::data_transfer::import_fresh(
        std::path::Path::new(path),
        std::path::Path::new(&db_path),
        &cipher,
    )
    .await
    .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    println!("Import complete. Start CrabGate and verify the Orders list.");
    Ok(())
}

fn main() -> loco_rs::Result<()> {
    let _error_reporting =
        crabgate::error_reporting::init().map_err(|error| loco_rs::Error::string(&error))?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> loco_rs::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("import") {
        let path = std::env::args().nth(2).ok_or_else(|| {
            loco_rs::Error::string("Usage: crabgate-cli import <exported.sqlite>")
        })?;
        if std::env::args().nth(3).is_some() {
            return Err(loco_rs::Error::string(
                "Usage: crabgate-cli import <exported.sqlite>",
            ));
        }
        return import_database(&path).await;
    }
    cli::main::<App, Migrator>().await
}
