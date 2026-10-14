use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, Statement, TransactionTrait,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::crypto::{CryptoError, OrganizerCipher};

#[derive(Debug, thiserror::Error)]
pub enum DoorError {
    #[error("Door Link not found or expired")]
    LinkUnavailable,
    #[error("This event has ended")]
    EventEnded,
    #[error("Enter a label")]
    InvalidLabel,
    #[error("Check-in not found")]
    CheckinNotFound,
    #[error(transparent)]
    Database(#[from] DbErr),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

pub struct DoorLink {
    pub id: String,
    pub event_id: String,
    pub label: String,
    pub token: String,
    pub revoked_at: Option<i64>,
}

#[derive(Clone)]
pub struct DoorTicket {
    pub id: String,
    pub code: String,
    pub attendee_name: Option<String>,
    pub ticket_type: String,
    pub seat: Option<String>,
    pub order_number: String,
    pub order_status: String,
    pub buyer_email: String,
    pub checked_at: Option<i64>,
    pub status: String,
}

impl DoorTicket {
    pub fn is_void(&self) -> bool {
        self.status == "Void" || self.order_status == "Refunded"
    }
}

pub enum CheckinResult {
    CheckedIn(DoorTicket),
    AlreadyCheckedIn(DoorTicket),
    VoidTicket(DoorTicket),
    WrongEvent,
    Unknown,
}

pub async fn create_link(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: &str,
    label: &str,
    now: i64,
) -> Result<DoorLink, DoorError> {
    let label = label.trim();
    if label.is_empty() || label.len() > 100 {
        return Err(DoorError::InvalidLabel);
    }
    let end = event_end(db, event_id).await?;
    if end <= now {
        return Err(DoorError::EventEnded);
    }
    let id = Uuid::new_v4().to_string();
    let token = Uuid::new_v4().simple().to_string();
    let hash = token_hash(&token);
    let encrypted = cipher.encrypt("door_link_token", &token)?;
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO door_links (id, event_id, label, token_hash, token_encrypted, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        [id.clone().into(), event_id.into(), label.into(), hash.into(), encrypted.into(), now.into()],
    )).await?;
    Ok(DoorLink {
        id,
        event_id: event_id.to_owned(),
        label: label.to_owned(),
        token,
        revoked_at: None,
    })
}

pub async fn list_links(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
) -> Result<Vec<DoorLink>, DoorError> {
    let rows = db.query_all_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT id, event_id, label, token_encrypted, revoked_at FROM door_links ORDER BY created_at DESC".to_owned(),
    )).await?;
    rows.into_iter()
        .map(|row| {
            let encrypted: String = row.try_get("", "token_encrypted")?;
            Ok(DoorLink {
                id: row.try_get("", "id")?,
                event_id: row.try_get("", "event_id")?,
                label: row.try_get("", "label")?,
                token: cipher.decrypt("door_link_token", &encrypted)?,
                revoked_at: row.try_get("", "revoked_at")?,
            })
        })
        .collect()
}

pub async fn verify_link(
    db: &DatabaseConnection,
    event_id: &str,
    token: &str,
    now: i64,
) -> Result<Option<String>, DoorError> {
    if token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT dl.id FROM door_links dl JOIN events e ON e.id = dl.event_id WHERE dl.event_id = ? AND dl.token_hash = ? AND dl.revoked_at IS NULL AND e.ends_at > ? AND e.status = 'Published'",
        [event_id.into(), token_hash(token).into(), now.into()],
    )).await?;
    row.map(|row| row.try_get("", "id"))
        .transpose()
        .map_err(DoorError::from)
}

pub async fn revoke_link(db: &DatabaseConnection, id: &str, now: i64) -> Result<(), DoorError> {
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE door_links SET revoked_at = COALESCE(revoked_at, ?) WHERE id = ?",
        [now.into(), id.into()],
    ))
    .await?;
    Ok(())
}

pub async fn check_in(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: &str,
    code: &str,
    actor_type: &str,
    actor_id: &str,
    now: i64,
) -> Result<CheckinResult, DoorError> {
    let transaction = db.begin().await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET updated_at = updated_at WHERE id = ?",
            [event_id.into()],
        ))
        .await?;
    let end = event_end(&transaction, event_id).await?;
    if end <= now {
        return Err(DoorError::EventEnded);
    }
    let row = transaction.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT t.id, t.event_id, t.code, t.attendee_name_encrypted, t.status, tt.name AS ticket_type, o.number AS order_number, o.status AS order_status, o.buyer_email_encrypted, ci.checked_at, s.section, s.row_label, s.seat_label FROM tickets t JOIN ticket_types tt ON tt.id = t.ticket_type_id JOIN orders o ON o.id = t.order_id LEFT JOIN checkins ci ON ci.ticket_id = t.id AND ci.undone_at IS NULL LEFT JOIN seats s ON s.id = t.seat_id WHERE t.code = ? COLLATE NOCASE",
        [code.trim().to_uppercase().into()],
    )).await?;
    let Some(row) = row else {
        return Ok(CheckinResult::Unknown);
    };
    let ticket_event: String = row.try_get("", "event_id")?;
    if ticket_event != event_id {
        return Ok(CheckinResult::WrongEvent);
    }
    let ticket = ticket_from_row(row, cipher)?;
    let event = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT status FROM events WHERE id = ?",
            [event_id.into()],
        ))
        .await?
        .ok_or(DoorError::LinkUnavailable)?;
    let event_status: String = event.try_get("", "status")?;
    if event_status == "Cancelled" {
        return Ok(CheckinResult::VoidTicket(ticket));
    }
    if ticket.is_void() {
        return Ok(CheckinResult::VoidTicket(ticket));
    }
    if ticket.checked_at.is_some() {
        return Ok(CheckinResult::AlreadyCheckedIn(ticket));
    }
    transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO checkins (id, ticket_id, event_id, checked_at, actor_type, actor_id) VALUES (?, ?, ?, ?, ?, ?)",
        [Uuid::new_v4().to_string().into(), ticket.id.clone().into(), event_id.into(), now.into(), actor_type.into(), actor_id.into()],
    )).await?;
    transaction.commit().await?;
    Ok(CheckinResult::CheckedIn(DoorTicket {
        checked_at: Some(now),
        ..ticket
    }))
}

