use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, Statement, TransactionTrait,
};
use uuid::Uuid;

use crate::crypto::{CryptoError, OrganizerCipher};

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("Import requires a fresh organizer database")]
    NotFresh,
    #[error("The SQLite file is not a compatible CrabGate export")]
    Incompatible,
    #[error(transparent)]
    Database(#[from] DbErr),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

struct TempDb {
    directory: PathBuf,
    path: PathBuf,
}
impl TempDb {
    fn new() -> Result<Self, TransferError> {
        let directory = std::env::temp_dir().join(format!("crabgate-transfer-{}", Uuid::new_v4()));
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let path = directory.join("data.sqlite");
        Ok(Self { directory, path })
    }
    fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(format!("{}-wal", self.path.display()));
        let _ = fs::remove_file(format!("{}-shm", self.path.display()));
        let _ = fs::remove_dir(&self.directory);
    }
}

async fn connect(path: &Path) -> Result<DatabaseConnection, TransferError> {
    Ok(sea_orm::Database::connect(format!("sqlite://{}?mode=rw", path.display())).await?)
}

async fn check_integrity(db: &DatabaseConnection) -> Result<(), TransferError> {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA integrity_check".to_owned(),
        ))
        .await?
        .ok_or(TransferError::Incompatible)?;
    let result: String = row.try_get_by_index(0)?;
    if result != "ok" {
        return Err(TransferError::Incompatible);
    }
    Ok(())
}

async fn versions(db: &DatabaseConnection) -> Result<Vec<String>, TransferError> {
    let rows = db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT version FROM seaql_migrations ORDER BY version".to_owned(),
        ))
        .await
        .map_err(|_| TransferError::Incompatible)?;
    if rows.is_empty() {
        return Err(TransferError::Incompatible);
    }
    rows.into_iter()
        .map(|row| row.try_get("", "version").map_err(TransferError::from))
        .collect()
}

async fn transform(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    decrypt: bool,
) -> Result<(), TransferError> {
    let columns = [
        ("holds", "buyer_email_encrypted", "buyer_email"),
        ("hold_attendees", "attendee_name_encrypted", "attendee_name"),
        ("orders", "buyer_email_encrypted", "buyer_email"),
        ("tickets", "attendee_name_encrypted", "attendee_name"),
        ("hold_answers", "answer_encrypted", "question_answer"),
        ("door_links", "token_encrypted", "door_link_token"),
        ("buyer_messages", "content_encrypted", "buyer_message"),
        (
            "buyer_message_recipients",
            "email_encrypted",
            "buyer_message_email",
        ),
    ];
    let transaction = db.begin().await?;
    for (table, column, associated_data) in columns {
        let rows = transaction
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT rowid, {column} AS value FROM {table} WHERE {column} IS NOT NULL"),
            ))
            .await?;
        for row in rows {
            let rowid: i64 = row.try_get("", "rowid")?;
            let value: String = row.try_get("", "value")?;
            let updated = if decrypt {
                cipher.decrypt(associated_data, &value)?
            } else {
                cipher.encrypt(associated_data, &value)?
            };
            transaction
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    format!("UPDATE {table} SET {column} = ? WHERE rowid = ?"),
                    [updated.into(), rowid.into()],
                ))
                .await?;
        }
    }
    for key in ["stripe_secret_key", "webhook_secret", "smtp_password"] {
        let row = transaction
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT value FROM organizer_settings WHERE key = ?",
                [key.into()],
            ))
            .await?;
        if let Some(row) = row {
            let value: String = row.try_get("", "value")?;
            let updated = if decrypt {
                cipher.decrypt(key, &value)?
            } else {
                cipher.encrypt(key, &value)?
            };
            transaction
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "UPDATE organizer_settings SET value = ? WHERE key = ?",
                    [updated.into(), key.into()],
                ))
                .await?;
        }
    }
    transaction.commit().await?;
    Ok(())
}

pub async fn export(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
) -> Result<Vec<u8>, TransferError> {
    let temp = TempDb::new()?;
    db.execute_unprepared(&format!("VACUUM INTO '{}'", temp.path().display()))
        .await?;
    let snapshot = connect(temp.path()).await?;
    check_integrity(&snapshot).await?;
    transform(&snapshot, cipher, true).await?;
    snapshot.close().await?;
    Ok(fs::read(temp.path())?)
}

pub async fn import_fresh(
    source_file: &Path,
    target_file: &Path,
    target_cipher: &OrganizerCipher,
) -> Result<(), TransferError> {
    let target = connect(target_file).await?;
    let source = connect(source_file).await?;
    check_integrity(&source).await?;
    if versions(&source).await? != versions(&target).await? {
        return Err(TransferError::Incompatible);
    }
    for table in [
        "settings",
        "events",
        "ticket_types",
        "holds",
        "hold_items",
        "hold_attendees",
        "orders",
        "tickets",
        "payment_events",
        "door_links",
        "checkins",
        "organizer_settings",
        "external_refunds",
    ] {
        let row = target
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT COUNT(*) AS count FROM {table}"),
            ))
            .await?;
        if row
            .ok_or(TransferError::Incompatible)?
            .try_get::<i64>("", "count")?
            != 0
        {
            return Err(TransferError::NotFresh);
        }
    }
    source.close().await?;
    target
        .execute_unprepared("PRAGMA wal_checkpoint(TRUNCATE)")
        .await?;
    target.close().await?;
    let temp = TempDb::new()?;
    fs::copy(source_file, temp.path())?;
    let imported = connect(temp.path()).await?;
    transform(&imported, target_cipher, false).await?;
    check_integrity(&imported).await?;
    imported.close().await?;
    if ["-wal", "-shm"]
        .iter()
        .any(|suffix| PathBuf::from(format!("{}{suffix}", target_file.display())).exists())
    {
        return Err(TransferError::Incompatible);
    }
    fs::rename(temp.path(), target_file)?;
    Ok(())
}
