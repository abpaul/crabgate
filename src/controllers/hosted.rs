use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Form, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Extension,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use loco_rs::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    auth::AdminOrganizer,
    hosted,
    mailers::hosted::HostedMailer,
    mailers::member::MemberMailer,
    members,
    organizers::{Edition, OrganizerDb, OrganizerRegistry},
    stripe::{self, StripeClient},
};

fn internal_error(error: impl std::fmt::Display) -> loco_rs::Error {
    loco_rs::Error::string(&error.to_string())
}

#[derive(Deserialize)]
struct SignupForm {
    name: String,
    subdomain: Option<String>,
    email: String,
    next_event: Option<String>,
}

async fn self_hosted_signup_state(ctx: &AppContext) -> Result<Option<(String, bool)>> {
    let row = ctx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT id, onboarding_at FROM organizers WHERE subdomain = 'local'".to_owned(),
        ))
        .await
        .map_err(internal_error)?;
    row.map(|row| {
        Ok((
            row.try_get("", "id").map_err(internal_error)?,
            row.try_get::<Option<i64>>("", "onboarding_at")
                .map_err(internal_error)?
                .is_some(),
        ))
    })
    .transpose()
}

fn hosted_only() -> Option<Response> {
    (Edition::from_environment() != Edition::Hosted).then(|| StatusCode::NOT_FOUND.into_response())
}

pub fn marketing_host(headers: &HeaderMap) -> bool {
    let base = std::env::var("SHOP_BASE_DOMAIN").unwrap_or_else(|_| "crabgate.com".to_owned());
    headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|host| host.split(':').next())
        == Some(base.as_str())
}

#[derive(Serialize)]
pub struct Guide {
    pub path: &'static str,
    pub title: &'static str,
    pub seo_title: &'static str,
    pub description: &'static str,
    pub audience: &'static str,
    pub template: &'static str,
}

pub const GUIDES: [Guide; 2] = [
    Guide {
        path: "/guides/workshop-registration-form",
        title: "Workshop registration form: a free template for paid workshops",
        seo_title: "Workshop registration form: free template",
        description: "Copy a free workshop registration template. Choose useful fields, separate buyers from attendees, and plan payment, capacity and confirmation.",
        audience: "Workshops",
        template: "marketing/guides/workshop-registration-form.html",
    },
    Guide {
        path: "/guides/sell-tickets-small-event",
        title: "How to sell tickets for a small event: a practical launch guide",
        seo_title: "How to sell tickets for a small event",
        description: "Plan your ticket price, test the buying journey, share one event link and prepare the door. Includes a free small-event launch checklist.",
        audience: "Rooms",
        template: "marketing/guides/sell-tickets-small-event.html",
    },
];

pub const LANDING_PAGES: [(&str, &str); 18] = [
    ("/guides", "marketing/guides/index.html"),
    ("/pricing", "marketing/pricing.html"),
    (
        "/eventbrite-alternative",
        "marketing/eventbrite-alternative.html",
    ),
    ("/for-rooms-with-an-audience", "marketing/rooms.html"),
    ("/for-conferences", "marketing/conferences.html"),
    ("/for-workshops", "marketing/workshops.html"),
    ("/privacy", "marketing/privacy.html"),
    ("/terms", "marketing/terms.html"),
    ("/tools", "marketing/tools/index.html"),
    (
        "/tools/ticketsource-fee-calculator",
        "marketing/tools/fees.html",
    ),
    (
        "/tools/humanitix-fee-calculator",
        "marketing/tools/fees.html",
    ),
    ("/tools/tito-fee-calculator", "marketing/tools/fees.html"),
    (
        "/tools/ticket-tailor-fee-calculator",
        "marketing/tools/fees.html",
    ),
    (
        "/tools/eventbrite-fee-calculator",
        "marketing/tools/fees.html",
    ),
    (
        "/tools/door-split-calculator",
        "marketing/tools/door-split.html",
    ),
    (
        "/tools/ticket-price-calculator",
        "marketing/tools/ticket-price.html",
    ),
    (
        "/tools/stripe-fee-calculator",
        "marketing/tools/stripe.html",
    ),
    ("/tools/ticket-maker", "marketing/tools/ticket-maker.html"),
];

