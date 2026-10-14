use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use crabgate::error_reporting;
use sentry::{
    protocol::Event, test::TestTransport, Client, ClientOptions, Hub, Scope, SentryFutureExt,
};
use tower::ServiceExt;
use tower_http::{catch_panic::CatchPanicLayer, trace::TraceLayer};
use tracing_subscriber::layer::SubscriberExt;

const TOKEN: &str = "door-token-7f3a91";

async fn panics() -> Response {
    panic!("seat map exploded")
}

async fn fails() -> loco_rs::Result<Response> {
    Err(loco_rs::Error::string("stripe call failed"))
}

async fn missing() -> loco_rs::Result<Response> {
    Err(loco_rs::Error::NotFound)
}

async fn bare_500() -> StatusCode {
    StatusCode::INTERNAL_SERVER_ERROR
}

async fn logs_then_fails() -> StatusCode {
    tracing::error!(error = "smtp refused", "Ticket email retry job failed");
    StatusCode::INTERNAL_SERVER_ERROR
}

// Loco installs its panic handler and its request tracer inside
// `after_routes`, so the reporting layers wrap the same two here.
fn app() -> Router {
    error_reporting::layer(
        Router::new()
            .route("/panics/{token}", get(panics))
            .route("/fails/{token}", get(fails))
            .route("/missing/{token}", get(missing))
            .route("/bare/{token}", get(bare_500))
            .route("/logs/{token}", get(logs_then_fails))
            .layer(CatchPanicLayer::custom(|_| {
                loco_rs::Error::InternalServerError.into_response()
            }))
            .layer(TraceLayer::new_for_http()),
    )
}

async fn events_for(path: &str) -> (StatusCode, Vec<Event<'static>>) {
    let transport = TestTransport::new();
    let mut options = ClientOptions::default();
    options.dsn = Some("https://public@example.com/1".parse().unwrap());
    options.transport = Some(Arc::new(transport.clone()));
    let hub = Arc::new(Hub::new(
        Some(Arc::new(Client::from(sentry::apply_defaults(options)))),
        Arc::new(Scope::default()),
    ));
    let _subscriber = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(error_reporting::tracing_layer()),
    );
    let path = path.to_owned();
    let status = async move {
        app()
            .oneshot(
                Request::get(format!("{path}?code=query-secret"))
                    .header("cookie", "session=cookie-secret")
                    .header("authorization", "Bearer header-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }
    .bind_hub(hub)
    .await;
    (status, transport.fetch_and_clear_events())
}

fn exception_value<'a>(event: &'a Event<'static>) -> Option<&'a str> {
    event.exception.last()?.value.as_deref()
}

#[tokio::test]
async fn a_controller_error_is_reported_once_with_its_text() {
    let (status, events) = events_for(&format!("/fails/{TOKEN}")).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(events.len(), 1);
    assert_eq!(exception_value(&events[0]), Some("stripe call failed"));
    assert_eq!(events[0].exception[0].ty, "ControllerError");
}

#[tokio::test]
async fn a_handler_panic_is_reported_once_with_its_text() {
    let (status, events) = events_for(&format!("/panics/{TOKEN}")).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(events.len(), 1);
    assert_eq!(exception_value(&events[0]), Some("seat map exploded"));
    let trace = events[0].exception[0].stacktrace.as_ref().unwrap();
    assert!(trace.frames.iter().any(|frame| {
        frame.filename.as_deref() == Some("error_reporting.rs") && frame.lineno == Some(21)
    }));
}

#[tokio::test]
async fn a_not_found_error_is_not_reported() {
    let (status, events) = events_for(&format!("/missing/{TOKEN}")).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(events.len(), 0);
}

#[tokio::test]
async fn a_500_without_an_error_is_reported_with_the_route() {
    let (_, events) = events_for(&format!("/bare/{TOKEN}")).await;

    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].message.as_deref(),
        Some("HTTP 500 response on GET /bare/{token}")
    );
}

#[tokio::test]
async fn an_error_log_line_is_reported_once() {
    let (_, events) = events_for(&format!("/logs/{TOKEN}")).await;

    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].message.as_deref(),
        Some("Ticket email retry job failed")
    );
}

#[tokio::test]
async fn an_event_names_the_route_and_holds_no_request_secret() {
    for path in ["/fails", "/panics", "/bare", "/logs"] {
        let (_, events) = events_for(&format!("{path}/{TOKEN}")).await;

        assert_eq!(
            events[0].tags.get("route").map(String::as_str),
            Some(format!("GET {path}/{{token}}").as_str())
        );
        let sent = serde_json::to_string(&events[0]).unwrap();
        for secret in [TOKEN, "query-secret", "cookie-secret", "header-secret"] {
            assert!(!sent.contains(secret), "{path} event contains {secret}");
        }
    }
}

#[test]
fn reporting_stays_off_without_a_dsn() {
    assert!(error_reporting::init_with(None).unwrap().is_none());
    assert!(error_reporting::init_with(Some("  ".to_owned()))
        .unwrap()
        .is_none());
    assert!(!error_reporting::enabled());
}

#[test]
fn an_invalid_dsn_stops_startup() {
    assert_eq!(
        error_reporting::init_with(Some("not a dsn".to_owned())).err(),
        Some("SENTRY_DSN is not a valid DSN: no valid url provided".to_owned())
    );
}
