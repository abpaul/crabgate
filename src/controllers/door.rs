use std::sync::Arc;

use axum::{
    extract::{Form, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Extension,
};
use axum_extra::extract::cookie::CookieJar;
use chrono::{TimeZone, Utc};
use chrono_tz::Tz;
use loco_rs::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{AdminOrganizer, DoorAccess},
    controllers::checkout::origin,
    door::{self, CheckinResult, DoorError, DoorTicket},
    events,
    mailers::member::MemberMailer,
    members::{self, Role},
    organizers::{OrganizerDb, OrganizerRegistry},
};

#[derive(Deserialize)]
struct NewLink {
    label: String,
    event_id: String,
}

#[derive(Deserialize)]
struct NewMember {
    email: String,
    role: String,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
    undone: Option<String>,
}

#[derive(Deserialize)]
struct CheckForm {
    code: String,
}

fn error_response(error: DoorError) -> Response {
    match error {
        DoorError::LinkUnavailable | DoorError::CheckinNotFound => {
            StatusCode::NOT_FOUND.into_response()
        }
        DoorError::EventEnded => (StatusCode::GONE, error.to_string()).into_response(),
        DoorError::InvalidLabel => (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        _ => {
            tracing::error!(%error, "Door error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn event_index(
    State(ctx): State<AppContext>,
    OrganizerDb { organizer, db }: OrganizerDb,
    jar: CookieJar,
    ViewEngine(view): ViewEngine<TeraView>,
) -> Result<Response> {
    let session = match jar.get("cg_session") {
        Some(cookie) => members::find_session(
            &ctx.db,
            &organizer.id,
            cookie.value(),
            Utc::now().timestamp(),
        )
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?,
        None => None,
    };
    let Some(session) = session else {
        return Ok(Redirect::to("/office/sign-in").into_response());
    };
    let now = Utc::now().timestamp();
    let events = events::list(&db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .into_iter()
        .filter(|event| event.status == "Published" && event.ends_at > now)
        .map(|event| json!({"id": event.id, "title": event.title, "date": events::formatted_date(&event), "venue": event.venue}))
        .collect::<Vec<_>>();
    format::render().view(
        &view,
        "door/events.html",
        data!({"organizer_name": organizer.name, "role": session.role.as_str(), "events": events}),
    )
}

async fn office_index(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
) -> Result<Response> {
    let cipher = registry
        .cipher(&organizer)
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let all_events = events::list(&db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let events = all_events
        .iter()
        .filter(|event| event.status == "Published" && event.ends_at > Utc::now().timestamp())
        .map(|event| json!({"id": event.id, "title": event.title}))
        .collect::<Vec<_>>();
    let all_links = door::list_links(&db, &cipher)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let mut links = Vec::new();
    for link in all_links {
        let event = all_events.iter().find(|event| event.id == link.event_id);
        let checkins = door::link_count(&db, &link.id)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        links.push(json!({
            "id": link.id, "label": link.label, "event_title": event.map(|event| event.title.as_str()).unwrap_or("Event"),
            "ends": event.map(events::formatted_date).unwrap_or_default(), "checkins": checkins,
            "revoked": link.revoked_at.is_some(),
            "url": format!("{}/door/{}?link={}", origin(&ctx, &organizer), link.event_id, link.token)
        }));
    }
    let members = members::list_members(&ctx.db, &organizer.id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .into_iter()
        .map(|member| json!({"id": member.id, "email": member.email, "role": member.role.as_str()}))
        .collect::<Vec<_>>();
    format::render().view(&view, "office/door.html", data!({"organizer_name": organizer.name, "active": "door", "links": links, "members": members, "events": events}))
}

async fn create_link(
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    Form(form): Form<NewLink>,
) -> Result<Response> {
    let cipher = registry
        .cipher(&organizer)
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    match door::create_link(
        &db,
        &cipher,
        &form.event_id,
        &form.label,
        Utc::now().timestamp(),
    )
    .await
    {
        Ok(_) => Ok(Redirect::to("/office/door").into_response()),
        Err(error) => Ok(error_response(error)),
    }
}

async fn revoke_link(
    AdminOrganizer { db, .. }: AdminOrganizer,
    Path(id): Path<String>,
) -> Result<Response> {
    door::revoke_link(&db, &id, Utc::now().timestamp())
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    Ok(Redirect::to("/office/door").into_response())
}

async fn invite_member(
    State(ctx): State<AppContext>,
    AdminOrganizer { organizer, .. }: AdminOrganizer,
    Form(form): Form<NewMember>,
) -> Result<Response> {
    if form.email.len() > 254 || !form.email.contains('@') {
        return Ok((
            StatusCode::BAD_REQUEST,
            "That email address doesn't look right. Please check it.",
        )
            .into_response());
    }
    let role = match form.role.as_str() {
        "Admin" => Role::Admin,
        "Door Staff" => Role::DoorStaff,
        _ => return Ok(StatusCode::BAD_REQUEST.into_response()),
    };
    match members::add_member(&ctx.db, &organizer.id, &form.email, role).await {
        Ok(_) => {}
        Err(error @ members::MemberError::LastAdmin) => {
            return Ok((StatusCode::BAD_REQUEST, error.to_string()).into_response());
        }
        Err(error) => return Err(loco_rs::Error::string(&error.to_string())),
    }
    let token = match members::request_magic_link(
        &ctx.db,
        &organizer.id,
        &form.email,
        Utc::now().timestamp(),
    )
    .await
    {
        Ok(token) => token,
        Err(members::MemberError::RateLimited) => {
            return Ok((
                StatusCode::TOO_MANY_REQUESTS,
                "Member saved, but the invitation email was not sent. Too many tries. Please wait a little and send the invite again.",
            )
                .into_response());
        }
        Err(error) => return Err(loco_rs::Error::string(&error.to_string())),
    };
    if let Some(token) = token {
        let url = format!("{}/office/magic-link/{token}", origin(&ctx, &organizer));
        if let Err(error) = MemberMailer::send_magic_link(&ctx, &form.email, &url).await {
            tracing::warn!(%error, "Member invitation delivery failed or is uncertain");
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                "Member and role saved, but invitation email delivery failed or is uncertain. Please wait a little and send the invite again from Members. Another attempt may deliver a duplicate email.",
            )
                .into_response());
        }
    }
    Ok(Redirect::to("/office/door").into_response())
}

async fn remove_member(
    State(ctx): State<AppContext>,
    AdminOrganizer { organizer, .. }: AdminOrganizer,
    Path(id): Path<String>,
) -> Result<Response> {
    let removed = members::remove_member(&ctx.db, &organizer.id, &id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    if !removed {
        return Ok((StatusCode::BAD_REQUEST, "You can't remove the last Admin.").into_response());
    }
    Ok(Redirect::to("/office/door").into_response())
}

async fn base_data(access: &DoorAccess) -> Result<Value> {
    let event = events::get(&access.db, &access.event_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .ok_or_else(|| loco_rs::Error::string("Event not found"))?;
    let (checked, total) = door::count(&access.db, &access.event_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let door_path = format!("/door/{}", access.event_id);
    let auth_query = access
        .link_token
        .as_ref()
        .map(|token| format!("?link={token}"))
        .unwrap_or_default();
    Ok(json!({
        "organizer_name": access.organizer.name,
        "event": {"title": event.title, "date": events::formatted_date(&event)},
        "checked": checked, "total": total,
        "door_path": door_path, "auth_query": auth_query,
        "link_token": access.link_token,
        "actor_label": if access.can_undo { "Member" } else { "Door Link" },
        "can_undo": access.can_undo
    }))
}

fn merge(mut base: Value, extra: Value) -> Value {
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}

async fn scan(access: DoorAccess, ViewEngine(view): ViewEngine<TeraView>) -> Result<Response> {
    format::render().view(
        &view,
        "door/scan.html",
        merge(base_data(&access).await?, json!({"active": "scan"})),
    )
}

async fn lookup(
    access: DoorAccess,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    ViewEngine(view): ViewEngine<TeraView>,
    Query(search): Query<SearchQuery>,
) -> Result<Response> {
    let query = search.q.unwrap_or_default();
    let cipher = registry
        .cipher(&access.organizer)
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let tickets = door::search(&access.db, &cipher, &access.event_id, &query)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let event = events::get(&access.db, &access.event_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .ok_or_else(|| loco_rs::Error::string("Event not found"))?;
    let now = Utc::now().timestamp();
    let matches = tickets
        .iter()
        .map(|ticket| {
            json!({
                "id": ticket.id, "code": ticket.code, "attendee_name": ticket.attendee_name,
                "ticket_type": ticket.ticket_type, "seat": ticket.seat,
                "order_number": ticket.order_number, "order_status": ticket.order_status,
                "buyer_email": ticket.buyer_email, "status": ticket.status,
                "admission": if event.status == "Cancelled" { "Event cancelled" }
                    else if event.ends_at <= now { "Event ended" }
                    else if ticket.is_void() { "Cannot admit" }
                    else if ticket.checked_at.is_some() { "Already checked in" }
                    else { "Ready to check in" },
                "can_check_in": event.status != "Cancelled" && event.ends_at > now && !ticket.is_void() && ticket.checked_at.is_none(),
                "checked_at": ticket.checked_at.map(|time| local_time(&event.timezone, time))
            })
        })
        .collect::<Vec<_>>();
    format::render().view(
        &view,
        "door/lookup.html",
        merge(
            base_data(&access).await?,
            json!({"active": "lookup", "query": query, "matches": matches, "undone": search.undone.as_deref() == Some("1")}),
        ),
    )
}

async fn check(
    access: DoorAccess,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    ViewEngine(view): ViewEngine<TeraView>,
    Form(form): Form<CheckForm>,
) -> Result<Response> {
    let cipher = registry
        .cipher(&access.organizer)
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let outcome = match door::check_in(
        &access.db,
        &cipher,
        &access.event_id,
        &form.code,
        access.actor_type,
        &access.actor_id,
        Utc::now().timestamp(),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => return Ok(error_response(error)),
    };
    let event = events::get(&access.db, &access.event_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .ok_or_else(|| loco_rs::Error::string("Event not found"))?;
    let (key, title, class, icon, ticket): (&str, &str, &str, &str, Option<DoorTicket>) =
        match outcome {
            CheckinResult::CheckedIn(ticket) => {
                ("checked-in", "Checked in", "result-ok", "✓", Some(ticket))
            }
            CheckinResult::AlreadyCheckedIn(ticket) => (
                "already",
                "Already checked in",
                "result-warn",
                "!",
                Some(ticket),
            ),
            CheckinResult::VoidTicket(ticket) => {
                ("void", "Void Ticket", "result-bad", "×", Some(ticket))
            }
            CheckinResult::WrongEvent => ("wrong-event", "Wrong Event", "result-bad", "×", None),
            CheckinResult::Unknown => ("unknown", "Unknown code", "result-bad", "?", None),
        };
    let checked_at = ticket
        .as_ref()
        .and_then(|ticket| ticket.checked_at)
        .map(|time| local_time(&event.timezone, time));
    let ticket = ticket.map(|ticket| json!({"id": ticket.id, "code": ticket.code, "attendee_name": ticket.attendee_name, "ticket_type": ticket.ticket_type, "seat": ticket.seat, "order_number": ticket.order_number}));
    format::render().view(&view, "door/result.html", merge(base_data(&access).await?, json!({"active": "scan", "result_key": key, "result_title": title, "result_class": class, "result_icon": icon, "ticket": ticket, "checked_at": checked_at})))
}

async fn undo(
    access: DoorAccess,
    Path((_, ticket_id)): Path<(String, String)>,
) -> Result<Response> {
    if !access.can_undo {
        return Ok(StatusCode::FORBIDDEN.into_response());
    }
    match door::undo(
        &access.db,
        &access.event_id,
        &ticket_id,
        &access.actor_id,
        Utc::now().timestamp(),
    )
    .await
    {
        Ok(()) => {
            Ok(Redirect::to(&format!("/door/{}/lookup?undone=1", access.event_id)).into_response())
        }
        Err(error) => Ok(error_response(error)),
    }
}

fn local_time(timezone: &str, timestamp: i64) -> String {
    timezone
        .parse::<Tz>()
        .ok()
        .and_then(|tz| tz.timestamp_opt(timestamp, 0).single())
        .map(|time| time.format("%H:%M").to_string())
        .unwrap_or_default()
}

pub fn routes() -> Routes {
    Routes::new()
        .add("/door", get(event_index))
        .add("/office/door", get(office_index))
        .add("/office/door/links", post(create_link))
        .add("/office/door/links/{id}/revoke", post(revoke_link))
        .add("/office/door/members", post(invite_member))
        .add("/office/door/members/{id}/remove", post(remove_member))
        .add("/door/{event_id}", get(scan))
        .add("/door/{event_id}/lookup", get(lookup))
        .add("/door/{event_id}/check", post(check))
        .add("/door/{event_id}/undo/{ticket_id}", post(undo))
}
