use std::collections::{HashMap, HashSet};

use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DatabaseTransaction, DbErr, Statement,
    TransactionTrait,
};
use serde::Serialize;
use uuid::Uuid;

use crate::inventory::Selection;

#[derive(Debug, thiserror::Error)]
pub enum SeatError {
    #[error("Enter a Section, Row, and 1 to 100 distinct seat labels")]
    InvalidLabels,
    #[error("A seat is unavailable or does not belong to this Event")]
    Unavailable,
    #[error("Add seats before the first sale.")]
    AlreadySelling,
    #[error("Ticket type capacity is below the number of seats")]
    Capacity,
    #[error(transparent)]
    Database(#[from] DbErr),
}

#[derive(Serialize)]
pub struct Seat {
    pub id: String,
    pub ticket_type_id: String,
    pub section: String,
    pub row: String,
    pub label: String,
    pub available: bool,
    pub active: bool,
}

pub async fn list_for_hold(db: &DatabaseConnection, hold_id: &str) -> Result<Vec<Seat>, SeatError> {
    let rows = db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT s.id, s.ticket_type_id, s.section, s.row_label, s.seat_label FROM hold_seats hs JOIN seats s ON s.id = hs.seat_id WHERE hs.hold_id = ? ORDER BY s.ticket_type_id, s.section, s.row_label, s.seat_label",
        [hold_id.into()]
    )).await?;
    rows.into_iter()
        .map(|row| {
            Ok(Seat {
                id: row.try_get("", "id")?,
                ticket_type_id: row.try_get("", "ticket_type_id")?,
                section: row.try_get("", "section")?,
                row: row.try_get("", "row_label")?,
                label: row.try_get("", "seat_label")?,
                active: true,
                available: false,
            })
        })
        .collect::<Result<Vec<_>, DbErr>>()
        .map_err(SeatError::from)
}

pub fn display(section: &str, row: &str, label: &str) -> String {
    format!("{section}, Row {row}, Seat {label}")
}

pub async fn assigned<C: ConnectionTrait>(db: &C, event_id: &str) -> Result<bool, DbErr> {
    Ok(db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT 1 FROM seats WHERE event_id = ? LIMIT 1",
            [event_id.into()],
        ))
        .await?
        .is_some())
}

pub async fn list<C: ConnectionTrait>(
    db: &C,
    event_id: &str,
    now: i64,
) -> Result<Vec<Seat>, SeatError> {
    let rows = db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT s.id, s.ticket_type_id, s.section, s.row_label, s.seat_label, s.active,
            EXISTS(SELECT 1 FROM tickets t WHERE t.seat_id = s.id AND t.status = 'Valid') AS sold,
            EXISTS(SELECT 1 FROM hold_seats hs JOIN holds h ON h.id = hs.hold_id WHERE hs.seat_id = s.id AND h.status = 'Open' AND h.expires_at > ?) AS held
         FROM seats s WHERE s.event_id = ? ORDER BY s.section, s.row_label, s.seat_label",
        [now.into(), event_id.into()]
    )).await?;
    rows.into_iter()
        .map(|row| {
            let active = row.try_get::<i64>("", "active")? == 1;
            let sold = row.try_get::<i64>("", "sold")? == 1;
            let held = row.try_get::<i64>("", "held")? == 1;
            Ok(Seat {
                id: row.try_get("", "id")?,
                ticket_type_id: row.try_get("", "ticket_type_id")?,
                section: row.try_get("", "section")?,
                row: row.try_get("", "row_label")?,
                label: row.try_get("", "seat_label")?,
                active,
                available: active && !sold && !held,
            })
        })
        .collect::<Result<Vec<_>, DbErr>>()
        .map_err(SeatError::from)
}

pub async fn available_for_events<C: ConnectionTrait>(
    db: &C,
    now: i64,
) -> Result<HashMap<String, Vec<Seat>>, SeatError> {
    let rows = db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT s.event_id, s.id, s.ticket_type_id, s.section, s.row_label, s.seat_label FROM seats s WHERE s.active = 1 AND NOT EXISTS(SELECT 1 FROM tickets t WHERE t.seat_id = s.id AND t.status = 'Valid') AND NOT EXISTS(SELECT 1 FROM hold_seats hs JOIN holds h ON h.id = hs.hold_id WHERE hs.seat_id = s.id AND h.status = 'Open' AND h.expires_at > ?) ORDER BY s.event_id, s.section, s.row_label, s.seat_label",
        [now.into()]
    )).await?;
    let mut result: HashMap<String, Vec<Seat>> = HashMap::new();
    for row in rows {
        result
            .entry(row.try_get("", "event_id")?)
            .or_default()
            .push(Seat {
                id: row.try_get("", "id")?,
                ticket_type_id: row.try_get("", "ticket_type_id")?,
                section: row.try_get("", "section")?,
                row: row.try_get("", "row_label")?,
                label: row.try_get("", "seat_label")?,
                active: true,
                available: true,
            });
    }
    Ok(result)
}

