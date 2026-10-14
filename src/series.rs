use std::collections::HashMap;

use chrono::{
    Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Weekday,
};
use chrono_tz::Tz;
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DatabaseTransaction, DbErr, QueryResult,
    Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    events::{self, Event, EventError, EventInput, TicketTypeInput},
    hosted,
    organizers::OrganizerRegistry,
    questions::{self, QuestionError},
    seats::{self, SeatError},
};

pub const DATES_AHEAD: i64 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frequency {
    Weekly,
    Fortnightly,
    Monthly,
}

impl Frequency {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Weekly" => Some(Self::Weekly),
            "Fortnightly" => Some(Self::Fortnightly),
            "Monthly" => Some(Self::Monthly),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Weekly => "Weekly",
            Self::Fortnightly => "Fortnightly",
            Self::Monthly => "Monthly",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SeriesError {
    #[error("{0}")]
    InvalidInput(String),
    #[error(transparent)]
    Event(#[from] EventError),
    #[error(transparent)]
    Question(#[from] QuestionError),
    #[error(transparent)]
    Seat(#[from] SeatError),
    #[error(transparent)]
    Database(#[from] DbErr),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl SeriesError {
    pub fn is_system(&self) -> bool {
        matches!(
            self,
            Self::Database(_)
                | Self::Json(_)
                | Self::Event(EventError::Database(_))
                | Self::Question(QuestionError::Database(_))
                | Self::Seat(SeatError::Database(_))
        )
    }
}

/// The parts of an Event that a copy or a Series date carries. Sales windows are stored as
/// minutes of local clock time before the start, so they move with the date.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Setup {
    pub title: String,
    pub description: String,
    pub venue: String,
    pub address: String,
    pub start_time: String,
    pub end_time: String,
    pub timezone: String,
    pub event_capacity: Option<i64>,
    pub max_per_order: i64,
    pub attendee_name_required: bool,
    pub ticket_types: Vec<TicketSetup>,
    pub questions: Vec<QuestionSetup>,
    pub seats: Vec<SeatSetup>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TicketSetup {
    pub name: String,
    pub price_cents: i64,
    pub capacity: i64,
    pub opens_minutes_before: Option<i64>,
    pub closes_minutes_before: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuestionSetup {
    pub scope: String,
    pub kind: String,
    pub label: String,
    pub options: Vec<String>,
    pub required: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SeatSetup {
    pub ticket_type: usize,
    pub section: String,
    pub row: String,
    pub label: String,
}

pub struct Cover {
    pub mime: String,
    pub bytes: Vec<u8>,
}

pub struct Series {
    pub id: String,
    pub frequency: Frequency,
    pub first_date: NaiveDate,
    pub end_date: Option<NaiveDate>,
    pub created_through: NaiveDate,
    pub status: String,
    pub publish: bool,
    pub setup: Setup,
    pub cover: Option<Cover>,
}

#[derive(Serialize)]
pub struct Panel {
    pub frequency: &'static str,
    pub rule: String,
    pub future_dates: i64,
    pub active: bool,
    pub reviews: Vec<Review>,
    pub existing: Vec<PreviewDate>,
    pub upcoming: Vec<PreviewDate>,
    pub proposed: Vec<PreviewDate>,
}

#[derive(Debug, Serialize)]
pub struct PreviewDate {
    pub date: String,
    pub start_time: String,
    pub end_time: String,
    pub timezone: String,
    pub status: String,
    pub event_id: Option<String>,
}

pub fn preview_dates(
    frequency: Frequency,
    first: NaiveDate,
    after: NaiveDate,
    end: Option<NaiveDate>,
    setup: &Setup,
    limit: usize,
    now: i64,
) -> Result<Vec<PreviewDate>, SeriesError> {
    let tz = zone(&setup.timezone)?;
    let today = tz
        .timestamp_opt(now, 0)
        .single()
        .ok_or_else(|| SeriesError::InvalidInput("Invalid current time".to_owned()))?
        .date_naive();
    let start = NaiveTime::parse_from_str(&setup.start_time, "%H:%M")
        .map_err(|_| SeriesError::InvalidInput("Invalid Series start time".to_owned()))?;
    let finish = NaiveTime::parse_from_str(&setup.end_time, "%H:%M")
        .map_err(|_| SeriesError::InvalidInput("Invalid Series end time".to_owned()))?;
    let mut cursor = after.max(today.pred_opt().unwrap_or(today));
    let mut dates = Vec::with_capacity(limit);
    while dates.len() < limit {
        let Some(date) = next_date(frequency, first, cursor) else {
            break;
        };
        if end.is_some_and(|end| date > end) {
            break;
        }
        cursor = date;
        let starts = tz.from_local_datetime(&date.and_time(start));
        if starts.single().is_some_and(|time| time.timestamp() <= now) {
            continue;
        }
        let end_date = if finish <= start {
            date.succ_opt()
        } else {
            Some(date)
        };
        let ends = end_date.map(|date| tz.from_local_datetime(&date.and_time(finish)));
        let status = if matches!(starts, LocalResult::None)
            || matches!(ends, Some(LocalResult::None) | None)
        {
            "Needs correction: local time does not exist because clocks change"
        } else if matches!(starts, LocalResult::Ambiguous(_, _))
            || matches!(ends, Some(LocalResult::Ambiguous(_, _)))
        {
            "Needs correction: local time occurs twice because clocks change"
        } else {
            "Planned; checked when created"
        };
        dates.push(PreviewDate {
            date: date.to_string(),
            start_time: setup.start_time.clone(),
            end_time: setup.end_time.clone(),
            timezone: setup.timezone.clone(),
            status: status.to_owned(),
            event_id: None,
        });
    }
    Ok(dates)
}

pub async fn preview_start(
    db: &DatabaseConnection,
    event: &Event,
    frequency: Frequency,
    end_date: Option<&str>,
    now: i64,
) -> Result<Vec<PreviewDate>, SeriesError> {
    let first = NaiveDate::parse_from_str(&event.date, "%Y-%m-%d").map_err(|_| {
        SeriesError::InvalidInput("Save a valid date for this event first.".to_owned())
    })?;
    let end = parse_end_date(end_date, first)?;
    let (setup, _) = capture(db, event).await?;
    let source_future = i64::from(event.starts_at > now);
    preview_dates(
        frequency,
        first,
        first,
        end,
        &setup,
        (DATES_AHEAD - source_future) as usize,
        now,
    )
}

fn parse_end_date(value: Option<&str>, first: NaiveDate) -> Result<Option<NaiveDate>, SeriesError> {
    let end = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
                SeriesError::InvalidInput("Enter a valid end date, or leave it empty.".to_owned())
            })
        })
        .transpose()?;
    if end.is_some_and(|end| end < first) {
        return Err(SeriesError::InvalidInput(
            "The end date is before this event. Choose a later end date, or leave it empty."
                .to_owned(),
        ));
    }
    Ok(end)
}

pub async fn capture(
    db: &DatabaseConnection,
    event: &Event,
) -> Result<(Setup, Option<Cover>), SeriesError> {
    let tz = zone(&event.timezone)?;
    let local_start = local_start(&event.date, &event.start_time)?;
    let ticket_types = events::list_ticket_types(db, &event.id).await?;
    let minutes_before = |epoch: i64| {
        tz.timestamp_opt(epoch, 0)
            .single()
            .map(|time| (local_start - time.naive_local()).num_minutes())
    };
    let seats = seats::list(db, &event.id, 0)
        .await?
        .into_iter()
        .filter(|seat| seat.active)
        .filter_map(|seat| {
            let ticket_type = ticket_types
                .iter()
                .position(|item| item.id == seat.ticket_type_id)?;
            Some(SeatSetup {
                ticket_type,
                section: seat.section,
                row: seat.row,
                label: seat.label,
            })
        })
        .collect();
    let setup = Setup {
        title: event.title.clone(),
        description: event.description.clone(),
        venue: event.venue.clone(),
        address: event.address.clone(),
        start_time: event.start_time.clone(),
        end_time: event.end_time.clone(),
        timezone: event.timezone.clone(),
        event_capacity: event.event_capacity,
        max_per_order: event.max_per_order,
        attendee_name_required: event.attendee_name_required,
        ticket_types: ticket_types
            .iter()
            .map(|item| TicketSetup {
                name: item.name.clone(),
                price_cents: item.price_cents,
                capacity: item.capacity,
                opens_minutes_before: item.sales_open.and_then(minutes_before),
                closes_minutes_before: item.sales_close.and_then(minutes_before),
            })
            .collect(),
        questions: questions::list(db, &event.id)
            .await?
            .into_iter()
            .map(|question| QuestionSetup {
                scope: question.scope,
                kind: question.kind,
                label: question.label,
                options: question.options,
                required: question.required,
            })
            .collect(),
        seats,
    };
    let cover = events::cover(db, &event.id)
        .await?
        .map(|(mime, bytes)| Cover { mime, bytes });
    Ok((setup, cover))
}

/// Creates a Draft Event from a setup. Callers pass a transaction so a failure leaves nothing.
async fn build(
    db: &DatabaseTransaction,
    setup: &Setup,
    cover: Option<&Cover>,
    date: NaiveDate,
    now: i64,
) -> Result<Event, SeriesError> {
    let date_text = date.format("%Y-%m-%d").to_string();
    let event = events::create(
        db,
        &EventInput {
            title: setup.title.clone(),
            date: date_text.clone(),
            start_time: setup.start_time.clone(),
            end_time: setup.end_time.clone(),
            timezone: setup.timezone.clone(),
            venue: setup.venue.clone(),
            address: setup.address.clone(),
            description: setup.description.clone(),
            event_capacity: setup.event_capacity,
            max_per_order: setup.max_per_order,
            attendee_name_required: setup.attendee_name_required,
        },
    )
    .await?;
    let tz = zone(&setup.timezone)?;
    let start = local_start(&date_text, &setup.start_time)?;
    let mut type_ids = Vec::with_capacity(setup.ticket_types.len());
    for ticket in &setup.ticket_types {
        let close = ticket
            .closes_minutes_before
            .and_then(|minutes| window(tz, start, minutes));
        let open = ticket
            .opens_minutes_before
            .and_then(|minutes| window(tz, start, minutes));
        let (open, close) = if close.as_ref().is_some_and(|(_, epoch)| *epoch <= now) {
            (None, None)
        } else {
            (open.filter(|(_, epoch)| *epoch > now), close)
        };
        let created = events::add_ticket_type(
            db,
            &event,
            &TicketTypeInput {
                name: ticket.name.clone(),
                price: format!(
                    "{}.{:02}",
                    ticket.price_cents / 100,
                    ticket.price_cents % 100
                ),
                capacity: ticket.capacity,
                sales_open: open.map(|(text, _)| text),
                sales_close: close.map(|(text, _)| text),
            },
        )
        .await?;
        type_ids.push(created.id);
    }
    for question in &setup.questions {
        questions::add(
            db,
            &event.id,
            &question.scope,
            &question.kind,
            &question.label,
            &question.options,
            question.required,
        )
        .await?;
    }
    let mut groups: Vec<(usize, &str, &str, Vec<String>)> = Vec::new();
    for seat in &setup.seats {
        match groups.iter_mut().find(|(ticket_type, section, row, _)| {
            *ticket_type == seat.ticket_type && *section == seat.section && *row == seat.row
        }) {
            Some(group) => group.3.push(seat.label.clone()),
            None => groups.push((
                seat.ticket_type,
                &seat.section,
                &seat.row,
                vec![seat.label.clone()],
            )),
        }
    }
    for (ticket_type, section, row, labels) in groups {
        let Some(ticket_type_id) = type_ids.get(ticket_type) else {
            continue;
        };
        for chunk in labels.chunks(100) {
            seats::add_many(db, &event.id, ticket_type_id, section, row, chunk, now).await?;
        }
    }
    if let Some(cover) = cover {
        events::save_cover(db, &event.id, &cover.mime, cover.bytes.clone()).await?;
    }
    Ok(events::get(db, &event.id)
        .await?
        .ok_or(EventError::NotFound)?)
}

pub async fn copy(
    db: &DatabaseConnection,
    event_id: &str,
    date: &str,
    now: i64,
) -> Result<Event, SeriesError> {
    let event = events::get(db, event_id)
        .await?
        .ok_or(EventError::NotFound)?;
    let date = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
        .map_err(|_| SeriesError::InvalidInput("Enter a valid date for the copy.".to_owned()))?;
    let (setup, cover) = capture(db, &event).await?;
    let transaction = db.begin().await?;
    let copy = build(&transaction, &setup, cover.as_ref(), date, now).await?;
    transaction.commit().await?;
    Ok(copy)
}

pub async fn start(
    db: &DatabaseConnection,
    event_id: &str,
    frequency: Frequency,
    end_date: Option<&str>,
    publish_check: &Result<(), String>,
    now: i64,
) -> Result<String, SeriesError> {
    let event = events::get(db, event_id)
        .await?
        .ok_or(EventError::NotFound)?;
    if event.status == "Cancelled" {
        return Err(SeriesError::InvalidInput(
            "A Cancelled event can't start a Series. Copy it to a new date instead.".to_owned(),
        ));
    }
    if event.series_id.is_some() {
        return Err(already_in_series());
    }
    let first_date = NaiveDate::parse_from_str(&event.date, "%Y-%m-%d").map_err(|_| {
        SeriesError::InvalidInput("Save a valid date for this event first.".to_owned())
    })?;
    let end_date = parse_end_date(end_date, first_date)?;
    let (setup, cover) = capture(db, &event).await?;
    let id = Uuid::new_v4().to_string();
    let transaction = db.begin().await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO series (id, frequency, first_date, end_date, created_through, status,
             publish, setup_json, cover, cover_mime, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, 'Active', ?, ?, ?, ?, ?, ?)",
            [
                id.clone().into(),
                frequency.as_str().into(),
                event.date.clone().into(),
                end_date
                    .map(|date| date.format("%Y-%m-%d").to_string())
                    .into(),
                event.date.clone().into(),
                i64::from(event.status == "Published").into(),
                serde_json::to_string(&setup)?.into(),
                cover.as_ref().map(|cover| cover.bytes.clone()).into(),
                cover.as_ref().map(|cover| cover.mime.clone()).into(),
                now.into(),
                now.into(),
            ],
        ))
        .await?;
    let linked = transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET series_id = ?, series_date = date WHERE id = ? AND series_id IS NULL",
            [id.clone().into(), event.id.clone().into()],
        ))
        .await?
        .rows_affected();
    if linked == 0 {
        return Err(already_in_series());
    }
    fill(&transaction, &id, publish_check, now).await?;
    transaction.commit().await?;
    Ok(id)
}

