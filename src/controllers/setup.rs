use std::sync::Arc;

use axum::{
    extract::{Form, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Extension, Json,
};
use chrono::Utc;
use loco_rs::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde::Deserialize;
use serde_json::json;

use crate::{
    auth::AdminOrganizer,
    controllers::checkout::origin,
    hosted,
    mailers::setup::SetupMailer,
    organizers::{Edition, Organizer, OrganizerRegistry},
    settings,
    stripe::{StripeClient, StripeError, WEBHOOK_EVENTS},
};

#[derive(Deserialize)]
struct StripeKeyForm {
    secret_key: String,
}
#[derive(Deserialize)]
struct WebhookForm {
    secret: String,
    #[serde(default)]
    attempt: String,
}
#[derive(Deserialize)]
struct CurrencyForm {
    currency: String,
}
#[derive(Deserialize)]
struct SetupQuery {
    step: Option<String>,
    error: Option<String>,
}

fn webhook_url(ctx: &AppContext, organizer: &Organizer) -> String {
    format!("{}/webhooks/stripe", origin(ctx, organizer))
}

fn back_to(step: &str, error: &str) -> Response {
    Redirect::to(&format!("/office/setup?step={step}&error={error}")).into_response()
}

#[derive(Deserialize)]
struct ContactForm {
    imprint_url: String,
    privacy_url: String,
    reply_to: String,
}

#[derive(Deserialize)]
struct BookingTermsForm {
    url: String,
    #[serde(default)]
    required: String,
}

async fn save_booking_terms(
    AdminOrganizer { db, .. }: AdminOrganizer,
    Form(form): Form<BookingTermsForm>,
) -> Result<Response> {
    match settings::set_booking_terms(&db, &form.url, form.required == "yes").await {
        Ok(()) => Ok(Redirect::to("/office/setup?step=details").into_response()),
        Err(settings::SettingsError::InvalidBookingTerms) => {
            Ok(back_to("details", "booking_terms"))
        }
        Err(error) => Err(loco_rs::Error::string(&error.to_string())),
    }
}

async fn index(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Query(query): Query<SetupQuery>,
) -> Result<Response> {
    let cipher = registry
        .cipher(&organizer)
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let setup_attempt = settings::get(&db, &cipher, "stripe_setup_attempt")
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?
        .unwrap_or_default();
    let key = settings::get(&db, &cipher, "stripe_secret_key")
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let webhook = settings::get(&db, &cipher, "webhook_secret")
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let key_tested = settings::get(&db, &cipher, "stripe_key_tested_at")
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?
        .is_some_and(|v| !v.is_empty());
    let webhook_tested = settings::get(&db, &cipher, "webhook_tested_at")
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?
        .is_some_and(|v| !v.is_empty());
    let webhook_auto = settings::get(&db, &cipher, "webhook_endpoint_id")
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?
        .is_some_and(|v| !v.is_empty());
    let smtp_receipt = settings::get(&db, &cipher, "smtp_tested_at")
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?
        .filter(|value| !value.is_empty());
    let smtp_accepted = smtp_receipt.as_deref().is_some_and(|value| {
        value
            .strip_prefix("accepted-v1:")
            .is_some_and(|timestamp| timestamp.parse::<i64>().is_ok())
    });
    let smtp_unconfirmed = matches!(
        query.error.as_deref(),
        Some("smtp_unconfirmed" | "smtp_receipt_unconfirmed")
    );
    let smtp_tested = smtp_accepted && !smtp_unconfirmed;
    let key_last = key
        .as_deref()
        .map(|value| {
            value
                .chars()
                .rev()
                .take(4)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        })
        .unwrap_or_default();
    let webhook_last = webhook
        .as_deref()
        .map(|value| {
            value
                .chars()
                .rev()
                .take(4)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        })
        .unwrap_or_default();
    let subscription = if Edition::from_environment() == Edition::Hosted {
        Some(
            hosted::access_state(&ctx.db, &organizer.id)
                .await
                .map_err(|error| loco_rs::Error::string(&error.to_string()))?,
        )
    } else {
        None
    };
    let setup_status = if Edition::from_environment() == Edition::Hosted {
        hosted::setup_status(&ctx.db, &organizer.id)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?
    } else {
        None
    };
    let master_key_warning = Edition::from_environment() == Edition::SelfHosted
        && settings::get(&db, &cipher, "master_key_confirmed_at")
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?
            .is_none();
    let currency = settings::currency(&db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let currency_locked = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT 1 FROM holds LIMIT 1",
            [],
        ))
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .is_some();
    let contact = settings::contact(&db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let booking_terms = settings::booking_terms(&db)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let hosted = subscription.is_some();
    let subscription_active = subscription
        .as_ref()
        .is_some_and(|state| hosted::subscription_allows_sales(state, hosted::now()));
    let has_event = !crate::events::list(&db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .is_empty();
    let mut steps = Vec::new();
    if !hosted {
        steps.push(("key", "Save your master key", !master_key_warning));
    }
    steps.push(("stripe", "Connect Stripe", key_tested));
    steps.push(("webhook", "Confirm payments", webhook_tested));
    if !hosted {
        steps.push(("email", "Check ticket emails", smtp_tested));
    }
    steps.push((
        "details",
        "Currency and contact",
        !contact.reply_to.is_empty(),
    ));
    if hosted {
        steps.push((
            "subscription",
            "Start your subscription",
            subscription_active,
        ));
    }
    steps.push(("event", "Create your first event", has_event));
    let current = query
        .step
        .as_deref()
        .and_then(|step| steps.iter().find(|(key, ..)| *key == step))
        .or_else(|| steps.iter().find(|(.., done)| !done))
        .map_or("finished", |(key, ..)| *key);
    let next = steps
        .iter()
        .skip_while(|(key, ..)| *key != current)
        .nth(1)
        .map(|(key, ..)| *key);
    let current_number = steps
        .iter()
        .position(|(key, ..)| *key == current)
        .map(|index| index + 1);
    let done = steps.iter().filter(|(.., done)| *done).count();
    let total = steps.len();
    let steps = steps
        .into_iter()
        .enumerate()
        .map(|(index, (key, title, done))| json!({"key": key, "title": title, "done": done, "number": index + 1}))
        .collect::<Vec<_>>();
    format::render().view(
        &view,
        "office/setup.html",
        data!({
            "organizer_name": organizer.name,
            "active": "setup",
            "steps": steps,
            "current": current,
            "current_number": current_number,
            "next": next,
            "error": query.error,
            "done": done,
            "total": total,
            "key_tested": key_tested,
            "webhook_tested": webhook_tested,
            "webhook_saved": !webhook_last.is_empty(),
            "smtp_tested": smtp_tested,
            "smtp_history_accepted": smtp_accepted && smtp_unconfirmed,
            "smtp_history_unverified": smtp_receipt.is_some() && !smtp_accepted,
            "key_last": key_last,
            "webhook_last": webhook_last,
            "webhook_auto": webhook_auto,
            "webhook_url": webhook_url(&ctx, &organizer),
            "setup_attempt": setup_attempt,
            "webhook_events": WEBHOOK_EVENTS,
            "hosted": hosted,
            "subscription_status": subscription.map(|state| state.subscription_status),
            "subscription_active": subscription_active,
            "has_event": has_event,
            "currency": currency,
            "currency_locked": currency_locked,
            "contact": contact, "booking_terms": booking_terms,
            "setup_status": setup_status
        }),
    )
}

async fn request_setup(
    State(ctx): State<AppContext>,
    AdminOrganizer { organizer, .. }: AdminOrganizer,
) -> Result<Response> {
    if Edition::from_environment() != Edition::Hosted {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    hosted::request_setup(&ctx.db, &organizer.id, hosted::now())
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    Ok(Redirect::to("/office/setup").into_response())
}

async fn save_contact(
    AdminOrganizer { db, .. }: AdminOrganizer,
    Form(form): Form<ContactForm>,
) -> Result<Response> {
    match settings::set_contact(&db, &form.imprint_url, &form.privacy_url, &form.reply_to).await {
        Ok(()) => Ok(Redirect::to("/office/setup").into_response()),
        Err(settings::SettingsError::InvalidLegalUrl | settings::SettingsError::InvalidReplyTo) => {
            Ok(back_to("details", "details"))
        }
        Err(error) => Err(loco_rs::Error::string(&error.to_string())),
    }
}

async fn save_currency(
    AdminOrganizer { db, .. }: AdminOrganizer,
    Form(form): Form<CurrencyForm>,
) -> Result<Response> {
    match settings::set_currency(&db, &form.currency).await {
        Ok(()) => Ok(Redirect::to("/office/setup?step=details").into_response()),
        Err(settings::SettingsError::InvalidCurrency | settings::SettingsError::CurrencyLocked) => {
            Ok(back_to("details", "currency"))
        }
        Err(error) => Err(loco_rs::Error::string(&error.to_string())),
    }
}

async fn confirm_master_key(
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
) -> Result<Response> {
    if Edition::from_environment() != Edition::SelfHosted {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let cipher = registry
        .cipher(&organizer)
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    settings::set(
        &db,
        &cipher,
        "master_key_confirmed_at",
        &Utc::now().timestamp().to_string(),
    )
    .await
    .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    Ok(Redirect::to("/office/setup").into_response())
}

async fn master_key_status(
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
) -> Result<Response> {
    if Edition::from_environment() != Edition::SelfHosted {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let cipher = registry
        .cipher(&organizer)
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let confirmed = settings::get(&db, &cipher, "master_key_confirmed_at")
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        .is_some();
    Ok(Json(json!({"show": !confirmed})).into_response())
}

async fn stripe_key(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    Form(form): Form<StripeKeyForm>,
) -> Result<Response> {
    let client = match StripeClient::from_secret(form.secret_key.trim()) {
        Ok(client) => client,
        Err(_) => return Ok(back_to("stripe", "key_format")),
    };
    let cipher = registry
        .cipher(&organizer)
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    let expected_attempt = settings::get(&db, &cipher, "stripe_setup_attempt")
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?
        .unwrap_or_default();
    if let Err(error) = client.test_key().await {
        tracing::warn!(%error, organizer_id = %organizer.id, "Stripe key test failed");
        return Ok(back_to("stripe", "key_rejected"));
    }
    let Some(attempt) = settings::begin_stripe_setup(
        &db,
        &cipher,
        form.secret_key.trim(),
        &Utc::now().timestamp().to_string(),
        &expected_attempt,
    )
    .await
    .map_err(|e| loco_rs::Error::string(&e.to_string()))?
    else {
        return Ok(back_to("webhook", "webhook_superseded"));
    };
    let url = webhook_url(&ctx, &organizer);
    let older_endpoints = client.other_webhook_endpoint_ids(&url, "").await;
    let endpoint = match client.create_webhook_endpoint(&url).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            tracing::warn!(%error, organizer_id = %organizer.id, "Stripe webhook endpoint was not created");
            return Ok(back_to(
                "webhook",
                match error {
                    StripeError::PermissionDenied(_) => "webhook_permission",
                    _ => "webhook_refused",
                },
            ));
        }
    };
    if !settings::finish_stripe_setup(
        &db,
        &cipher,
        &attempt,
        &endpoint,
        &Utc::now().timestamp().to_string(),
    )
    .await
    .map_err(|e| loco_rs::Error::string(&e.to_string()))?
    {
        return Ok(back_to("webhook", "webhook_superseded"));
    }
    let cleanup = match older_endpoints {
        Ok(ids) => client.delete_webhook_endpoints(&ids).await,
        Err(error) => Err(error),
    };
    if let Err(error) = cleanup {
        tracing::warn!(%error, organizer_id = %organizer.id, "Older Stripe webhook endpoints were not removed");
        return Ok(back_to("webhook", "webhook_duplicate"));
    }
    Ok(Redirect::to("/office/setup").into_response())
}

async fn webhook_secret(
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer { organizer, db, .. }: AdminOrganizer,
    Form(form): Form<WebhookForm>,
) -> Result<Response> {
    if !form.secret.trim().starts_with("whsec_") {
        return Ok(back_to("webhook", "secret_format"));
    }
    let cipher = registry
        .cipher(&organizer)
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    if !settings::set_manual_webhook(&db, &cipher, form.secret.trim(), &form.attempt)
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?
    {
        return Ok(back_to("webhook", "webhook_superseded"));
    }
    Ok(Redirect::to("/office/setup?step=webhook").into_response())
}

async fn smtp_test(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    AdminOrganizer {
        organizer,
        db,
        member_id,
    }: AdminOrganizer,
) -> Result<Response> {
    let row = ctx
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT email FROM members WHERE id = ?",
            [member_id.into()],
        ))
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?
        .ok_or_else(|| loco_rs::Error::string("Member not found"))?;
    let email: String = row
        .try_get("", "email")
        .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
    if SetupMailer::send_test(&ctx, &email, &organizer.name)
        .await
        .is_err()
    {
        return Ok(back_to("email", "smtp_unconfirmed"));
    }
    let receipt = async {
        let cipher = registry
            .cipher(&organizer)
            .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
        settings::set(
            &db,
            &cipher,
            "smtp_tested_at",
            &format!("accepted-v1:{}", Utc::now().timestamp()),
        )
        .await
        .map_err(|e| loco_rs::Error::string(&e.to_string()))
    }
    .await;
    if receipt.is_err() {
        tracing::warn!("Setup email accepted but receipt storage could not be confirmed");
        return Ok(back_to("email", "smtp_receipt_unconfirmed"));
    }
    Ok(Redirect::to("/office/setup").into_response())
}

pub fn routes() -> Routes {
    Routes::new()
        .add("/office/setup", get(index))
        .add("/office/setup/currency", post(save_currency))
        .add("/office/setup/contact", post(save_contact))
        .add("/office/setup/booking-terms", post(save_booking_terms))
        .add("/office/setup/request", post(request_setup))
        .add("/office/setup/master-key", post(confirm_master_key))
        .add("/office/setup/master-key-status", get(master_key_status))
        .add("/office/setup/stripe", post(stripe_key))
        .add("/office/setup/webhook", post(webhook_secret))
        .add("/office/setup/email", post(smtp_test))
}