fn setup_offer(today: chrono::NaiveDate) -> Option<(String, i64)> {
    let ends =
        std::env::var("CRABGATE_SETUP_OFFER_ENDS").unwrap_or_else(|_| "2026-12-20".to_owned());
    let ends = chrono::NaiveDate::parse_from_str(&ends, "%Y-%m-%d").ok()?;
    let days_left = (ends - today).num_days();
    (days_left >= 0).then(|| (ends.format("%-d %B").to_string(), days_left))
}

pub fn marketing_data(path: &str) -> Value {
    let offer = setup_offer(chrono::Utc::now().date_naive());
    let rates: Value = serde_json::from_str(include_str!(
        "../../assets/static/marketing/tools/rates.json"
    ))
    .expect("tool rates must be valid JSON");
    let provider = rates["providers"].as_array().and_then(|providers| {
        providers.iter().find(|provider| {
            path == format!(
                "/tools/{}-fee-calculator",
                provider["id"].as_str().unwrap_or_default()
            )
        })
    });
    let guide = GUIDES.iter().find(|guide| guide.path == path);
    let guide_schema = guide.map(|guide| {
        super::page::script_json(&json!({
            "@context": "https://schema.org",
            "@type": "Article",
            "headline": guide.title,
            "mainEntityOfPage": format!("{}{path}", hosted::origin(None)),
            "author": {
                "@type": "Person",
                "name": "Paul Bădărău",
                "url": "https://badarau.net"
            }
        }))
    });
    json!({
        "guides": GUIDES,
        "guide": guide,
        "guide_schema": guide_schema,
        "tool_rates": rates,
        "tool_rates_json": include_str!("../../assets/static/marketing/tools/rates.json"),
        "provider": provider,
        "offer_ends": offer.as_ref().map(|(ends, _)| ends),
        "days_left": offer.as_ref().map(|(_, days_left)| days_left),
        "path": path,
        "canonical": format!("{}{path}", hosted::origin(None)),
        "price": std::env::var("CRABGATE_ANNUAL_PRICE").unwrap_or_else(|_| "$99".to_owned()),
        "setup_url": "/signup",
    })
}

async fn landing_page(
    headers: HeaderMap,
    ViewEngine(view): ViewEngine<TeraView>,
    path: &'static str,
    template: &'static str,
) -> Result<Response> {
    if let Some(response) = hosted_only() {
        return Ok(response);
    }
    if !marketing_host(&headers) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    format::render().view(&view, template, marketing_data(path))
}

#[derive(Deserialize)]
struct SignInQuery {
    shop: Option<String>,
}

async fn sign_in_page(
    headers: HeaderMap,
    ViewEngine(view): ViewEngine<TeraView>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    Query(query): Query<SignInQuery>,
) -> Result<Response> {
    if Edition::from_environment() == Edition::SelfHosted {
        return Ok(Redirect::to("/office/sign-in").into_response());
    }
    if !marketing_host(&headers) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let base_domain =
        std::env::var("SHOP_BASE_DOMAIN").unwrap_or_else(|_| "crabgate.com".to_owned());
    let mut page = marketing_data("/sign-in");
    page["base_domain"] = json!(base_domain);
    page["shop"] = json!("");
    page["error"] = Value::Null;
    let mut status = StatusCode::OK;
    if let Some(shop) = query.shop {
        let shop = shop.trim().to_ascii_lowercase();
        let found = if shop.len() <= 63 {
            match registry.resolve(&format!("{shop}.{base_domain}")).await {
                Ok(scope) => Some(scope.organizer),
                Err(
                    crate::organizers::OrganizerError::NotFound
                    | crate::organizers::OrganizerError::InvalidSubdomain
                    | crate::organizers::OrganizerError::Suspended,
                ) => None,
                Err(error) => return Err(internal_error(error)),
            }
        } else {
            None
        };
        if let Some(organizer) = found {
            return Ok(Redirect::to(&format!(
                "{}/office/sign-in",
                hosted::origin(Some(&organizer.subdomain))
            ))
            .into_response());
        }
        page["shop"] = json!(shop);
        page["error"] = json!("We couldn't find that shop. Check the address in your setup email.");
        status = StatusCode::BAD_REQUEST;
    }
    let response = format::render().view(&view, "marketing/sign-in.html", page)?;
    Ok((status, response).into_response())
}

