use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use chrono::{NaiveDate, SecondsFormat, TimeZone, Utc};
use chrono_tz::Tz;
use loco_rs::prelude::*;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Value};

use crate::{
    controllers::events::cover_response,
    events::{self, Event, TicketBoxState},
    inventory,
    organizers::{self, Edition, OrganizerDb, OrganizerError, OrganizerRegistry},
};

async fn index(
    State(ctx): State<AppContext>,
    headers: HeaderMap,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    ViewEngine(view): ViewEngine<TeraView>,
) -> Result<Response> {
    if organizers::self_hosted_signup_required(&ctx.db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
    {
        return Ok(axum::response::Redirect::to("/signup").into_response());
    }
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if Edition::from_environment() == Edition::Hosted {
        let base = std::env::var("SHOP_BASE_DOMAIN").unwrap_or_else(|_| "crabgate.com".to_owned());
        if host.split(':').next() == Some(base.as_str()) {
            return format::render().view(
                &view,
                "marketing/index.html",
                crate::controllers::hosted::marketing_data("/"),
            );
        }
    }
    let OrganizerDb { organizer, db } = match registry.resolve(host).await {
        Ok(value) => value,
        Err(OrganizerError::NotFound | OrganizerError::InvalidSubdomain) => {
            return Ok(crate::app::not_found())
        }
        Err(OrganizerError::Suspended) => {
            return Ok(StatusCode::SERVICE_UNAVAILABLE.into_response())
        }
        Err(error) => return Err(loco_rs::Error::string(&error.to_string())),
    };
    shop_index(OrganizerDb { organizer, db }, ViewEngine(view)).await
}

async fn shop_index(
    OrganizerDb { organizer, db }: OrganizerDb,
    ViewEngine(view): ViewEngine<TeraView>,
) -> Result<Response> {
    let now = Utc::now().timestamp();
    let currency = crate::settings::currency(&db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let contact = crate::settings::contact(&db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let all = events::list(&db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let type_summaries = events::ticket_type_summaries(&db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let mut groups: Vec<Value> = Vec::new();
    let mut current_month = String::new();
    let mut group_events = Vec::new();
    for event in all
        .into_iter()
        .filter(|event| event.status == "Published" && event.ends_at > now)
    {
        let Some(date) = NaiveDate::parse_from_str(&event.date, "%Y-%m-%d").ok() else {
            continue;
        };
        let month = date.format("%B %Y").to_string();
        if !current_month.is_empty() && current_month != month {
            groups.push(json!({"month": current_month, "count": group_events.len(), "events": group_events}));
            group_events = Vec::new();
        }
        current_month = month;
        let summary = type_summaries.get(&event.id);
        let min_price = summary.and_then(|summary| summary.min_price_cents);
        let price = min_price.map_or_else(
            || "Tickets not on sale yet".to_owned(),
            |price| {
                if summary.is_some_and(|summary| summary.count > 1) && price > 0 {
                    format!("From {}", events::format_price(price, &currency))
                } else {
                    events::format_price(price, &currency)
                }
            },
        );
        group_events.push(json!({
            "id": event.id,
            "title": event.title,
            "venue": event.venue,
            "has_cover": event.has_cover,
            "when": format!("{}, {} to {}", date.format("%a %-d %b"), event.start_time, event.end_time),
            "day": date.format("%-d").to_string(),
            "weekday": date.format("%a").to_string(),
            "month_short": date.format("%b").to_string(),
            "tab": date.format("%d/%m").to_string(),
            "price": price
        }));
    }
    if !current_month.is_empty() {
        groups.push(
            json!({"month": current_month, "count": group_events.len(), "events": group_events}),
        );
    }
    format::render().view(
        &view,
        "shop/index.html",
        data!({"organizer_name": organizer.name, "groups": groups, "contact": contact}),
    )
}

async fn event(
    State(ctx): State<AppContext>,
    OrganizerDb { organizer, db }: OrganizerDb,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
) -> Result<Response> {
    if let Some(html) = registry.event_page(&organizer.id, &id).await {
        return Ok(event_page_response(html));
    }
    let event = events::get(&db, &id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let Some(event) = event.filter(|event| event.status != "Draft") else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let now = Utc::now().timestamp();
    let currency = crate::settings::currency(&db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let contact = crate::settings::contact(&db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let ticket_types = events::list_ticket_types(&db, &event.id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let seats = crate::seats::list(&db, &event.id, now)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let assigned_seating = !seats.is_empty();
    let (event_sold, event_holds) = inventory::open_counts(&db, &event.id, now)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let tz: Tz = event
        .timezone
        .parse()
        .map_err(|_| loco_rs::Error::string("Invalid event time zone"))?;
    let origin = crate::controllers::checkout::origin(&ctx, &organizer);
    let event_url = format!("{origin}/events/{}", event.id);
    let mut ticket_views = Vec::new();
    let mut offers = Vec::new();
    for item in &ticket_types {
        let (type_sold, type_holds) = inventory::type_counts(&db, &item.id, now)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        let mut state = events::ticket_box_state(
            &event,
            item,
            type_sold,
            type_holds,
            event_sold,
            event_holds,
            now,
        );
        let remaining = item
            .capacity
            .saturating_sub(type_sold + type_holds)
            .min(
                event
                    .event_capacity
                    .map(|cap| cap.saturating_sub(event_sold + event_holds))
                    .unwrap_or(i64::MAX),
            )
            .max(0);
        let type_seats = seats
            .iter()
            .filter(|seat| seat.ticket_type_id == item.id && seat.active)
            .collect::<Vec<_>>();
        let remaining = if assigned_seating {
            remaining.min(
                i64::try_from(type_seats.iter().filter(|seat| seat.available).count())
                    .map_err(|error| loco_rs::Error::string(&error.to_string()))?,
            )
        } else {
            remaining
        };
        if state == TicketBoxState::OnSale && remaining == 0 {
            state = TicketBoxState::SoldOut;
        }
        let max = remaining.min(event.max_per_order);
        ticket_views.push(json!({
            "id": item.id,
            "name": item.name,
            "price": events::format_price(item.price_cents, &currency),
            "price_cents": item.price_cents,
            "state": state.as_str(),
            "max": max,
            "remaining": remaining,
            "seats": type_seats,
            "low_stock": state == TicketBoxState::OnSale && remaining <= 6
        }));
        let mut offer = json!({
            "@type": "Offer",
            "name": item.name,
            "price": format!("{}.{:02}", item.price_cents / 100, item.price_cents % 100),
            "priceCurrency": currency,
            "availability": match state {
                TicketBoxState::OnSale => "https://schema.org/InStock",
                TicketBoxState::ComingSoon => "https://schema.org/PreSale",
                TicketBoxState::SoldOut => "https://schema.org/SoldOut",
                TicketBoxState::SalesEnded => "https://schema.org/Discontinued",
            },
            "url": event_url
        });
        if let Some(open) = item.sales_open.filter(|open| *open > now) {
            offer["validFrom"] = json!(local_iso(tz, open)?);
        }
        if state != TicketBoxState::SalesEnded {
            offers.push(offer);
        }
    }
    let mut structured = json!({
        "@context": "https://schema.org",
        "@type": "Event",
        "name": event.title,
        "startDate": local_iso(tz, event.starts_at)?,
        "endDate": local_iso(tz, event.ends_at)?,
        "eventStatus": if event.status == "Cancelled" {
            "https://schema.org/EventCancelled"
        } else {
            "https://schema.org/EventScheduled"
        },
        "eventAttendanceMode": "https://schema.org/OfflineEventAttendanceMode",
        "location": {
            "@type": "Place",
            "name": event.venue,
            "address": {"@type": "PostalAddress", "streetAddress": event.address}
        },
        "organizer": {"@type": "Organization", "name": organizer.name, "url": origin},
        "url": event_url,
        "offers": offers
    });
    if !event.description.trim().is_empty() {
        structured["description"] = json!(event.description);
    }
    // The cover route serves Published events only.
    if event.has_cover && event.status == "Published" {
        structured["image"] = json!([format!("{event_url}/cover")]);
    }
    let directions_url = format!(
        "https://www.openstreetmap.org/search?query={}",
        utf8_percent_encode(&event.address, NON_ALPHANUMERIC)
    );
    let paragraphs = event
        .description
        .split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty())
        .collect::<Vec<_>>();
    let date = NaiveDate::parse_from_str(&event.date, "%Y-%m-%d")
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let view_event = event_view(&event, date);
    let purchase_available = ticket_views.iter().any(|item| item["state"] == "on-sale");
    let sales_notice = if event.status == "Cancelled" {
        "This event is cancelled."
    } else if now >= event.ends_at {
        "This event has ended."
    } else if now >= event.starts_at {
        "This event has started. Ticket sales have ended."
    } else if ticket_views.is_empty() {
        "Tickets aren't on sale yet."
    } else {
        "No tickets are on sale right now."
    };
    let html = view.render(
        "shop/event.html",
        data!({
            "organizer_name": organizer.name,
            "event": view_event,
            "ticket_types": ticket_views,
            "purchase_available": purchase_available,
            "sales_notice": sales_notice,
            "assigned_seating": assigned_seating,
            "currency": currency,
            "contact": contact,
            "directions_url": directions_url,
            "description_paragraphs": paragraphs,
            "structured_data": script_json(&structured)
        }),
    )?;
    registry
        .cache_event_page(&organizer.id, &id, html.clone())
        .await;
    Ok(event_page_response(html))
}

fn local_iso(tz: Tz, timestamp: i64) -> Result<String> {
    Utc.timestamp_opt(timestamp, 0)
        .single()
        .map(|time| {
            time.with_timezone(&tz)
                .to_rfc3339_opts(SecondsFormat::Secs, false)
        })
        .ok_or_else(|| loco_rs::Error::string("Event time is out of range"))
}

/// Serializes JSON for an inline `<script>` element, so no value can close the element.
pub(super) fn script_json(value: &Value) -> String {
    value
        .to_string()
        .replace("</", "<\\/")
        .replace("<!--", "\\u003c!--")
}

fn event_page_response(html: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=1"),
        ],
        html,
    )
        .into_response()
}

async fn cover(OrganizerDb { db, .. }: OrganizerDb, Path(id): Path<String>) -> Result<Response> {
    let event = events::get(&db, &id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    if !event.is_some_and(|event| event.status == "Published") {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    cover_response(&db, &id).await
}

fn event_view(event: &Event, date: NaiveDate) -> Value {
    json!({
        "id": event.id,
        "title": event.title,
        "day": date.format("%-d").to_string(),
        "formatted_date": events::formatted_date(event),
        "start_time": event.start_time,
        "end_time": event.end_time,
        "timezone": event.timezone,
        "zone_label": zone_label(&event.timezone),
        "venue": event.venue,
        "address": event.address,
        "has_cover": event.has_cover
    })
}

fn zone_label(timezone: &str) -> String {
    match timezone {
        "Europe/Dublin" => "Irish time".to_owned(),
        "Europe/London" => "UK time".to_owned(),
        _ => {
            timezone
                .split('/')
                .next_back()
                .unwrap_or(timezone)
                .replace('_', " ")
                + " time"
        }
    }
}

pub fn routes() -> Routes {
    Routes::new()
        .add("/", get(index))
        .add("/events/{id}", get(event))
        .add("/events/{id}/cover", get(cover))
        .add("/embed/events/{id}", get(super::embed::selector))
        .add("/embed/series/{id}", get(super::embed::series_selector))
        .add("/series/{id}", get(super::embed::series_selector))
        .add("/events/{id}/selection", get(super::embed::handoff))
}
