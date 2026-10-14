use std::path::Path;

use async_trait::async_trait;
use axum::{
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use loco_rs::{
    app::{AppContext, Hooks, Initializer},
    bgworker::Queue,
    boot::{create_app, BootResult, StartMode},
    config::Config,
    controller::AppRoutes,
    environment::Environment,
    logger,
    task::Tasks,
    Result,
};
use migration::Migrator;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer};

use crate::{controllers, error_reporting, initializers, security};

pub struct App;

#[async_trait]
impl Hooks for App {
    async fn on_shutdown(ctx: &AppContext) {
        if let Some(runtime) = ctx
            .shared_store
            .get::<std::sync::Arc<crate::initializers::organizers::OrganizerRuntime>>()
        {
            if let Err(error) = runtime.shutdown().await {
                tracing::error!(%error, "Organizer shutdown failed");
            }
        }
    }

    fn app_name() -> &'static str {
        env!("CARGO_CRATE_NAME")
    }

    fn app_version() -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }

    async fn boot(
        mode: StartMode,
        environment: &Environment,
        config: Config,
    ) -> Result<BootResult> {
        create_app::<Self, Migrator>(mode, environment, config).await
    }

    fn init_logger(ctx: &AppContext) -> Result<bool> {
        if !error_reporting::enabled() {
            return Ok(false);
        }
        let config = &ctx.config.logger;
        if config
            .file_appender
            .as_ref()
            .is_some_and(|appender| appender.enable)
        {
            return Err(loco_rs::Error::string(
                "The log file appender is not supported when SENTRY_DSN is set",
            ));
        }
        let mut layers = vec![error_reporting::tracing_layer().boxed()];
        if config.enable {
            layers.push(logger::init_layer(std::io::stdout, &config.format, true));
        }
        tracing_subscriber::registry()
            .with(layers)
            .with(logger::init_env_filter::<Self>(
                config.override_filter.as_ref(),
                &config.level,
            ))
            .init();
        Ok(true)
    }

    async fn initializers(_ctx: &AppContext) -> Result<Vec<Box<dyn Initializer>>> {
        Ok(vec![
            Box::new(initializers::view_engine::ViewEngineInitializer),
            Box::new(initializers::organizers::OrganizerInitializer),
        ])
    }

    fn routes(_ctx: &AppContext) -> AppRoutes {
        AppRoutes::with_default_routes()
            .add_route(controllers::members::routes())
            .add_route(controllers::events::routes())
            .add_route(controllers::buyer_messages::routes())
            .add_route(controllers::checkout::routes())
            .add_route(controllers::door::routes())
            .add_route(controllers::admin_orders::routes())
            .add_route(controllers::setup::routes())
            .add_route(controllers::hosted::routes())
            .add_route(controllers::operator::routes())
            .add_route(controllers::webhooks::routes())
            .add_route(controllers::page::routes())
            .add_route(controllers::discovery::routes())
    }

    async fn after_routes(router: axum::Router, _ctx: &AppContext) -> Result<axum::Router> {
        Ok(error_reporting::layer(
            router
                .fallback(|| async { not_found() })
                .layer(DefaultBodyLimit::max(3_500_000))
                .layer(axum::middleware::from_fn(security::protect))
                .layer(axum::middleware::from_fn(crate::hosted::redirect_www)),
        ))
    }

    async fn connect_workers(_ctx: &AppContext, _queue: &Queue) -> Result<()> {
        Ok(())
    }

    fn register_tasks(_tasks: &mut Tasks) {}

    async fn truncate(_ctx: &AppContext) -> Result<()> {
        Ok(())
    }

    async fn seed(_ctx: &AppContext, _base: &Path) -> Result<()> {
        Ok(())
    }
}

pub fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(include_str!("../assets/static/404.html")),
    )
        .into_response()
}