#[derive(Deserialize)]
struct TicketQrQuery {
    text: String,
}

async fn ticket_qr(headers: HeaderMap, Query(query): Query<TicketQrQuery>) -> Response {
    if hosted_only().is_some() || !marketing_host(&headers) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if query.text.chars().count() > 200 || query.text.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match crate::qr::svg(query.text.as_bytes()) {
        Ok(svg) => (
            [
                (axum::http::header::CONTENT_TYPE, "image/svg+xml"),
                (axum::http::header::CACHE_CONTROL, "private, max-age=86400"),
            ],
            svg,
        )
            .into_response(),
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}

fn signup_view(
    view: &TeraView,
    error: Option<String>,
    sent: bool,
    name: &str,
    subdomain: &str,
    email: &str,
    next_event: &str,
) -> Result<Response> {
    let mut page = marketing_data("/signup");
    page["next_event"] = json!(next_event);
    page["error"] = json!(error);
    page["sent"] = json!(sent);
    page["base_domain"] =
        json!(std::env::var("SHOP_BASE_DOMAIN").unwrap_or_else(|_| "crabgate.com".to_owned()));
    page["name"] = json!(name);
    page["subdomain"] = json!(subdomain);
    page["email"] = json!(email);
    format::render().view(view, "marketing/signup.html", page)
}

async fn signup_page(
    State(ctx): State<AppContext>,
    headers: HeaderMap,
    ViewEngine(view): ViewEngine<TeraView>,
) -> Result<Response> {
    if Edition::from_environment() == Edition::SelfHosted {
        if std::env::var("CRABGATE_ADMIN_EMAIL").is_err() {
            return Ok(StatusCode::NOT_FOUND.into_response());
        }
        let Some((_, claimed)) = self_hosted_signup_state(&ctx).await? else {
            return Ok(StatusCode::SERVICE_UNAVAILABLE.into_response());
        };
        if claimed {
            return Ok(Redirect::to("/office/sign-in").into_response());
        }
        return format::render().view(
            &view,
            "marketing/self-hosted-signup.html",
            data!({"error": null, "name": "", "email": ""}),
        );
    }
    if let Some(response) = hosted_only() {
        return Ok(response);
    }
    if !marketing_host(&headers) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    signup_view(&view, None, false, "", "", "", "")
}

async fn signup_submit(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    headers: HeaderMap,
    ViewEngine(view): ViewEngine<TeraView>,
    Form(form): Form<SignupForm>,
) -> Result<Response> {
    if Edition::from_environment() == Edition::SelfHosted {
        let Some((id, claimed)) = self_hosted_signup_state(&ctx).await? else {
            return Ok(StatusCode::SERVICE_UNAVAILABLE.into_response());
        };
        if claimed {
            return Ok(Redirect::to("/office/sign-in").into_response());
        }
        let Ok(admin_email) = std::env::var("CRABGATE_ADMIN_EMAIL") else {
            return Ok(StatusCode::NOT_FOUND.into_response());
        };
        let name = form.name.trim();
        let email = form.email.trim();
        if name.is_empty()
            || name.len() > 120
            || email.is_empty()
            || !email.eq_ignore_ascii_case(admin_email.trim())
        {
            return format::render().view(
                &view,
                "marketing/self-hosted-signup.html",
                data!({"error": "Enter an organizer name and the Admin email used during installation.", "name": form.name, "email": form.email}),
            );
        }
        let updated = ctx
            .db
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE organizers SET name = ?, onboarding_at = ? WHERE id = ? AND onboarding_at IS NULL",
                [name.into(), hosted::now().into(), id.clone().into()],
            ))
            .await
            .map_err(internal_error)?;
        if updated.rows_affected() != 1 {
            return Ok(Redirect::to("/office/sign-in").into_response());
        }
        let token = members::request_magic_link(&ctx.db, &id, email, hosted::now())
            .await
            .map_err(internal_error)?
            .ok_or_else(|| loco_rs::Error::string("Admin email is not configured"))?;
        let url = format!("{}/office/magic-link/{token}", ctx.config.server.full_url());
        MemberMailer::send_magic_link(&ctx, email, &url).await?;
        return Ok(Redirect::to("/office/sign-in?sent=1").into_response());
    }
    if let Some(response) = hosted_only() {
        return Ok(response);
    }
    if !marketing_host(&headers) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let subdomain = form.subdomain.as_deref().unwrap_or_default();
    let next_event = form.next_event.as_deref().unwrap_or_default().trim();
    let signup = match hosted::public_signup(
        &registry,
        &ctx.db,
        &form.name,
        subdomain,
        &form.email,
        hosted::now(),
    )
    .await
    {
        Ok(signup) => signup,
        Err(error) => {
            let status = if matches!(error, hosted::HostedError::TooManySignups) {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::OK
            };
            let response = signup_view(
                &view,
                Some(error.to_string()),
                false,
                &form.name,
                subdomain,
                &form.email,
                next_event,
            )?;
            return Ok((status, response).into_response());
        }
    };
    let url = format!(
        "{}/verify/{}",
        hosted::origin(None),
        signup.verification_token
    );
    if let Err(error) =
        HostedMailer::verify(&ctx, &form.email.trim().to_ascii_lowercase(), &url).await
    {
        tracing::error!(%error, organizer_id = %signup.organizer.id, "Signup verification delivery failed");
        let response = signup_view(&view,
            Some("Your shop was created, but we could not send the verification email. Use the recovery link below to try again. You do not need to create another shop.".to_owned()),
            false, &form.name, subdomain, &form.email, next_event)?;
        return Ok((StatusCode::SERVICE_UNAVAILABLE, response).into_response());
    }
    let organizer = &signup.organizer;
    if let Err(error) = HostedMailer::signup_notice(
        &ctx,
        &organizer.name,
        &form.email.trim().to_ascii_lowercase(),
        &hosted::origin(Some(&organizer.subdomain)),
        next_event,
    )
    .await
    {
        tracing::error!(%error, organizer_id = %organizer.id, "Signup notice failed");
    }
    signup_view(
        &view,
        None,
        true,
        &form.name,
        subdomain,
        &form.email,
        next_event,
    )
}