fn already_in_series() -> SeriesError {
    SeriesError::InvalidInput("This event already belongs to a Series.".to_owned())
}

pub async fn published_dates(
    db: &DatabaseConnection,
    id: &str,
    now: i64,
) -> Result<Option<Vec<Event>>, SeriesError> {
    let exists = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id FROM series WHERE id = ?",
            [id.into()],
        ))
        .await?
        .is_some();
    if !exists {
        return Ok(None);
    }
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id FROM events WHERE series_id = ? AND status = 'Published' AND starts_at > ? ORDER BY starts_at, id",
            [id.into(), now.into()],
        ))
        .await?;
    let mut dates = Vec::with_capacity(rows.len());
    for row in rows {
        let event_id: String = row.try_get("", "id")?;
        if let Some(event) = events::get(db, &event_id).await? {
            if event.series_id.as_deref() == Some(id)
                && event.status == "Published"
                && event.starts_at > now
            {
                dates.push(event);
            }
        }
    }
    Ok(Some(dates))
}

pub async fn top_up(
    db: &DatabaseConnection,
    publish_check: &Result<(), String>,
    now: i64,
) -> Result<usize, SeriesError> {
    let ids = db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT id FROM series WHERE status = 'Active' ORDER BY created_at, id",
        ))
        .await?
        .into_iter()
        .map(|row| row.try_get::<String>("", "id"))
        .collect::<Result<Vec<_>, _>>()?;
    let mut created = 0;
    for id in ids {
        let transaction = db.begin().await?;
        match fill(&transaction, &id, publish_check, now).await {
            Ok(count) => {
                transaction.commit().await?;
                created += count;
            }
            Err(error) => tracing::error!(%error, series_id = %id, "Series top-up failed"),
        }
    }
    Ok(created)
}

