use std::sync::Arc;

use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use loco_rs::prelude::*;
use serde::Deserialize;

use crate::{
    admin_orders,
    controllers::admin_orders::process_order_work,
    controllers::checkout::send_order_email,
    crypto::OrganizerCipher,
    mailers::late_payment::LatePaymentMailer,
    orders::{self, PaymentOutcome},
    organizers::{OrganizerDb, OrganizerRegistry},
    settings,
    stripe::{self, RefundStatus},
};

#[derive(Deserialize)]
struct StripeEvent {
    #[serde(rename = "type")]
    kind: String,
    data: StripeData,
}

#[derive(Deserialize)]
struct StripeData {
    object: StripeSession,
}

#[derive(Deserialize)]
struct StripeSession {
    id: String,
    payment_status: Option<String>,
    amount_total: Option<i64>,
    currency: Option<String>,
    payment_intent: Option<String>,
    metadata: Option<StripeMetadata>,
    refunded: Option<bool>,
}

#[derive(Deserialize)]
struct StripeMetadata {
    hold_id: Option<String>,
}

async fn send_hold_refund_email(
    ctx: &AppContext,
    organizer_name: &str,
    db: &sea_orm::DatabaseConnection,
    cipher: &OrganizerCipher,
    hold_id: &str,
) -> Result<()> {
    let token = match orders::claim_hold_refund_email(db, hold_id)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
    {
        orders::HoldRefundEmailClaim::Sent => return Ok(()),
        orders::HoldRefundEmailClaim::Busy => {
            return Err(loco_rs::Error::string(
                "Late-payment refund mail is still pending; retry this webhook later.",
            ));
        }
        orders::HoldRefundEmailClaim::Owned(token) => token,
    };
    let result = async {
        let email = orders::hold_buyer_email(db, cipher, hold_id)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        let contact = settings::contact(db)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        if !orders::hold_refund_email_claim_current(db, hold_id, &token)
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?
        {
            return Err(loco_rs::Error::string("Late-payment refund mail claim expired before sending; retry this webhook later."));
        }
        LatePaymentMailer::send_refund(
            ctx,
            &email,
            organizer_name,
            (!contact.reply_to.is_empty()).then_some(contact.reply_to.as_str()),
        )
        .await?;
        orders::mark_hold_refund_email_sent(db, hold_id, &token, orders::now())
            .await
            .map_err(|error| {
                tracing::error!(%error, "Late-payment refund mail accepted by SMTP, but receipt storage failed; replay may deliver a duplicate");
                loco_rs::Error::string("Late-payment refund mail accepted by SMTP, but receipt storage is uncertain; replay may deliver a duplicate.")
            })
    }.await;
    if result.is_err() {
        orders::release_hold_refund_email_claim(db, hold_id, &token)
            .await
            .map_err(|error| {
                tracing::error!(%error, "Cannot release late-payment mail claim; retry after expiry; SMTP acceptance may be uncertain");
                loco_rs::Error::string("Late-payment refund mail failed or is uncertain and claim release failed; retry after claim expiry.")
            })?;
    }
    result
}

