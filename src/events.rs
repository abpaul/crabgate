use std::collections::HashMap;

use chrono::{LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DatabaseTransaction, DbErr, QueryResult,
    Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::inventory;

#[derive(Clone, Debug)]
pub struct Event {
    pub id: String,
    pub title: String,
    pub description: String,
    pub date: String,
    pub start_time: String,
    pub end_time: String,
    pub timezone: String,
    pub starts_at: i64,
    pub ends_at: i64,
    pub venue: String,
    pub address: String,
    pub status: String,
    pub event_capacity: Option<i64>,
    pub max_per_order: i64,
    pub attendee_name_required: bool,
    pub has_cover: bool,
    pub first_sale_at: Option<i64>,
    pub updated_at: i64,
    pub series_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TicketType {
    pub id: String,
    pub event_id: String,
    pub name: String,
    pub price_cents: i64,
    pub capacity: i64,
    pub sales_open: Option<i64>,
    pub sales_close: Option<i64>,
    pub sort_order: i64,
}

pub struct TicketTypeSummary {
    pub capacity: i64,
    pub min_price_cents: Option<i64>,
    pub count: i64,
}

pub struct EventSalesSummary {
    pub sold: i64,
    pub revenue_cents: i64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct EventInput {
    pub title: String,
    pub date: String,
    pub start_time: String,
    pub end_time: String,
    pub timezone: String,
    pub venue: String,
    pub address: String,
    pub description: String,
    pub event_capacity: Option<i64>,
    pub max_per_order: i64,
    #[serde(default)]
    pub attendee_name_required: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TicketTypeInput {
    pub name: String,
    pub price: String,
    pub capacity: i64,
    pub sales_open: Option<String>,
    pub sales_close: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TicketBoxState {
    OnSale,
    ComingSoon,
    SalesEnded,
    SoldOut,
}

impl TicketBoxState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnSale => "on-sale",
            Self::ComingSoon => "coming-soon",
            Self::SalesEnded => "sales-ended",
            Self::SoldOut => "sold-out",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("Event not found")]
    NotFound,
    #[error("{0}")]
    Rule(String),
    #[error(transparent)]
    Database(#[from] DbErr),
}

pub async fn create<C: ConnectionTrait>(db: &C, input: &EventInput) -> Result<Event, EventError> {
    let (starts_at, ends_at) = validate_event(input)?;
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "INSERT INTO events (id, title, description, date, start_time, end_time, timezone,
         starts_at, ends_at, venue, address, status, event_capacity, max_per_order,
         attendee_name_required, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'Draft', ?, ?, ?, ?, ?)",
        [
            id.clone().into(),
            input.title.trim().into(),
            input.description.trim().into(),
            input.date.clone().into(),
            input.start_time.clone().into(),
            input.end_time.clone().into(),
            input.timezone.clone().into(),
            starts_at.into(),
            ends_at.into(),
            input.venue.trim().into(),
            input.address.trim().into(),
            input.event_capacity.into(),
            input.max_per_order.into(),
            i64::from(input.attendee_name_required).into(),
            now.into(),
            now.into(),
        ],
    ))
    .await?;
    get(db, &id).await?.ok_or(EventError::NotFound)
}

pub async fn update(
    db: &DatabaseConnection,
    id: &str,
    input: &EventInput,
) -> Result<Event, EventError> {
    let (starts_at, ends_at) = validate_event(input)?;
    let transaction = db.begin().await?;
    let current = locked_event(&transaction, id).await?;
    if current.status == "Cancelled" {
        return Err(EventError::Rule(
            "A Cancelled Event cannot be edited".to_owned(),
        ));
    }
    if current.timezone != input.timezone
        && list_ticket_types(&transaction, id)
            .await?
            .iter()
            .any(|item| item.sales_open.is_some() || item.sales_close.is_some())
    {
        return Err(EventError::InvalidInput("Clear saved sales windows before changing the event time zone. Then set the windows again in the new zone.".to_owned()));
    }
    if starts_at != current.starts_at {
        for item in list_ticket_types(&transaction, id).await? {
            validate_sales_window(starts_at, item.sales_open, item.sales_close).map_err(
                |error| {
                    EventError::InvalidInput(format!(
                    "Adjust the saved sales window for {} before changing the event time: {error}",
                    item.name
                ))
                },
            )?;
        }
    }
    if let Some(capacity) = input.event_capacity {
        let (sold, held) = inventory::open_counts(&transaction, id, Utc::now().timestamp()).await?;
        if capacity < sold + held {
            return Err(EventError::InvalidInput(
                "Event capacity cannot be less than sold tickets and open holds".to_owned(),
            ));
        }
    }
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET title = ?, description = ?, date = ?, start_time = ?, end_time = ?,
         timezone = ?, starts_at = ?, ends_at = ?, venue = ?, address = ?, event_capacity = ?,
         max_per_order = ?, attendee_name_required = ?, updated_at = ? WHERE id = ?",
            [
                input.title.trim().into(),
                input.description.trim().into(),
                input.date.clone().into(),
                input.start_time.clone().into(),
                input.end_time.clone().into(),
                input.timezone.clone().into(),
                starts_at.into(),
                ends_at.into(),
                input.venue.trim().into(),
                input.address.trim().into(),
                input.event_capacity.into(),
                input.max_per_order.into(),
                i64::from(input.attendee_name_required).into(),
                Utc::now().timestamp().into(),
                id.into(),
            ],
        ))
        .await?;
    if (current.date != input.date
            || current.start_time != input.start_time
            || current.end_time != input.end_time
            || current.timezone != input.timezone
            || current.venue != input.venue.trim()
            || current.address != input.address.trim())
        && transaction.query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT o.id FROM orders o WHERE o.event_id = ? AND o.status != 'Refunded'
             AND EXISTS (SELECT 1 FROM tickets t WHERE t.order_id = o.id AND t.event_id = o.event_id AND t.status = 'Valid') LIMIT 1",
            [id.into()],
        )).await?.is_some()
    {
        let notice_id = Uuid::new_v4().to_string();
        let old_when = format!(
            "{} {} to {} ({})",
            current.date, current.start_time, current.end_time, current.timezone
        );
        let new_when = format!(
            "{} {} to {} ({})",
            input.date, input.start_time, input.end_time, input.timezone
        );
        let old_place = format!("{}, {}", current.venue, current.address);
        let new_place = format!("{}, {}", input.venue.trim(), input.address.trim());
        transaction.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO event_change_notices (id, event_id, old_when, new_when, old_place, new_place, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            [notice_id.clone().into(), id.into(), old_when.into(), new_when.into(), old_place.into(), new_place.into(), Utc::now().timestamp().into()],
        )).await?;
        transaction.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO event_change_recipients (notice_id, order_id)
             SELECT ?, o.id FROM orders o WHERE o.event_id = ? AND o.status != 'Refunded'
             AND EXISTS (SELECT 1 FROM tickets t WHERE t.order_id = o.id AND t.event_id = o.event_id AND t.status = 'Valid')",
            [notice_id.into(), id.into()],
        )).await?;
    }
    transaction.commit().await?;
    get(db, id).await?.ok_or(EventError::NotFound)
}