pub async fn run(
    control: &DatabaseConnection,
    registry: &OrganizerRegistry,
    now: i64,
) -> loco_rs::Result<()> {
    let scopes = registry
        .active_organizers()
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    for organizer in scopes {
        if registry.background_jobs_stopping() {
            break;
        }
        let scope = match registry.resolve_organizer(&organizer).await {
            Ok(scope) => scope,
            Err(error) => {
                tracing::error!(%error, organizer_id = %organizer.id, "Tenant background database unavailable");
                continue;
            }
        };
        let check = hosted::publish_check(control, registry, &scope.organizer, &scope.db, now)
            .await
            .map_err(|error| error.to_string());
        if let Err(error) = top_up(&scope.db, &check, now).await {
            tracing::error!(%error, organizer_id = %scope.organizer.id, "Series top-up failed");
        }
    }
    Ok(())
}

/// Creates the missing dates of one Series. The first statement writes, so SQLite gives this
/// transaction the write lock before it reads, and a second top-up waits instead of racing.
async fn fill(
    transaction: &DatabaseTransaction,
    series_id: &str,
    publish_check: &Result<(), String>,
    now: i64,
) -> Result<usize, SeriesError> {
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE series SET updated_at = updated_at WHERE id = ?",
            [series_id.into()],
        ))
        .await?;
    let Some(series) = load(transaction, series_id).await? else {
        return Ok(0);
    };
    if series.status != "Active" {
        return Ok(0);
    }
    let mut future = future_dates(transaction, series_id, now).await?;
    let tz = zone(&series.setup.timezone)?;
    let start_time = NaiveTime::parse_from_str(&series.setup.start_time, "%H:%M")
        .map_err(|_| SeriesError::InvalidInput("Invalid Series start time".to_owned()))?;
    let mut cursor = series.created_through;
    let mut created = 0;
    let today = tz
        .timestamp_opt(now, 0)
        .single()
        .ok_or_else(|| SeriesError::InvalidInput("Invalid current time".to_owned()))?
        .date_naive();
    while future < DATES_AHEAD {
        let Some(date) = next_date(series.frequency, series.first_date, cursor) else {
            break;
        };
        if series.end_date.is_some_and(|end| date > end) {
            break;
        }
        cursor = date;
        if date < today {
            continue;
        }
        if let LocalResult::Single(starts) = tz.from_local_datetime(&date.and_time(start_time)) {
            if starts.timestamp() <= now {
                continue;
            }
        }
        let savepoint = transaction.begin().await?;
        let event = match build(&savepoint, &series.setup, series.cover.as_ref(), date, now).await {
            Ok(event) => event,
            Err(error) if error.is_system() => return Err(error),
            Err(error) => {
                savepoint.rollback().await?;
                transaction.execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "INSERT INTO series_exceptions (series_id, date, reason, setup_json, cover, cover_mime)
                     VALUES (?, ?, ?, ?, ?, ?)",
                    [series_id.into(), date.to_string().into(), error.to_string().into(),
                     serde_json::to_string(&series.setup)?.into(),
                     series.cover.as_ref().map(|cover| cover.bytes.clone()).into(),
                     series.cover.as_ref().map(|cover| cover.mime.clone()).into()],
                )).await?;
                future += 1;
                tracing::warn!(%error, series_id, %date, "Series date needs correction");
                continue;
            }
        };
        savepoint
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE events SET series_id = ?, series_date = ? WHERE id = ?",
                [
                    series_id.into(),
                    date.format("%Y-%m-%d").to_string().into(),
                    event.id.clone().into(),
                ],
            ))
            .await?;
        let mut review_reason = None;
        if series.publish {
            match publish_check {
                Ok(()) => match events::publish(&savepoint, &event.id, now).await {
                    Ok(()) => {}
                    Err(EventError::Database(error)) => return Err(error.into()),
                    Err(error) => {
                        review_reason = Some(error.to_string());
                        tracing::warn!(%error, series_id, %date, "Series date stays a Draft");
                    }
                },
                Err(reason) => {
                    review_reason = Some(reason.clone());
                    tracing::warn!(error = %reason, series_id, %date, "Series date stays a Draft");
                }
            }
        }
        if let Some(reason) = review_reason {
            savepoint
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "UPDATE events SET series_review_reason = ? WHERE id = ?",
                    [reason.into(), event.id.into()],
                ))
                .await?;
        }
        savepoint.commit().await?;
        future += 1;
        created += 1;
    }
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE series SET created_through = ?, updated_at = ? WHERE id = ?",
            [
                cursor.format("%Y-%m-%d").to_string().into(),
                now.into(),
                series_id.into(),
            ],
        ))
        .await?;
    Ok(created)
}

