use std::collections::HashSet;

use chrono::Utc;
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, Statement, TransactionTrait,
};
use uuid::Uuid;

use crate::{
    discounts::{self, DiscountError},
    seats::{self, SeatError},
};

pub const HOLD_DURATION_SECONDS: i64 = 31 * 60;

#[derive(Clone, Debug)]
pub struct Selection {
    pub ticket_type_id: String,
    pub quantity: i64,
}

#[derive(Clone, Debug)]
pub struct Hold {
    pub id: String,
    pub event_id: String,
    pub expires_at: i64,
    pub total_cents: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum InventoryError {
    #[error("Choose at least one ticket.")]
    Empty,
    #[error("Sorry, there aren't enough tickets left for that.")]
    SoldOut,
    #[error("This ticket isn't on sale.")]
    NotOnSale,
    #[error("That's more tickets than one order can have.")]
    OrderLimit,
    #[error(transparent)]
    Database(#[from] DbErr),
    #[error(transparent)]
    Discount(#[from] DiscountError),
    #[error(transparent)]
    Seat(#[from] SeatError),
}

pub async fn reserve(
    db: &DatabaseConnection,
    event_id: &str,
    selections: &[Selection],
    now: i64,
) -> Result<Hold, InventoryError> {
    reserve_with_discount(db, event_id, selections, None, now).await
}

pub async fn reserve_with_discount(
    db: &DatabaseConnection,
    event_id: &str,
    selections: &[Selection],
    discount_code: Option<&str>,
    now: i64,
) -> Result<Hold, InventoryError> {
    reserve_with_options(db, event_id, selections, discount_code, &[], now).await
}

pub async fn reserve_with_options(
    db: &DatabaseConnection,
    event_id: &str,
    selections: &[Selection],
    discount_code: Option<&str>,
    seat_ids: &[String],
    now: i64,
) -> Result<Hold, InventoryError> {
    if selections.is_empty() || selections.iter().any(|selection| selection.quantity <= 0) {
        return Err(InventoryError::Empty);
    }
    let distinct: HashSet<&str> = selections
        .iter()
        .map(|selection| selection.ticket_type_id.as_str())
        .collect();
    if distinct.len() != selections.len() {
        return Err(InventoryError::OrderLimit);
    }
    let transaction = db.begin().await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET updated_at = updated_at WHERE id = ?",
            [event_id.into()],
        ))
        .await?;
    let event = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT status, starts_at, event_capacity, max_per_order FROM events WHERE id = ?",
            [event_id.into()],
        ))
        .await?
        .ok_or(InventoryError::NotOnSale)?;
    let status: String = event.try_get("", "status")?;
    let starts_at: i64 = event.try_get("", "starts_at")?;
    let event_capacity: Option<i64> = event.try_get("", "event_capacity")?;
    let max_per_order: i64 = event.try_get("", "max_per_order")?;
    if status != "Published" || now >= starts_at {
        return Err(InventoryError::NotOnSale);
    }
    let quantity: i64 = selections.iter().map(|selection| selection.quantity).sum();
    if quantity > max_per_order {
        return Err(InventoryError::OrderLimit);
    }
    let assigned = seats::assigned(&transaction, event_id).await?;
    if assigned {
        if seat_ids.len() != quantity as usize
            || seat_ids.iter().collect::<HashSet<_>>().len() != seat_ids.len()
        {
            return Err(SeatError::Unavailable.into());
        }
        let mut seat_counts = std::collections::HashMap::<String, i64>::new();
        for seat_id in seat_ids {
            let seat = transaction.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT s.ticket_type_id, s.active,
                    EXISTS(SELECT 1 FROM tickets t WHERE t.seat_id = s.id AND t.status = 'Valid') AS sold,
                    EXISTS(SELECT 1 FROM hold_seats hs JOIN holds h ON h.id = hs.hold_id WHERE hs.seat_id = s.id AND h.status = 'Open' AND h.expires_at > ?) AS held
                 FROM seats s WHERE s.id = ? AND s.event_id = ?",
                [now.into(), seat_id.clone().into(), event_id.into()]
            )).await?.ok_or(SeatError::Unavailable)?;
            if seat.try_get::<i64>("", "active")? != 1
                || seat.try_get::<i64>("", "sold")? != 0
                || seat.try_get::<i64>("", "held")? != 0
            {
                return Err(SeatError::Unavailable.into());
            }
            *seat_counts
                .entry(seat.try_get("", "ticket_type_id")?)
                .or_default() += 1;
        }
        if selections.iter().any(|selection| {
            seat_counts
                .get(&selection.ticket_type_id)
                .copied()
                .unwrap_or(0)
                != selection.quantity
        }) {
            return Err(SeatError::Unavailable.into());
        }
    } else if !seat_ids.is_empty() {
        return Err(SeatError::Unavailable.into());
    }
    let sold_event = count(
        &transaction,
        "SELECT COUNT(*) AS count FROM tickets WHERE event_id = ? AND status = 'Valid'",
        event_id,
    )
    .await?;
    let held_event = held_count(&transaction, event_id, now, false).await?;
    if event_capacity.is_some_and(|capacity| sold_event + held_event + quantity > capacity) {
        return Err(InventoryError::SoldOut);
    }
    let mut total_cents = 0_i64;
    for selection in selections {
        let row = transaction.query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT price_cents, capacity, sales_open, sales_close FROM ticket_types WHERE id = ? AND event_id = ?",
            [selection.ticket_type_id.clone().into(), event_id.into()],
        )).await?.ok_or(InventoryError::NotOnSale)?;
        let price_cents: i64 = row.try_get("", "price_cents")?;
        let capacity: i64 = row.try_get("", "capacity")?;
        let sales_open: Option<i64> = row.try_get("", "sales_open")?;
        let sales_close: Option<i64> = row.try_get("", "sales_close")?;
        if sales_open.is_some_and(|open| now < open) || now >= sales_close.unwrap_or(starts_at) {
            return Err(InventoryError::NotOnSale);
        }
        let sold = count(
            &transaction,
            "SELECT COUNT(*) AS count FROM tickets WHERE ticket_type_id = ? AND status = 'Valid'",
            &selection.ticket_type_id,
        )
        .await?;
        let held = held_count(&transaction, &selection.ticket_type_id, now, true).await?;
        if sold + held + selection.quantity > capacity {
            return Err(InventoryError::SoldOut);
        }
        total_cents = total_cents
            .checked_add(
                price_cents
                    .checked_mul(selection.quantity)
                    .ok_or(InventoryError::OrderLimit)?,
            )
            .ok_or(InventoryError::OrderLimit)?;
    }
    let applied = if let Some(code) = discount_code.filter(|code| !code.trim().is_empty()) {
        if total_cents == 0 {
            return Err(DiscountError::Unavailable.into());
        }
        Some(discounts::apply(&transaction, event_id, code, total_cents, now).await?)
    } else {
        None
    };
    let id = Uuid::new_v4().to_string();
    let expires_at = now + HOLD_DURATION_SECONDS;
    transaction.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "INSERT INTO holds (id, event_id, status, expires_at, created_at) VALUES (?, ?, 'Open', ?, ?)",
        [id.clone().into(), event_id.into(), expires_at.into(), now.into()],
    )).await?;
    transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO hold_questions (hold_id, question_id, scope, kind, label, options_json, required, sort_order) SELECT ?, id, scope, kind, label, options_json, required, sort_order FROM event_questions WHERE event_id = ?",
        [id.clone().into(), event_id.into()]
    )).await?;
    let terms = crate::settings::booking_terms(&transaction).await?;
    let snapshot =
        serde_json::to_string(&terms).map_err(|error| DbErr::Custom(error.to_string()))?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE holds SET booking_terms_snapshot = ? WHERE id = ?",
            [snapshot.into(), id.clone().into()],
        ))
        .await?;
    if let Some(applied) = &applied {
        transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO hold_discounts (hold_id, code_id, code_snapshot, subtotal_cents, discount_cents) VALUES (?, ?, ?, ?, ?)",
            [id.clone().into(), applied.code_id.clone().into(), applied.code.clone().into(), total_cents.into(), applied.discount_cents.into()]
        )).await?;
    }
    for seat_id in seat_ids {
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "INSERT INTO hold_seats (hold_id, seat_id) VALUES (?, ?)",
                [id.clone().into(), seat_id.clone().into()],
            ))
            .await?;
    }
    for selection in selections {
        let price_row = transaction
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT price_cents FROM ticket_types WHERE id = ?",
                [selection.ticket_type_id.clone().into()],
            ))
            .await?
            .ok_or(InventoryError::NotOnSale)?;
        let price_cents: i64 = price_row.try_get("", "price_cents")?;
        transaction.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO hold_items (hold_id, ticket_type_id, quantity, price_cents) VALUES (?, ?, ?, ?)",
            [id.clone().into(), selection.ticket_type_id.clone().into(), selection.quantity.into(), price_cents.into()],
        )).await?;
    }
    transaction.commit().await?;
    Ok(Hold {
        id,
        event_id: event_id.to_owned(),
        expires_at,
        total_cents: total_cents - applied.map_or(0, |discount| discount.discount_cents),
    })
}