pub enum TicketTypeConnection<'a> {
    Connection(&'a DatabaseConnection),
    Transaction(&'a DatabaseTransaction),
}

impl<'a> From<&'a DatabaseConnection> for TicketTypeConnection<'a> {
    fn from(db: &'a DatabaseConnection) -> Self {
        Self::Connection(db)
    }
}

impl<'a> From<&'a tenant_sqlite_loco::TenantHandle> for TicketTypeConnection<'a> {
    fn from(db: &'a tenant_sqlite_loco::TenantHandle) -> Self {
        Self::Connection(db)
    }
}

impl<'a> From<&'a DatabaseTransaction> for TicketTypeConnection<'a> {
    fn from(db: &'a DatabaseTransaction) -> Self {
        Self::Transaction(db)
    }
}

async fn locked_event(db: &DatabaseTransaction, id: &str) -> Result<Event, EventError> {
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE events SET updated_at = updated_at WHERE id = ?",
        [id.into()],
    ))
    .await?;
    get(db, id).await?.ok_or(EventError::NotFound)
}

pub async fn add_ticket_type<'a>(
    db: impl Into<TicketTypeConnection<'a>>,
    event: &Event,
    input: &TicketTypeInput,
) -> Result<TicketType, EventError> {
    match db.into() {
        TicketTypeConnection::Connection(db) => {
            let transaction = db.begin().await?;
            let ticket = add_ticket_type_in_transaction(&transaction, &event.id, input).await?;
            transaction.commit().await?;
            Ok(ticket)
        }
        TicketTypeConnection::Transaction(db) => {
            add_ticket_type_in_transaction(db, &event.id, input).await
        }
    }
}

