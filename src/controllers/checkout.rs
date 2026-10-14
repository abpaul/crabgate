use std::{collections::HashMap, sync::Arc};

use axum::{
    extract::{Form, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Extension,
};
use chrono::TimeZone;
use chrono_tz::Tz;
use loco_rs::prelude::*;
use sea_orm::{SqliteTransactionMode, TransactionOptions, TransactionTrait};
use serde::Deserialize;
use serde_json::json;

use crate::{
    admin_orders, events, hosted,
    inventory::{self, InventoryError, Selection},
    links,
    mailers::ticket::TicketMailer,
    orders::{self, OrderError, PaymentOutcome},
    organizers::{Edition, Organizer, OrganizerDb, OrganizerRegistry},
    questions, seats, settings,
    stripe::StripeLineItem,
};

#[derive(Deserialize)]
struct OrderLink {
    expires_at: i64,
    sig: String,
    tickets_resent: Option<String>,
}

pub fn origin(ctx: &AppContext, organizer: &Organizer) -> String {
    if Edition::from_environment() == Edition::Hosted {
        hosted::origin(Some(&organizer.subdomain))
    } else {
        ctx.config.server.full_url()
    }
}

fn order_url(ctx: &AppContext, organizer: &Organizer, order_id: &str, now: i64) -> Result<String> {
    let expires_at = now + 365 * 24 * 60 * 60;
    let sig = links::sign_order(&organizer.id, order_id, expires_at)
        .ok_or_else(|| loco_rs::Error::string("Order links are not configured"))?;
    Ok(format!(
        "{}/orders/{order_id}?expires_at={expires_at}&sig={sig}",
        origin(ctx, organizer)
    ))
}

fn order_error(error: OrderError) -> Response {
    match error {
        OrderError::HoldNotFound => StatusCode::NOT_FOUND.into_response(),
        OrderError::HoldExpired => (StatusCode::GONE, error.to_string()).into_response(),
        OrderError::InvalidEmail | OrderError::MissingName => {
            (StatusCode::BAD_REQUEST, error.to_string()).into_response()
        }
        _ => {
            tracing::error!(%error, "Order error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn inventory_error(error: InventoryError) -> Response {
    match error {
        InventoryError::Database(error) => {
            tracing::error!(%error, "Inventory error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        _ => (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    }
}

async fn recovery(
    organizer: &Organizer,
    db: &sea_orm::DatabaseConnection,
    view: &TeraView,
    status: StatusCode,
    title: &str,
    message: &str,
    action: Option<(&str, &str)>,
) -> Result<Response> {
    recovery_with_checkout(
        organizer,
        db,
        view,
        status,
        title,
        message,
        RecoveryLinks {
            action,
            unpaid_checkout: None,
        },
    )
    .await
}

struct RecoveryLinks<'a> {
    action: Option<(&'a str, &'a str)>,
    unpaid_checkout: Option<&'a str>,
}

async fn recovery_with_checkout(
    organizer: &Organizer,
    db: &sea_orm::DatabaseConnection,
    view: &TeraView,
    status: StatusCode,
    title: &str,
    message: &str,
    links: RecoveryLinks<'_>,
) -> Result<Response> {
    let contact = settings::contact(db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let mut response = format::render().view(
        view,
        "shop/checkout-state.html",
        data!({
            "organizer_name": organizer.name, "contact": contact,
            "title": title, "message": message,
            "action_url": links.action.map(|(url, _)| url),
            "action_label": links.action.map(|(_, label)| label),
            "unpaid_checkout_url": links.unpaid_checkout
        }),
    )?;
    *response.status_mut() = status;
    Ok(response)
}

async fn closed_checkout(
    organizer: &Organizer,
    db: &sea_orm::DatabaseConnection,
    view: &TeraView,
    hold: &orders::HoldDetails,
    message: &str,
) -> Result<Response> {
    let event_url = format!("/events/{}", hold.event_id);
    recovery(
        organizer,
        db,
        view,
        StatusCode::GONE,
        "Checkout closed",
        message,
        Some((&event_url, "Back to event")),
    )
    .await
}

async fn start(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    OrganizerDb { organizer, db }: OrganizerDb,
    ViewEngine(view): ViewEngine<TeraView>,
    Form(fields): Form<Vec<(String, String)>>,
) -> Result<Response> {
    let mut form = HashMap::new();
    for (key, value) in fields {
        if key.len() > 64 || value.len() > 128 || form.insert(key, value).is_some() {
            return Ok(StatusCode::BAD_REQUEST.into_response());
        }
    }
    if Edition::from_environment() == Edition::Hosted {
        let state = hosted::access_state(&ctx.db, &organizer.id)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        if !hosted::subscription_allows_sales(&state, hosted::now()) {
            return Ok((
                StatusCode::FORBIDDEN,
                "Organizer subscription is not active",
            )
                .into_response());
        }
    }
    let Some(event_id) = form.get("event_id") else {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    };
    let ticket_types = events::list_ticket_types(&db, event_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let mut selections = Vec::new();
    for (key, value) in &form {
        if let Some(ticket_type_id) = key.strip_prefix("quantity_") {
            if !ticket_types.iter().any(|kind| kind.id == ticket_type_id) {
                return Ok(StatusCode::BAD_REQUEST.into_response());
            }
            let Ok(quantity) = value.parse::<i64>() else {
                return Ok(StatusCode::BAD_REQUEST.into_response());
            };
            if !(0..=100).contains(&quantity) {
                return Ok(StatusCode::BAD_REQUEST.into_response());
            }
            if quantity > 0 {
                selections.push(Selection {
                    ticket_type_id: ticket_type_id.to_owned(),
                    quantity,
                });
            }
        }
    }
    let seat_ids = form
        .keys()
        .filter_map(|key| key.strip_prefix("seat_").map(str::to_owned))
        .collect::<Vec<_>>();
    if !seat_ids.is_empty() {
        selections = match seats::selections_for_ids(&db, event_id, &seat_ids).await {
            Ok(selections) => selections,
            Err(error) => return Ok(inventory_error(error.into())),
        };
    }
    let hold = match inventory::reserve_with_options(
        &db,
        event_id,
        &selections,
        form.get("discount_code").map(String::as_str),
        &seat_ids,
        inventory::now(),
    )
    .await
    {
        Ok(hold) => hold,
        Err(error) => return Ok(inventory_error(error)),
    };
    if hold.total_cents > 0 {
        let currency = settings::currency(&db)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        let cipher = registry
            .cipher(&organizer)
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        let stripe = match settings::stripe_client(&db, &cipher).await {
            Ok(stripe) => stripe,
            Err(_) => {
                orders::expire_hold(&db, &hold.id)
                    .await
                    .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
                return recovery(&organizer, &db, &view, StatusCode::SERVICE_UNAVAILABLE,
                    "Payment unavailable", "This shop can't take payments yet. No booking was created. Contact the organizer or return to the event to try later.",
                    Some((&format!("/events/{}", hold.event_id), "Back to event"))).await;
            }
        };
        let details = orders::hold_details(&db, &hold.id)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        let event = events::get(&db, &details.event_id)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?
            .ok_or_else(|| loco_rs::Error::string("Event not found"))?;
        let description = format!(
            "{}, {}, {}",
            events::formatted_date(&event),
            event.start_time,
            event.venue
        );
        let items = if details.discount_cents == 0 {
            details
                .items
                .iter()
                .map(|item| StripeLineItem {
                    name: format!("{}, {}", event.title, item.name),
                    description: description.clone(),
                    price_cents: item.price_cents,
                    quantity: item.quantity,
                })
                .collect::<Vec<_>>()
        } else {
            let mut remaining = details.discount_cents;
            let mut lines = Vec::new();
            for item in &details.items {
                for _ in 0..item.quantity {
                    let reduction = remaining.min(item.price_cents);
                    remaining -= reduction;
                    lines.push(StripeLineItem {
                        name: format!("{}, {}", event.title, item.name),
                        description: description.clone(),
                        price_cents: item.price_cents - reduction,
                        quantity: 1,
                    });
                }
            }
            lines
        };
        let base = origin(&ctx, &organizer);
        let session = match stripe
            .create_session(
                &hold.id,
                &currency,
                hold.expires_at,
                &format!(
                    "{base}/checkout/{}/confirmation?session_id={{CHECKOUT_SESSION_ID}}",
                    hold.id
                ),
                &format!("{base}/checkout/{}", hold.id),
                &items,
            )
            .await
        {
            Ok(session) => session,
            Err(error) => {
                tracing::error!(%error, "Stripe Checkout Session failed");
                orders::expire_hold(&db, &hold.id)
                    .await
                    .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
                if matches!(error, crate::stripe::StripeError::InvalidExpiry) {
                    return closed_checkout(&organizer, &db, &view, &orders::hold_details(&db, &hold.id).await.map_err(|error| loco_rs::Error::string(&error.to_string()))?, "There is not enough time left to start payment. The tickets have been released. Please choose them again.").await;
                }
                return recovery(&organizer, &db, &view, StatusCode::BAD_GATEWAY,
                    "Payment could not start", "The payment couldn't start. The tickets have been released. If you already paid, do not book again; contact the organizer. Otherwise return to the event and try again.",
                    Some((&format!("/events/{}", hold.event_id), "Back to event"))).await;
            }
        };
        if session.expires_at != Some(hold.expires_at) {
            tracing::error!(hold_id = hold.id, expected = hold.expires_at, actual = ?session.expires_at, "Stripe Checkout deadline mismatch");
            orders::expire_hold(&db, &hold.id)
                .await
                .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
            if let Err(error) = stripe.expire_session(&session.id).await {
                tracing::error!(%error, "Could not expire a mismatched Checkout Session");
            }
            return recovery(&organizer, &db, &view, StatusCode::BAD_GATEWAY, "Payment could not start", "The payment deadline could not be confirmed. The tickets have been released. Please choose them again. If you already paid, do not book again; contact the organizer.", Some((&format!("/events/{}", hold.event_id), "Back to event"))).await;
        }
        if let Err(error) =
            orders::attach_session_with_url(&db, &hold.id, &session.id, Some(&session.url)).await
        {
            if let Err(expire_error) = stripe.expire_session(&session.id).await {
                tracing::error!(%expire_error, "Could not expire an unattached Checkout Session");
            }
            return Ok(order_error(error));
        }
    }
    Ok(Redirect::to(&format!("/checkout/{}", hold.id)).into_response())
}

async fn details(
    OrganizerDb { organizer, db }: OrganizerDb,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
) -> Result<Response> {
    render_details(&organizer, &db, &view, &id, None, None).await
}

async fn render_details(
    organizer: &Organizer,
    db: &sea_orm::DatabaseConnection,
    view: &TeraView,
    id: &str,
    error: Option<&str>,
    values: Option<&HashMap<String, String>>,
) -> Result<Response> {
    let hold = match orders::hold_details(db, id).await {
        Ok(hold) => hold,
        Err(OrderError::HoldNotFound) => return recovery(organizer, db, view, StatusCode::NOT_FOUND,
            "Checkout not found", "This checkout link is missing or invalid. If you already paid, contact the organizer before booking again.", None).await,
        Err(error) => return Ok(order_error(error)),
    };
    if orders::order_id_for_hold(db, id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .is_some()
    {
        return Ok(Redirect::to(&format!("/checkout/{id}/confirmation")).into_response());
    }
    let currency = settings::currency(db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let contact = settings::contact(db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    if hold.expires_at > hold.created_at + inventory::HOLD_DURATION_SECONDS {
        orders::expire_hold(db, id)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        return closed_checkout(organizer, db, view, &hold, "This checkout uses an old payment deadline. The tickets have been released. Please choose them again. If you already paid, do not book again; contact the organizer.").await;
    }
    if hold.status != "Open" || hold.expires_at <= orders::now() {
        return closed_checkout(organizer, db, view, &hold, "This checkout has expired or closed. Please choose them again if you did not pay. If you already paid, do not book again; reload the confirmation page or contact the organizer.").await;
    }
    let event = events::get(db, &hold.event_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .ok_or_else(|| loco_rs::Error::string("Event not found"))?;
    let mut attendees = Vec::new();
    let value = |key: &str| {
        values
            .and_then(|values| values.get(key))
            .cloned()
            .unwrap_or_default()
    };
    let mut held_seats = seats::list_for_hold(db, &hold.id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    for item in &hold.items {
        for _ in 0..item.quantity {
            let ordinal = attendees.len();
            let seat = held_seats
                .iter()
                .position(|seat| seat.ticket_type_id == item.ticket_type_id)
                .map(|index| held_seats.remove(index))
                .map(|seat| seats::display(&seat.section, &seat.row, &seat.label));
            attendees.push(json!({"ordinal": ordinal, "number": ordinal + 1, "ticket_type": item.name, "seat": seat, "name": value(&format!("name_{ordinal}"))}));
        }
    }
    let items = hold
        .items
        .iter()
        .map(|item| {
            json!({
                "name": item.name, "quantity": item.quantity,
                "total": events::format_price(item.quantity * item.price_cents, &currency)
            })
        })
        .collect::<Vec<_>>();
    let questions = questions::list_for_hold(db, &hold.id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let mut order_questions = Vec::new();
    let mut attendee_questions = Vec::new();
    for question in &questions {
        if question.scope == "Order" {
            let field = questions::field_name(question, 0);
            order_questions
                .push(json!({"question": question, "field": field, "value": value(&field)}));
        } else {
            for ordinal in 0..attendees.len() {
                let field = questions::field_name(question, ordinal);
                attendee_questions.push(json!({"question": question, "field": field, "value": value(&field), "number": ordinal + 1}));
            }
        }
    }
    let tz: Tz = event
        .timezone
        .parse()
        .map_err(|_| loco_rs::Error::string("Invalid event time zone"))?;
    let ends_local = tz
        .timestamp_opt(hold.expires_at, 0)
        .single()
        .ok_or_else(|| loco_rs::Error::string("Invalid hold time"))?
        .format("%d %b %H:%M:%S %Z")
        .to_string();
    let booking_terms = crate::booking_terms::display(db, &hold.id, orders::now())
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    format::render().view(view, "shop/checkout.html", data!({
        "organizer_name": organizer.name, "contact": contact,
        "event": {"id": event.id, "title": event.title, "venue": event.venue, "formatted_date": events::formatted_date(&event), "attendee_name_required": event.attendee_name_required},
        "hold": {"id": hold.id, "ends_local": ends_local, "expires_at": hold.expires_at, "total_cents": hold.total_cents,
            "discount_code": hold.discount_code,
            "discount": events::format_price(hold.discount_cents, &currency),
            "subtotal": events::format_price(hold.subtotal_cents, &currency)},
        "items": items, "attendees": attendees,
        "order_questions": order_questions, "attendee_questions": attendee_questions, "booking_terms": booking_terms,
        "total": events::format_price(hold.total_cents, &currency), "error": error, "email": value("email")
    }))
}

async fn submit(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    OrganizerDb { organizer, db }: OrganizerDb,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Response> {
    let transaction = db
        .begin_with_options(TransactionOptions {
            sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
            ..Default::default()
        })
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    if orders::order_id_for_hold(&transaction, &id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .is_some()
    {
        transaction
            .rollback()
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        return Ok(Redirect::to(&format!("/checkout/{id}/confirmation")).into_response());
    }
    let hold = match orders::hold_details(&transaction, &id).await {
        Ok(hold) => hold,
        Err(OrderError::HoldNotFound) => {
            transaction
                .rollback()
                .await
                .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
            return recovery(&organizer, &db, &view, StatusCode::NOT_FOUND,
                "Checkout not found", "This checkout link is missing or invalid. If you already paid, contact the organizer before booking again.", None).await;
        }
        Err(error) => return Ok(order_error(error)),
    };
    if hold.expires_at > hold.created_at + inventory::HOLD_DURATION_SECONDS {
        transaction
            .rollback()
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        orders::expire_hold(&db, &id)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        return closed_checkout(&organizer, &db, &view, &hold, "This checkout uses an old payment deadline. The tickets have been released. Please choose them again. If you already paid, do not book again; contact the organizer.").await;
    }
    if hold.status != "Open" || hold.expires_at <= orders::now() {
        transaction
            .rollback()
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        return closed_checkout(&organizer, &db, &view, &hold, "This checkout has expired or closed. Please choose them again if you did not pay. If you already paid, do not book again; reload the confirmation page or contact the organizer.").await;
    }
    let event = events::get(&transaction, &hold.event_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .ok_or_else(|| loco_rs::Error::string("Event not found"))?;
    let quantity: usize = hold.items.iter().map(|item| item.quantity as usize).sum();
    let questions = questions::list_for_hold(&transaction, &hold.id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let names = (0..quantity)
        .map(|ordinal| {
            form.get(&format!("name_{ordinal}"))
                .cloned()
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    let cipher = registry
        .cipher(&organizer)
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let Some(email) = form.get("email") else {
        transaction
            .rollback()
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        return Ok((StatusCode::BAD_REQUEST, "Please enter your email address.").into_response());
    };
    match questions::save_answers_in(&transaction, &cipher, &hold.id, &questions, quantity, &form)
        .await
    {
        Ok(()) => {}
        Err(questions::QuestionError::InvalidAnswer) => {
            transaction
                .rollback()
                .await
                .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
            let mut response = render_details(
                &organizer,
                &db,
                &view,
                &id,
                Some("Please answer every required question."),
                Some(&form),
            )
            .await?;
            *response.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(response);
        }
        Err(error) => {
            transaction
                .rollback()
                .await
                .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
            return Err(loco_rs::Error::string(&error.to_string()));
        }
    }
    if let Err(error) = orders::save_checkout_details_in(
        &transaction,
        &cipher,
        &hold,
        email,
        &names,
        event.attendee_name_required,
        orders::now(),
        Some(&crate::booking_terms::Submission {
            token: form
                .get("booking_terms_token")
                .map(String::as_str)
                .unwrap_or_default(),
            accepted: form
                .get("booking_terms_ack")
                .is_some_and(|value| value == "yes"),
        }),
    )
    .await
    {
        transaction
            .rollback()
            .await
            .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
        if matches!(
            error,
            OrderError::InvalidEmail
                | OrderError::MissingName
                | OrderError::BookingTerms(
                    crate::booking_terms::BookingTermsError::ReviewRequired
                        | crate::booking_terms::BookingTermsError::AcknowledgementRequired
                )
        ) {
            let mut response = render_details(
                &organizer,
                &db,
                &view,
                &id,
                Some(&error.to_string()),
                Some(&form),
            )
            .await?;
            *response.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(response);
        }
        if matches!(error, OrderError::HoldExpired) {
            return closed_checkout(&organizer, &db, &view, &hold, "This checkout has expired or closed. Please choose them again if you did not pay. If you already paid, do not book again; contact the organizer.").await;
        }
        return Ok(order_error(error));
    }
    transaction
        .commit()
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    if hold.total_cents == 0 {
        let currency = settings::currency(&db)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        let session_id = format!("free_{}", hold.id);
        orders::attach_session(&db, &hold.id, &session_id, orders::now())
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        let result =
            orders::complete_payment_in_currency(&db, &session_id, "", 0, &currency, orders::now())
                .await
                .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        if let PaymentOutcome::Paid(order_id) = result {
            if let Err(error) = send_order_email(&ctx, &registry, &organizer, &db, &order_id).await
            {
                tracing::error!(%error, order_id, "Ticket email remains queued");
            }
        }
        return Ok(Redirect::to(&format!("/checkout/{id}/confirmation")).into_response());
    }
    let session_url = orders::session_url(&db, &hold.id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let Some(session_url) = session_url else {
        orders::expire_hold(&db, &hold.id)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        return closed_checkout(&organizer, &db, &view, &hold, "This checkout has no payment session. The tickets have been released. Please choose them again. If you already paid, do not book again; contact the organizer.").await;
    };
    Ok(Redirect::to(&session_url).into_response())
}

async fn cancel(
    ViewEngine(view): ViewEngine<TeraView>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    OrganizerDb { organizer, db }: OrganizerDb,
    Path(id): Path<String>,
) -> Result<Response> {
    let hold = match orders::hold_details(&db, &id).await {
        Ok(hold) => hold,
        Err(error) => return Ok(order_error(error)),
    };
    if orders::order_id_for_hold(&db, &id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .is_some()
    {
        return Ok(Redirect::to(&format!("/checkout/{id}/confirmation")).into_response());
    }
    if hold.status == "Open" {
        orders::expire_hold(&db, &id)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        if let Some(session_id) = session_for_hold(&db, &id).await? {
            let cipher = registry
                .cipher(&organizer)
                .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
            let cancellation = match settings::stripe_client(&db, &cipher).await {
                Ok(stripe) => stripe.expire_session(&session_id).await,
                Err(error) => {
                    tracing::error!(%error, "Checkout cancellation provider unavailable");
                    return recovery(&organizer, &db, &view, StatusCode::BAD_GATEWAY, "Cancellation not confirmed",
                        "The tickets have been released, but Stripe could not confirm payment cancellation. If you paid, do not book again; check your confirmation or contact the organizer.",
                        Some((&format!("/checkout/{id}/confirmation"), "Check payment status"))).await;
                }
            };
            if let Err(error) = cancellation {
                tracing::error!(%error, "Checkout cancellation could not be confirmed by Stripe");
                return recovery(&organizer, &db, &view, StatusCode::BAD_GATEWAY, "Cancellation not confirmed", "The tickets have been released, but Stripe could not confirm payment cancellation. If you paid, do not book again; check your confirmation or contact the organizer.", Some((&format!("/checkout/{id}/confirmation"), "Check payment status"))).await;
            }
        }
    }
    Ok(Redirect::to(&format!("/events/{}", hold.event_id)).into_response())
}

async fn session_for_hold(db: &sea_orm::DatabaseConnection, id: &str) -> Result<Option<String>> {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT stripe_session_id FROM holds WHERE id = ?",
            [id.into()],
        ))
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    row.map(|row| row.try_get::<Option<String>>("", "stripe_session_id"))
        .transpose()
        .map(Option::flatten)
        .map_err(|error| loco_rs::Error::string(&error.to_string()))
}

async fn confirmation(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    OrganizerDb { organizer, db }: OrganizerDb,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
) -> Result<Response> {
    let Some(order_id) = orders::order_id_for_hold(&db, &id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
    else {
        let hold = match orders::hold_details(&db, &id).await {
            Ok(hold) => hold,
            Err(OrderError::HoldNotFound) => return recovery(&organizer, &db, &view, StatusCode::NOT_FOUND,
                "Checkout not found", "This checkout link is missing or invalid. If you already paid, contact the organizer before booking again.", None).await,
            Err(error) => return Ok(order_error(error)),
        };
        let confirmation_url = format!("/checkout/{id}/confirmation");
        if hold.status == "RefundPending" || hold.status == "Refunded" {
            return recovery(&organizer, &db, &view, StatusCode::OK,
                if hold.status == "Refunded" { "Payment refunded" } else { "Refund pending" },
                if hold.status == "Refunded" { "This payment was refunded. No tickets were issued. Contact the organizer if you need help with the refund." } else { "The payment arrived after tickets could no longer be issued. The refund is not confirmed yet. Do not book again while this is unresolved; contact the organizer." },
                Some((&confirmation_url, "Check status again"))).await;
        }
        if hold.status != "Open" || hold.expires_at <= orders::now() {
            return closed_checkout(&organizer, &db, &view, &hold,
                "This checkout has expired or closed. Please choose them again if you did not pay. If you already paid, do not book again; reload this confirmation page or contact the organizer.").await;
        }
        if hold.total_cents == 0 {
            return recovery(
                &organizer,
                &db,
                &view,
                StatusCode::OK,
                "Checkout not complete",
                "No tickets have been issued. Complete your details to get your tickets.",
                Some((&format!("/checkout/{id}"), "Continue checkout")),
            )
            .await;
        }
        return recovery_with_checkout(&organizer, &db, &view, StatusCode::ACCEPTED, "Payment not confirmed",
            "We have not confirmed your payment yet. Reload this page in a moment. If you paid, do not book again or start another payment; contact the organizer if this does not change. If you did not pay, return to your existing checkout to continue before its deadline.",
            RecoveryLinks { action: Some((&confirmation_url, "Check payment status again")), unpaid_checkout: Some(&format!("/checkout/{id}")) }).await;
    };
    render_order(
        &ctx,
        &registry,
        &organizer,
        &db,
        &view,
        &order_id,
        OrderPresentation {
            is_confirmation: true,
            tickets_resent: false,
        },
    )
    .await
}

async fn show_order(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    OrganizerDb { organizer, db }: OrganizerDb,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(id): Path<String>,
    link: std::result::Result<Query<OrderLink>, axum::extract::rejection::QueryRejection>,
) -> Result<Response> {
    let link = match link {
        Ok(Query(link)) => link,
        Err(_) => return recovery(&organizer, &db, &view, StatusCode::BAD_REQUEST,
            "Invalid order link", "This order link is incomplete or invalid. Use the full link from your ticket email, or contact the organizer. Do not book again to recover an existing order.", None).await,
    };
    if !links::verify_order(
        &organizer.id,
        &id,
        link.expires_at,
        &link.sig,
        orders::now(),
    ) {
        return recovery(&organizer, &db, &view, StatusCode::FORBIDDEN, "Order link unavailable", "This order link is invalid or has expired. Use a current link from your ticket email, or contact the organizer. Do not book again to recover an existing order.", None).await;
    }
    render_order(
        &ctx,
        &registry,
        &organizer,
        &db,
        &view,
        &id,
        OrderPresentation {
            is_confirmation: false,
            tickets_resent: link.tickets_resent.as_deref() == Some("1"),
        },
    )
    .await
}

struct OrderPresentation {
    is_confirmation: bool,
    tickets_resent: bool,
}

async fn render_order(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    organizer: &Organizer,
    db: &sea_orm::DatabaseConnection,
    view: &TeraView,
    id: &str,
    presentation: OrderPresentation,
) -> Result<Response> {
    let cipher = registry
        .cipher(organizer)
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let order = match orders::get_order(db, &cipher, id).await {
        Ok(order) => order,
        Err(error) => return Ok(order_error(error)),
    };
    let progress = admin_orders::work_progress(db, id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let refund_message = match progress.refund.as_str() {
        "Completed" | "Pending" | "Failed" | "Reconciliation required" if order.total_cents == 0 => Some("No money was paid for this order. No money will be returned."),
        "Completed" => Some("This order was refunded in full."),
        "Reconciliation required" => Some("The organizer needs to check the amount returned for this order. A full refund is not confirmed. Contact the organizer for an update."),
        "Pending" | "Failed" => Some("Your refund has not been confirmed. The organizer handles refund retries. Contact the organizer for an update."),
        _ if order.status == "RefundPending" || order.status == "Refunded" => Some("Your refund has not been confirmed. The organizer handles refund retries. Contact the organizer for an update."),
        _ => None,
    };
    let currency = settings::currency(db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let contact = settings::contact(db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let event = events::get(db, &order.event_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .ok_or_else(|| loco_rs::Error::string("Event not found"))?;
    let ticket_types = events::list_ticket_types(db, &event.id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let order_url = order_url(ctx, organizer, &order.id, orders::now())?;
    let first_name = order
        .tickets
        .first()
        .and_then(|ticket| ticket.attendee_name.as_deref())
        .and_then(|name| name.split_whitespace().next());
    let tickets = order.tickets.iter().enumerate().map(|(index, ticket)| json!({
        "id": ticket.id, "number": index + 1, "code": ticket.code,
        "attendee_name": ticket.attendee_name,
        "seat": ticket.seat,
        "status": ticket.status,
        "ticket_type": ticket_types.iter().find(|item| item.id == ticket.ticket_type_id).map(|item| item.name.as_str()).unwrap_or("Ticket"),
        "qr_url": format!("/orders/{id}/qr/{}?{}", ticket.id, order_url.split('?').nth(1).unwrap_or_default())
    })).collect::<Vec<_>>();
    let query = order_url.split('?').nth(1).unwrap_or_default();
    let initial_email_accepted = orders::email_sent(db, id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    format::render().view(view, "shop/order.html", data!({
        "organizer_name": organizer.name, "contact": contact,
        "is_confirmation": presentation.is_confirmation,
        "tickets_resent": presentation.tickets_resent,
        "initial_email_accepted": initial_email_accepted,
        "first_name": first_name,
        "refund_message": refund_message,
        "order": {"number": order.number, "status": order.status, "buyer_email": order.buyer_email, "total": events::format_price(order.total_cents, &currency), "ticket_count": order.tickets.len()},
        "event": {"title": event.title, "status": event.status, "formatted_date": events::formatted_date(&event), "start_time": event.start_time, "end_time": event.end_time, "venue": event.venue, "address": event.address, "attendee_name_required": event.attendee_name_required},
        "tickets": tickets,
        "order_url": order_url,
        "resend_url": format!("/orders/{id}/resend?{query}"),
        "names_url": format!("/orders/{id}/names?{query}"),
        "can_edit_names": order.status == "Paid" && event.status != "Cancelled" && event.starts_at > orders::now(),
        "can_resend": order.status == "Paid" && !order.buyer_email.is_empty() && event.status != "Cancelled"
    }))
}

pub async fn send_order_email(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    organizer: &Organizer,
    db: &sea_orm::DatabaseConnection,
    order_id: &str,
) -> Result<()> {
    deliver_order_email(
        ctx,
        registry,
        organizer,
        db,
        order_id,
        TicketEmailIntent::Initial,
    )
    .await
}

pub async fn resend_order_email(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    organizer: &Organizer,
    db: &sea_orm::DatabaseConnection,
    order_id: &str,
) -> Result<()> {
    deliver_order_email(
        ctx,
        registry,
        organizer,
        db,
        order_id,
        TicketEmailIntent::Resend,
    )
    .await
}

#[derive(PartialEq)]
enum TicketEmailIntent {
    Initial,
    Resend,
}

pub fn ticket_email_failure(error: loco_rs::Error) -> Response {
    tracing::error!(%error, "Ticket email acceptance could not be confirmed");
    (StatusCode::BAD_GATEWAY, "Ticket email acceptance could not be confirmed. Check with the organizer before retrying; another attempt may deliver a duplicate.").into_response()
}

fn ticket_email_refused(error: &loco_rs::Error) -> bool {
    match error {
        loco_rs::Error::WithBacktrace { inner, .. } => ticket_email_refused(inner),
        loco_rs::Error::Smtp(smtp) => smtp.is_permanent(),
        _ => false,
    }
}

/// The first Ticket email is automatic: it needs a claim, and its result decides whether the
/// retry job tries again. A resend is an explicit request: it takes no claim and counts as no
/// attempt, and its acceptance is the receipt only for an Order whose automatic attempts stopped.
async fn deliver_order_email(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    organizer: &Organizer,
    db: &sea_orm::DatabaseConnection,
    order_id: &str,
    intent: TicketEmailIntent,
) -> Result<()> {
    if intent == TicketEmailIntent::Resend {
        compose_order_email(ctx, registry, organizer, db, order_id, None).await?;
        if let Err(error) = orders::record_resend_receipt(db, order_id, orders::now()).await {
            tracing::error!(%error, order_id, "Ticket email resend receipt was not saved");
        }
        return Ok(());
    }
    if orders::email_sent(db, order_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
    {
        return Ok(());
    }
    let Some(attempt) = orders::claim_email(db, order_id, orders::now())
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
    else {
        return Ok(());
    };
    let result = compose_order_email(ctx, registry, organizer, db, order_id, Some(attempt)).await;
    let Err(error) = &result else {
        orders::mark_email_sent(db, order_id, attempt, orders::now())
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        return Ok(());
    };
    let failure = if ticket_email_refused(error) {
        orders::EmailFailure::Refused
    } else {
        orders::EmailFailure::Unconfirmed
    };
    match orders::record_email_failure(db, order_id, attempt, failure).await {
        Ok(true) => ticket_email_stopped(&organizer.id, order_id),
        Ok(false) => {}
        Err(error) => {
            tracing::error!(%error, order_id, "Ticket email attempt result was not saved");
        }
    }
    result
}

pub fn ticket_email_stopped(organizer_id: &str, order_id: &str) {
    tracing::error!(
        organizer_id,
        order_id,
        "Ticket email automatic attempts stopped; use Resend tickets"
    );
}

/// `attempt` is the claimed automatic attempt; a resend has none. A claimed attempt reaches
/// SMTP only while it still owns its claim.
async fn compose_order_email(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    organizer: &Organizer,
    db: &sea_orm::DatabaseConnection,
    order_id: &str,
    attempt: Option<i64>,
) -> Result<()> {
    let cipher = registry
        .cipher(organizer)
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let order = orders::get_order(db, &cipher, order_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    if order.status != "Paid" || order.buyer_email.is_empty() {
        if let (Some(attempt), true) = (attempt, order.buyer_email.is_empty()) {
            orders::record_email_failure(db, order_id, attempt, orders::EmailFailure::NoRecipient)
                .await
                .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        }
        return Err(loco_rs::Error::string("This order has no tickets to send."));
    }
    let event = events::get(db, &order.event_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .ok_or_else(|| loco_rs::Error::string("Event not found"))?;
    if event.status == "Cancelled" {
        return Err(loco_rs::Error::string(
            "This event is cancelled; its tickets cannot be resent.",
        ));
    }
    let order_url = order_url(ctx, organizer, order_id, orders::now())?;
    let ticket_types = events::list_ticket_types(db, &order.event_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let tickets = order.tickets.iter().map(|ticket| json!({
        "code": ticket.code, "attendee_name": ticket.attendee_name, "seat": ticket.seat,
        "ticket_type": ticket_types.iter().find(|item| item.id == ticket.ticket_type_id).map_or("Ticket", |item| item.name.as_str()), "qr_url": format!("{}/orders/{order_id}/qr/{}?{}", origin(ctx, organizer), ticket.id, order_url.split('?').nth(1).unwrap_or_default())
    })).collect::<Vec<_>>();
    let contact = settings::contact(db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    if let Some(attempt) = attempt {
        if !orders::email_claim_current(db, order_id, attempt)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        {
            return Err(loco_rs::Error::string(
                "The Ticket email attempt lost its claim before the send; nothing was sent.",
            ));
        }
    }
    TicketMailer::send_confirmation(
        ctx,
        &order.buyer_email,
        &organizer.name,
        &order.number,
        &order_url,
        json!(tickets),
        (!contact.reply_to.is_empty()).then_some(contact.reply_to.as_str()),
    )
    .await
}

async fn qr(
    OrganizerDb { organizer, db }: OrganizerDb,
    Path((order_id, ticket_id)): Path<(String, String)>,
    Query(link): Query<OrderLink>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
) -> Result<Response> {
    if !links::verify_order(
        &organizer.id,
        &order_id,
        link.expires_at,
        &link.sig,
        orders::now(),
    ) {
        return Ok(StatusCode::FORBIDDEN.into_response());
    }
    let cipher = registry
        .cipher(&organizer)
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let order = orders::get_order(&db, &cipher, &order_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let Some(ticket) = order.tickets.iter().find(|ticket| ticket.id == ticket_id) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    if order.status != "Paid" || ticket.status != "Valid" {
        return Ok(StatusCode::GONE.into_response());
    }
    let png = crate::qr::png(ticket.code.as_bytes())
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        png,
    )
        .into_response())
}

async fn resend(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    OrganizerDb { organizer, db }: OrganizerDb,
    Path(id): Path<String>,
    Query(link): Query<OrderLink>,
) -> Result<Response> {
    if !links::verify_order(
        &organizer.id,
        &id,
        link.expires_at,
        &link.sig,
        orders::now(),
    ) {
        return Ok(StatusCode::FORBIDDEN.into_response());
    }
    let cipher = registry
        .cipher(&organizer)
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let order = orders::get_order(&db, &cipher, &id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    if order.status != "Paid" || order.buyer_email.is_empty() {
        return Ok((
            StatusCode::FORBIDDEN,
            "Tickets can only be resent for a paid order with a recipient email.",
        )
            .into_response());
    }
    let event = events::get(&db, &order.event_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    if event.is_none_or(|event| event.status == "Cancelled") {
        return Ok((
            StatusCode::FORBIDDEN,
            "Tickets cannot be resent for a cancelled or missing event.",
        )
            .into_response());
    }
    if !orders::claim_resend(&db, &id, orders::now())
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
    {
        return Ok((StatusCode::TOO_MANY_REQUESTS, "Ticket email limit reached. You can request up to three resends per hour. Try again next hour.").into_response());
    }
    if let Err(error) = resend_order_email(&ctx, &registry, &organizer, &db, &id).await {
        return Ok(ticket_email_failure(error));
    }
    Ok(Redirect::to(&format!(
        "/orders/{id}?expires_at={}&sig={}&tickets_resent=1",
        link.expires_at, link.sig
    ))
    .into_response())
}

async fn names(
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    OrganizerDb { organizer, db }: OrganizerDb,
    Path(id): Path<String>,
    Query(link): Query<OrderLink>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Response> {
    if !links::verify_order(
        &organizer.id,
        &id,
        link.expires_at,
        &link.sig,
        orders::now(),
    ) {
        return Ok(StatusCode::FORBIDDEN.into_response());
    }
    let cipher = registry
        .cipher(&organizer)
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let order = orders::get_order(&db, &cipher, &id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    if order.status != "Paid" {
        return Ok(StatusCode::FORBIDDEN.into_response());
    }
    let event = events::get(&db, &order.event_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .ok_or_else(|| loco_rs::Error::string("Event not found"))?;
    let names = order
        .tickets
        .iter()
        .map(|ticket| {
            (
                ticket.id.clone(),
                form.get(&format!("name_{}", ticket.id))
                    .cloned()
                    .unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    if let Err(error) = orders::update_attendee_names(
        &db,
        &cipher,
        &order,
        &names,
        event.starts_at,
        event.attendee_name_required,
        orders::now(),
    )
    .await
    {
        return Ok(order_error(error));
    }
    Ok(Redirect::to(&format!(
        "/orders/{id}?expires_at={}&sig={}",
        link.expires_at, link.sig
    ))
    .into_response())
}

pub fn routes() -> Routes {
    Routes::new()
        .add("/checkout", post(start))
        .add("/checkout/{id}", get(details).post(submit))
        .add("/checkout/{id}/cancel", post(cancel))
        .add("/checkout/{id}/confirmation", get(confirmation))
        .add("/orders/{id}", get(show_order))
        .add("/orders/{id}/qr/{ticket_id}", get(qr))
        .add("/orders/{id}/resend", post(resend))
        .add("/orders/{id}/names", post(names))
}