#[derive(Deserialize)]
struct RecoveryForm {
    subdomain: String,
    email: String,
}

enum RecoveryState {
    Form,
    Requested,
    InvalidLink,
}

fn recovery_view(
    view: &TeraView,
    status: StatusCode,
    error: Option<&str>,
    state: RecoveryState,
    subdomain: &str,
    email: &str,
) -> Result<Response> {
    let mut page = marketing_data("/signup/recover");
    page["error"] = json!(error);
    page["requested"] = json!(matches!(state, RecoveryState::Requested));
    page["show_form"] = json!(!matches!(state, RecoveryState::InvalidLink));
    page["subdomain"] = json!(subdomain);
    page["email"] = json!(email);
    page["base_domain"] =
        json!(std::env::var("SHOP_BASE_DOMAIN").unwrap_or_else(|_| "crabgate.com".to_owned()));
    page["marketing_origin"] = json!(hosted::origin(None));
    let response = format::render().view(view, "marketing/signup-recovery.html", page)?;
    Ok((status, response).into_response())
}

fn invalid_verification_view(view: &TeraView) -> Result<Response> {
    recovery_view(view, StatusCode::BAD_REQUEST,
        Some("This signup link is invalid, expired, or already used. Request another link, or sign in if you finished signup."),
        RecoveryState::InvalidLink, "", "")
}