async fn future_dates<C: ConnectionTrait>(
    db: &C,
    series_id: &str,
    now: i64,
) -> Result<i64, SeriesError> {
    let mut future = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM events WHERE series_id = ? AND starts_at > ?",
            [series_id.into(), now.into()],
        ))
        .await?
        .map(|row| row.try_get::<i64>("", "count"))
        .transpose()?
        .unwrap_or(0);
    for row in db
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT date, setup_json FROM series_exceptions WHERE series_id = ?",
            [series_id.into()],
        ))
        .await?
    {
        let setup: Setup = serde_json::from_str(&row.try_get::<String>("", "setup_json")?)?;
        let today = zone(&setup.timezone)?
            .timestamp_opt(now, 0)
            .single()
            .ok_or_else(|| SeriesError::InvalidInput("Invalid current time".to_owned()))?
            .date_naive();
        if row.try_get::<String>("", "date")? >= today.to_string() {
            future += 1;
        }
    }
    Ok(future)
}

pub async fn use_setup_from(
    db: &DatabaseConnection,
    event_id: &str,
    now: i64,
) -> Result<(), SeriesError> {
    let event = events::get(db, event_id)
        .await?
        .ok_or(EventError::NotFound)?;
    let series_id = event.series_id.clone().ok_or_else(not_in_series)?;
    let (setup, cover) = capture(db, &event).await?;
    let updated = db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE series SET setup_json = ?, cover = ?, cover_mime = ?, updated_at = ?
             WHERE id = ? AND status = 'Active'",
            [
                serde_json::to_string(&setup)?.into(),
                cover.as_ref().map(|cover| cover.bytes.clone()).into(),
                cover.as_ref().map(|cover| cover.mime.clone()).into(),
                now.into(),
                series_id.into(),
            ],
        ))
        .await?
        .rows_affected();
    if updated == 0 {
        return Err(SeriesError::InvalidInput(
            "This Series has stopped, so it won't create more dates.".to_owned(),
        ));
    }
    Ok(())
}

