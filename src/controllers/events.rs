use axum::{
    extract::{multipart::MultipartRejection, DefaultBodyLimit, Form, Multipart, Path, Query},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use chrono::{TimeZone, Utc};
use chrono_tz::Tz;
use loco_rs::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::organizers::OrganizerRegistry;
use crate::{
    auth::AdminOrganizer,
    discounts, event_notices,
    events::{self, Event, EventError, EventInput, TicketTypeInput},
    hosted,
    organizers::Edition,
    questions, seats,
    series::{self, Frequency, SeriesError},
    settings,
};
use axum::{extract::State, Extension};
use std::sync::Arc;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct EventForm {
    title: String,
    date: String,
    start_time: String,
    end_time: String,
    timezone: String,
    venue: String,
    address: String,
    description: String,
    #[serde(default)]
    event_capacity: String,
    max_per_order: String,
    #[serde(default)]
    attendee_name_required: Option<String>,
    #[serde(default)]
    confirm_timezone_change: Option<String>,
}

impl TryFrom<EventForm> for EventInput {
    type Error = EventError;

    fn try_from(form: EventForm) -> std::result::Result<Self, Self::Error> {
        let event_capacity =
            if form.event_capacity.trim().is_empty() {
                None
            } else {
                Some(form.event_capacity.parse().map_err(|_| {
                    EventError::InvalidInput("Enter a valid event capacity".to_owned())
                })?)
            };
        Ok(Self {
            title: form.title,
            date: form.date,
            start_time: form.start_time,
            end_time: form.end_time,
            timezone: form.timezone,
            venue: form.venue,
            address: form.address,
            description: form.description,
            event_capacity,
            max_per_order: form.max_per_order.parse().map_err(|_| {
                EventError::InvalidInput("Maximum per order must be 1 to 100".to_owned())
            })?,
            attendee_name_required: form.attendee_name_required.is_some(),
        })
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct TicketTypeForm {
    name: String,
    price: String,
    capacity: String,
    sales_open: Option<String>,
    sales_close: Option<String>,
}

#[derive(Deserialize)]
struct QuestionForm {
    scope: String,
    kind: String,
    label: String,
    options: String,
    required: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct SeatForm {
    ticket_type_id: String,
    section: String,
    row: String,
    labels: String,
}

#[derive(Deserialize)]
struct CopyForm {
    date: String,
}

#[derive(Deserialize, Serialize)]
struct RepeatForm {
    #[serde(default)]
    frequency: String,
    #[serde(default)]
    end_date: String,
}

#[derive(Deserialize)]
struct RepeatPreviewQuery {
    frequency: Option<String>,
    #[serde(default)]
    end_date: String,
}

#[derive(Deserialize)]
struct EmbedOriginsForm {
    origins: String,
}

impl Default for RepeatForm {
    fn default() -> Self {
        Self {
            frequency: "Weekly".to_owned(),
            end_date: String::new(),
        }
    }
}

#[derive(Deserialize)]
struct DiscountForm {
    code: String,
    kind: String,
    value: String,
    starts: String,
    ends: String,
    max_redemptions: String,
}

impl TryFrom<TicketTypeForm> for TicketTypeInput {
    type Error = EventError;

    fn try_from(form: TicketTypeForm) -> std::result::Result<Self, Self::Error> {
        Ok(Self {
            name: form.name,
            price: form.price,
            capacity: form.capacity.parse().map_err(|_| {
                EventError::InvalidInput("Enter a valid ticket type capacity".to_owned())
            })?,
            sales_open: form.sales_open,
            sales_close: form.sales_close,
        })
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    show: Option<String>,
}

async fn index(
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Query(query): Query<EventsQuery>,
) -> Result<Response> {
    render_events(&db, &organizer, &view, query, None).await
}

async fn render_events(
    db: &sea_orm::DatabaseConnection,
    organizer: &crate::organizers::Organizer,
    view: &TeraView,
    query: EventsQuery,
    correction: Option<(&str, &str, &CorrectionForm, &str)>,
) -> Result<Response> {
    let now = Utc::now().timestamp();
    let currency = settings::currency(db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let all = events::list(db).await.map_err(internal_error)?;
    let type_summaries = events::ticket_type_summaries(db)
        .await
        .map_err(internal_error)?;
    let sales_summaries = events::event_sales_summaries(db)
        .await
        .map_err(internal_error)?;
    let series_labels = series::labels(db).await.map_err(series_internal_error)?;
    let mut reviews = series::reviews(db).await.map_err(series_internal_error)?;
    if let Some((id, date, form, _)) = correction {
        if let Some(review) = reviews.iter_mut().find(|review| {
            review.series_id == id && review.date == date && review.event_id.is_none()
        }) {
            review.start_time = form.start_time.clone();
            review.end_time = form.end_time.clone();
        }
    }
    let selected = query.show.unwrap_or_else(|| "all".to_owned());
    let filters = [
        ("all", "All"),
        ("upcoming", "Upcoming"),
        ("drafts", "Drafts"),
        ("past", "Past"),
        ("cancelled", "Cancelled"),
    ]
    .into_iter()
    .map(|(key, label)| {
        json!({
            "key": key,
            "label": label,
            "count": all.iter().filter(|event| matches_filter(event, key, now)).count(),
            "active": selected == key
        })
    })
    .collect::<Vec<_>>();
    let mut rows = Vec::new();
    for event in all
        .iter()
        .filter(|event| matches_filter(event, &selected, now))
    {
        let types = type_summaries.get(&event.id);
        let capacity = event
            .event_capacity
            .unwrap_or_else(|| types.map_or(0, |summary| summary.capacity));
        let sales = sales_summaries.get(&event.id);
        let sold = sales.map_or(0, |summary| summary.sold);
        let revenue_cents = sales.map_or(0, |summary| summary.revenue_cents);
        let percent_sold = if capacity > 0 {
            sold.saturating_mul(100)
                .saturating_div(capacity)
                .clamp(0, 100)
        } else {
            0
        };
        let status_label = display_status(event, now);
        rows.push(json!({
            "id": event.id,
            "title": event.title,
            "date_short": format!("{}, {}", events::formatted_date(event), event.start_time),
            "status_label": status_label,
            "status_class": status_class(status_label),
            "series_label": event.series_id.as_ref().and_then(|id| series_labels.get(id)),
            "capacity": capacity,
            "sold": sold,
            "percent_sold": percent_sold,
            "revenue": if revenue_cents == 0 { format!("{}0", events::currency_symbol(&currency)) } else { events::format_price(revenue_cents, &currency) }
        }));
    }
    let mut response = format::render().view(
        view,
        "office/events.html",
        data!({"organizer_name": organizer.name, "active": "events", "filters": filters, "events": rows,
            "series_reviews": reviews, "correction_error": correction.map(|(_, _, _, message)| message)}),
    )?;
    if correction.is_some() {
        *response.status_mut() = StatusCode::BAD_REQUEST;
    }
    Ok(response)
}

async fn new(
    AdminOrganizer { organizer, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
) -> Result<Response> {
    format::render().view(
        &view,
        "office/event-edit.html",
        data!({
            "organizer_name": organizer.name,
            "active": "events",
            "is_new": true,
            "event": {
                "title": "", "date": "", "start_time": "19:00", "end_time": "21:00",
                "timezone": "Europe/Dublin", "venue": "", "address": "",
                "description": "", "event_capacity": "", "max_per_order": 10,
            "attendee_name_required": false
            },
            "error": null,
            "zone_options": zone_options(None),
            "hosted_edition": Edition::from_environment() == Edition::Hosted,
            "has_unsaved_edits": false
        }),
    )
}

async fn create(
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Form(form): Form<EventForm>,
) -> Result<Response> {
    let input = match EventInput::try_from(form.clone()) {
        Ok(input) => input,
        Err(error) => return render_new_error(&view, &organizer, &form, &error.to_string()),
    };
    match events::create(&db, &input).await {
        Ok(event) => Ok(Redirect::to(&format!("/office/events/{}", event.id)).into_response()),
        Err(EventError::InvalidInput(message)) => {
            render_new_error(&view, &organizer, &form, &message)
        }
        Err(error) => Ok(event_error(error)),
    }
}

fn render_new_error(
    view: &TeraView,
    organizer: &crate::organizers::Organizer,
    form: &EventForm,
    message: &str,
) -> Result<Response> {
    let mut response = format::render().view(
        view,
        "office/event-edit.html",
        data!({
            "organizer_name": organizer.name,
            "active": "events", "is_new": true,
            "event": {
                "title": form.title, "date": form.date, "start_time": form.start_time,
                "end_time": form.end_time, "timezone": form.timezone, "venue": form.venue,
                "address": form.address, "description": form.description,
                "event_capacity": form.event_capacity, "max_per_order": form.max_per_order,
                "attendee_name_required": form.attendee_name_required.is_some()
            },
            "event_error": message,
            "zone_options": zone_options(Some(&form.timezone)),
            "hosted_edition": Edition::from_environment() == Edition::Hosted,
            "has_unsaved_edits": true
        }),
    )?;
    *response.status_mut() = StatusCode::BAD_REQUEST;
    Ok(response)
}

async fn edit(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
    Query(query): Query<RepeatPreviewQuery>,
) -> Result<Response> {
    let form = RepeatForm {
        frequency: query
            .frequency
            .unwrap_or_else(|| RepeatForm::default().frequency),
        end_date: query.end_date,
    };
    render_edit(
        &ctx,
        &registry,
        &organizer,
        &db,
        &view,
        &id,
        EditFeedback {
            repeat_form: Some(&form),
            ..EditFeedback::default()
        },
    )
    .await
}

#[derive(Default)]
struct EditFeedback<'a> {
    error: Option<&'a str>,
    cover_error: Option<&'a str>,
    event_form: Option<&'a EventForm>,
    ticket_form: Option<(&'a str, &'a TicketTypeForm)>,
    seat_form: Option<&'a SeatForm>,
    repeat_form: Option<&'a RepeatForm>,
    embed_form: Option<&'a EmbedOriginsForm>,
    embed_error: Option<&'a str>,
}

async fn render_edit(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    organizer: &crate::organizers::Organizer,
    db: &sea_orm::DatabaseConnection,
    view: &TeraView,
    id: &str,
    feedback: EditFeedback<'_>,
) -> Result<Response> {
    let EditFeedback {
        error,
        cover_error,
        event_form,
        ticket_form,
        seat_form,
        repeat_form,
        embed_form,
        embed_error,
    } = feedback;
    let event = match events::get(db, id).await {
        Ok(Some(event)) => event,
        Ok(None) => return Ok(StatusCode::NOT_FOUND.into_response()),
        Err(error) => return Ok(event_error(error)),
    };
    let ticket_types = match events::list_ticket_types(db, id).await {
        Ok(types) => types,
        Err(error) => return Ok(event_error(error)),
    };
    let questions = questions::list(db, id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let discount_codes = discounts::list(db, id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let currency = settings::currency(db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let seat_list = seats::list(db, id, Utc::now().timestamp())
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let hosted_checklist = if Edition::from_environment() == Edition::Hosted {
        let state = hosted::access_state(&ctx.db, &organizer.id)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        let cipher = registry
            .cipher(organizer)
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        let stripe_tested = settings::get(db, &cipher, "stripe_key_tested_at")
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?
            .is_some_and(|value| !value.is_empty());
        let webhook_tested = settings::get(db, &cipher, "webhook_tested_at")
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?
            .is_some_and(|value| !value.is_empty());
        Some(json!({
            "verified": state.verified,
            "subscription": hosted::subscription_allows_sales(&state, hosted::now()),
            "subscription_status": state.subscription_status,
            "stripe_tested": stripe_tested,
            "webhook_tested": webhook_tested,
            "ready": hosted::publish_allowed(&state, stripe_tested, webhook_tested, hosted::now()).is_ok()
        }))
    } else {
        None
    };
    let mut type_views = ticket_types
        .iter()
        .map(|item| {
            let tz: Tz = event.timezone.parse().unwrap_or(chrono_tz::UTC);
            json!({
                "id": item.id,
                "name": item.name,
                "price": format!("{}.{:02}", item.price_cents / 100, item.price_cents % 100),
                "capacity": item.capacity,
                "sales_open": item.sales_open.and_then(|time| tz.timestamp_opt(time, 0).single()).map(|time| time.format("%Y-%m-%dT%H:%M").to_string()).unwrap_or_default(),
                "sales_close": item.sales_close.and_then(|time| tz.timestamp_opt(time, 0).single()).map(|time| time.format("%Y-%m-%dT%H:%M").to_string()).unwrap_or_default()
            })
        })
        .collect::<Vec<_>>();
    let status_label = display_status(&event, Utc::now().timestamp());
    let series_panel = series::panel(db, &event, Utc::now().timestamp())
        .await
        .map_err(series_internal_error)?;
    let origins = settings::embed_origins(db)
        .await
        .map_err(|error| Error::string(&error.to_string()))?;
    let origin = super::checkout::origin(ctx, organizer);
    let event_url = format!("{origin}/events/{}", event.id);
    let event_snippet = embed_snippet(
        &format!("{origin}/embed/events/{}", event.id),
        &event_url,
        "CrabGate event tickets",
    );
    let series_url = event
        .series_id
        .as_ref()
        .map(|id| format!("{origin}/series/{id}"));
    let series_snippet = event.series_id.as_ref().map(|id| {
        embed_snippet(
            &format!("{origin}/embed/series/{id}"),
            &format!("{origin}/series/{id}"),
            "CrabGate Series dates and tickets",
        )
    });
    let default_repeat = RepeatForm::default();
    let repeat_form = repeat_form.unwrap_or(&default_repeat);
    let preview_result = if event.series_id.is_none() && event.status != "Cancelled" {
        match Frequency::parse(&repeat_form.frequency) {
            Some(frequency) => {
                series::preview_start(
                    db,
                    &event,
                    frequency,
                    Some(&repeat_form.end_date),
                    Utc::now().timestamp(),
                )
                .await
            }
            None => Err(SeriesError::InvalidInput(
                "Choose how often this event repeats.".to_owned(),
            )),
        }
    } else {
        Ok(Vec::new())
    };
    let (repeat_preview, repeat_error) = match preview_result {
        Ok(dates) => (dates, None),
        Err(error) if error.is_system() => return Err(series_internal_error(error)),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    let monthly_position = chrono::NaiveDate::parse_from_str(&event.date, "%Y-%m-%d")
        .map(series::monthly_position)
        .unwrap_or_default();
    let mut event_view = json!({
        "id": event.id,
        "title": event.title,
        "date": event.date,
        "start_time": event.start_time,
        "end_time": event.end_time,
        "timezone": event.timezone,
        "venue": event.venue,
        "address": event.address,
        "description": event.description,
        "event_capacity": event.event_capacity.map(|value| value.to_string()).unwrap_or_default(),
        "max_per_order": event.max_per_order,
        "attendee_name_required": event.attendee_name_required,
        "has_cover": event.has_cover,
        "status": event.status,
        "has_sales": event.first_sale_at.is_some(),
        "starts_future": event.starts_at > Utc::now().timestamp(),
        "confirm_timezone_change": false,
        "start_preview": chrono::DateTime::<Utc>::from_timestamp(event.starts_at, 0).map(|time| time.with_timezone(&event.timezone.parse::<Tz>().unwrap_or(chrono_tz::UTC)).format("%a %-d %b %Y, %H:%M %Z").to_string()).unwrap_or_default(),
        "end_preview": chrono::DateTime::<Utc>::from_timestamp(event.ends_at, 0).map(|time| time.with_timezone(&event.timezone.parse::<Tz>().unwrap_or(chrono_tz::UTC)).format("%a %-d %b %Y, %H:%M %Z").to_string()).unwrap_or_default()
    });
    if let Some(form) = event_form {
        event_view["title"] = json!(form.title);
        event_view["date"] = json!(form.date);
        event_view["start_time"] = json!(form.start_time);
        event_view["end_time"] = json!(form.end_time);
        event_view["timezone"] = json!(form.timezone);
        event_view["venue"] = json!(form.venue);
        event_view["address"] = json!(form.address);
        event_view["description"] = json!(form.description);
        event_view["event_capacity"] = json!(form.event_capacity);
        event_view["max_per_order"] = json!(form.max_per_order);
        event_view["attendee_name_required"] = json!(form.attendee_name_required.is_some());
        event_view["confirm_timezone_change"] = json!(form.confirm_timezone_change.is_some());
    }
    if let Some((target, form)) = ticket_form {
        if target != "new" {
            if let Some(item) = type_views.iter_mut().find(|item| item["id"] == target) {
                item["name"] = json!(form.name);
                item["price"] = json!(form.price);
                item["capacity"] = json!(form.capacity);
                item["sales_open"] = json!(form.sales_open);
                item["sales_close"] = json!(form.sales_close);
            }
        }
    }
    let preview_invalid = repeat_error.is_some();
    let mut response = format::render().view(
        view,
        "office/event-edit.html",
        data!({
            "organizer_name": organizer.name,
            "active": "events",
            "is_new": false,
            "event": event_view,
            "ticket_types": type_views,
            "questions": questions,
            "seats": seat_list,
            "discount_codes": discount_codes.iter().map(|code| json!({
                "id": code.id,
                "code": code.code,
                "kind": code.kind,
                "value": if code.kind == "Percent" { format!("{}%", code.value) } else { events::format_price(code.value, &currency) },
                "used": code.used,
                "limit": code.max_redemptions,
                "active": code.active,
                "starts": code.starts_at.and_then(|time| chrono::DateTime::<Utc>::from_timestamp(time, 0)).map(|time| time.with_timezone(&event.timezone.parse::<Tz>().unwrap_or(chrono_tz::UTC)).format("%Y-%m-%d").to_string()),
                "ends": code.ends_at.and_then(|time| chrono::DateTime::<Utc>::from_timestamp(time - 1, 0)).map(|time| time.with_timezone(&event.timezone.parse::<Tz>().unwrap_or(chrono_tz::UTC)).format("%Y-%m-%d").to_string())
            })).collect::<Vec<_>>(),
            "status_label": status_label,
            "currency": currency,
            "status_class": status_class(status_label),
            "error": error,
            "cover_error": cover_error,
            "event_error": if event_form.is_some() { error } else { None },
            "ticket_error": if ticket_form.is_some() { error } else { None },
            "seat_error": if seat_form.is_some() { error } else { None },
            "seat_form": seat_form,
            "ticket_error_id": ticket_form.map(|(target, _)| target),
            "new_type": ticket_form.filter(|(target, _)| *target == "new").map(|(_, form)| json!(form)),
            "zone_options": zone_options(event_form.map(|form| form.timezone.as_str()).or(Some(&event.timezone))),
            "hosted_edition": Edition::from_environment() == Edition::Hosted,
            "hosted_checklist": hosted_checklist,
            "series": series_panel,
            "embed_origins": embed_form.map(|form| form.origins.clone()).unwrap_or_else(|| origins.as_slice().join("\n")),
            "embed_enabled": !origins.as_slice().is_empty(),
            "embed_error": embed_error,
            "event_embed_snippet": event_snippet,
            "event_hosted_url": event_url,
            "series_embed_snippet": series_snippet,
            "series_hosted_url": series_url,
            "monthly_position": monthly_position,
            "repeat_form": repeat_form,
            "repeat_preview": repeat_preview,
            "repeat_error": repeat_error,
            "has_unsaved_edits": event_form.is_some() || ticket_form.is_some()
        }),
    )?;
    if preview_invalid {
        *response.status_mut() = StatusCode::BAD_REQUEST;
    }
    Ok(response)
}

fn embed_snippet(selector: &str, hosted: &str, title: &str) -> String {
    let escape = |value: &str| {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#39;")
    };
    format!(
        "<iframe src=\"{}\" title=\"{title}\" width=\"100%\" height=\"600\"></iframe>\n<p><a href=\"{}\" target=\"_top\">Choose tickets on CrabGate</a></p>",
        escape(selector),
        escape(hosted)
    )
}

async fn save_embed_origins(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
    Form(form): Form<EmbedOriginsForm>,
) -> Result<Response> {
    match events::get(&db, &id).await {
        Ok(Some(_)) => {}
        Ok(None) => return Ok(StatusCode::NOT_FOUND.into_response()),
        Err(error) => return Ok(event_error(error)),
    }
    let origins = form
        .origins
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    match settings::set_embed_origins(&db, &origins).await {
        Ok(()) => Ok(Redirect::to(&format!("/office/events/{id}#embed")).into_response()),
        Err(settings::SettingsError::InvalidEmbedOrigins) => {
            let mut response = render_edit(
                &ctx,
                &registry,
                &organizer,
                &db,
                &view,
                &id,
                EditFeedback {
                    embed_form: Some(&form),
                    embed_error: Some("Enter up to 16 exact HTTPS origins, one per line. Do not include a trailing slash, path, wildcard, query, fragment or credentials. Saved origins have not changed."),
                    ..EditFeedback::default()
                },
            )
            .await?;
            *response.status_mut() = StatusCode::BAD_REQUEST;
            Ok(response)
        }
        Err(error) => Err(Error::string(&error.to_string())),
    }
}

async fn render_cover_error(
    hosted_render: (&AppContext, &OrganizerRegistry),
    organizer: &crate::organizers::Organizer,
    db: &sea_orm::DatabaseConnection,
    view: &TeraView,
    id: &str,
    status: StatusCode,
    message: &str,
) -> Result<Response> {
    let mut response = render_edit(
        hosted_render.0,
        hosted_render.1,
        organizer,
        db,
        view,
        id,
        EditFeedback {
            cover_error: Some(message),
            ..EditFeedback::default()
        },
    )
    .await?;
    *response.status_mut() = status;
    Ok(response)
}

async fn save(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
    Form(form): Form<EventForm>,
) -> Result<Response> {
    let input = match EventInput::try_from(form.clone()) {
        Ok(input) => input,
        Err(error) => {
            return render_event_error(
                (&ctx, &registry),
                &organizer,
                &db,
                &view,
                &id,
                &form,
                &error.to_string(),
            )
            .await
        }
    };
    if let Some(current) = events::get(&db, &id).await.map_err(internal_error)? {
        if current.timezone != input.timezone && form.confirm_timezone_change.is_none() {
            return render_event_error((&ctx, &registry), &organizer, &db, &view, &id, &form, "Changing the time zone changes when this event starts and ends. Review the times and confirm the change.").await;
        }
    }
    match events::update(&db, &id, &input).await {
        Ok(_) => {
            if let Err(error) =
                event_notices::deliver_pending(&ctx, &registry, &organizer, &db).await
            {
                tracing::error!(%error, event_id = %id, "Event change notices remain queued");
            }
            Ok(Redirect::to(&format!("/office/events/{id}")).into_response())
        }
        Err(EventError::InvalidInput(message)) => {
            render_event_error(
                (&ctx, &registry),
                &organizer,
                &db,
                &view,
                &id,
                &form,
                &message,
            )
            .await
        }
        Err(error) => Ok(event_error(error)),
    }
}

fn zone_options(selected: Option<&str>) -> Vec<String> {
    let mut zones: Vec<String> = chrono_tz::TZ_VARIANTS
        .iter()
        .map(ToString::to_string)
        .collect();
    if let Some(selected) = selected {
        if !zones.iter().any(|zone| zone == selected) {
            zones.insert(0, selected.to_owned());
        }
    }
    zones
}

async fn render_event_error(
    hosted_render: (&AppContext, &OrganizerRegistry),
    organizer: &crate::organizers::Organizer,
    db: &sea_orm::DatabaseConnection,
    view: &TeraView,
    id: &str,
    form: &EventForm,
    message: &str,
) -> Result<Response> {
    let mut response = render_edit(
        hosted_render.0,
        hosted_render.1,
        organizer,
        db,
        view,
        id,
        EditFeedback {
            error: Some(message),
            event_form: Some(form),
            ..EditFeedback::default()
        },
    )
    .await?;
    *response.status_mut() = StatusCode::BAD_REQUEST;
    Ok(response)
}

async fn add_ticket_type(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
    Form(form): Form<TicketTypeForm>,
) -> Result<Response> {
    let event = match events::get(&db, &id).await {
        Ok(Some(event)) => event,
        Ok(None) => return Ok(StatusCode::NOT_FOUND.into_response()),
        Err(error) => return Ok(event_error(error)),
    };
    let input = match TicketTypeInput::try_from(form.clone()) {
        Ok(input) => input,
        Err(error) => {
            return render_ticket_error(
                (&ctx, &registry),
                &organizer,
                &db,
                &view,
                &id,
                ("new", &form),
                &error.to_string(),
            )
            .await
        }
    };
    match events::add_ticket_type(&*db, &event, &input).await {
        Ok(_) => Ok(Redirect::to(&format!("/office/events/{id}")).into_response()),
        Err(EventError::InvalidInput(message)) => {
            render_ticket_error(
                (&ctx, &registry),
                &organizer,
                &db,
                &view,
                &id,
                ("new", &form),
                &message,
            )
            .await
        }
        Err(error) => Ok(event_error(error)),
    }
}

async fn save_ticket_type(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path((id, type_id)): Path<(String, String)>,
    Form(form): Form<TicketTypeForm>,
) -> Result<Response> {
    let event = match events::get(&db, &id).await {
        Ok(Some(event)) => event,
        Ok(None) => return Ok(StatusCode::NOT_FOUND.into_response()),
        Err(error) => return Ok(event_error(error)),
    };
    let input = match TicketTypeInput::try_from(form.clone()) {
        Ok(input) => input,
        Err(error) => {
            return render_ticket_error(
                (&ctx, &registry),
                &organizer,
                &db,
                &view,
                &id,
                (&type_id, &form),
                &error.to_string(),
            )
            .await
        }
    };
    match events::update_ticket_type(&db, &event, &type_id, &input).await {
        Ok(()) => Ok(Redirect::to(&format!("/office/events/{id}")).into_response()),
        Err(EventError::InvalidInput(message)) => {
            render_ticket_error(
                (&ctx, &registry),
                &organizer,
                &db,
                &view,
                &id,
                (&type_id, &form),
                &message,
            )
            .await
        }
        Err(error) => Ok(event_error(error)),
    }
}

async fn add_question(
    AdminOrganizer { db, .. }: AdminOrganizer,
    Path(id): Path<String>,
    Form(form): Form<QuestionForm>,
) -> Result<Response> {
    let options = form
        .options
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    match questions::add(
        &db,
        &id,
        &form.scope,
        &form.kind,
        &form.label,
        &options,
        form.required.is_some(),
    )
    .await
    {
        Ok(_) => Ok(Redirect::to(&format!("/office/events/{id}#questions")).into_response()),
        Err(questions::QuestionError::Database(error)) => {
            Err(loco_rs::Error::string(&error.to_string()))
        }
        Err(error) => Ok((StatusCode::BAD_REQUEST, error.to_string()).into_response()),
    }
}

async fn add_seats(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
    Form(form): Form<SeatForm>,
) -> Result<Response> {
    let event = events::get(&db, &id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let Some(event) = event else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let labels = form
        .labels
        .lines()
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let result = if event.status == "Cancelled" || event.starts_at <= seats_now() {
        Err(seats::SeatError::AlreadySelling)
    } else {
        seats::add_many(
            &db,
            &id,
            &form.ticket_type_id,
            &form.section,
            &form.row,
            &labels,
            seats_now(),
        )
        .await
    };
    match result {
        Ok(()) => Ok(Redirect::to(&format!("/office/events/{id}#seats")).into_response()),
        Err(seats::SeatError::Database(error)) => Err(loco_rs::Error::string(&error.to_string())),
        Err(error) => {
            let message = error.to_string();
            let mut response = render_edit(
                &ctx,
                &registry,
                &organizer,
                &db,
                &view,
                &id,
                EditFeedback {
                    error: Some(&message),
                    seat_form: Some(&form),
                    ..EditFeedback::default()
                },
            )
            .await?;
            *response.status_mut() = StatusCode::BAD_REQUEST;
            Ok(response)
        }
    }
}

fn seats_now() -> i64 {
    Utc::now().timestamp()
}

async fn deactivate_seat(
    AdminOrganizer { db, .. }: AdminOrganizer,
    Path((id, seat_id)): Path<(String, String)>,
) -> Result<Response> {
    match seats::deactivate(&db, &id, &seat_id, seats_now()).await {
        Ok(()) => Ok(Redirect::to(&format!("/office/events/{id}#seats")).into_response()),
        Err(seats::SeatError::Database(error)) => Err(loco_rs::Error::string(&error.to_string())),
        Err(error) => Ok((StatusCode::BAD_REQUEST, error.to_string()).into_response()),
    }
}

async fn delete_question(
    AdminOrganizer { db, .. }: AdminOrganizer,
    Path((id, question_id)): Path<(String, String)>,
) -> Result<Response> {
    questions::delete(&db, &id, &question_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    Ok(Redirect::to(&format!("/office/events/{id}#questions")).into_response())
}

fn discount_date(
    value: &str,
    timezone: Tz,
    inclusive_end: bool,
) -> std::result::Result<Option<i64>, String> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| "Enter a valid discount date".to_owned())?;
    let date = if inclusive_end {
        date.succ_opt().ok_or("Enter a valid discount end date")?
    } else {
        date
    };
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .ok_or("Enter a valid discount date")?;
    timezone
        .from_local_datetime(&midnight)
        .single()
        .map(|time| Some(time.timestamp()))
        .ok_or_else(|| {
            "This date starts during a time-zone change. Choose another date.".to_owned()
        })
}

async fn add_discount(
    AdminOrganizer { db, .. }: AdminOrganizer,
    Path(id): Path<String>,
    Form(form): Form<DiscountForm>,
) -> Result<Response> {
    let Some(event) = events::get(&db, &id).await.map_err(internal_error)? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let timezone: Tz = event
        .timezone
        .parse()
        .map_err(|_| loco_rs::Error::string("Invalid event time zone"))?;
    let starts = match discount_date(&form.starts, timezone, false) {
        Ok(value) => value,
        Err(message) => return Ok((StatusCode::BAD_REQUEST, message).into_response()),
    };
    let ends = match discount_date(&form.ends, timezone, true) {
        Ok(value) => value,
        Err(message) => return Ok((StatusCode::BAD_REQUEST, message).into_response()),
    };
    let value = if form.kind == "Fixed" {
        match events::parse_price(&form.value) {
            Ok(value) => value,
            Err(_) => {
                return Ok((StatusCode::BAD_REQUEST, "Enter a valid fixed amount").into_response())
            }
        }
    } else {
        match form.value.parse::<i64>() {
            Ok(value) => value,
            Err(_) => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    "Enter a whole percent from 1 to 100",
                )
                    .into_response())
            }
        }
    };
    let limit = if form.max_redemptions.trim().is_empty() {
        None
    } else {
        match form.max_redemptions.parse::<i64>() {
            Ok(value) => Some(value),
            Err(_) => {
                return Ok(
                    (StatusCode::BAD_REQUEST, "Enter a valid redemption limit").into_response()
                )
            }
        }
    };
    match discounts::create(
        &db,
        discounts::NewDiscount {
            event_id: &id,
            code: &form.code,
            kind: &form.kind,
            value,
            starts_at: starts,
            ends_at: ends,
            max_redemptions: limit,
        },
        Utc::now().timestamp(),
    )
    .await
    {
        Ok(_) => Ok(Redirect::to(&format!("/office/events/{id}#discounts")).into_response()),
        Err(discounts::DiscountError::Database(error)) => {
            Err(loco_rs::Error::string(&error.to_string()))
        }
        Err(error) => Ok((StatusCode::BAD_REQUEST, error.to_string()).into_response()),
    }
}

async fn deactivate_discount(
    AdminOrganizer { db, .. }: AdminOrganizer,
    Path((id, code_id)): Path<(String, String)>,
) -> Result<Response> {
    discounts::deactivate(&db, &id, &code_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    Ok(Redirect::to(&format!("/office/events/{id}#discounts")).into_response())
}

async fn render_ticket_error(
    hosted_render: (&AppContext, &OrganizerRegistry),
    organizer: &crate::organizers::Organizer,
    db: &sea_orm::DatabaseConnection,
    view: &TeraView,
    id: &str,
    ticket: (&str, &TicketTypeForm),
    message: &str,
) -> Result<Response> {
    let mut response = render_edit(
        hosted_render.0,
        hosted_render.1,
        organizer,
        db,
        view,
        id,
        EditFeedback {
            error: Some(message),
            ticket_form: Some(ticket),
            ..EditFeedback::default()
        },
    )
    .await?;
    *response.status_mut() = StatusCode::BAD_REQUEST;
    Ok(response)
}

async fn publish(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    Path(id): Path<String>,
) -> Result<Response> {
    let now = Utc::now().timestamp();
    match hosted::publish_check(&ctx.db, &registry, &organizer, &db, now).await {
        Ok(()) => {}
        Err(
            error @ (hosted::HostedError::VerificationRequired
            | hosted::HostedError::SubscriptionRequired
            | hosted::HostedError::SetupRequired),
        ) => return Ok((StatusCode::FORBIDDEN, error.to_string()).into_response()),
        Err(error) => return Err(loco_rs::Error::string(&error.to_string())),
    }
    match events::publish(&db, &id, now).await {
        Ok(()) => Ok(Redirect::to(&format!("/office/events/{id}")).into_response()),
        Err(error) => Ok(event_error(error)),
    }
}

#[derive(Deserialize)]
struct CorrectionForm {
    start_time: String,
    end_time: String,
}

async fn correct_series_date(
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path((id, date)): Path<(String, String)>,
    Form(form): Form<CorrectionForm>,
) -> Result<Response> {
    match series::correct_exception(
        &db,
        &id,
        &date,
        &form.start_time,
        &form.end_time,
        Utc::now().timestamp(),
    )
    .await
    {
        Ok(event) => Ok(Redirect::to(&format!("/office/events/{}", event.id)).into_response()),
        Err(SeriesError::Event(EventError::NotFound)) => Ok(StatusCode::NOT_FOUND.into_response()),
        Err(error) if error.is_system() => Err(series_internal_error(error)),
        Err(error) => {
            render_events(
                &db,
                &organizer,
                &view,
                EventsQuery { show: None },
                Some((&id, &date, &form, &error.to_string())),
            )
            .await
        }
    }
}

async fn copy(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
    Form(form): Form<CopyForm>,
) -> Result<Response> {
    match series::copy(&db, &id, &form.date, Utc::now().timestamp()).await {
        Ok(event) => Ok(Redirect::to(&format!("/office/events/{}", event.id)).into_response()),
        Err(error) => {
            render_series_error(
                (&ctx, &registry),
                &organizer,
                &db,
                &view,
                &id,
                "We couldn't copy this event.",
                error,
            )
            .await
        }
    }
}

async fn repeat(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
    Form(form): Form<RepeatForm>,
) -> Result<Response> {
    let now = Utc::now().timestamp();
    let result = match Frequency::parse(&form.frequency) {
        Some(frequency) => {
            let check = hosted::publish_check(&ctx.db, &registry, &organizer, &db, now)
                .await
                .map_err(|error| error.to_string());
            series::start(&db, &id, frequency, Some(&form.end_date), &check, now).await
        }
        None => Err(SeriesError::InvalidInput(
            "Choose how often this event repeats.".to_owned(),
        )),
    };
    match result {
        Ok(_) => Ok(Redirect::to(&format!("/office/events/{id}#series")).into_response()),
        Err(error) => {
            if error.is_system() {
                return Err(series_internal_error(error));
            }
            if matches!(error, SeriesError::Event(EventError::NotFound)) {
                return Ok(StatusCode::NOT_FOUND.into_response());
            }
            let message = format!("We couldn't start the Series. {error}");
            let mut response = render_edit(
                &ctx,
                &registry,
                &organizer,
                &db,
                &view,
                &id,
                EditFeedback {
                    error: Some(&message),
                    repeat_form: Some(&form),
                    ..EditFeedback::default()
                },
            )
            .await?;
            *response.status_mut() = StatusCode::BAD_REQUEST;
            Ok(response)
        }
    }
}

async fn use_series_setup(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
) -> Result<Response> {
    match series::use_setup_from(&db, &id, Utc::now().timestamp()).await {
        Ok(()) => Ok(Redirect::to(&format!("/office/events/{id}#series")).into_response()),
        Err(error) => {
            render_series_error(
                (&ctx, &registry),
                &organizer,
                &db,
                &view,
                &id,
                "We couldn't update the Series.",
                error,
            )
            .await
        }
    }
}

async fn stop_series(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
) -> Result<Response> {
    match series::stop(&db, &id, Utc::now().timestamp()).await {
        Ok(()) => Ok(Redirect::to(&format!("/office/events/{id}#series")).into_response()),
        Err(error) => {
            render_series_error(
                (&ctx, &registry),
                &organizer,
                &db,
                &view,
                &id,
                "We couldn't stop the Series.",
                error,
            )
            .await
        }
    }
}

async fn render_series_error(
    hosted_render: (&AppContext, &OrganizerRegistry),
    organizer: &crate::organizers::Organizer,
    db: &sea_orm::DatabaseConnection,
    view: &TeraView,
    id: &str,
    summary: &str,
    error: SeriesError,
) -> Result<Response> {
    if matches!(error, SeriesError::Event(EventError::NotFound)) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    if error.is_system() {
        tracing::error!(%error, event_id = %id, "Series action failed");
        return Err(series_internal_error(error));
    }
    let message = format!("{summary} {error}");
    let mut response = render_edit(
        hosted_render.0,
        hosted_render.1,
        organizer,
        db,
        view,
        id,
        EditFeedback {
            error: Some(&message),
            ..EditFeedback::default()
        },
    )
    .await?;
    *response.status_mut() = StatusCode::BAD_REQUEST;
    Ok(response)
}

fn series_internal_error(error: SeriesError) -> loco_rs::Error {
    loco_rs::Error::string(&error.to_string())
}

async fn draft(
    AdminOrganizer { db, .. }: AdminOrganizer,
    Path(id): Path<String>,
) -> Result<Response> {
    match events::return_to_draft(&db, &id).await {
        Ok(()) => Ok(Redirect::to(&format!("/office/events/{id}")).into_response()),
        Err(error) => Ok(event_error(error)),
    }
}

async fn upload_cover(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
    multipart: std::result::Result<Multipart, MultipartRejection>,
) -> Result<Response> {
    let mut multipart = match multipart {
        Ok(multipart) => multipart,
        Err(_) => {
            return render_cover_error(
                (&ctx, &registry),
                &organizer,
                &db,
                &view,
                &id,
                StatusCode::BAD_REQUEST,
                "Unreadable upload. Choose the image again.",
            )
            .await
        }
    };
    let field = match multipart.next_field().await {
        Ok(Some(field)) if field.name() == Some("cover") => field,
        _ => {
            return render_cover_error(
                (&ctx, &registry),
                &organizer,
                &db,
                &view,
                &id,
                StatusCode::BAD_REQUEST,
                "Choose an image to upload.",
            )
            .await
        }
    };
    let mime = field.content_type().unwrap_or_default().to_owned();
    let bytes = match field.bytes().await {
        Ok(bytes) => bytes.to_vec(),
        Err(error) => {
            let message = if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                "Image is over 3 MB. Choose a smaller image."
            } else {
                "Unreadable upload. Choose the image again."
            };
            return render_cover_error(
                (&ctx, &registry),
                &organizer,
                &db,
                &view,
                &id,
                error.status(),
                message,
            )
            .await;
        }
    };
    if !matches!(multipart.next_field().await, Ok(None)) {
        return render_cover_error(
            (&ctx, &registry),
            &organizer,
            &db,
            &view,
            &id,
            StatusCode::BAD_REQUEST,
            "Unreadable upload. Choose one image and try again.",
        )
        .await;
    }
    match events::save_cover(&db, &id, &mime, bytes).await {
        Ok(()) => Ok(Redirect::to(&format!("/office/events/{id}")).into_response()),
        Err(EventError::InvalidInput(message)) => {
            render_cover_error(
                (&ctx, &registry),
                &organizer,
                &db,
                &view,
                &id,
                StatusCode::BAD_REQUEST,
                &message,
            )
            .await
        }
        Err(error) => Ok(event_error(error)),
    }
}

async fn office_cover(
    AdminOrganizer { db, .. }: AdminOrganizer,
    Path(id): Path<String>,
) -> Result<Response> {
    cover_response(&db, &id).await
}

pub async fn cover_response(db: &sea_orm::DatabaseConnection, id: &str) -> Result<Response> {
    match events::cover(db, id).await {
        Ok(Some((mime, bytes))) => {
            let mut headers = HeaderMap::new();
            let mime = HeaderValue::from_str(&mime)
                .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
            headers.insert(header::CONTENT_TYPE, mime);
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=300"),
            );
            Ok((headers, bytes).into_response())
        }
        Ok(None) => Ok(StatusCode::NOT_FOUND.into_response()),
        Err(error) => Ok(event_error(error)),
    }
}

pub fn routes() -> Routes {
    Routes::new()
        .prefix("office")
        .add("/events", get(index).post(create))
        .add("/events/new", get(new))
        .add(
            "/series/{id}/dates/{date}/correct",
            post(correct_series_date),
        )
        .add("/events/{id}", get(edit).post(save))
        .add("/events/{id}/embed-origins", post(save_embed_origins))
        .add("/events/{id}/ticket-types", post(add_ticket_type))
        .add("/events/{id}/questions", post(add_question))
        .add("/events/{id}/seats", post(add_seats))
        .add(
            "/events/{id}/seats/{seat_id}/deactivate",
            post(deactivate_seat),
        )
        .add("/events/{id}/discounts", post(add_discount))
        .add(
            "/events/{id}/discounts/{code_id}/deactivate",
            post(deactivate_discount),
        )
        .add(
            "/events/{id}/questions/{question_id}/delete",
            post(delete_question),
        )
        .add(
            "/events/{id}/ticket-types/{type_id}",
            post(save_ticket_type),
        )
        .add("/events/{id}/publish", post(publish))
        .add("/events/{id}/draft", post(draft))
        .add("/events/{id}/copy", post(copy))
        .add("/events/{id}/repeat", post(repeat))
        .add("/events/{id}/series/setup", post(use_series_setup))
        .add("/events/{id}/series/stop", post(stop_series))
        .add(
            "/events/{id}/cover",
            get(office_cover)
                .post(upload_cover)
                .layer(DefaultBodyLimit::max(3_500_000)),
        )
}

fn matches_filter(event: &Event, filter: &str, now: i64) -> bool {
    match filter {
        "upcoming" => event.status == "Published" && event.starts_at > now,
        "drafts" => event.status == "Draft",
        "past" => event.status == "Published" && event.ends_at <= now,
        "cancelled" => event.status == "Cancelled",
        _ => true,
    }
}

fn display_status(event: &Event, now: i64) -> &'static str {
    if event.status == "Published" && event.ends_at <= now {
        "Past"
    } else if event.status == "Published" {
        "Published"
    } else if event.status == "Cancelled" {
        "Cancelled"
    } else {
        "Draft"
    }
}

fn status_class(status: &str) -> &'static str {
    match status {
        "Published" => "status-yellow",
        "Cancelled" => "status-struck",
        "Past" => "status-muted",
        _ => "status-draft",
    }
}

fn event_error(error: EventError) -> Response {
    match error {
        EventError::NotFound => StatusCode::NOT_FOUND.into_response(),
        EventError::InvalidInput(message) | EventError::Rule(message) => {
            (StatusCode::BAD_REQUEST, message).into_response()
        }
        EventError::Database(error) => {
            tracing::error!(%error, "Event database error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn internal_error(error: EventError) -> loco_rs::Error {
    loco_rs::Error::string(&error.to_string())
}