async fn add_ticket_type_in_transaction(
    db: &DatabaseTransaction,
    event_id: &str,
    input: &TicketTypeInput,
) -> Result<TicketType, EventError> {
    let event = locked_event(db, event_id).await?;
    let (price_cents, sales_open, sales_close) = validate_ticket_type(&event, input)?;
    let id = Uuid::new_v4().to_string();
    let sort_order = list_ticket_types(db, &event.id).await?.len() as i64;
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "INSERT INTO ticket_types (id, event_id, name, price_cents, capacity, sales_open,
         sales_close, sort_order) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        [
            id.clone().into(),
            event.id.clone().into(),
            input.name.trim().into(),
            price_cents.into(),
            input.capacity.into(),
            sales_open.into(),
            sales_close.into(),
            sort_order.into(),
        ],
    ))
    .await?;
    Ok(TicketType {
        id,
        event_id: event.id.clone(),
        name: input.name.trim().to_owned(),
        price_cents,
        capacity: input.capacity,
        sales_open,
        sales_close,
        sort_order,
    })
}

pub async fn update_ticket_type(
    db: &DatabaseConnection,
    event: &Event,
    id: &str,
    input: &TicketTypeInput,
) -> Result<(), EventError> {
    let transaction = db.begin().await?;
    let event = locked_event(&transaction, &event.id).await?;
    let (price_cents, sales_open, sales_close) = validate_ticket_type(&event, input)?;
    let (sold, held) = inventory::type_counts(&transaction, id, Utc::now().timestamp()).await?;
    if input.capacity < sold + held {
        return Err(EventError::InvalidInput(
            "Ticket type capacity cannot be less than sold tickets and open holds".to_owned(),
        ));
    }
    let seat_count = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM seats WHERE ticket_type_id = ? AND event_id = ?",
            [id.into(), event.id.clone().into()],
        ))
        .await?
        .ok_or(EventError::NotFound)?
        .try_get::<i64>("", "count")?;
    if input.capacity < seat_count {
        return Err(EventError::InvalidInput(
            "Ticket type capacity cannot be less than its assigned seats".to_owned(),
        ));
    }
    let affected = transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE ticket_types SET name = ?, price_cents = ?, capacity = ?, sales_open = ?,
         sales_close = ? WHERE id = ? AND event_id = ?",
            [
                input.name.trim().into(),
                price_cents.into(),
                input.capacity.into(),
                sales_open.into(),
                sales_close.into(),
                id.into(),
                event.id.clone().into(),
            ],
        ))
        .await?
        .rows_affected();
    if affected == 0 {
        return Err(EventError::NotFound);
    }
    transaction.commit().await?;
    Ok(())
}

fn validate_ticket_type(
    event: &Event,
    input: &TicketTypeInput,
) -> Result<(i64, Option<i64>, Option<i64>), EventError> {
    if event.status == "Cancelled" {
        return Err(EventError::Rule(
            "A Cancelled Event cannot be edited".to_owned(),
        ));
    }
    if input.name.trim().is_empty() || input.name.len() > 100 {
        return Err(EventError::InvalidInput(
            "Enter a ticket type name".to_owned(),
        ));
    }
    if input.capacity < 0 {
        return Err(EventError::InvalidInput(
            "Capacity cannot be negative".to_owned(),
        ));
    }
    let price_cents = parse_price(&input.price)?;
    let sales_open = parse_window(input.sales_open.as_deref(), event)?;
    let sales_close = parse_window(input.sales_close.as_deref(), event)?;
    validate_sales_window(event.starts_at, sales_open, sales_close)?;
    Ok((price_cents, sales_open, sales_close))
}

fn validate_sales_window(
    starts_at: i64,
    sales_open: Option<i64>,
    sales_close: Option<i64>,
) -> Result<(), EventError> {
    if let Some(close) = sales_close {
        if close > starts_at {
            return Err(EventError::InvalidInput(
                "Sales window must close before the event starts".to_owned(),
            ));
        }
    }
    if sales_open.is_some_and(|open| open >= sales_close.unwrap_or(starts_at)) {
        return Err(EventError::InvalidInput(
            "Sales window opening must be before closing".to_owned(),
        ));
    }
    Ok(())
}

