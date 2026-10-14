//! Optional error reporting through the Sentry SDK.
//!
//! Reporting is off unless `SENTRY_DSN` is set. Events carry the route
//! template and never the request path, query, headers, cookies, or body,
//! because Door Links and similar URLs hold a token.

use std::cell::RefCell;

use axum::{
    extract::{MatchedPath, Request},
    middleware::Next,
    response::Response,
    Router,
};
use sentry::{
    integrations::{
        tower::NewSentryLayer,
        tracing::{event_from_event, EventMapping},
    },
    protocol::{Event, Exception},
    types::Dsn,
    ClientInitGuard, ClientOptions, Hub, Level,
};
use tracing::field::{Field, Visit};
use tracing_subscriber::{registry::LookupSpan, Layer};

const CONTROLLER_TARGET: &str = "loco_rs::controller";
const CATCH_PANIC_TARGET: &str = "loco_rs::controller::middleware::catch_panic";
const REQUEST_TRACE_TARGET: &str = "tower_http::trace::on_failure";

tokio::task_local! {
    static CONTROLLER_ERROR: RefCell<Option<String>>;
}

/// Starts the SDK from `SENTRY_DSN`. Call this before the async runtime
/// starts, and keep the guard until the process exits.
///
/// # Errors
/// Returns an error when `SENTRY_DSN` is set and is not a valid DSN.
pub fn init() -> Result<Option<ClientInitGuard>, String> {
    init_with(std::env::var("SENTRY_DSN").ok())
}

/// # Errors
/// Returns an error when `dsn` is not a valid DSN.
pub fn init_with(dsn: Option<String>) -> Result<Option<ClientInitGuard>, String> {
    let Some(dsn) = dsn.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    let dsn: Dsn = dsn
        .trim()
        .parse()
        .map_err(|error| format!("SENTRY_DSN is not a valid DSN: {error}"))?;
    let mut options = ClientOptions::default();
    options.dsn = Some(dsn);
    options.release = std::env::var("SENTRY_RELEASE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(Into::into)
        .or_else(|| sentry::release_name!());
    options.send_default_pii = false;
    options.auto_session_tracking = false;
    Ok(Some(sentry::init(options)))
}

#[must_use]
pub fn enabled() -> bool {
    Hub::current()
        .client()
        .is_some_and(|client| client.is_enabled())
}

/// Reports each `ERROR` log line. A Loco controller error is held for the
/// request middleware, which knows the response status.
pub fn tracing_layer<S>() -> impl Layer<S>
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span>,
{
    sentry::integrations::tracing::layer()
        .span_filter(|_| false)
        .event_mapper(|event, ctx| {
            let metadata = event.metadata();
            if *metadata.level() != tracing::Level::ERROR {
                return EventMapping::Ignore;
            }
            // The panic integration reports the panic, and the request
            // middleware reports each 5xx response.
            if [CATCH_PANIC_TARGET, REQUEST_TRACE_TARGET].contains(&metadata.target()) {
                return EventMapping::Ignore;
            }
            if metadata.target() == CONTROLLER_TARGET && hold_controller_error(event) {
                return EventMapping::Ignore;
            }
            EventMapping::Event(Box::new(event_from_event(event, &ctx)))
        })
}

/// Adds error reporting to the router. Returns the router unchanged when
/// reporting is off.
pub fn layer(router: Router) -> Router {
    if !enabled() {
        return router;
    }
    router
        .layer(axum::middleware::from_fn(report_server_error))
        .layer(NewSentryLayer::<Request>::new_from_top())
}

async fn report_server_error(request: Request, next: Next) -> Response {
    let route = format!(
        "{} {}",
        request.method(),
        request
            .extensions()
            .get::<MatchedPath>()
            .map_or("unmatched", MatchedPath::as_str)
    );
    sentry::configure_scope(|scope| scope.set_tag("route", &route));

    let (response, controller_error) = CONTROLLER_ERROR
        .scope(RefCell::new(None), async {
            let response = next.run(request).await;
            (response, CONTROLLER_ERROR.with(RefCell::take))
        })
        .await;

    // A panic or an `ERROR` log line in this request is already reported.
    let status = response.status();
    if status.is_server_error() && Hub::current().last_event_id().is_none() {
        if let Some(message) = controller_error {
            sentry::capture_event(Event {
                level: Level::Error,
                exception: vec![Exception {
                    ty: "ControllerError".to_owned(),
                    value: Some(message),
                    ..Exception::default()
                }]
                .into(),
                ..Event::default()
            });
        } else {
            sentry::capture_message(
                &format!("HTTP {} response on {route}", status.as_u16()),
                Level::Error,
            );
        }
    }
    response
}

fn hold_controller_error(event: &tracing::Event<'_>) -> bool {
    let mut fields = ControllerErrorFields::default();
    event.record(&mut fields);
    let Some(message) = fields.error else {
        return false;
    };
    CONTROLLER_ERROR
        .try_with(|held| *held.borrow_mut() = Some(message))
        .is_ok()
}

#[derive(Default)]
struct ControllerErrorFields {
    error: Option<String>,
}

impl Visit for ControllerErrorFields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "error.msg" {
            self.error = Some(format!("{value:?}"));
        }
    }
}