async fn recovery_page(
    headers: HeaderMap,
    ViewEngine(view): ViewEngine<TeraView>,
) -> Result<Response> {
    if let Some(response) = hosted_only() {
        return Ok(response);
    }
    if !marketing_host(&headers) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    recovery_view(&view, StatusCode::OK, None, RecoveryState::Form, "", "")
}

async fn recovery_submit(
    State(ctx): State<AppContext>,
    headers: HeaderMap,
    ViewEngine(view): ViewEngine<TeraView>,
    Form(form): Form<RecoveryForm>,
) -> Result<Response> {
    if let Some(response) = hosted_only() {
        return Ok(response);
    }
    if !marketing_host(&headers) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let recovery =
        match hosted::recover_verification(&ctx.db, &form.subdomain, &form.email, hosted::now())
            .await
        {
            Ok(recovery) => recovery,
            Err(hosted::HostedError::InvalidRecovery) => {
                return recovery_view(
                    &view,
                    StatusCode::BAD_REQUEST,
                    Some("Enter your Shop address and a valid signup email."),
                    RecoveryState::Form,
                    &form.subdomain,
                    &form.email,
                )
            }
            Err(error) => return Err(internal_error(error)),
        };
    if let Some(recovery) = recovery {
        let url = format!("{}/verify/{}", hosted::origin(None), recovery.token);
        if let Err(error) = HostedMailer::verify(&ctx, &recovery.email, &url).await {
            tracing::error!(%error, organizer_id = %recovery.organizer_id, "Signup verification recovery delivery failed");
        }
    }
    recovery_view(
        &view,
        StatusCode::OK,
        None,
        RecoveryState::Requested,
        &form.subdomain,
        &form.email,
    )
}

async fn verify(
    State(ctx): State<AppContext>,
    Path(token): Path<String>,
    ViewEngine(view): ViewEngine<TeraView>,
) -> Result<Response> {
    if let Some(response) = hosted_only() {
        return Ok(response);
    }
    let (_, _, subdomain) = match hosted::pending_verification(&ctx.db, &token, hosted::now()).await
    {
        Ok(verification) => verification,
        Err(hosted::HostedError::InvalidVerification) => return invalid_verification_view(&view),
        Err(error) => return Err(internal_error(error)),
    };
    Ok(Redirect::to(&format!(
        "{}/office/complete-signup/{token}",
        hosted::origin(Some(&subdomain))
    ))
    .into_response())
}