async fn count<C: ConnectionTrait>(db: &C, sql: &str, id: &str) -> Result<i64, DbErr> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            sql,
            [id.into()],
        ))
        .await?
        .unwrap();
    row.try_get("", "count")
}

async fn held_count<C: ConnectionTrait>(
    db: &C,
    id: &str,
    now: i64,
    by_type: bool,
) -> Result<i64, DbErr> {
    let sql = if by_type {
        "SELECT COALESCE(SUM(hi.quantity), 0) AS count FROM hold_items hi JOIN holds h ON h.id = hi.hold_id WHERE hi.ticket_type_id = ? AND h.status = 'Open' AND h.expires_at > ?"
    } else {
        "SELECT COALESCE(SUM(hi.quantity), 0) AS count FROM hold_items hi JOIN holds h ON h.id = hi.hold_id WHERE h.event_id = ? AND h.status = 'Open' AND h.expires_at > ?"
    };
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            sql,
            [id.into(), now.into()],
        ))
        .await?
        .unwrap();
    row.try_get("", "count")
}

pub async fn open_counts<C: ConnectionTrait>(
    db: &C,
    event_id: &str,
    now: i64,
) -> Result<(i64, i64), DbErr> {
    let sold = count(
        db,
        "SELECT COUNT(*) AS count FROM tickets WHERE event_id = ? AND status = 'Valid'",
        event_id,
    )
    .await?;
    let held = held_count(db, event_id, now, false).await?;
    Ok((sold, held))
}

pub async fn type_counts<C: ConnectionTrait>(
    db: &C,
    ticket_type_id: &str,
    now: i64,
) -> Result<(i64, i64), DbErr> {
    let sold = count(
        db,
        "SELECT COUNT(*) AS count FROM tickets WHERE ticket_type_id = ? AND status = 'Valid'",
        ticket_type_id,
    )
    .await?;
    let held = held_count(db, ticket_type_id, now, true).await?;
    Ok((sold, held))
}

pub fn now() -> i64 {
    Utc::now().timestamp()
}