pub async fn stop(db: &DatabaseConnection, event_id: &str, now: i64) -> Result<(), SeriesError> {
    let event = events::get(db, event_id)
        .await?
        .ok_or(EventError::NotFound)?;
    let series_id = event.series_id.ok_or_else(not_in_series)?;
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE series SET status = 'Stopped', updated_at = ? WHERE id = ?",
        [now.into(), series_id.into()],
    ))
    .await?;
    Ok(())
}

fn not_in_series() -> SeriesError {
    SeriesError::InvalidInput("This event isn't part of a Series.".to_owned())
}

pub async fn panel(
    db: &DatabaseConnection,
    event: &Event,
    now: i64,
) -> Result<Option<Panel>, SeriesError> {
    let Some(series_id) = &event.series_id else {
        return Ok(None);
    };
    let Some(series) = load(db, series_id).await? else {
        return Ok(None);
    };
    let today = zone(&series.setup.timezone)?
        .timestamp_opt(now, 0)
        .single()
        .map_or(series.first_date, |time| time.date_naive());
    let existing = db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT id, date, start_time, end_time, timezone, status FROM events WHERE series_id = ? AND starts_at > ? ORDER BY starts_at",
        [series_id.into(), now.into()],
    )).await?.into_iter().map(|row| Ok(PreviewDate {
        date: row.try_get("", "date")?, start_time: row.try_get("", "start_time")?,
        end_time: row.try_get("", "end_time")?, timezone: row.try_get("", "timezone")?,
        status: row.try_get("", "status")?, event_id: Some(row.try_get("", "id")?),
    })).collect::<Result<Vec<_>, DbErr>>()?;
    let (proposed_setup, _) = capture(db, event).await?;
    let upcoming = if series.status == "Active" {
        preview_dates(
            series.frequency,
            series.first_date,
            series.created_through,
            series.end_date,
            &series.setup,
            DATES_AHEAD as usize,
            now,
        )?
    } else {
        Vec::new()
    };
    let proposed = if series.status == "Active" {
        preview_dates(
            series.frequency,
            series.first_date,
            series.created_through,
            series.end_date,
            &proposed_setup,
            DATES_AHEAD as usize,
            now,
        )?
    } else {
        Vec::new()
    };
    Ok(Some(Panel {
        frequency: series.frequency.as_str(),
        rule: describe(&series, today),
        future_dates: future_dates(db, series_id, now).await?,
        active: series.status == "Active",
        existing,
        upcoming,
        proposed,
        reviews: reviews(db)
            .await?
            .into_iter()
            .filter(|review| review.series_id == *series_id)
            .collect(),
    }))
}

