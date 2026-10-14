use std::sync::Arc;

use axum::Extension;
use axum::{
    extract::{Form, Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use loco_rs::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde::Deserialize;
use serde_json::json;

use crate::{
    auth::AdminOrganizer,
    hosted,
    mailers::{hosted::HostedMailer, member::MemberMailer},
    operator,
    organizers::{Edition, OrganizerRegistry},
};

fn error(value: impl std::fmt::Display) -> loco_rs::Error {
    loco_rs::Error::string(&value.to_string())
}

#[derive(Deserialize)]
struct EmailForm {
    email: String,
}

#[derive(Deserialize)]
struct GrantForm {
    hours: i64,
}

#[derive(Deserialize)]
struct SuspensionForm {
    reason: String,
}

#[derive(Deserialize)]
struct SetupStatusForm {
    status: String,
}

#[derive(Deserialize)]
struct CreateOrganizerForm {
    name: String,
    subdomain: String,
    email: String,
}

async fn sign_in(ViewEngine(view): ViewEngine<TeraView>) -> Result<Response> {
    if Edition::from_environment() != Edition::Hosted {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    format::render().view(&view, "operator/sign-in.html", data!({"sent": false}))
}

async fn request_link(
    State(ctx): State<AppContext>,
    Form(form): Form<EmailForm>,
) -> Result<Response> {
    if Edition::from_environment() != Edition::Hosted {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    if let Some(token) = operator::request_link(&ctx.db, &form.email, crate::hosted::now())
        .await
        .map_err(error)?
    {
        MemberMailer::send_magic_link(
            &ctx,
            &form.email,
            &format!(
                "{}/operator/magic-link/{token}",
                crate::hosted::origin(None)
            ),
        )
        .await?;
    }
    Ok(Redirect::to("/operator/sign-in?sent=1").into_response())
}

async fn claim(
    State(ctx): State<AppContext>,
    Path(token): Path<String>,
    jar: CookieJar,
) -> Result<Response> {
    if Edition::from_environment() != Edition::Hosted {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let Some(session) = operator::claim_link(&ctx.db, &token, crate::hosted::now())
        .await
        .map_err(error)?
    else {
        return Ok(StatusCode::UNAUTHORIZED.into_response());
    };
    let base_domain =
        std::env::var("SHOP_BASE_DOMAIN").unwrap_or_else(|_| "crabgate.com".to_owned());
    let cookie = Cookie::build(("cg_operator_session", session))
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(ctx.environment == loco_rs::environment::Environment::Production)
        .domain(base_domain)
        .path("/")
        .build();
    Ok((jar.add(cookie), Redirect::to("/operator")).into_response())
}

async fn sign_out(State(ctx): State<AppContext>, jar: CookieJar) -> Result<Response> {
    if let Some(cookie) = jar.get("cg_operator_session") {
        operator::revoke_session(&ctx.db, cookie.value())
            .await
            .map_err(error)?;
    }
    let base_domain =
        std::env::var("SHOP_BASE_DOMAIN").unwrap_or_else(|_| "crabgate.com".to_owned());
    let cookie = Cookie::build(("cg_operator_session", ""))
        .domain(base_domain)
        .path("/")
        .build();
    Ok((jar.remove(cookie), Redirect::to("/operator/sign-in")).into_response())
}

async fn operator_id(ctx: &AppContext, jar: &CookieJar) -> Result<Option<String>> {
    let Some(token) = jar.get("cg_operator_session") else {
        return Ok(None);
    };
    operator::session(&ctx.db, token.value(), crate::hosted::now())
        .await
        .map_err(error)
}

async fn index(
    State(ctx): State<AppContext>,
    jar: CookieJar,
    ViewEngine(view): ViewEngine<TeraView>,
) -> Result<Response> {
    if Edition::from_environment() != Edition::Hosted {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    if operator_id(&ctx, &jar).await?.is_none() {
        return Ok(Redirect::to("/operator/sign-in").into_response());
    }
    let rows = ctx.db.query_all_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT o.id, o.name, o.subdomain, o.suspension_reason, COALESCE(s.status, 'Pending') AS status, sr.status AS setup_status FROM organizers o LEFT JOIN subscriptions s ON s.organizer_id = o.id LEFT JOIN setup_requests sr ON sr.organizer_id = o.id ORDER BY o.created_at DESC".to_owned()
    )).await.map_err(error)?;
    let mut organizers = Vec::new();
    for row in rows {
        organizers.push(json!({
            "id": row.try_get::<String>("", "id").map_err(error)?,
            "name": row.try_get::<String>("", "name").map_err(error)?,
            "subdomain": row.try_get::<String>("", "subdomain").map_err(error)?,
            "suspension_reason": row.try_get::<Option<String>>("", "suspension_reason").map_err(error)?,
            "status": row.try_get::<String>("", "status").map_err(error)?,
            "setup_status": row.try_get::<Option<String>>("", "setup_status").map_err(error)?
        }));
    }
    format::render().view(
        &view,
        "operator/index.html",
        data!({"organizers": organizers}),
    )
}

async fn update_setup(
    State(ctx): State<AppContext>,
    jar: CookieJar,
    Path(id): Path<String>,
    Form(form): Form<SetupStatusForm>,
) -> Result<Response> {
    if Edition::from_environment() != Edition::Hosted {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    if operator_id(&ctx, &jar).await?.is_none() {
        return Ok(StatusCode::FORBIDDEN.into_response());
    }
    if !hosted::update_setup_status(&ctx.db, &id, &form.status, hosted::now())
        .await
        .map_err(error)?
    {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    }
    Ok(Redirect::to("/operator").into_response())
}

async fn create_organizer(
    State(ctx): State<AppContext>,
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    jar: CookieJar,
    Form(form): Form<CreateOrganizerForm>,
) -> Result<Response> {
    if Edition::from_environment() != Edition::Hosted {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let Some(operator_id) = operator_id(&ctx, &jar).await? else {
        return Ok(StatusCode::FORBIDDEN.into_response());
    };
    let signup = match hosted::signup(
        &registry,
        &ctx.db,
        &form.name,
        &form.subdomain,
        &form.email,
        hosted::now(),
    )
    .await
    {
        Ok(signup) => signup,
        Err(hosted::HostedError::InvalidSignup) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                "Enter a valid name, Shop address, and Admin email",
            )
                .into_response());
        }
        Err(hosted::HostedError::ShopAddressTaken) => {
            return Ok((StatusCode::CONFLICT, "This Shop address is taken").into_response());
        }
        Err(cause) => return Err(error(cause)),
    };
    operator::audit(
        &ctx.db,
        &signup.organizer.id,
        &operator_id,
        "POST",
        "/operator/organizers",
        hosted::now(),
    )
    .await
    .map_err(error)?;
    let url = format!(
        "{}/verify/{}",
        hosted::origin(None),
        signup.verification_token
    );
    HostedMailer::verify(&ctx, form.email.trim(), &url).await?;
    Ok(Redirect::to("/operator").into_response())
}

async fn suspend(
    State(ctx): State<AppContext>,
    jar: CookieJar,
    Path(id): Path<String>,
    Form(form): Form<SuspensionForm>,
) -> Result<Response> {
    let Some(operator_id) = operator_id(&ctx, &jar).await? else {
        return Ok(StatusCode::FORBIDDEN.into_response());
    };
    let reason = form.reason.trim();
    if reason.is_empty() || reason.len() > 200 {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    }
    ctx.db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE organizers SET suspension_reason = ? WHERE id = ?",
            [reason.into(), id.clone().into()],
        ))
        .await
        .map_err(error)?;
    operator::audit(
        &ctx.db,
        &id,
        &operator_id,
        "POST",
        "/operator/suspend",
        crate::hosted::now(),
    )
    .await
    .map_err(error)?;
    Ok(Redirect::to("/operator").into_response())
}

async fn restore(
    State(ctx): State<AppContext>,
    jar: CookieJar,
    Path(id): Path<String>,
) -> Result<Response> {
    let Some(operator_id) = operator_id(&ctx, &jar).await? else {
        return Ok(StatusCode::FORBIDDEN.into_response());
    };
    ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE organizers SET suspension_reason = NULL WHERE id = ? AND suspension_reason != 'migration failed'", [id.clone().into()]
    )).await.map_err(error)?;
    operator::audit(
        &ctx.db,
        &id,
        &operator_id,
        "POST",
        "/operator/restore",
        crate::hosted::now(),
    )
    .await
    .map_err(error)?;
    Ok(Redirect::to("/operator").into_response())
}