async fn complete_signup(
    State(ctx): State<AppContext>,
    OrganizerDb { organizer, .. }: OrganizerDb,
    Path(token): Path<String>,
    jar: CookieJar,
    ViewEngine(view): ViewEngine<TeraView>,
) -> Result<Response> {
    if let Some(response) = hosted_only() {
        return Ok(response);
    }
    let (id, email, _) = match hosted::pending_verification(&ctx.db, &token, hosted::now()).await {
        Ok(verification) => verification,
        Err(hosted::HostedError::InvalidVerification) => return invalid_verification_view(&view),
        Err(error) => return Err(internal_error(error)),
    };
    if id != organizer.id {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    match hosted::verify_email(&ctx.db, &token, hosted::now()).await {
        Ok(_) => {}
        Err(hosted::HostedError::InvalidVerification) => return invalid_verification_view(&view),
        Err(error) => return Err(internal_error(error)),
    }
    let session = members::create_session_for_email(&ctx.db, &id, &email, hosted::now())
        .await
        .map_err(internal_error)?
        .ok_or_else(|| loco_rs::Error::string("Signup Admin is missing"))?;
    let cookie = Cookie::build(("cg_session", session))
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(ctx.environment == loco_rs::environment::Environment::Production)
        .path("/")
        .build();
    Ok((jar.add(cookie), Redirect::to("/office/setup")).into_response())
}

async fn billing_start(
    State(ctx): State<AppContext>,
    AdminOrganizer { organizer, .. }: AdminOrganizer,
) -> Result<Response> {
    if let Some(response) = hosted_only() {
        return Ok(response);
    }
    let Some(attempt) = hosted::billing_attempt(&ctx.db, &organizer.id, hosted::now())
        .await
        .map_err(internal_error)?
    else {
        return Ok(Redirect::to("/office/setup").into_response());
    };
    if attempt.is_expired(hosted::now()) {
        return Ok(
            Redirect::to("/office/setup?step=subscription&error=billing_expired").into_response(),
        );
    }
    if let Some(url) = &attempt.checkout_url {
        return Ok(Redirect::to(url).into_response());
    }
    if !attempt.can_request_checkout(hosted::now()) {
        return Ok(
            Redirect::to("/office/setup?step=subscription&error=billing_retry_wait")
                .into_response(),
        );
    }
    let key = std::env::var("CRABGATE_BILLING_SECRET_KEY")
        .map_err(|_| loco_rs::Error::string("Billing is not configured"))?;
    let price = std::env::var("CRABGATE_BILLING_PRICE_ID")
        .map_err(|_| loco_rs::Error::string("Billing price is not configured"))?;
    let client = StripeClient::from_secret(&key).map_err(internal_error)?;
    let row = ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "SELECT m.email FROM members m JOIN memberships ms ON ms.member_id = m.id WHERE ms.organizer_id = ? AND ms.role = 'Admin' LIMIT 1", [organizer.id.clone().into()])).await.map_err(internal_error)?.ok_or_else(|| loco_rs::Error::string("Admin missing"))?;
    let email: String = row.try_get("", "email").map_err(internal_error)?;
    let origin = hosted::origin(Some(&organizer.subdomain));
    let session = client
        .create_subscription_session(crate::stripe::SubscriptionCheckout {
            organizer_id: &organizer.id,
            attempt_key: &attempt.key,
            expires_at: attempt.expires_at,
            email: &email,
            price_id: &price,
            success_url: &format!("{origin}/office/events?billing=success"),
            cancel_url: &format!("{origin}/office/events?billing=cancelled"),
        })
        .await
        .map_err(internal_error)?;
    if !hosted::save_billing_checkout(
        &ctx.db,
        &organizer.id,
        &attempt.key,
        &session.id,
        &session.url,
    )
    .await
    .map_err(internal_error)?
    {
        return Ok(StatusCode::CONFLICT.into_response());
    }
    if attempt.is_expired(hosted::now()) {
        return Ok(
            Redirect::to("/office/setup?step=subscription&error=billing_expired").into_response(),
        );
    }
    Ok(Redirect::to(&session.url).into_response())
}

async fn billing_portal(
    State(ctx): State<AppContext>,
    AdminOrganizer { organizer, .. }: AdminOrganizer,
) -> Result<Response> {
    if let Some(response) = hosted_only() {
        return Ok(response);
    }
    let customer = ctx
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT stripe_customer_id FROM subscriptions WHERE organizer_id = ?",
            [organizer.id.clone().into()],
        ))
        .await
        .map_err(internal_error)?
        .and_then(|row| row.try_get::<Option<String>>("", "stripe_customer_id").ok())
        .flatten()
        .filter(|customer| !customer.is_empty());
    let Some(customer) = customer else {
        return Ok(Redirect::to("/office/setup?step=subscription").into_response());
    };
    let key = std::env::var("CRABGATE_BILLING_SECRET_KEY")
        .map_err(|_| loco_rs::Error::string("Billing is not configured"))?;
    let client = StripeClient::from_secret(&key).map_err(internal_error)?;
    let url = client
        .create_portal_session(
            &customer,
            &format!(
                "{}/office/setup?step=subscription",
                hosted::origin(Some(&organizer.subdomain))
            ),
        )
        .await
        .map_err(internal_error)?;
    Ok(Redirect::to(&url).into_response())
}

