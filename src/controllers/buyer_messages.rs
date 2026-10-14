use std::sync::Arc;

use axum::{
    extract::{Form, Path},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Extension,
};
use loco_rs::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    auth::AdminOrganizer,
    buyer_messages::{self, MessageError},
    events,
    organizers::OrganizerRegistry,
};

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct Draft {
    subject: String,
    body: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Intent {
    confirm_send: Option<String>,
    snapshot: String,
    retry_uncertain: Option<String>,
}

async fn page(
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    admin: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(event_id): Path<String>,
) -> Result<Response> {
    render(&registry, &admin, &view, &event_id, Draft::default(), None).await
}

async fn render(
    registry: &OrganizerRegistry,
    admin: &AdminOrganizer,
    view: &TeraView,
    event_id: &str,
    draft: Draft,
    failure: Option<&MessageError>,
) -> Result<Response> {
    let Some(event) = events::get(&admin.db, event_id)
        .await
        .map_err(|_| Error::InternalServerError)?
    else {
        return Ok((StatusCode::NOT_FOUND, "Event unavailable").into_response());
    };
    let cipher = registry
        .cipher(&admin.organizer)
        .map_err(|_| Error::InternalServerError)?;
    let count = buyer_messages::recipients(&*admin.db, &cipher, event_id)
        .await
        .map_err(|_| Error::InternalServerError)?
        .len();
    let messages = buyer_messages::messages(&admin.db, &cipher, event_id)
        .await
        .map_err(|_| Error::InternalServerError)?;
    let status = match failure {
        Some(MessageError::Missing) => StatusCode::NOT_FOUND,
        Some(MessageError::Changed) => StatusCode::CONFLICT,
        Some(MessageError::Invalid | MessageError::Empty) => StatusCode::UNPROCESSABLE_ENTITY,
        Some(_) => StatusCode::INTERNAL_SERVER_ERROR,
        None => StatusCode::OK,
    };
    let error = failure.map(|error| match error {
        MessageError::Unavailable
        | MessageError::Database(_)
        | MessageError::Crypto(_)
        | MessageError::Json(_) => "Message service unavailable. Try again.".to_owned(),
        _ => error.to_string(),
    });
    let response = view.render("office/buyer-messages.html", json!({"organizer_name": admin.organizer.name, "active": "events", "event": {"id": event.id, "title": event.title}, "count": count, "messages": messages, "draft": draft, "error": error, "can_write": !admin.member_id.is_empty()}))?;
    Ok((status, response).into_response())
}

async fn preview(
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    admin: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path(event_id): Path<String>,
    Form(draft): Form<Draft>,
) -> Result<Response> {
    let cipher = registry
        .cipher(&admin.organizer)
        .map_err(|_| Error::InternalServerError)?;
    match buyer_messages::preview(
        &admin.db,
        &cipher,
        &event_id,
        &draft.subject,
        &draft.body,
        chrono::Utc::now().timestamp(),
    )
    .await
    {
        Ok(_) => Ok(
            Redirect::to(&format!("/office/events/{event_id}/messages#history")).into_response(),
        ),
        Err(error) => render(&registry, &admin, &view, &event_id, draft, Some(&error)).await,
    }
}

async fn send(
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    admin: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path((event_id, id)): Path<(String, String)>,
    Form(intent): Form<Intent>,
) -> Result<Response> {
    if intent.confirm_send.as_deref() != Some("yes") {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            "Confirm that you intend to send this Buyer Message",
        )
            .into_response());
    }
    let cipher = registry
        .cipher(&admin.organizer)
        .map_err(|_| Error::InternalServerError)?;
    match buyer_messages::queue(
        &admin.db,
        &cipher,
        &event_id,
        &id,
        &intent.snapshot,
        chrono::Utc::now().timestamp(),
    )
    .await
    {
        Ok(()) => Ok(
            Redirect::to(&format!("/office/events/{event_id}/messages#message-{id}"))
                .into_response(),
        ),
        Err(error) => {
            render(
                &registry,
                &admin,
                &view,
                &event_id,
                Draft::default(),
                Some(&error),
            )
            .await
        }
    }
}

async fn retry(
    Extension(registry): Extension<Arc<OrganizerRegistry>>,
    admin: AdminOrganizer,
    ViewEngine(view): ViewEngine<TeraView>,
    Path((event_id, id)): Path<(String, String)>,
    Form(intent): Form<Intent>,
) -> Result<Response> {
    match buyer_messages::retry(
        &admin.db,
        &event_id,
        &id,
        intent.retry_uncertain.as_deref() == Some("yes"),
        chrono::Utc::now().timestamp(),
    )
    .await
    {
        Ok(()) => Ok(
            Redirect::to(&format!("/office/events/{event_id}/messages#message-{id}"))
                .into_response(),
        ),
        Err(error) => {
            render(
                &registry,
                &admin,
                &view,
                &event_id,
                Draft::default(),
                Some(&error),
            )
            .await
        }
    }
}

pub fn routes() -> Routes {
    Routes::new()
        .add("/office/events/{event_id}/messages", get(page))
        .add("/office/events/{event_id}/messages/preview", post(preview))
        .add("/office/events/{event_id}/messages/{id}/send", post(send))
        .add("/office/events/{event_id}/messages/{id}/retry", post(retry))
}