#[derive(Serialize)]
pub struct Review {
    pub series_id: String,
    pub date: String,
    pub title: String,
    pub start_time: String,
    pub end_time: String,
    pub timezone: String,
    pub reason: String,
    pub event_id: Option<String>,
}

pub async fn reviews(db: &DatabaseConnection) -> Result<Vec<Review>, SeriesError> {
    let mut reviews = Vec::new();
    for row in db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT series_id, date, reason, setup_json FROM series_exceptions ORDER BY date",
        ))
        .await?
    {
        let setup: Setup = serde_json::from_str(&row.try_get::<String>("", "setup_json")?)?;
        reviews.push(Review {
            series_id: row.try_get("", "series_id")?,
            date: row.try_get("", "date")?,
            reason: row.try_get("", "reason")?,
            title: setup.title,
            start_time: setup.start_time,
            end_time: setup.end_time,
            timezone: setup.timezone,
            event_id: None,
        });
    }
    for row in db.query_all_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT id, series_id, date, title, start_time, end_time, timezone, series_review_reason
         FROM events WHERE series_id IS NOT NULL AND status = 'Draft' ORDER BY date"
    )).await? {
        reviews.push(Review {
            series_id: row.try_get("", "series_id")?, date: row.try_get("", "date")?,
            title: row.try_get("", "title")?, start_time: row.try_get("", "start_time")?,
            end_time: row.try_get("", "end_time")?, timezone: row.try_get("", "timezone")?,
            reason: row.try_get::<Option<String>>("", "series_review_reason")?
                .unwrap_or_else(|| "This date is a Draft. Review its details and current Setup before publishing.".to_owned()),
            event_id: Some(row.try_get("", "id")?),
        });
    }
    reviews.sort_by(|a, b| a.date.cmp(&b.date));
    Ok(reviews)
}

