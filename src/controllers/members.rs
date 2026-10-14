use axum::{
    extract::{Form, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use chrono::Utc;
use loco_rs::prelude::*;
use serde::Deserialize;

use crate::{
    mailers::member::MemberMailer,
    members::{self, MemberError, Role},
    organizers::{self, Edition, OrganizerDb},
};

#[derive(Deserialize)]
struct SignInForm {
    email: String,
}

#[derive(Deserialize)]
struct SignInQuery {
    sent: Option<String>,
}

#[debug_handler]
async fn sign_in(
    State(ctx): State<AppContext>,
    OrganizerDb { .. }: OrganizerDb,
    ViewEngine(view): ViewEngine<TeraView>,
    Query(query): Query<SignInQuery>,
) -> Result<Response> {
    if organizers::self_hosted_signup_required(&ctx.db)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
    {
        return Ok(Redirect::to("/signup").into_response());
    }
    format::render().view(
        &view,
        "office/sign-in.html",
        data!({"sent": query.sent.as_deref() == Some("1")}),
    )
}

#[debug_handler]
async fn request_link(
    State(ctx): State<AppContext>,
    OrganizerDb { organizer, .. }: OrganizerDb,
    Form(form): Form<SignInForm>,
) -> Result<Response> {
    if form.email.len() > 254 || !form.email.contains('@') {
        return Ok((
            StatusCode::BAD_REQUEST,
            "That email address doesn't look right. Please check it.",
        )
            .into_response());
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
        Err(MemberError::RateLimited) => {
            return Ok((
                StatusCode::TOO_MANY_REQUESTS,
                "Too many tries. Please wait a little and try again.",
            )
                .into_response())
        }
        Err(error) => return Err(loco_rs::Error::string(&error.to_string())),
    };
    if let Some(token) = token {
        let origin = if Edition::from_environment() == Edition::Hosted {
            crate::hosted::origin(Some(&organizer.subdomain))
        } else {
            ctx.config.server.full_url()
        };
        let url = format!("{origin}/office/magic-link/{token}");
        if let Err(error) = MemberMailer::send_magic_link(&ctx, form.email.trim(), &url).await {
            tracing::warn!(%error, "Member sign-in link delivery failed or is uncertain");
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                "Sign-in email delivery failed or is uncertain. Please wait a little and request a new link from the sign-in page. Another attempt may deliver a duplicate email.",
            )
                .into_response());
        }
    }
    Ok(Redirect::to("/office/sign-in?sent=1").into_response())
}

#[debug_handler]
async fn claim_link(
    State(ctx): State<AppContext>,
    OrganizerDb { organizer, .. }: OrganizerDb,
    Path(token): Path<String>,
    jar: CookieJar,
) -> Result<Response> {
    let session = members::claim_magic_link(&ctx.db, &organizer.id, &token, Utc::now().timestamp())
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let Some(session) = session else {
        return Ok((
            StatusCode::UNAUTHORIZED,
            "This sign-in link has expired or was already used. Ask for a new one.",
        )
            .into_response());
    };
    let member = members::find_session(&ctx.db, &organizer.id, &session, Utc::now().timestamp())
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    let Some(member) = member else {
        return Ok((
            StatusCode::UNAUTHORIZED,
            "Your membership is no longer available. Contact your organizer.",
        )
            .into_response());
    };
    let destination = match member.role {
        Role::Admin => "/office/events",
        Role::DoorStaff => "/door",
    };
    let cookie = Cookie::build(("cg_session", session))
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(ctx.environment == loco_rs::environment::Environment::Production)
        .path("/")
        .build();
    Ok((jar.add(cookie), Redirect::to(destination)).into_response())
}

async fn sign_out(
    State(ctx): State<AppContext>,
    OrganizerDb { organizer, .. }: OrganizerDb,
    jar: CookieJar,
) -> Result<Response> {
    if let Some(cookie) = jar.get("cg_session") {
        members::revoke_session(&ctx.db, &organizer.id, cookie.value())
            .await
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    }
    let cookie = Cookie::build(("cg_session", "")).path("/").build();
    Ok((jar.remove(cookie), Redirect::to("/office/sign-in")).into_response())
}

pub fn routes() -> Routes {
    Routes::new()
        .prefix("office")
        .add("/sign-in", get(sign_in).post(request_link))
        .add("/sign-out", axum::routing::post(sign_out))
        .add("/magic-link/{token}", get(claim_link))
}