pub async fn add_many<C: ConnectionTrait + TransactionTrait<Transaction = DatabaseTransaction>>(
    db: &C,
    event_id: &str,
    ticket_type_id: &str,
    section: &str,
    row: &str,
    labels: &[String],
    now: i64,
) -> Result<(), SeatError> {
    let section = section.trim();
    let row = row.trim();
    let labels = labels
        .iter()
        .map(|label| label.trim().to_owned())
        .collect::<Vec<_>>();
    let distinct = labels.iter().collect::<HashSet<_>>();
    if section.is_empty()
        || section.chars().count() > 60
        || row.is_empty()
        || row.chars().count() > 40
        || !(1..=100).contains(&labels.len())
        || distinct.len() != labels.len()
        || labels
            .iter()
            .any(|label| label.is_empty() || label.chars().count() > 20)
    {
        return Err(SeatError::InvalidLabels);
    }
    let transaction = db.begin().await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET updated_at = updated_at WHERE id = ?",
            [event_id.into()],
        ))
        .await?;
    let type_row = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT capacity FROM ticket_types WHERE id = ? AND event_id = ?",
            [ticket_type_id.into(), event_id.into()],
        ))
        .await?
        .ok_or(SeatError::Unavailable)?;
    let capacity: i64 = type_row.try_get("", "capacity")?;
    let existing = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM seats WHERE ticket_type_id = ?",
            [ticket_type_id.into()],
        ))
        .await?
        .ok_or(SeatError::Unavailable)?;
    let existing_count: i64 = existing.try_get("", "count")?;
    if existing_count + labels.len() as i64 > capacity {
        return Err(SeatError::Capacity);
    }
    for label in &labels {
        if transaction.query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT 1 FROM seats WHERE event_id = ? AND section = ? AND row_label = ? AND seat_label = ?",
            [event_id.into(), section.into(), row.into(), label.clone().into()],
        )).await?.is_some() {
            return Err(SeatError::InvalidLabels);
        }
    }
    if !assigned(&transaction, event_id).await? {
        let sold = transaction.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT (SELECT COUNT(*) FROM tickets WHERE event_id = ?) + (SELECT COUNT(*) FROM holds WHERE event_id = ? AND status = 'Open' AND expires_at > ?) AS count",
            [event_id.into(), event_id.into(), now.into()]
        )).await?.ok_or(SeatError::Unavailable)?;
        if sold.try_get::<i64>("", "count")? > 0 {
            return Err(SeatError::AlreadySelling);
        }
    }
    for label in labels {
        transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO seats (id, event_id, ticket_type_id, section, row_label, seat_label, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            [Uuid::new_v4().to_string().into(), event_id.into(), ticket_type_id.into(), section.into(), row.into(), label.into(), now.into()]
        )).await?;
    }
    transaction.commit().await?;
    Ok(())
}

pub async fn deactivate(
    db: &DatabaseConnection,
    event_id: &str,
    seat_id: &str,
    now: i64,
) -> Result<(), SeatError> {
    let result = db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE seats SET active = 0 WHERE id = ? AND event_id = ? AND active = 1
         AND NOT EXISTS (SELECT 1 FROM tickets WHERE seat_id = ? AND status = 'Valid')
         AND NOT EXISTS (SELECT 1 FROM hold_seats hs JOIN holds h ON h.id = hs.hold_id WHERE hs.seat_id = ? AND h.status = 'Open' AND h.expires_at > ?)",
        [seat_id.into(), event_id.into(), seat_id.into(), seat_id.into(), now.into()]
    )).await?;
    if result.rows_affected() == 0 {
        return Err(SeatError::Unavailable);
    }
    Ok(())
}

pub async fn selections_for_ids(
    db: &DatabaseConnection,
    event_id: &str,
    seat_ids: &[String],
) -> Result<Vec<Selection>, SeatError> {
    if seat_ids.is_empty()
        || seat_ids.len() > 100
        || seat_ids.iter().collect::<HashSet<_>>().len() != seat_ids.len()
    {
        return Err(SeatError::Unavailable);
    }
    let mut types = HashMap::<String, i64>::new();
    for seat_id in seat_ids {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT ticket_type_id FROM seats WHERE id = ? AND event_id = ? AND active = 1",
                [seat_id.clone().into(), event_id.into()],
            ))
            .await?
            .ok_or(SeatError::Unavailable)?;
        *types.entry(row.try_get("", "ticket_type_id")?).or_default() += 1;
    }
    Ok(types
        .into_iter()
        .map(|(ticket_type_id, quantity)| Selection {
            ticket_type_id,
            quantity,
        })
        .collect())
}