pub async fn publish<C: ConnectionTrait>(db: &C, id: &str, now: i64) -> Result<(), EventError> {
    let event = get(db, id).await?.ok_or(EventError::NotFound)?;
    if event.status != "Draft" || event.starts_at <= now {
        return Err(EventError::Rule(
            "This event cannot be published".to_owned(),
        ));
    }
    if list_ticket_types(db, id).await?.is_empty() {
        return Err(EventError::Rule(
            "Add a ticket type before you publish.".to_owned(),
        ));
    }
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE events SET status = 'Published', updated_at = ? WHERE id = ?",
        [now.into(), id.into()],
    ))
    .await?;
    Ok(())
}

pub async fn return_to_draft(db: &DatabaseConnection, id: &str) -> Result<(), EventError> {
    let event = get(db, id).await?.ok_or(EventError::NotFound)?;
    if event.status != "Published" || event.first_sale_at.is_some() {
        return Err(EventError::Rule(
            "This event cannot return to Draft".to_owned(),
        ));
    }
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE events SET status = 'Draft', updated_at = ? WHERE id = ?",
        [Utc::now().timestamp().into(), id.into()],
    ))
    .await?;
    Ok(())
}

pub async fn get<C: ConnectionTrait>(db: &C, id: &str) -> Result<Option<Event>, EventError> {
    db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT id, title, description, date, start_time, end_time, timezone, starts_at,
         ends_at, venue, address, status, event_capacity, max_per_order, attendee_name_required,
         cover IS NOT NULL AS has_cover, first_sale_at, updated_at, series_id FROM events WHERE id = ?",
        [id.into()],
    ))
    .await?
    .map(event_from_row)
    .transpose()
}

pub async fn list(db: &DatabaseConnection) -> Result<Vec<Event>, EventError> {
    db.query_all_raw(Statement::from_string(
        DatabaseBackend::Sqlite,
        "SELECT id, title, description, date, start_time, end_time, timezone, starts_at,
         ends_at, venue, address, status, event_capacity, max_per_order, attendee_name_required,
         cover IS NOT NULL AS has_cover, first_sale_at, updated_at, series_id FROM events ORDER BY starts_at",
    ))
    .await?
    .into_iter()
    .map(event_from_row)
    .collect()
}

pub async fn list_ticket_types<C: ConnectionTrait>(
    db: &C,
    event_id: &str,
) -> Result<Vec<TicketType>, EventError> {
    db.query_all_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT id, event_id, name, price_cents, capacity, sales_open, sales_close, sort_order
         FROM ticket_types WHERE event_id = ? ORDER BY sort_order, id",
        [event_id.into()],
    ))
    .await?
    .into_iter()
    .map(ticket_type_from_row)
    .collect()
}

pub async fn list_all_ticket_types<C: ConnectionTrait>(
    db: &C,
) -> Result<HashMap<String, Vec<TicketType>>, EventError> {
    let rows = db.query_all_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT id, event_id, name, price_cents, capacity, sales_open, sales_close, sort_order FROM ticket_types ORDER BY event_id, sort_order, id"
    )).await?;
    let mut result: HashMap<String, Vec<TicketType>> = HashMap::new();
    for row in rows {
        let kind = ticket_type_from_row(row)?;
        result.entry(kind.event_id.clone()).or_default().push(kind);
    }
    Ok(result)
}

pub async fn ticket_type_summaries(
    db: &DatabaseConnection,
) -> Result<HashMap<String, TicketTypeSummary>, EventError> {
    let rows = db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT event_id, SUM(capacity) AS capacity, MIN(price_cents) AS min_price_cents, COUNT(*) AS type_count FROM ticket_types GROUP BY event_id".to_owned(),
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("", "event_id")?,
                TicketTypeSummary {
                    capacity: row.try_get("", "capacity")?,
                    min_price_cents: row.try_get("", "min_price_cents")?,
                    count: row.try_get("", "type_count")?,
                },
            ))
        })
        .collect()
}