pub async fn correct_exception(
    db: &DatabaseConnection,
    series_id: &str,
    date: &str,
    start_time: &str,
    end_time: &str,
    now: i64,
) -> Result<Event, SeriesError> {
    let transaction = db.begin().await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE series SET updated_at = updated_at WHERE id = ?",
            [series_id.into()],
        ))
        .await?;
    let row = transaction.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT setup_json, cover, cover_mime FROM series_exceptions WHERE series_id = ? AND date = ?",
        [series_id.into(), date.into()]
    )).await?.ok_or(EventError::NotFound)?;
    let mut setup: Setup = serde_json::from_str(&row.try_get::<String>("", "setup_json")?)?;
    setup.start_time = start_time.to_owned();
    setup.end_time = end_time.to_owned();
    let cover = row
        .try_get::<Option<Vec<u8>>>("", "cover")?
        .map(|bytes| {
            row.try_get::<String>("", "cover_mime")
                .map(|mime| Cover { bytes, mime })
        })
        .transpose()?;
    let date_value = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|_| SeriesError::InvalidInput("Enter a valid date".to_owned()))?;
    let event = build(&transaction, &setup, cover.as_ref(), date_value, now).await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET series_id = ?, series_date = ? WHERE id = ?",
            [series_id.into(), date.into(), event.id.clone().into()],
        ))
        .await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "DELETE FROM series_exceptions WHERE series_id = ? AND date = ?",
            [series_id.into(), date.into()],
        ))
        .await?;
    transaction.commit().await?;
    Ok(events::get(db, &event.id)
        .await?
        .ok_or(EventError::NotFound)?)
}

pub async fn labels(db: &DatabaseConnection) -> Result<HashMap<String, &'static str>, SeriesError> {
    db.query_all_raw(Statement::from_string(
        DatabaseBackend::Sqlite,
        "SELECT id, frequency, status FROM series",
    ))
    .await?
    .into_iter()
    .map(|row| {
        let status: String = row.try_get("", "status")?;
        let frequency: String = row.try_get("", "frequency")?;
        let label = match (status.as_str(), Frequency::parse(&frequency)) {
            ("Active", Some(Frequency::Weekly)) => "Repeats weekly",
            ("Active", Some(Frequency::Fortnightly)) => "Repeats every 2 weeks",
            ("Active", Some(Frequency::Monthly)) => "Repeats monthly",
            _ => "Series stopped",
        };
        Ok((row.try_get("", "id")?, label))
    })
    .collect()
}

pub fn describe(series: &Series, today: NaiveDate) -> String {
    let weekday = weekday_name(series.first_date.weekday());
    let time = &series.setup.start_time;
    let rule = match series.frequency {
        Frequency::Weekly => format!("Every {weekday} at {time}"),
        Frequency::Fortnightly => format!("Every 2 weeks on {weekday} at {time}"),
        Frequency::Monthly => format!(
            "Every month on the {} at {time}",
            monthly_position(series.first_date)
        ),
    };
    match series.end_date {
        Some(end) if end.year() == today.year() => {
            format!("{rule}, until {}", end.format("%-d %B"))
        }
        Some(end) => format!("{rule}, until {}", end.format("%-d %B %Y")),
        None => format!("{rule}, with no end date"),
    }
}

/// For example "second Thursday". A fifth weekday becomes "last", because most months have
/// only four.
pub fn monthly_position(date: NaiveDate) -> String {
    let ordinal = match date.day0() / 7 {
        0 => "first",
        1 => "second",
        2 => "third",
        3 => "fourth",
        _ => "last",
    };
    format!("{ordinal} {}", weekday_name(date.weekday()))
}

