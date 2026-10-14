use std::sync::Arc;

use axum::{
    extract::{Form, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Extension,
};
use chrono::{TimeZone, Utc};
use loco_rs::prelude::*;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde::Deserialize;
use serde_json::json;

use crate::{
    admin_orders::{self, AdminOrderError},
    auth::AdminOrganizer,
    controllers::checkout::{resend_order_email, send_order_email, ticket_email_failure},
    data_transfer, events,
    initializers::organizers::OrganizerRuntime,
    mailers::refund::RefundMailer,
    orders,
    organizers::{OrganizerDb, OrganizerRegistry},
    settings,
    tenant_jobs::{Admission, Budget, Pass},
};

#[derive(Deserialize)]
struct OrdersQuery {
    q: Option<String>,
    event: Option<String>,
    page: Option<String>,
}

#[derive(Deserialize)]
struct OrderFeedback {
    tickets_resent: Option<String>,
    ticket_email: Option<String>,
}

#[derive(Deserialize)]
struct ComplimentaryForm {
    ticket_type_id: String,
    seat_id: Option<String>,
    email: String,
    name: String,
}

fn error_response(error: AdminOrderError) -> Response {
    match error {
        AdminOrderError::NotFound => StatusCode::NOT_FOUND.into_response(),
        AdminOrderError::CannotRefund
        | AdminOrderError::NoCapacity
        | AdminOrderError::SeatRequired
        | AdminOrderError::InvalidEmail => {
            (StatusCode::BAD_REQUEST, error.to_string()).into_response()
        }
        _ => {
            tracing::error!(%error, "Admin Order error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn amount(cents: i64, currency: &str) -> String {
    events::format_price(cents, currency)
}

async fn index(
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    Query(query): Query<OrdersQuery>,
    ViewEngine(view): ViewEngine<TeraView>,
) -> Result<Response> {
    let cipher = registry
        .cipher(&organizer)
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let page_number = query
        .page
        .as_deref()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|page| *page > 0)
        .unwrap_or(1);
    let page = admin_orders::list_page(
        &db,
        &cipher,
        query.event.as_deref().filter(|s| !s.is_empty()),
        query.q.as_deref().unwrap_or_default(),
        page_number,
    )
    .await
    .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let currency = settings::currency(&db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let previous_page = (page_number > 1).then(|| page_number - 1);
    let next_page = page.has_next.then(|| page_number.checked_add(1)).flatten();
    let shown_start = u64::from(page_number - 1) * admin_orders::ORDERS_PAGE_SIZE as u64 + 1;
    let shown_end = shown_start + page.orders.len() as u64 - 1;
    let rows = page.orders.into_iter().map(|item| {
        json!({"id": item.id, "number": item.number, "event_title": item.event_title, "buyer_email": item.buyer_email,
            "total": amount(item.total_cents, &currency), "status": item.status, "complimentary": item.complimentary,
            "ticket_count": item.ticket_count, "created": Utc.timestamp_opt(item.created_at, 0).single().map(|t| t.format("%-d %b %Y, %H:%M").to_string()).unwrap_or_default()})
    }).collect::<Vec<_>>();
    let all_events = events::list(&db)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let mut events = Vec::new();
    let mut ticket_types = Vec::new();
    let mut seats = Vec::new();
    let mut available_seats = crate::seats::available_for_events(&db, Utc::now().timestamp())
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let mut all_types = events::list_all_ticket_types(&db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    for event in all_events {
        for seat in available_seats.remove(&event.id).unwrap_or_default() {
            seats.push(json!({"id": seat.id, "type_id": seat.ticket_type_id, "label": format!("{}, {}", event.title, crate::seats::display(&seat.section, &seat.row, &seat.label))}));
        }
        for kind in all_types.remove(&event.id).unwrap_or_default() {
            ticket_types.push(json!({"event_id": event.id, "event_title": event.title, "id": kind.id, "name": kind.name}));
        }
        events.push(json!({"id": event.id, "title": event.title, "selected": query.event.as_deref() == Some(event.id.as_str())}));
    }
    let query_url =
        utf8_percent_encode(query.q.as_deref().unwrap_or_default(), NON_ALPHANUMERIC).to_string();
    let event_url =
        utf8_percent_encode(query.event.as_deref().unwrap_or_default(), NON_ALPHANUMERIC)
            .to_string();
    format::render().view(&view, "office/orders.html", data!({"organizer_name": organizer.name, "active": "orders", "orders": rows, "previous_page": previous_page, "next_page": next_page, "shown_start": shown_start, "shown_end": shown_end, "query_url": query_url, "event_url": event_url, "events": events, "ticket_types": ticket_types, "seats": seats, "query": query.q.unwrap_or_default(), "selected_event": query.event.unwrap_or_default()}))
}

fn checkin_time(timestamp: i64) -> String {
    Utc.timestamp_opt(timestamp, 0)
        .single()
        .map(|time| time.format("%-d %b %Y, %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "Time unavailable".to_owned())
}

async fn attendees(
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer {
        organizer,
        db,
        member_id,
    }: AdminOrganizer,
    Query(query): Query<OrdersQuery>,
    ViewEngine(view): ViewEngine<TeraView>,
) -> Result<Response> {
    let selected = query.event.as_deref().filter(|id| !id.is_empty());
    let all_events = events::list(&db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let event_unavailable =
        selected.is_some_and(|id| !all_events.iter().any(|event| event.id == id));
    let cipher = registry
        .cipher(&organizer)
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let mut load_failed = false;
    let rows = if event_unavailable {
        Vec::new()
    } else {
        match admin_orders::attendees(&db, &cipher, selected, query.q.as_deref().unwrap_or_default()).await {
            Ok(rows) => rows.into_iter().map(|row| {
                let checked = row.checked_at.map(checkin_time);
                let undone = row.undone_at.map(checkin_time);
                json!({"ticket_id": row.ticket_id, "code": row.code, "name": row.name, "ticket_type": row.ticket_type,
                    "seat": row.seat, "ticket_status": row.ticket_status, "order_id": row.order_id,
                    "order_number": row.order_number, "order_status": row.order_status, "event_title": row.event_title,
                    "buyer_email": row.buyer_email, "admission_valid": row.admission_valid, "checked": checked, "undone": undone})
            }).collect::<Vec<_>>(),
            Err(error) => {
                tracing::error!(%error, "Attendee roster unavailable");
                load_failed = true;
                Vec::new()
            }
        }
    };
    let events = all_events.into_iter().map(|event| json!({"id": event.id, "title": event.title, "selected": selected == Some(event.id.as_str())})).collect::<Vec<_>>();
    let status = if event_unavailable {
        StatusCode::NOT_FOUND
    } else if load_failed {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    };
    let response = format::render().view(&view, "office/attendees.html", data!({
        "organizer_name": organizer.name, "active": "attendees", "attendees": rows,
        "events": events, "query": query.q.unwrap_or_default(), "selected_event": query.event.unwrap_or_default(),
        "can_manage": !member_id.is_empty(), "event_unavailable": event_unavailable, "load_failed": load_failed
    }))?;
    Ok((status, response).into_response())
}

async fn detail(
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    Path(id): Path<String>,
    Query(feedback): Query<OrderFeedback>,
    ViewEngine(view): ViewEngine<TeraView>,
) -> Result<Response> {
    let cipher = registry
        .cipher(&organizer)
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let order = match admin_orders::order(&db, &cipher, &id).await {
        Ok(order) => order,
        Err(error) => return Ok(error_response(error)),
    };
    let currency = settings::currency(&db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let answers = crate::questions::order_answers(&db, &cipher, &id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let discount = crate::discounts::order_summary(&db, &id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let event = events::get(&db, &order.event_id)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?
        .ok_or_else(|| loco_rs::Error::string("Event missing"))?;
    let kinds = events::list_ticket_types(&db, &event.id)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let activity = admin_orders::ticket_activity(&db, &id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let tickets = order.tickets.iter().map(|ticket| {
        let kind = kinds.iter().find(|kind| kind.id == ticket.ticket_type_id).map(|kind| kind.name.as_str()).unwrap_or("Ticket");
        let number = activity.iter().find(|item| item.ticket_id == ticket.id).map(|item| item.attendee_number);
        let answers = answers.iter().filter(|answer| answer.attendee_number.is_some() && answer.attendee_number == number).collect::<Vec<_>>();
        let history = activity.iter().filter(|item| item.ticket_id == ticket.id).filter_map(|item| {
            item.checked_at.map(|checked| json!({"checked": checkin_time(checked), "undone": item.undone_at.map(checkin_time)}))
        }).collect::<Vec<_>>();
        json!({"id": ticket.id, "name": ticket.attendee_name, "kind": kind, "code": ticket.code, "status": ticket.status, "seat": ticket.seat,
            "admission_valid": ticket.status == "Valid" && order.status != "Refunded" && event.status != "Cancelled", "history": history, "answers": answers})
    }).collect::<Vec<_>>();
    let order_answers = answers
        .iter()
        .filter(|answer| answer.attendee_number.is_none())
        .collect::<Vec<_>>();
    let booking_terms = crate::booking_terms::order_evidence(&db, &id)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let progress = admin_orders::work_progress(&db, &id)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let email = orders::email_progress(&db, &id, orders::now())
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let limit = orders::EMAIL_ATTEMPT_LIMIT;
    let ticket_email = match email.state.as_str() {
        _ if email.accepted => Some("Accepted by the mail server.".to_owned()),
        "No recipient" => Some("Not sent. This order has no recipient email.".to_owned()),
        _ if order.status != "Paid" || event.status == "Cancelled" => None,
        "Refused" => Some(format!("Automatic attempts stopped after {limit} tries. The mail server refused the last attempt. Check the address and the mail server, then use Resend tickets.")),
        "Stopped" => Some(format!("Automatic attempts stopped after {limit} tries. Mail acceptance was not confirmed. Check the mail server, then use Resend tickets.")),
        _ if email.attempts == 0 => Some("Not sent yet. CrabGate will try automatically.".to_owned()),
        _ if email.attempts >= limit => Some(format!("The last of {limit} automatic attempts is in progress.")),
        _ => Some(format!("Mail acceptance not confirmed after {} of {limit} automatic attempts. CrabGate will try again.", email.attempts)),
    };
    let row = json!({"work": progress, "ticket_email": ticket_email, "id": order.id, "number": order.number, "event_id": event.id, "event_title": event.title,
        "buyer_email": order.buyer_email, "total": amount(order.total_cents, &currency), "status": order.status,
        "can_refund": (order.status == "Paid" || order.status == "RefundPending") && progress.refund != "Reconciliation required", "can_resend": order.status == "Paid" && !order.buyer_email.is_empty() && event.status != "Cancelled", "is_free": order.total_cents == 0, "tickets": tickets});
    format::render().view(
        &view,
        "office/order.html",
        data!({"organizer_name": organizer.name, "active": "orders", "tickets_resent": feedback.tickets_resent.as_deref() == Some("1"), "ticket_email_unconfirmed": feedback.ticket_email.as_deref() == Some("unconfirmed"), "order": row, "answers": order_answers, "booking_terms": booking_terms.map(|evidence| json!({"url": evidence.terms.url, "wording": evidence.terms.wording, "accepted": checkin_time(evidence.accepted_at)})), "discount": discount.map(|(code, subtotal, discount_cents)| json!({"code": code, "subtotal": amount(subtotal, &currency), "amount": amount(discount_cents, &currency)}))}),
    )
}

async fn refund(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    Path(id): Path<String>,
) -> Result<Response> {
    match admin_orders::payment_intent(&db, &id).await {
        Ok((status, _, _, _))
            if matches!(status.as_str(), "Paid" | "RefundPending" | "Refunded") => {}
        Ok(_) => return Ok(error_response(AdminOrderError::CannotRefund)),
        Err(error) => return Ok(error_response(error)),
    }
    process_order_work(&ctx, &registry, &organizer, &db, &id, true).await?;
    Ok(Redirect::to(&format!("/office/orders/{id}")).into_response())
}

#[derive(Deserialize)]
struct NoticeReconciliation {
    notice: String,
    outcome: String,
    confirmed: Option<String>,
}

async fn reconcile_notice(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    Path(id): Path<String>,
    Form(form): Form<NoticeReconciliation>,
) -> Result<Response> {
    if form.confirmed.as_deref() != Some("yes")
        || !matches!(form.notice.as_str(), "cancellation" | "refund")
        || !matches!(form.outcome.as_str(), "accepted" | "retry")
    {
        return Ok((
            StatusCode::BAD_REQUEST,
            "Confirm the mail server result before recovery.",
        )
            .into_response());
    }
    let cancellation = form.notice == "cancellation";
    let accepted = form.outcome == "accepted";
    let changed =
        admin_orders::reconcile_notice(&db, &id, cancellation, accepted, Utc::now().timestamp())
            .await
            .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    if !changed {
        return Ok((StatusCode::CONFLICT, "Notice is still in flight, already reconciled, or refund is not complete. Wait at least five minutes after the send started.").into_response());
    }
    if !accepted {
        send_notice(&ctx, &registry, &organizer, &db, &id, cancellation).await?;
    }
    Ok(Redirect::to(&format!("/office/orders/{id}")).into_response())
}

pub(crate) async fn send_notice(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    organizer: &crate::organizers::Organizer,
    db: &sea_orm::DatabaseConnection,
    id: &str,
    cancelled: bool,
) -> Result<()> {
    let cipher = registry
        .cipher(organizer)
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let order = admin_orders::order(db, &cipher, id)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    if order.buyer_email.is_empty() {
        admin_orders::skip_notice(db, id, cancelled)
            .await
            .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
        return Ok(());
    }
    let event = events::get(db, &order.event_id)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?
        .ok_or_else(|| loco_rs::Error::string("Event missing"))?;
    let contact = settings::contact(db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    if !admin_orders::claim_notice(db, id, cancelled)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?
    {
        return Ok(());
    }
    let sent = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        RefundMailer::send(
            ctx,
            &order.buyer_email,
            &organizer.name,
            &order.number,
            &event.title,
            cancelled,
            (!contact.reply_to.is_empty()).then_some(contact.reply_to.as_str()),
        ),
    )
    .await;
    let sent = match sent {
        Ok(result) => result,
        Err(_) => {
            admin_orders::record_notice_error(db, id, cancelled, false)
                .await
                .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
            return Err(loco_rs::Error::string("Mail acceptance is uncertain after timeout; check the mail server before recovery."));
        }
    };
    if let Err(error) = sent {
        admin_orders::record_notice_error(db, id, cancelled, notice_retry_is_safe(&error))
            .await
            .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
        return Err(error);
    }
    admin_orders::mark_notice_sent(db, id, cancelled, Utc::now().timestamp())
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    Ok(())
}

async fn cancel_event(
    State(ctx): State<AppContext>,
    AdminOrganizer { db, .. }: AdminOrganizer,
    Path(event_id): Path<String>,
) -> Result<Response> {
    if let Err(error) = admin_orders::cancel_event(&db, &event_id, Utc::now().timestamp()).await {
        return Ok(error_response(error));
    }
    if let Some(runtime) = ctx.shared_store.get::<Arc<OrganizerRuntime>>() {
        runtime.wake_refund_work();
    }
    Ok(Redirect::to(&format!("/office/events/{event_id}")).into_response())
}

async fn anonymize(
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    Path(id): Path<String>,
) -> Result<Response> {
    let cipher = registry
        .cipher(&organizer)
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    match admin_orders::anonymize(&db, &cipher, &id).await {
        Ok(()) => Ok(Redirect::to(&format!("/office/orders/{id}")).into_response()),
        Err(error) => Ok(error_response(error)),
    }
}

async fn resend(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    Path(id): Path<String>,
) -> Result<Response> {
    let cipher = registry
        .cipher(&organizer)
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let order = match admin_orders::order(&db, &cipher, &id).await {
        Ok(order) => order,
        Err(error) => return Ok(error_response(error)),
    };
    if order.buyer_email.is_empty() || order.status != "Paid" {
        return Ok((
            StatusCode::BAD_REQUEST,
            "Tickets can only be resent for a paid order with a recipient email.",
        )
            .into_response());
    }
    let event = events::get(&db, &order.event_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    if event.is_none_or(|event| event.status == "Cancelled") {
        return Ok((
            StatusCode::BAD_REQUEST,
            "Tickets cannot be resent for a cancelled or missing event.",
        )
            .into_response());
    }
    if let Err(error) = resend_order_email(&ctx, &registry, &organizer, &db, &id).await {
        return Ok(ticket_email_failure(error));
    }
    Ok(Redirect::to(&format!("/office/orders/{id}?tickets_resent=1")).into_response())
}

async fn complimentary(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    Form(form): Form<ComplimentaryForm>,
) -> Result<Response> {
    let cipher = registry
        .cipher(&organizer)
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let Some(type_row) = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT event_id FROM ticket_types WHERE id = ?",
            [form.ticket_type_id.clone().into()],
        ))
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?
    else {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    };
    let event_id: String = type_row
        .try_get("", "event_id")
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let id = match admin_orders::complimentary_with_seat(
        &db,
        &cipher,
        &event_id,
        (
            &form.ticket_type_id,
            form.seat_id.as_deref().filter(|id| !id.is_empty()),
        ),
        &form.email,
        &form.name,
        Utc::now().timestamp(),
    )
    .await
    {
        Ok(id) => id,
        Err(error) => return Ok(error_response(error)),
    };
    if let Err(error) = send_order_email(&ctx, &registry, &organizer, &db, &id).await {
        tracing::error!(%error, order_id = %id, "Tickets created; mail acceptance could not be confirmed");
        return Ok(
            Redirect::to(&format!("/office/orders/{id}?ticket_email=unconfirmed")).into_response(),
        );
    }
    Ok(Redirect::to(&format!("/office/orders/{id}")).into_response())
}

pub fn routes() -> Routes {
    Routes::new()
        .add("/office/orders", get(index))
        .add("/office/attendees", get(attendees))
        .add("/office/orders/complimentary", post(complimentary))
        .add("/office/orders/{id}", get(detail))
        .add("/office/orders/{id}/refund", post(refund))
        .add(
            "/office/orders/{id}/notice-reconcile",
            post(reconcile_notice),
        )
        .add("/office/orders/{id}/anonymize", post(anonymize))
        .add("/office/orders/{id}/resend", post(resend))
        .add(
            "/office/events/{event_id}/export/{filename}",
            get(export_csv),
        )
        .add("/office/events/{event_id}/cancel", post(cancel_event))
        .add("/office/data/export", get(export_database))
}

async fn export_database(
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer {
        organizer,
        db,
        member_id,
    }: AdminOrganizer,
) -> Result<Response> {
    if member_id.is_empty() {
        return Ok(StatusCode::FORBIDDEN.into_response());
    }
    let cipher = registry
        .cipher(&organizer)
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let data = data_transfer::export(&db, &cipher)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.sqlite3"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=crabgate-export.sqlite",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        data,
    )
        .into_response())
}

async fn export_csv(
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer {
        organizer,
        db,
        member_id,
    }: AdminOrganizer,
    Path((event_id, filename)): Path<(String, String)>,
) -> Result<Response> {
    if member_id.is_empty() {
        return Ok(StatusCode::FORBIDDEN.into_response());
    }
    if events::get(&db, &event_id)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?
        .is_none()
    {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let kind = match filename.as_str() {
        "orders.csv" => admin_orders::Report::Orders,
        "tickets.csv" => admin_orders::Report::Tickets,
        "checkins.csv" => admin_orders::Report::Checkins,
        _ => return Ok(StatusCode::NOT_FOUND.into_response()),
    };
    let cipher = registry
        .cipher(&organizer)
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let csv = admin_orders::report(&db, &cipher, &event_id, kind)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=report.csv",
            ),
        ],
        csv,
    )
        .into_response())
}

pub async fn process_order_work(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    organizer: &crate::organizers::Organizer,
    db: &sea_orm::DatabaseConnection,
    id: &str,
    request_refund: bool,
) -> Result<()> {
    process_order_refund(registry, organizer, db, id, request_refund).await?;
    process_order_notices(ctx, registry, organizer, db, id).await
}

async fn process_order_refund(
    registry: &OrganizerRegistry,
    organizer: &crate::organizers::Organizer,
    db: &sea_orm::DatabaseConnection,
    id: &str,
    request_refund: bool,
) -> Result<()> {
    let (status, _, total, _) = admin_orders::payment_intent(db, id)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let progress = admin_orders::work_progress(db, id)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    if progress.refund_claimed
        || ((status != "Refunded" || progress.refund != "Completed")
            && (request_refund || matches!(progress.refund.as_str(), "Pending" | "Failed")))
    {
        let cipher = registry
            .cipher(organizer)
            .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
        let stripe = if total > 0 {
            match settings::stripe_client(db, &cipher).await {
                Ok(client) => Some(client),
                Err(error) => {
                    tracing::error!(%error, order_id = %id, "Refund provider configuration failed");
                    None
                }
            }
        } else {
            None
        };
        if let Err(error) =
            admin_orders::refund(db, id, stripe.as_ref(), Utc::now().timestamp()).await
        {
            tracing::error!(%error, order_id = %id, "Refund remains queued");
        }
    }
    Ok(())
}

async fn process_order_notices(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    organizer: &crate::organizers::Organizer,
    db: &sea_orm::DatabaseConnection,
    id: &str,
) -> Result<()> {
    let (_, _, _, event_id) = admin_orders::payment_intent(db, id)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let cancelled = events::get(db, &event_id)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?
        .is_some_and(|event| event.status == "Cancelled");
    if cancelled {
        if let Err(error) = send_notice(ctx, registry, organizer, db, id, true).await {
            tracing::error!(%error, order_id = %id, "Cancellation notice remains queued");
        }
    }
    let progress = admin_orders::work_progress(db, id)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    if matches!(progress.refund_notice.as_str(), "Pending" | "Failed") {
        if let Err(error) = send_notice(ctx, registry, organizer, db, id, false).await {
            tracing::error!(%error, order_id = %id, "Refund notice remains queued");
        }
    }
    Ok(())
}

async fn due_orders(
    scope: &OrganizerDb,
) -> std::result::Result<Vec<sea_orm::QueryResult>, sea_orm::DbErr> {
    scope.db.query_all_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT id FROM orders WHERE refund_work_state IN ('Pending', 'Failed') OR cancel_notice_state IN ('Pending', 'Failed') OR refund_notice_state IN ('Pending', 'Failed') OR refund_claim_until <= unixepoch() OR (refund_claim_token IS NOT NULL AND refund_claim_until IS NULL) ORDER BY created_at, id".to_owned()
    )).await
}

#[derive(Clone, Copy)]
enum RefundWork {
    Refunds,
    Notices,
}

/// Refunds first, then notices. The background jobs run the two parts as separate loops so
/// that a slow mail server cannot delay a refund.
pub async fn run_refund_work(ctx: &AppContext, registry: &OrganizerRegistry) -> Result<()> {
    run_refunds(ctx, registry, Budget::REFUNDS).await?;
    run_refund_notices(ctx, registry, Budget::REFUNDS).await
}

pub async fn run_refunds(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    budget: Budget,
) -> Result<()> {
    run_due_orders(ctx, registry, budget, RefundWork::Refunds).await
}

pub async fn run_refund_notices(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    budget: Budget,
) -> Result<()> {
    run_due_orders(ctx, registry, budget, RefundWork::Notices).await
}

async fn run_due_orders(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    budget: Budget,
    work: RefundWork,
) -> Result<()> {
    let mut pass = Pass::new(budget);
    while let Some(organizers) = pass
        .next_round(registry)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?
    {
        for organizer in organizers {
            let Some((scope, mut turn)) = pass.turn(registry, &organizer).await else {
                continue;
            };
            let rows = match due_orders(&scope).await {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::error!(%error, organizer_id = %organizer.id, "Tenant refund work scan failed");
                    continue;
                }
            };
            for row in rows {
                let id: String = match row.try_get("", "id") {
                    Ok(id) => id,
                    Err(error) => {
                        tracing::error!(%error, organizer_id = %organizer.id, "Tenant refund work row failed");
                        continue;
                    }
                };
                match turn.admit(&id) {
                    Admission::Granted => {}
                    Admission::AlreadyAttempted => continue,
                    Admission::OutOfBudget => break,
                }
                let result = match work {
                    RefundWork::Refunds => {
                        process_order_refund(registry, &scope.organizer, &scope.db, &id, false)
                            .await
                    }
                    RefundWork::Notices => {
                        process_order_notices(ctx, registry, &scope.organizer, &scope.db, &id).await
                    }
                };
                if let Err(error) = result {
                    tracing::error!(%error, order_id = %id, "Order refund work failed");
                }
            }
        }
    }
    Ok(())
}

fn notice_retry_is_safe(error: &loco_rs::Error) -> bool {
    match error {
        loco_rs::Error::WithBacktrace { inner, .. } => notice_retry_is_safe(inner),
        loco_rs::Error::Smtp(smtp) => {
            if smtp.is_transient() || smtp.is_permanent() {
                return true;
            }
            let mut source = std::error::Error::source(smtp);
            while let Some(error) = source {
                if let Some(io) = error.downcast_ref::<std::io::Error>() {
                    return io.kind() == std::io::ErrorKind::ConnectionRefused;
                }
                source = error.source();
            }
            false
        }
        _ => true,
    }
}