pub async fn event_sales_summaries(
    db: &DatabaseConnection,
) -> Result<HashMap<String, EventSalesSummary>, EventError> {
    let rows = db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT event_id, SUM(sold) AS sold, SUM(revenue_cents) AS revenue_cents FROM (
                SELECT event_id, COUNT(*) AS sold, 0 AS revenue_cents FROM tickets WHERE status = 'Valid' GROUP BY event_id
                UNION ALL
                SELECT event_id, 0 AS sold, SUM(total_cents) AS revenue_cents FROM orders WHERE status = 'Paid' GROUP BY event_id
             ) GROUP BY event_id".to_owned(),
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("", "event_id")?,
                EventSalesSummary {
                    sold: row.try_get("", "sold")?,
                    revenue_cents: row.try_get("", "revenue_cents")?,
                },
            ))
        })
        .collect()
}

pub fn ticket_box_state(
    event: &Event,
    ticket_type: &TicketType,
    type_sold: i64,
    type_open_holds: i64,
    event_sold: i64,
    event_open_holds: i64,
    now: i64,
) -> TicketBoxState {
    if event.status != "Published"
        || now >= event.starts_at
        || now >= ticket_type.sales_close.unwrap_or(event.starts_at)
    {
        TicketBoxState::SalesEnded
    } else if type_sold + type_open_holds >= ticket_type.capacity
        || event
            .event_capacity
            .is_some_and(|capacity| event_sold + event_open_holds >= capacity)
    {
        TicketBoxState::SoldOut
    } else if ticket_type.sales_open.is_some_and(|open| now < open) {
        TicketBoxState::ComingSoon
    } else {
        TicketBoxState::OnSale
    }
}

pub fn parse_price(price: &str) -> Result<i64, EventError> {
    let value = price.trim();
    let (euros, cents) = match value.split_once('.') {
        Some((euros, cents)) if cents.len() <= 2 && !cents.is_empty() => {
            let padded = format!("{cents:0<2}");
            (euros, padded)
        }
        None => (value, "00".to_owned()),
        _ => return Err(EventError::InvalidInput("Enter a valid price".to_owned())),
    };
    if !euros.bytes().all(|byte| byte.is_ascii_digit())
        || !cents.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(EventError::InvalidInput("Enter a valid price".to_owned()));
    }
    let euros: i64 = euros
        .parse()
        .map_err(|_| EventError::InvalidInput("Enter a valid price".to_owned()))?;
    let cents: i64 = cents
        .parse()
        .map_err(|_| EventError::InvalidInput("Enter a valid price".to_owned()))?;
    euros
        .checked_mul(100)
        .and_then(|value| value.checked_add(cents))
        .ok_or_else(|| EventError::InvalidInput("Price is too large".to_owned()))
}

pub fn formatted_date(event: &Event) -> String {
    NaiveDate::parse_from_str(&event.date, "%Y-%m-%d")
        .map(|date| date.format("%a %-d %B %Y").to_string())
        .unwrap_or_else(|_| event.date.clone())
}

pub fn formatted_price(cents: i64) -> String {
    format_price(cents, "EUR")
}

pub fn format_price(cents: i64, currency: &str) -> String {
    let symbol = currency_symbol(currency);
    if cents == 0 {
        "Free".to_owned()
    } else if cents % 100 == 0 {
        format!("{symbol}{}", cents / 100)
    } else {
        format!("{symbol}{}.{:02}", cents / 100, cents % 100)
    }
}

pub fn currency_symbol(currency: &str) -> &'static str {
    match currency {
        "GBP" => "£",
        "USD" => "$",
        _ => "€",
    }
}

pub async fn save_cover<C: ConnectionTrait>(
    db: &C,
    event_id: &str,
    mime: &str,
    bytes: Vec<u8>,
) -> Result<(), EventError> {
    let event = get(db, event_id).await?.ok_or(EventError::NotFound)?;
    if event.status == "Cancelled" {
        return Err(EventError::Rule(
            "A Cancelled Event cannot be edited".to_owned(),
        ));
    }
    if bytes.len() > 3_000_000 {
        return Err(EventError::InvalidInput(
            "Image is over 3 MB. Choose a smaller image.".to_owned(),
        ));
    }
    let actual_mime = cover_format(&bytes).ok_or_else(|| {
        let message = if matches!(mime, "image/jpeg" | "image/png" | "image/webp") {
            "Unreadable image. Choose a valid JPG, PNG, or WebP file."
        } else {
            "Unsupported format. Choose a JPG, PNG, or WebP image."
        };
        EventError::InvalidInput(message.to_owned())
    })?;
    if mime != actual_mime && matches!(mime, "image/jpeg" | "image/png" | "image/webp") {
        return Err(EventError::InvalidInput(
            "Unreadable image. Choose a valid JPG, PNG, or WebP file.".to_owned(),
        ));
    }
    let affected = db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET cover = ?, cover_mime = ?, updated_at = ? WHERE id = ?",
            [
                bytes.into(),
                actual_mime.into(),
                Utc::now().timestamp().into(),
                event_id.into(),
            ],
        ))
        .await?
        .rows_affected();
    if affected == 0 {
        return Err(EventError::NotFound);
    }
    Ok(())
}