async fn billing_webhook(
    State(ctx): State<AppContext>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    if let Some(response) = hosted_only() {
        return Ok(response);
    }
    let Ok(secret) = std::env::var("CRABGATE_BILLING_WEBHOOK_SECRET") else {
        return Ok(StatusCode::SERVICE_UNAVAILABLE.into_response());
    };
    let signature = headers
        .get("Stripe-Signature")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if stripe::verify_webhook(&body, signature, &secret, hosted::now()).is_err() {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    }
    let event: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return Ok(StatusCode::BAD_REQUEST.into_response()),
    };
    let Some(event_created) = event["created"].as_i64() else {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    };
    let kind = event["type"].as_str().unwrap_or_default();
    let object = &event["data"]["object"];
    let subscription_id = object["subscription"]
        .as_str()
        .or_else(|| object["parent"]["subscription_details"]["subscription"].as_str())
        .or_else(|| object["id"].as_str())
        .unwrap_or_default();
    match kind {
        "checkout.session.completed"
            if object["mode"] == "subscription" && object["payment_status"] == "paid" =>
        {
            let Some(id) = object["client_reference_id"].as_str() else {
                return Ok(StatusCode::OK.into_response());
            };
            let Some(sub_id) = object["subscription"].as_str() else {
                return Ok(StatusCode::BAD_REQUEST.into_response());
            };
            let customer = object["customer"].as_str().unwrap_or_default();
            let session_id = object["id"].as_str().unwrap_or_default();
            if !hosted::subscription_checkout_complete(
                &ctx.db,
                id,
                session_id,
                customer,
                sub_id,
                hosted::now(),
                event_created,
            )
            .await
            .map_err(internal_error)?
            {
                let ours = ctx
                    .db
                    .query_one_raw(Statement::from_sql_and_values(
                        DatabaseBackend::Sqlite,
                        "SELECT 1 AS found FROM subscriptions WHERE organizer_id = ?",
                        [id.into()],
                    ))
                    .await
                    .map_err(internal_error)?
                    .is_some();
                if !ours {
                    return Ok(StatusCode::OK.into_response());
                }
                let stale = ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                    "SELECT 1 AS found FROM subscriptions WHERE organizer_id = ? AND stripe_checkout_session_id = ? AND (status IN ('Grace', 'Lapsed') OR last_billing_event_created > ?)",
                    [id.into(), session_id.into(), event_created.into()]
                )).await.map_err(internal_error)?.is_some();
                return Ok(if stale {
                    StatusCode::OK
                } else {
                    StatusCode::BAD_REQUEST
                }
                .into_response());
            }
        }
        "invoice.payment_failed" if !subscription_id.is_empty() => {
            hosted::subscription_failed(&ctx.db, subscription_id, hosted::now(), event_created)
                .await
                .map_err(internal_error)?
        }
        "customer.subscription.updated"
            if !subscription_id.is_empty() && object["status"] == "active" =>
        {
            let row = ctx
                .db
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT organizer_id FROM subscriptions WHERE stripe_subscription_id = ?",
                    [subscription_id.into()],
                ))
                .await
                .map_err(internal_error)?;
            if let Some(row) = row {
                let id: String = row.try_get("", "organizer_id").map_err(internal_error)?;
                hosted::subscription_active(
                    &ctx.db,
                    &id,
                    "",
                    subscription_id,
                    hosted::now(),
                    event_created,
                )
                .await
                .map_err(internal_error)?;
            }
        }
        "customer.subscription.deleted" if !subscription_id.is_empty() => {
            hosted::subscription_cancelled(&ctx.db, subscription_id, hosted::now(), event_created)
                .await
                .map_err(internal_error)?
        }
        _ => {}
    }
    Ok(StatusCode::OK.into_response())
}