fn weekday_name(weekday: Weekday) -> &'static str {
    match weekday {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
}

pub fn next_date(frequency: Frequency, first: NaiveDate, after: NaiveDate) -> Option<NaiveDate> {
    if after < first {
        return Some(first);
    }
    match frequency {
        Frequency::Weekly | Frequency::Fortnightly => {
            let step = if frequency == Frequency::Weekly {
                7
            } else {
                14
            };
            let steps = (after - first).num_days() / step + 1;
            first.checked_add_signed(Duration::days(steps * step))
        }
        Frequency::Monthly => {
            let week = first.day0() / 7 + 1;
            let (mut year, mut month) = (after.year(), after.month());
            for _ in 0..2 {
                if let Some(date) = weekday_in_month(year, month, first.weekday(), week) {
                    if date > after {
                        return Some(date);
                    }
                }
                (year, month) = if month == 12 {
                    (year + 1, 1)
                } else {
                    (year, month + 1)
                };
            }
            None
        }
    }
}

fn weekday_in_month(year: i32, month: u32, weekday: Weekday, week: u32) -> Option<NaiveDate> {
    if week <= 4 {
        return NaiveDate::from_weekday_of_month_opt(year, month, weekday, week as u8);
    }
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let mut date = NaiveDate::from_ymd_opt(next_year, next_month, 1)?.pred_opt()?;
    while date.weekday() != weekday {
        date = date.pred_opt()?;
    }
    Some(date)
}

async fn load<C: ConnectionTrait>(db: &C, id: &str) -> Result<Option<Series>, SeriesError> {
    db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT id, frequency, first_date, end_date, created_through, status, publish,
         setup_json, cover, cover_mime FROM series WHERE id = ?",
        [id.into()],
    ))
    .await?
    .map(series_from_row)
    .transpose()
}

fn series_from_row(row: QueryResult) -> Result<Series, SeriesError> {
    let date = |column: &str| -> Result<NaiveDate, SeriesError> {
        let value: String = row.try_get("", column)?;
        NaiveDate::parse_from_str(&value, "%Y-%m-%d")
            .map_err(|_| SeriesError::InvalidInput(format!("Invalid Series {column}")))
    };
    let frequency: String = row.try_get("", "frequency")?;
    let end_date: Option<String> = row.try_get("", "end_date")?;
    let cover: Option<Vec<u8>> = row.try_get("", "cover")?;
    let cover_mime: Option<String> = row.try_get("", "cover_mime")?;
    let setup: String = row.try_get("", "setup_json")?;
    Ok(Series {
        id: row.try_get("", "id")?,
        frequency: Frequency::parse(&frequency)
            .ok_or_else(|| SeriesError::InvalidInput("Invalid Series frequency".to_owned()))?,
        first_date: date("first_date")?,
        end_date: end_date
            .map(|value| {
                NaiveDate::parse_from_str(&value, "%Y-%m-%d")
                    .map_err(|_| SeriesError::InvalidInput("Invalid Series end date".to_owned()))
            })
            .transpose()?,
        created_through: date("created_through")?,
        status: row.try_get("", "status")?,
        publish: row.try_get::<i64>("", "publish")? == 1,
        setup: serde_json::from_str(&setup)?,
        cover: cover
            .zip(cover_mime)
            .map(|(bytes, mime)| Cover { mime, bytes }),
    })
}

fn zone(name: &str) -> Result<Tz, SeriesError> {
    name.parse()
        .map_err(|_| SeriesError::InvalidInput("Enter a valid time zone".to_owned()))
}

fn local_start(date: &str, time: &str) -> Result<NaiveDateTime, SeriesError> {
    let date = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|_| SeriesError::InvalidInput("Enter a valid date".to_owned()))?;
    let time = NaiveTime::parse_from_str(time, "%H:%M")
        .map_err(|_| SeriesError::InvalidInput("Enter a valid start time".to_owned()))?;
    Ok(date.and_time(time))
}

/// A sales window moment the given local minutes before the start. A moment that a clock change
/// skips or repeats moves one hour later, so it names exactly one instant.
fn window(tz: Tz, start: NaiveDateTime, minutes_before: i64) -> Option<(String, i64)> {
    let mut local = start - Duration::minutes(minutes_before);
    for _ in 0..2 {
        if let LocalResult::Single(time) = tz.from_local_datetime(&local) {
            return Some((local.format("%Y-%m-%dT%H:%M").to_string(), time.timestamp()));
        }
        local += Duration::hours(1);
    }
    None
}