pub async fn cover(
    db: &DatabaseConnection,
    event_id: &str,
) -> Result<Option<(String, Vec<u8>)>, EventError> {
    db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT cover_mime, cover FROM events WHERE id = ? AND cover IS NOT NULL",
        [event_id.into()],
    ))
    .await?
    .map(|row| Ok((row.try_get("", "cover_mime")?, row.try_get("", "cover")?)))
    .transpose()
}

fn validate_event(input: &EventInput) -> Result<(i64, i64), EventError> {
    if input.title.trim().len() < 2 || input.title.len() > 120 {
        return Err(EventError::InvalidInput("Enter an event name".to_owned()));
    }
    if input.venue.trim().is_empty() || input.address.trim().is_empty() {
        return Err(EventError::InvalidInput(
            "Enter the venue and address".to_owned(),
        ));
    }
    if input.description.len() > 10_000 {
        return Err(EventError::InvalidInput(
            "About this event is too long".to_owned(),
        ));
    }
    if input.event_capacity.is_some_and(|capacity| capacity < 0) {
        return Err(EventError::InvalidInput(
            "Event capacity cannot be negative".to_owned(),
        ));
    }
    if !(1..=100).contains(&input.max_per_order) {
        return Err(EventError::InvalidInput(
            "Maximum per order must be 1 to 100".to_owned(),
        ));
    }
    let date = NaiveDate::parse_from_str(&input.date, "%Y-%m-%d")
        .map_err(|_| EventError::InvalidInput("Enter a valid date".to_owned()))?;
    let start = NaiveTime::parse_from_str(&input.start_time, "%H:%M")
        .map_err(|_| EventError::InvalidInput("Enter a valid start time".to_owned()))?;
    let end = NaiveTime::parse_from_str(&input.end_time, "%H:%M")
        .map_err(|_| EventError::InvalidInput("Enter a valid end time".to_owned()))?;
    let tz: Tz = input
        .timezone
        .parse()
        .map_err(|_| EventError::InvalidInput("Enter a valid time zone".to_owned()))?;
    let end_date = if end <= start {
        date.succ_opt()
            .ok_or_else(|| EventError::InvalidInput("Invalid end time".to_owned()))?
    } else {
        date
    };
    let starts_at = local_to_epoch(tz, date.and_time(start))?;
    let ends_at = local_to_epoch(tz, end_date.and_time(end))?;
    Ok((starts_at, ends_at))
}

fn parse_window(input: Option<&str>, event: &Event) -> Result<Option<i64>, EventError> {
    let Some(value) = input.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    let date_time = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M")
        .map_err(|_| EventError::InvalidInput("Enter a valid sales window".to_owned()))?;
    let tz: Tz = event
        .timezone
        .parse()
        .map_err(|_| EventError::InvalidInput("Invalid event time zone".to_owned()))?;
    Ok(Some(local_to_epoch(tz, date_time)?))
}

fn local_to_epoch(tz: Tz, local: NaiveDateTime) -> Result<i64, EventError> {
    match tz.from_local_datetime(&local) {
        LocalResult::Single(time) => Ok(time.timestamp()),
        LocalResult::None => Err(EventError::InvalidInput(format!("{} does not exist in {tz} because clocks change. Choose another local time.", local.format("%d %b %Y %H:%M")))),
        LocalResult::Ambiguous(_, _) => Err(EventError::InvalidInput(format!("{} occurs twice in {tz} because clocks change. Choose a time outside the repeated hour.", local.format("%d %b %Y %H:%M")))),
    }
}

