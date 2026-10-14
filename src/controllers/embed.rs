use std::collections::HashMap;

use axum::{
    extract::{Path, RawQuery, State},
    http::{header, HeaderValue, StatusCode},
    response::Response,
};
use loco_rs::prelude::*;
use serde_json::json;

use crate::{
    events::{self, TicketBoxState},
    hosted, inventory,
    organizers::{Edition, OrganizerDb},
    seats,
    security::SelectorFraming,
    series, settings,
};

pub async fn selector(
    State(ctx): State<AppContext>,
    tenant: OrganizerDb,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response> {
    selector_page(ctx, tenant, view, id, query, false).await
}

pub async fn handoff(
    State(ctx): State<AppContext>,
    tenant: OrganizerDb,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response> {
    render(ctx, tenant, view, id, query, true, None).await
}

pub async fn series_selector(
    State(ctx): State<AppContext>,
    tenant: OrganizerDb,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response> {
    selector_page(ctx, tenant, view, id, query, true).await
}

async fn selector_page(
    ctx: AppContext,
    tenant: OrganizerDb,
    view: TeraView,
    id: String,
    query: Option<String>,
    is_series: bool,
) -> Result<Response> {
    let origins = settings::embed_origins(&tenant.db)
        .await
        .map_err(|error| Error::string(&error.to_string()))?;
    let organizer_name = tenant.organizer.name.clone();
    let public_url = format!(
        "{}/{}/{id}",
        super::checkout::origin(&ctx, &tenant.organizer),
        if is_series { "series" } else { "events" }
    );
    let result = if is_series {
        series_page(ctx, tenant, view.clone(), id, query).await
    } else {
        render(ctx, tenant, view.clone(), id, query, false, None).await
    };
    let mut response = match result {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(%error, "Embedded selector failed");
            let mut response = format::render().view(
                &view,
                "shop/selector.html",
                data!({
                    "organizer_name":organizer_name,"contact":{"imprint_url":"","privacy_url":""},"event":null,
                    "event_url":if is_series { None } else { Some(&public_url) },
                    "handoff":false,"ticket_types":[],
                    "series":is_series.then(|| json!({"url":public_url,"dates":[]})),
                    "selection_url":null,"notice":"Tickets are unavailable right now. Open the page on CrabGate or contact the organizer.",
                    "can_continue":false,"total":""
                }),
            )?;
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            response
        }
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .extensions_mut()
        .insert(SelectorFraming::new(origins));
    Ok(response)
}

async fn series_page(
    ctx: AppContext,
    tenant: OrganizerDb,
    view: TeraView,
    id: String,
    query: Option<String>,
) -> Result<Response> {
    let now = inventory::now();
    let mut status = StatusCode::OK;
    let mut notice = String::new();
    let selected = series_selection(query.as_deref().unwrap_or_default());
    let dates = if uuid::Uuid::parse_str(&id).is_ok_and(|uuid| uuid.to_string() == id) {
        series::published_dates(&tenant.db, &id, now)
            .await
            .map_err(|error| Error::string(&error.to_string()))?
    } else {
        None
    };
    let mut options = Vec::new();
    let mut event_id = String::new();
    if let Some(dates) = dates {
        if Edition::from_environment() == Edition::Hosted {
            let access = hosted::access_state(&ctx.db, &tenant.organizer.id)
                .await
                .map_err(|error| Error::string(&error.to_string()))?;
            if !hosted::subscription_allows_sales(&access, now) {
                status = StatusCode::CONFLICT;
                notice = "Tickets are unavailable for this Series. Contact the organizer.".into();
            }
        }
        if status == StatusCode::OK {
            for event in dates {
                let types = events::list_ticket_types(&tenant.db, &event.id)
                    .await
                    .map_err(|error| Error::string(&error.to_string()))?;
                let (sold, held) = inventory::open_counts(&tenant.db, &event.id, now).await?;
                let mut states = Vec::with_capacity(types.len());
                for kind in types {
                    let (type_sold, type_held) =
                        inventory::type_counts(&tenant.db, &kind.id, now).await?;
                    states.push(events::ticket_box_state(
                        &event, &kind, type_sold, type_held, sold, held, now,
                    ));
                }
                let state = if states.contains(&TicketBoxState::OnSale) {
                    "Tickets available"
                } else if states.contains(&TicketBoxState::ComingSoon) {
                    "Sales have not opened yet"
                } else if states.contains(&TicketBoxState::SoldOut) {
                    "Sold out"
                } else {
                    "Sales have ended"
                };
                let chosen =
                    selected.as_ref().and_then(|value| value.as_deref()) == Some(event.id.as_str());
                if chosen {
                    event_id.clone_from(&event.id);
                }
                options.push(json!({
                    "id":event.id,"label":format!("{} · {} · {} · {} · {}",
                        events::formatted_date(&event),event.start_time,event.timezone,event.venue,state),
                    "selected":chosen
                }));
            }
            if selected.is_none()
                || selected.as_ref().is_some_and(|value| value.is_some()) && event_id.is_empty()
            {
                status = StatusCode::BAD_REQUEST;
                notice = "Invalid date selection. Choose a published date from this Series.".into();
            } else if options.is_empty() {
                notice = "No published future dates are available for this Series. Contact the organizer.".into();
            } else if event_id.is_empty() {
                notice = "Choose one date, then choose tickets for that date.".into();
            }
        }
    } else {
        status = StatusCode::NOT_FOUND;
        notice = "Series unavailable. Check the Series link with the organizer.".into();
    }
    let url = format!(
        "{}/series/{id}",
        super::checkout::origin(&ctx, &tenant.organizer)
    );
    render(
        ctx,
        tenant,
        view,
        event_id,
        None,
        false,
        Some(SeriesPicker {
            id,
            url,
            options,
            status,
            notice,
        }),
    )
    .await
}

struct SeriesPicker {
    id: String,
    url: String,
    options: Vec<serde_json::Value>,
    status: StatusCode,
    notice: String,
}

fn series_selection(query: &str) -> Option<Option<String>> {
    if query.is_empty() {
        return Some(None);
    }
    if query.len() > 128 {
        return None;
    }
    let mut url = reqwest::Url::parse("https://selection.invalid").ok()?;
    url.set_query(Some(query));
    let mut pairs = url.query_pairs();
    let (key, value) = pairs.next()?;
    if key != "event_id"
        || pairs.next().is_some()
        || uuid::Uuid::parse_str(&value).ok()?.to_string() != value
    {
        return None;
    }
    Some(Some(value.into_owned()))
}

fn selections(query: &str) -> Option<HashMap<String, i64>> {
    if query.len() > 4096 {
        return None;
    }
    let mut url = reqwest::Url::parse("https://selection.invalid").ok()?;
    url.set_query(Some(query));
    let mut selected = HashMap::new();
    for (key, value) in url.query_pairs() {
        if selected.len() >= 32 {
            return None;
        }
        let id = key.strip_prefix("quantity_")?;
        if uuid::Uuid::parse_str(id).ok()?.to_string() != id
            || value.is_empty()
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || value.len() > 3
        {
            return None;
        }
        let quantity = value.parse::<i64>().ok()?;
        if quantity > 100 || selected.insert(id.to_owned(), quantity).is_some() {
            return None;
        }
    }
    Some(selected)
}

async fn render(
    ctx: AppContext,
    OrganizerDb { organizer, db }: OrganizerDb,
    view: TeraView,
    id: String,
    query: Option<String>,
    handoff: bool,
    picker: Option<SeriesPicker>,
) -> Result<Response> {
    let canonical_id = uuid::Uuid::parse_str(&id).is_ok_and(|uuid| uuid.to_string() == id);
    let event = events::get(&db, &id)
        .await
        .map_err(|error| Error::string(&error.to_string()))?;
    let event = event.filter(|event| {
        canonical_id
            && event.status != "Draft"
            && picker.as_ref().is_none_or(|picker| {
                event.series_id.as_deref() == Some(picker.id.as_str())
                    && event.status == "Published"
                    && event.starts_at > inventory::now()
            })
    });
    let event_url = event.as_ref().map(|event| {
        format!(
            "{}/events/{}",
            super::checkout::origin(&ctx, &organizer),
            event.id
        )
    });
    let contact = settings::contact(&db)
        .await
        .map_err(|error| Error::string(&error.to_string()))?;
    let currency = settings::currency(&db)
        .await
        .map_err(|error| Error::string(&error.to_string()))?;
    let mut status = picker
        .as_ref()
        .map_or(StatusCode::OK, |picker| picker.status);
    let mut notice = picker
        .as_ref()
        .map_or_else(String::new, |picker| picker.notice.clone());
    let mut rows = Vec::new();
    let mut total = 0_i64;
    let mut purchase_available = false;
    let selected = if handoff {
        selections(query.as_deref().unwrap_or_default())
    } else {
        Some(HashMap::new())
    };
    if !handoff && query.is_some() || selected.is_none() {
        status = StatusCode::BAD_REQUEST;
        notice = "Invalid ticket selection. Open the event and choose tickets again.".into();
    }
    if let Some(event) = &event {
        let now = inventory::now();
        let subscription_active = if Edition::from_environment() == Edition::Hosted {
            let state = hosted::access_state(&ctx.db, &organizer.id)
                .await
                .map_err(|error| Error::string(&error.to_string()))?;
            hosted::subscription_allows_sales(&state, now)
        } else {
            true
        };
        let assigned = seats::assigned(&db, &id).await?;
        if status == StatusCode::OK
            && (event.status != "Published" || now >= event.starts_at || !subscription_active)
        {
            status = StatusCode::CONFLICT;
            notice = "Tickets are unavailable for this event. Open the event for details.".into();
        }
        if status == StatusCode::OK && assigned {
            notice = "This event uses assigned seats. Open the event to choose your seats.".into();
            if handoff {
                status = StatusCode::CONFLICT;
            }
        }
        if status == StatusCode::OK && !assigned {
            let types = events::list_ticket_types(&db, &id)
                .await
                .map_err(|error| Error::string(&error.to_string()))?;
            let (sold, held) = inventory::open_counts(&db, &id, now).await?;
            let mut selected = selected.unwrap_or_default();
            let mut quantity_total = 0_i64;
            for kind in types {
                let quantity = selected.remove(&kind.id).unwrap_or(0);
                let (type_sold, type_held) = inventory::type_counts(&db, &kind.id, now).await?;
                let state =
                    events::ticket_box_state(event, &kind, type_sold, type_held, sold, held, now);
                let remaining = kind
                    .capacity
                    .saturating_sub(type_sold + type_held)
                    .min(
                        event
                            .event_capacity
                            .map(|capacity| capacity.saturating_sub(sold + held))
                            .unwrap_or(i64::MAX),
                    )
                    .max(0);
                let max = remaining.min(event.max_per_order);
                if quantity > 0 && (state != TicketBoxState::OnSale || quantity > max) {
                    status = StatusCode::BAD_REQUEST;
                    notice = "Selected tickets are unavailable or exceed the order limit. Choose tickets again.".into();
                }
                quantity_total += quantity;
                total = match kind
                    .price_cents
                    .checked_mul(quantity)
                    .and_then(|amount| total.checked_add(amount))
                {
                    Some(total) => total,
                    None => {
                        status = StatusCode::BAD_REQUEST;
                        0
                    }
                };
                purchase_available |= state == TicketBoxState::OnSale;
                if !handoff || quantity > 0 {
                    rows.push(json!({"id":kind.id,"name":kind.name,"price":events::format_price(kind.price_cents,&currency),"state":state.as_str(),"max":max,"quantity":quantity}));
                }
            }
            if handoff
                && (!selected.is_empty()
                    || quantity_total == 0
                    || quantity_total > event.max_per_order
                    || event
                        .event_capacity
                        .is_some_and(|capacity| sold + held + quantity_total > capacity))
            {
                status = StatusCode::BAD_REQUEST;
                notice = "Invalid ticket selection or order limit exceeded. Choose tickets again."
                    .into();
            }
            if !purchase_available && notice.is_empty() {
                notice = "No tickets are on sale right now. Sold-out and sale-window states are shown below.".into();
            }
        }
    } else if picker.is_none() || !id.is_empty() {
        status = StatusCode::NOT_FOUND;
        notice = "Event unavailable. Check the event link with the organizer.".into();
    }
    if !handoff && rows.len() > 32 && status == StatusCode::OK {
        purchase_available = false;
        notice = "Open the event on CrabGate to choose from all ticket types.".into();
    }
    let mut response = format::render().view(
        &view,
        "shop/selector.html",
        data!({
            "organizer_name":organizer.name,"contact":contact,"event":event.as_ref().map(|event| json!({"id":event.id,"title":event.title,"date":events::formatted_date(event),"start_time":event.start_time,"venue":event.venue,"timezone":event.timezone})),
            "event_url":event_url,"handoff":handoff,"ticket_types":rows,
            "series":picker.as_ref().map(|picker| json!({"url":picker.url,"dates":picker.options})),
            "selection_url":event_url.as_ref().map(|url|format!("{url}/selection")),
            "notice":notice,"can_continue":status == StatusCode::OK && purchase_available,
            "total":events::format_price(total,&currency)
        }),
    )?;
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}