async fn stripe_webhook(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    OrganizerDb { organizer, db }: OrganizerDb,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let cipher = registry
        .cipher(&organizer)
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let Some(configuration) = settings::webhook_configuration(&db, &cipher)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
    else {
        return Ok(StatusCode::SERVICE_UNAVAILABLE.into_response());
    };
    let signature = headers
        .get("Stripe-Signature")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if stripe::verify_webhook(&body, signature, configuration.secret(), orders::now()).is_err() {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    }
    let event: StripeEvent = match serde_json::from_slice(&body) {
        Ok(event) => event,
        Err(_) => return Ok(StatusCode::BAD_REQUEST.into_response()),
    };
    settings::mark_webhook_received(&db, &configuration, &orders::now().to_string())
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    if matches!(
        event.kind.as_str(),
        "charge.refunded" | "refund.created" | "refund.updated" | "refund.failed"
    ) {
        if event.kind == "charge.refunded" && event.data.object.refunded != Some(true) {
            return Ok(StatusCode::OK.into_response());
        }
        let Some(intent) = event.data.object.payment_intent else {
            return Ok(StatusCode::BAD_REQUEST.into_response());
        };
        if let Some(id) = admin_orders::order_id_by_payment_intent(&db, &intent)
            .await
            .map_err(|e| loco_rs::Error::string(&e.to_string()))?
        {
            let stripe = settings::stripe_client(&db, &cipher)
                .await
                .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
            admin_orders::reconcile_refund(&db, &id, &stripe, orders::now())
                .await
                .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
            let progress = admin_orders::work_progress(&db, &id)
                .await
                .map_err(|e| loco_rs::Error::string(&e.to_string()))?;
            if progress.refund != "Completed" {
                return Ok(StatusCode::OK.into_response());
            }
            process_order_work(&ctx, &registry, &organizer, &db, &id, false).await?;
        } else if event.kind == "charge.refunded" {
            if let Some(hold_id) = orders::record_external_refund(&db, &intent, orders::now())
                .await
                .map_err(|e| loco_rs::Error::string(&e.to_string()))?
            {
                send_hold_refund_email(&ctx, &organizer.name, &db, &cipher, &hold_id).await?;
            }
        }
        return Ok(StatusCode::OK.into_response());
    }
    if !matches!(
        event.kind.as_str(),
        "checkout.session.completed"
            | "checkout.session.async_payment_succeeded"
            | "checkout.session.expired"
            | "checkout.session.async_payment_failed"
    ) {
        return Ok(StatusCode::OK.into_response());
    }
    let session = event.data.object;
    // An Organizer's Stripe account also sends events for Checkout Sessions other tools created.
    let Some(hold_id) = session.metadata.and_then(|metadata| metadata.hold_id) else {
        return Ok(StatusCode::OK.into_response());
    };
    match event.kind.as_str() {
        "checkout.session.completed" | "checkout.session.async_payment_succeeded"
            if session.payment_status.as_deref() == Some("paid") =>
        {
            if let Err(error) =
                orders::attach_session(&db, &hold_id, &session.id, orders::now()).await
            {
                tracing::error!(%error, "Cannot attach paid Checkout Session");
                return Ok(StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
            let Some(amount_total) = session.amount_total else {
                return Ok(StatusCode::BAD_REQUEST.into_response());
            };
            let Some(currency) = session.currency else {
                return Ok(StatusCode::BAD_REQUEST.into_response());
            };
            let Some(payment_intent) = session.payment_intent else {
                return Ok(StatusCode::BAD_REQUEST.into_response());
            };
            match orders::complete_payment_in_currency(
                &db,
                &session.id,
                &payment_intent,
                amount_total,
                &currency,
                orders::now(),
            )
            .await
            {
                Ok(PaymentOutcome::Paid(id) | PaymentOutcome::AlreadyProcessed(id)) => {
                    send_order_email(&ctx, &registry, &organizer, &db, &id).await?;
                }
                Ok(PaymentOutcome::RefundNeeded) => {
                    let stripe = settings::stripe_client(&db, &cipher)
                        .await
                        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
                    let refund_status = stripe
                        .refund(&payment_intent, &format!("late-{}", session.id))
                        .await
                        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
                    if refund_status == RefundStatus::Pending {
                        return Ok(StatusCode::OK.into_response());
                    }
                    orders::record_external_refund(&db, &payment_intent, orders::now())
                        .await
                        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
                    send_hold_refund_email(&ctx, &organizer.name, &db, &cipher, &hold_id).await?;
                }
                Ok(PaymentOutcome::AlreadyRefunded) => {
                    send_hold_refund_email(&ctx, &organizer.name, &db, &cipher, &hold_id).await?;
                }
                Err(error) => {
                    tracing::error!(%error, "Checkout Session could not be completed");
                    return Ok(StatusCode::INTERNAL_SERVER_ERROR.into_response());
                }
            }
        }
        "checkout.session.expired" | "checkout.session.async_payment_failed" => {
            orders::expire_hold(&db, &hold_id)
                .await
                .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        }
        _ => {}
    }
    Ok(StatusCode::OK.into_response())
}

pub fn routes() -> Routes {
    Routes::new().add("/webhooks/stripe", post(stripe_webhook))
}