fn event_from_row(row: QueryResult) -> Result<Event, EventError> {
    Ok(Event {
        id: row.try_get("", "id")?,
        title: row.try_get("", "title")?,
        description: row.try_get("", "description")?,
        date: row.try_get("", "date")?,
        start_time: row.try_get("", "start_time")?,
        end_time: row.try_get("", "end_time")?,
        timezone: row.try_get("", "timezone")?,
        starts_at: row.try_get("", "starts_at")?,
        ends_at: row.try_get("", "ends_at")?,
        venue: row.try_get("", "venue")?,
        address: row.try_get("", "address")?,
        status: row.try_get("", "status")?,
        event_capacity: row.try_get("", "event_capacity")?,
        max_per_order: row.try_get("", "max_per_order")?,
        attendee_name_required: row.try_get::<i64>("", "attendee_name_required")? != 0,
        has_cover: row.try_get::<i64>("", "has_cover")? != 0,
        first_sale_at: row.try_get("", "first_sale_at")?,
        updated_at: row.try_get("", "updated_at")?,
        series_id: row.try_get("", "series_id")?,
    })
}

fn ticket_type_from_row(row: QueryResult) -> Result<TicketType, EventError> {
    Ok(TicketType {
        id: row.try_get("", "id")?,
        event_id: row.try_get("", "event_id")?,
        name: row.try_get("", "name")?,
        price_cents: row.try_get("", "price_cents")?,
        capacity: row.try_get("", "capacity")?,
        sales_open: row.try_get("", "sales_open")?,
        sales_close: row.try_get("", "sales_close")?,
        sort_order: row.try_get("", "sort_order")?,
    })
}

fn cover_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8]) && bytes.ends_with(&[0xff, 0xd9]) {
        let mut offset = 2;
        let mut saw_frame = false;
        while offset + 4 <= bytes.len() && bytes[offset] == 0xff {
            let marker = bytes[offset + 1];
            if marker == 0xda {
                return (saw_frame && offset + 6 < bytes.len()).then_some("image/jpeg");
            }
            let length =
                u16::from_be_bytes(bytes.get(offset + 2..offset + 4)?.try_into().ok()?) as usize;
            if length < 2 || offset.checked_add(length + 2)? > bytes.len() {
                return None;
            }
            if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
                saw_frame = true;
            }
            offset += length + 2;
        }
        return None;
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        let mut offset = 8usize;
        let mut saw_header = false;
        let mut saw_data = false;
        while offset + 12 <= bytes.len() {
            let size = u32::from_be_bytes(bytes[offset..offset + 4].try_into().ok()?) as usize;
            let end = offset.checked_add(size + 12)?;
            if end > bytes.len() {
                return None;
            }
            let kind = &bytes[offset + 4..offset + 8];
            if !kind.iter().all(u8::is_ascii_alphabetic) {
                return None;
            }
            let data = &bytes[offset + 8..end - 4];
            let crc = u32::from_be_bytes(bytes[end - 4..end].try_into().ok()?);
            if png_crc(&bytes[offset + 4..end - 4]) != crc {
                return None;
            }
            match kind {
                b"IHDR" if offset == 8 && size == 13 => {
                    saw_header = data[..4] != [0; 4] && data[4..8] != [0; 4];
                }
                b"IDAT" if saw_header && !data.is_empty() => saw_data = true,
                b"IEND" if saw_data && size == 0 && end == bytes.len() => return Some("image/png"),
                _ if !saw_header => return None,
                _ => {}
            }
            offset = end;
        }
        return None;
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        let size = u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?) as usize;
        if size.checked_add(8)? != bytes.len() {
            return None;
        }
        let mut offset = 12usize;
        let mut saw_image = false;
        while offset + 8 <= bytes.len() {
            let kind = bytes.get(offset..offset + 4)?;
            let size =
                u32::from_le_bytes(bytes.get(offset + 4..offset + 8)?.try_into().ok()?) as usize;
            let data_end = offset.checked_add(8)?.checked_add(size)?;
            let end = data_end.checked_add(size % 2)?;
            if end > bytes.len() {
                return None;
            }
            if matches!(kind, b"VP8 " | b"VP8L") && size >= 5 {
                saw_image = true;
            }
            offset = end;
        }
        return (saw_image && offset == bytes.len()).then_some("image/webp");
    }
    None
}

fn png_crc(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}