pub async fn search(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: &str,
    query: &str,
) -> Result<Vec<DoorTicket>, DoorError> {
    let needle = query.trim().to_lowercase();
    if needle.len() < 2 {
        return Ok(Vec::new());
    }
    let rows = db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT t.id, t.code, t.attendee_name_encrypted, t.status, tt.name AS ticket_type, o.number AS order_number, o.status AS order_status, o.buyer_email_encrypted, ci.checked_at, s.section, s.row_label, s.seat_label FROM tickets t JOIN ticket_types tt ON tt.id = t.ticket_type_id JOIN orders o ON o.id = t.order_id LEFT JOIN checkins ci ON ci.ticket_id = t.id AND ci.undone_at IS NULL LEFT JOIN seats s ON s.id = t.seat_id WHERE t.event_id = ? ORDER BY o.created_at, t.id",
        [event_id.into()],
    )).await?;
    let mut matches = Vec::new();
    for row in rows {
        let ticket = ticket_from_row(row, cipher)?;
        if ticket.code.to_lowercase().contains(&needle)
            || ticket.buyer_email.to_lowercase().contains(&needle)
            || ticket
                .attendee_name
                .as_deref()
                .is_some_and(|name| name.to_lowercase().contains(&needle))
        {
            matches.push(ticket);
            if matches.len() == 25 {
                break;
            }
        }
    }
    Ok(matches)
}

pub async fn count(db: &DatabaseConnection, event_id: &str) -> Result<(i64, i64), DoorError> {
    let checked = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM checkins WHERE event_id = ? AND undone_at IS NULL",
            [event_id.into()],
        ))
        .await?
        .unwrap()
        .try_get("", "count")?;
    let total = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM tickets WHERE event_id = ? AND status = 'Valid'",
            [event_id.into()],
        ))
        .await?
        .unwrap()
        .try_get("", "count")?;
    Ok((checked, total))
}

pub async fn link_count(db: &DatabaseConnection, link_id: &str) -> Result<i64, DoorError> {
    let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT COUNT(*) AS count FROM checkins WHERE actor_type = 'Door Link' AND actor_id = ? AND undone_at IS NULL",
        [link_id.into()],
    )).await?.unwrap();
    Ok(row.try_get("", "count")?)
}

pub async fn undo(
    db: &DatabaseConnection,
    event_id: &str,
    ticket_id: &str,
    member_id: &str,
    now: i64,
) -> Result<(), DoorError> {
    let affected = db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE checkins SET undone_at = ?, undo_member_id = ? WHERE ticket_id = ? AND event_id = ? AND undone_at IS NULL",
        [now.into(), member_id.into(), ticket_id.into(), event_id.into()],
    )).await?.rows_affected();
    if affected == 0 {
        return Err(DoorError::CheckinNotFound);
    }
    Ok(())
}

async fn event_end<C: ConnectionTrait>(db: &C, event_id: &str) -> Result<i64, DoorError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT ends_at FROM events WHERE id = ?",
            [event_id.into()],
        ))
        .await?
        .ok_or(DoorError::LinkUnavailable)?;
    Ok(row.try_get("", "ends_at")?)
}

fn ticket_from_row(
    row: sea_orm::QueryResult,
    cipher: &OrganizerCipher,
) -> Result<DoorTicket, DoorError> {
    let encrypted: Option<String> = row.try_get("", "attendee_name_encrypted")?;
    Ok(DoorTicket {
        id: row.try_get("", "id")?,
        code: row.try_get("", "code")?,
        attendee_name: encrypted
            .map(|value| cipher.decrypt("attendee_name", &value))
            .transpose()?,
        ticket_type: row.try_get("", "ticket_type")?,
        seat: match (
            row.try_get::<Option<String>>("", "section")?,
            row.try_get::<Option<String>>("", "row_label")?,
            row.try_get::<Option<String>>("", "seat_label")?,
        ) {
            (Some(section), Some(row), Some(label)) => {
                Some(crate::seats::display(&section, &row, &label))
            }
            _ => None,
        },
        order_number: row.try_get("", "order_number")?,
        order_status: row.try_get("", "order_status")?,
        buyer_email: cipher.decrypt(
            "buyer_email",
            &row.try_get::<String>("", "buyer_email_encrypted")?,
        )?,
        checked_at: row.try_get("", "checked_at")?,
        status: row.try_get("", "status")?,
    })
}

fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}