async fn grants(
    State(ctx): State<AppContext>,
    AdminOrganizer { organizer, .. }: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
) -> Result<Response> {
    let rows = ctx.db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT id, expires_at, revoked_at FROM access_grants WHERE organizer_id = ? ORDER BY expires_at DESC", [organizer.id.clone().into()]
    )).await.map_err(error)?;
    let mut grants = Vec::new();
    for row in rows {
        grants.push(json!({"id": row.try_get::<String>("", "id").map_err(error)?, "expires_at": row.try_get::<i64>("", "expires_at").map_err(error)?, "revoked": row.try_get::<Option<i64>>("", "revoked_at").map_err(error)?.is_some()}));
    }
    format::render().view(
        &view,
        "office/access-grants.html",
        data!({"organizer_name": organizer.name, "grants": grants}),
    )
}

async fn create_grant(
    State(ctx): State<AppContext>,
    AdminOrganizer {
        organizer,
        member_id,
        ..
    }: AdminOrganizer,
    Form(form): Form<GrantForm>,
) -> Result<Response> {
    if ![24, 72, 168].contains(&form.hours) {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    }
    operator::grant(
        &ctx.db,
        &organizer.id,
        &member_id,
        form.hours,
        crate::hosted::now(),
    )
    .await
    .map_err(error)?;
    Ok(Redirect::to("/office/access-grants").into_response())
}

async fn revoke_grant(
    State(ctx): State<AppContext>,
    AdminOrganizer { organizer, .. }: AdminOrganizer,
    Path(id): Path<String>,
) -> Result<Response> {
    operator::revoke(&ctx.db, &organizer.id, &id, crate::hosted::now())
        .await
        .map_err(error)?;
    Ok(Redirect::to("/office/access-grants").into_response())
}

pub fn routes() -> Routes {
    Routes::new()
        .add("/operator", get(index))
        .add("/operator/sign-in", get(sign_in).post(request_link))
        .add("/operator/sign-out", post(sign_out))
        .add("/operator/magic-link/{token}", get(claim))
        .add("/operator/organizers", post(create_organizer))
        .add("/operator/organizers/{id}/suspend", post(suspend))
        .add("/operator/organizers/{id}/restore", post(restore))
        .add("/operator/organizers/{id}/setup", post(update_setup))
        .add("/office/access-grants", get(grants).post(create_grant))
        .add("/office/access-grants/{id}/revoke", post(revoke_grant))
}
