use axum::{
    extract::{FromRequestParts, State},
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::CookieJar;
use chrono::Utc;
use loco_rs::app::AppContext;

use crate::{
    door,
    members::{self, Role},
    operator,
    organizers::{Organizer, OrganizerDb},
};

pub struct DoorAccess {
    pub organizer: Organizer,
    pub db: tenant_sqlite_loco::TenantHandle,
    pub event_id: String,
    pub actor_type: &'static str,
    pub actor_id: String,
    pub can_undo: bool,
    pub link_token: Option<String>,
}

impl FromRequestParts<AppContext> for DoorAccess {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppContext,
    ) -> Result<Self, Self::Rejection> {
        let event_id = parts
            .uri
            .path()
            .trim_start_matches("/door/")
            .split('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        if event_id.is_empty() {
            return Err(StatusCode::NOT_FOUND.into_response());
        }
        let OrganizerDb { organizer, db } = OrganizerDb::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        let jar = CookieJar::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        let State(ctx) = State::<AppContext>::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        if let Some(cookie) = jar.get("cg_session") {
            let session = members::find_session(
                &ctx.db,
                &organizer.id,
                cookie.value(),
                Utc::now().timestamp(),
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
            if let Some(session) = session {
                return Ok(Self {
                    organizer,
                    db,
                    event_id,
                    actor_type: "Member",
                    actor_id: session.member_id,
                    can_undo: true,
                    link_token: None,
                });
            }
        }
        let token = parts
            .uri
            .query()
            .and_then(|query| query.split('&').find_map(|part| part.strip_prefix("link=")))
            .unwrap_or_default();
        let link_id = door::verify_link(&db, &event_id, token, Utc::now().timestamp())
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
        let Some(link_id) = link_id else {
            return Err(StatusCode::FORBIDDEN.into_response());
        };
        Ok(Self {
            organizer,
            db,
            event_id,
            actor_type: "Door Link",
            actor_id: link_id,
            can_undo: false,
            link_token: Some(token.to_owned()),
        })
    }
}

pub struct AdminOrganizer {
    pub organizer: Organizer,
    pub db: tenant_sqlite_loco::TenantHandle,
    pub member_id: String,
}

impl FromRequestParts<AppContext> for AdminOrganizer {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppContext,
    ) -> Result<Self, Self::Rejection> {
        let OrganizerDb { organizer, db } = OrganizerDb::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        let jar = CookieJar::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        let State(ctx) = State::<AppContext>::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        if let Some(token) = jar.get("cg_session") {
            let session = members::find_session(
                &ctx.db,
                &organizer.id,
                token.value(),
                Utc::now().timestamp(),
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
            match session {
                Some(session) if session.role == Role::Admin => {
                    return Ok(Self {
                        organizer,
                        db,
                        member_id: session.member_id,
                    })
                }
                Some(_) => return Err(StatusCode::FORBIDDEN.into_response()),
                None => {}
            }
        }
        if let Some(token) = jar.get("cg_operator_session") {
            let now = Utc::now().timestamp();
            let operator_id = operator::session(&ctx.db, token.value(), now)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
            if let Some(operator_id) = operator_id {
                let path = parts.uri.path();
                if !operator::has_grant(&ctx.db, &organizer.id, now)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?
                    || parts.method != axum::http::Method::GET
                    || path.starts_with("/office/setup")
                    || path.starts_with("/office/members")
                    || path.starts_with("/office/door")
                    || path.starts_with("/office/access-grants")
                    || path.starts_with("/office/billing")
                {
                    return Err(StatusCode::FORBIDDEN.into_response());
                }
                operator::audit(
                    &ctx.db,
                    &organizer.id,
                    &operator_id,
                    parts.method.as_str(),
                    path,
                    now,
                )
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
                return Ok(Self {
                    organizer,
                    db,
                    member_id: String::new(),
                });
            }
        }
        Err(Redirect::to("/office/sign-in").into_response())
    }
}