async fn tenant_check(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    Path(id): Path<String>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Response> {
    tenant_operation(&ctx, &registry, &id, &jar, Some(&headers)).await
}

async fn tenant_retry(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    Path(id): Path<String>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Response> {
    if headers.get("origin").and_then(|value| value.to_str().ok())
        != Some(hosted::origin(None).as_str())
    {
        return Ok(StatusCode::FORBIDDEN.into_response());
    }
    tenant_operation(&ctx, &registry, &id, &jar, None).await
}

async fn tenant_operation(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    id: &str,
    jar: &CookieJar,
    headers: Option<&HeaderMap>,
) -> Result<Response> {
    let retry = headers.is_none();
    if let Some(response) = hosted_only() {
        return Ok(response);
    }
    let Some(cookie) = jar.get("cg_operator_session") else {
        return Ok(StatusCode::FORBIDDEN.into_response());
    };
    let Some(operator_id) = crate::operator::session(&ctx.db, cookie.value(), hosted::now())
        .await
        .map_err(internal_error)?
    else {
        return Ok(StatusCode::FORBIDDEN.into_response());
    };
    if retry {
        if ctx
            .db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT id FROM organizers WHERE id = ?",
                [id.into()],
            ))
            .await
            .map_err(internal_error)?
            .is_none()
        {
            return Ok((
                StatusCode::NOT_FOUND,
                axum::Json(json!({"organizer_id": id, "ready": false})),
            )
                .into_response());
        }
        crate::operator::audit(
            &ctx.db,
            id,
            &operator_id,
            "POST",
            "/operator/organizers/migration/retry",
            hosted::now(),
        )
        .await
        .map_err(internal_error)?;
    }
    let result = if retry {
        registry.retry_migration(id).await
    } else {
        registry.tenant_readiness(id).await
    };
    let result = match (result, headers) {
        (Ok(latest), Some(headers)) => {
            let host = headers
                .get("host")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            match registry.resolve(host).await {
                Ok(scope) if scope.organizer.id == id => Ok(latest),
                Ok(_) => Err(crate::organizers::OrganizerError::NotFound),
                Err(error) => Err(error),
            }
        }
        (result, _) => result,
    };
    let (status, body) = match result {
        Ok(latest) => (
            StatusCode::OK,
            json!({"organizer_id": id, "ready": true, "latest_migration": latest}),
        ),
        Err(crate::organizers::OrganizerError::NotFound) => (
            StatusCode::NOT_FOUND,
            json!({"organizer_id": id, "ready": false}),
        ),
        Err(crate::organizers::OrganizerError::RecoveryNotAllowed) => (
            StatusCode::CONFLICT,
            json!({"organizer_id": id, "ready": false}),
        ),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"organizer_id": id, "ready": false}),
        ),
    };
    Ok((status, [("cache-control", "no-store")], axum::Json(body)).into_response())
}

pub fn routes() -> Routes {
    let mut routes = Routes::new();
    for (path, template) in LANDING_PAGES {
        routes = routes.add(
            path,
            get(move |headers: HeaderMap, view: ViewEngine<TeraView>| {
                landing_page(headers, view, path, template)
            }),
        );
    }
    for guide in GUIDES {
        routes = routes.add(
            guide.path,
            get(move |headers: HeaderMap, view: ViewEngine<TeraView>| {
                landing_page(headers, view, guide.path, guide.template)
            }),
        );
    }
    routes
        .add("/operator/organizers/{id}/readiness", get(tenant_check))
        .add(
            "/operator/organizers/{id}/migration/retry",
            post(tenant_retry),
        )
        .add("/sign-in", get(sign_in_page))
        .add("/tools/ticket-qr", get(ticket_qr))
        .add("/signup", get(signup_page).post(signup_submit))
        .add("/signup/recover", get(recovery_page).post(recovery_submit))
        .add("/verify/{token}", get(verify))
        .add("/office/complete-signup/{token}", get(complete_signup))
        .add("/webhooks/billing", post(billing_webhook))
        .add("/office/billing/start", post(billing_start))
        .add("/office/billing/portal", post(billing_portal))
}
