use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, Statement, TransactionTrait,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const DAY: i64 = 86_400;

fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

pub async fn seed(control: &DatabaseConnection) -> Result<(), DbErr> {
    if let Ok(emails) = std::env::var("CRABGATE_OPERATOR_EMAILS") {
        for email in emails
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            control
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "INSERT OR IGNORE INTO operators (id, email) VALUES (?, ?)",
                    [
                        Uuid::new_v4().to_string().into(),
                        email.to_ascii_lowercase().into(),
                    ],
                ))
                .await?;
        }
    }
    Ok(())
}

pub async fn request_link(
    control: &DatabaseConnection,
    email: &str,
    now: i64,
) -> Result<Option<String>, DbErr> {
    let row = control
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id FROM operators WHERE email = ?",
            [email.trim().to_ascii_lowercase().into()],
        ))
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let id: String = row.try_get("", "id")?;
    let token = Uuid::new_v4().simple().to_string();
    let inserted = control.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO operator_magic_links (token_hash, operator_id, expires_at, created_at)
         SELECT ?, ?, ?, ? WHERE (SELECT COUNT(*) FROM operator_magic_links WHERE operator_id = ? AND created_at > ?) < 3",
        [hash(&token).into(), id.clone().into(), (now + 900).into(), now.into(), id.into(), (now - 3600).into()]
    )).await?;
    Ok((inserted.rows_affected() == 1).then_some(token))
}

pub async fn claim_link(
    control: &DatabaseConnection,
    token: &str,
    now: i64,
) -> Result<Option<String>, DbErr> {
    let transaction = control.begin().await?;
    let row = transaction.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT operator_id FROM operator_magic_links WHERE token_hash = ? AND used_at IS NULL AND expires_at > ?", [hash(token).into(), now.into()]
    )).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let id: String = row.try_get("", "operator_id")?;
    let result = transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE operator_magic_links SET used_at = ? WHERE token_hash = ? AND used_at IS NULL",
            [now.into(), hash(token).into()],
        ))
        .await?;
    if result.rows_affected() != 1 {
        return Ok(None);
    }
    let session = Uuid::new_v4().simple().to_string();
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO operator_sessions (token_hash, operator_id, expires_at) VALUES (?, ?, ?)",
            [hash(&session).into(), id.into(), (now + DAY).into()],
        ))
        .await?;
    transaction.commit().await?;
    Ok(Some(session))
}

pub async fn session(
    control: &DatabaseConnection,
    token: &str,
    now: i64,
) -> Result<Option<String>, DbErr> {
    let row = control
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT operator_id FROM operator_sessions WHERE token_hash = ? AND expires_at > ?",
            [hash(token).into(), now.into()],
        ))
        .await?;
    row.map(|row| row.try_get("", "operator_id")).transpose()
}

pub async fn revoke_session(control: &DatabaseConnection, token: &str) -> Result<(), DbErr> {
    control
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "DELETE FROM operator_sessions WHERE token_hash = ?",
            [hash(token).into()],
        ))
        .await?;
    Ok(())
}

pub async fn grant(
    control: &DatabaseConnection,
    organizer_id: &str,
    member_id: &str,
    hours: i64,
    now: i64,
) -> Result<String, DbErr> {
    let id = Uuid::new_v4().to_string();
    control.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO access_grants (id, organizer_id, member_id, expires_at) VALUES (?, ?, ?, ?)",
        [id.clone().into(), organizer_id.into(), member_id.into(), (now + hours * 3600).into()]
    )).await?;
    Ok(id)
}

pub async fn revoke(
    control: &DatabaseConnection,
    organizer_id: &str,
    grant_id: &str,
    now: i64,
) -> Result<(), DbErr> {
    control
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE access_grants SET revoked_at = ? WHERE id = ? AND organizer_id = ?",
            [now.into(), grant_id.into(), organizer_id.into()],
        ))
        .await?;
    Ok(())
}

pub async fn has_grant(
    control: &DatabaseConnection,
    organizer_id: &str,
    now: i64,
) -> Result<bool, DbErr> {
    Ok(control.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT 1 AS found FROM access_grants WHERE organizer_id = ? AND revoked_at IS NULL AND expires_at > ? LIMIT 1", [organizer_id.into(), now.into()]
    )).await?.is_some())
}

pub async fn audit(
    control: &DatabaseConnection,
    organizer_id: &str,
    operator_id: &str,
    method: &str,
    path: &str,
    now: i64,
) -> Result<(), DbErr> {
    control.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO audit_log (organizer_id, operator_id, method, path, created_at) VALUES (?, ?, ?, ?, ?)",
        [organizer_id.into(), operator_id.into(), method.into(), path.into(), now.into()]
    )).await?;
    Ok(())
}
