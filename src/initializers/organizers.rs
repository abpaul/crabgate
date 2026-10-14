use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::{Extension, Router};
use loco_rs::{
    app::{AppContext, Initializer},
    environment::Environment,
    Error, Result,
};
use tokio::{
    sync::{watch, Notify},
    task::JoinHandle,
    time::Instant,
};

use crate::{
    buyer_messages,
    controllers::admin_orders,
    crypto::MasterKey,
    event_notices, hosted_jobs, operator, order_notices,
    organizers::{Edition, OrganizerRegistry},
    series,
    tenant_jobs::Budget,
};

pub struct OrganizerInitializer;

const TENANT_JOB_PERIOD: Duration = Duration::from_secs(300);

type Job = for<'a> fn(
    &'a AppContext,
    &'a OrganizerRegistry,
) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

struct TenantJob {
    failure: &'static str,
    wake: Arc<Notify>,
    wake_after: Option<Arc<Notify>>,
    run: Job,
}

#[derive(Default)]
pub struct RefundWake {
    refunds: Arc<Notify>,
    notices: Arc<Notify>,
}

impl RefundWake {
    /// Starts the refund and refund notice jobs now instead of at their next period.
    pub fn wake(&self) {
        self.refunds.notify_one();
        self.notices.notify_one();
    }
}

pub struct OrganizerRuntime {
    registry: Arc<OrganizerRegistry>,
    refund_wake: RefundWake,
    stop: watch::Sender<bool>,
    jobs: tokio::sync::Mutex<RuntimeJobs>,
}

#[derive(Default)]
struct RuntimeJobs {
    handles: Vec<JoinHandle<()>>,
    failures: Vec<String>,
    shutdown_result: Option<std::result::Result<(), String>>,
}

impl OrganizerRuntime {
    pub fn registry(&self) -> Arc<OrganizerRegistry> {
        self.registry.clone()
    }

    pub fn wake_refund_work(&self) {
        self.refund_wake.wake();
    }

    pub async fn start_tenant_jobs(&self, ctx: &AppContext) -> Result<()> {
        if *self.stop.borrow() {
            return Err(Error::string("Organizer runtime is stopping"));
        }
        let mut jobs = self.jobs.lock().await;
        if *self.stop.borrow() {
            return Err(Error::string("Organizer runtime is stopping"));
        }
        jobs.handles.extend(spawn_tenant_jobs(
            ctx,
            &self.registry,
            &self.refund_wake,
            TENANT_JOB_PERIOD,
            self.stop.subscribe(),
        ));
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        let mut jobs = self.jobs.lock().await;
        if let Some(result) = &jobs.shutdown_result {
            return result.clone().map_err(|cause| Error::string(&cause));
        }
        self.registry.stop_background_jobs();
        self.stop.send_replace(true);
        while let Some(job) = jobs.handles.last_mut() {
            let result = job.await;
            jobs.handles.pop();
            if let Err(error) = result {
                tracing::error!(%error, "Organizer job stopped unexpectedly");
                jobs.failures
                    .push(format!("Organizer job stopped unexpectedly: {error}"));
            }
        }
        if let Err(error) = self.registry.shutdown().await {
            jobs.failures.push(error.to_string());
        }
        let result = if jobs.failures.is_empty() {
            tracing::info!("Organizer shutdown completed");
            Ok(())
        } else {
            Err(jobs.failures.join("; "))
        };
        jobs.shutdown_result = Some(result.clone());
        result.map_err(|cause| Error::string(&cause))
    }
}

/// One loop per job, so that a slow job cannot delay another job. Each loop runs one pass at
/// a time; a pass that outlasts the period is followed by the next pass at once. Every
/// refund pass wakes the notice job when it ends, so that refund confirmations follow it.
pub fn spawn_tenant_jobs(
    ctx: &AppContext,
    registry: &Arc<OrganizerRegistry>,
    refund_wake: &RefundWake,
    period: Duration,
    stop: watch::Receiver<bool>,
) -> Vec<JoinHandle<()>> {
    let job = |failure, run| TenantJob {
        failure,
        wake: Arc::default(),
        wake_after: None,
        run,
    };
    let jobs = [
        job("Event notice job failed", |ctx, registry| {
            Box::pin(event_notices::run(ctx, registry))
        }),
        TenantJob {
            failure: "Refund job failed",
            wake: refund_wake.refunds.clone(),
            wake_after: Some(refund_wake.notices.clone()),
            run: |ctx, registry| {
                Box::pin(admin_orders::run_refunds(ctx, registry, Budget::REFUNDS))
            },
        },
        TenantJob {
            failure: "Refund and cancellation notice job failed",
            wake: refund_wake.notices.clone(),
            wake_after: None,
            run: |ctx, registry| {
                Box::pin(admin_orders::run_refund_notices(
                    ctx,
                    registry,
                    Budget::REFUNDS,
                ))
            },
        },
        job("Buyer message job failed", |ctx, registry| {
            Box::pin(buyer_messages::run(ctx, registry))
        }),
        job("Ticket email retry job failed", |ctx, registry| {
            Box::pin(order_notices::run(ctx, registry))
        }),
        job("Series top-up job failed", |ctx, registry| {
            Box::pin(series::run(
                &ctx.db,
                registry,
                chrono::Utc::now().timestamp(),
            ))
        }),
    ];
    jobs.into_iter()
        .map(|job| {
            let ctx = ctx.clone();
            let registry = registry.clone();
            let mut stop = stop.clone();
            tokio::spawn(async move {
                let mut next = Instant::now();
                loop {
                    if *stop.borrow() {
                        break;
                    }
                    tokio::select! {
                        biased;
                        changed = stop.changed() => {
                            if changed.is_err() || *stop.borrow() {
                                break;
                            }
                            continue;
                        }
                        _ = tokio::time::timeout_at(next, job.wake.notified()) => {}
                    }
                    if *stop.borrow() {
                        break;
                    }
                    next = Instant::now() + period;
                    if let Err(error) = (job.run)(&ctx, &registry).await {
                        tracing::error!(%error, "{}", job.failure);
                    }
                    if let Some(wake_after) = &job.wake_after {
                        wake_after.notify_one();
                    }
                }
            })
        })
        .collect()
}

fn spawn_hosted_job(
    ctx: &AppContext,
    registry: &Arc<OrganizerRegistry>,
    mut stop: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let ctx = ctx.clone();
    let registry = registry.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3600));
        loop {
            if *stop.borrow() {
                break;
            }
            tokio::select! {
                biased;
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                    continue;
                }
                _ = interval.tick() => {}
            }
            if *stop.borrow() {
                break;
            }
            if let Err(error) =
                hosted_jobs::run(&ctx, &registry, chrono::Utc::now().timestamp()).await
            {
                tracing::error!(%error, "Hosted lifecycle job failed");
            }
        }
    })
}

#[async_trait]
impl Initializer for OrganizerInitializer {
    fn name(&self) -> String {
        "organizers".to_owned()
    }

    async fn after_routes(&self, router: Router, ctx: &AppContext) -> Result<Router> {
        let encoded_master = match std::env::var("CRABGATE_MASTER_KEY") {
            Ok(value) => value,
            Err(_) if ctx.environment == Environment::Test => {
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned()
            }
            Err(_) => return Err(Error::string("CRABGATE_MASTER_KEY is required")),
        };
        let master_key = MasterKey::from_base64(&encoded_master)
            .map_err(|error| Error::string(&error.to_string()))?;
        let database_dir = std::env::var("CRABGATE_ORGANIZER_DATABASE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                if ctx.environment == Environment::Test {
                    PathBuf::from("data/test-organizers")
                } else {
                    PathBuf::from("data/organizers")
                }
            });
        let registry = Arc::new(
            OrganizerRegistry::new(
                ctx.db.clone(),
                ctx.config.database.clone(),
                Edition::from_environment(),
                std::env::var("SHOP_BASE_DOMAIN").unwrap_or_else(|_| "crabgate.com".to_owned()),
                database_dir,
                master_key,
            )
            .map_err(|error| Error::string(&error.to_string()))?,
        );
        registry
            .prepare()
            .await
            .map_err(|error| Error::string(&error.to_string()))?;
        let refund_wake = RefundWake::default();
        let (stop, stopped) = watch::channel(false);
        let mut jobs = Vec::new();
        if ctx.environment != Environment::Test {
            jobs = spawn_tenant_jobs(
                ctx,
                &registry,
                &refund_wake,
                TENANT_JOB_PERIOD,
                stopped.clone(),
            );
        }
        if Edition::from_environment() == Edition::Hosted {
            operator::seed(&ctx.db)
                .await
                .map_err(|error| Error::string(&error.to_string()))?;
            if ctx.environment != Environment::Test {
                jobs.push(spawn_hosted_job(ctx, &registry, stopped));
            }
        }
        ctx.shared_store.insert(Arc::new(OrganizerRuntime {
            registry: registry.clone(),
            refund_wake,
            stop,
            jobs: tokio::sync::Mutex::new(RuntimeJobs {
                handles: jobs,
                ..Default::default()
            }),
        }));
        let router = registry.pool_service().install(router, ctx)?;
        Ok(router.layer(Extension(registry)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    };
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::layer::SubscriberExt;

    fn capture() -> (Arc<Mutex<Vec<String>>>, impl tracing::Subscriber) {
        struct Capture(Arc<Mutex<Vec<String>>>);
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut message = Message(String::new());
                event.record(&mut message);
                self.0.lock().unwrap().push(message.0);
            }
        }
        let logs = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Capture(logs.clone()));
        (logs, subscriber)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn interrupted_shutdown_retains_active_jobs_for_the_next_waiter() {
        loco_rs::testing::prelude::request::<App, _, _>(|_, ctx| async move {
            let runtime = ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
            let release = Arc::new(Notify::new());
            let finished = Arc::new(AtomicBool::new(false));
            let task_release = release.clone();
            let task_finished = finished.clone();
            runtime
                .jobs
                .lock()
                .await
                .handles
                .push(tokio::spawn(async move {
                    task_release.notified().await;
                    task_finished.store(true, Ordering::SeqCst);
                }));
            let mut stopped = runtime.stop.subscribe();
            let closing = runtime.clone();
            let waiter = tokio::spawn(async move { closing.shutdown().await });
            tokio::time::timeout(Duration::from_secs(5), stopped.changed())
                .await
                .unwrap()
                .unwrap();
            assert!(*stopped.borrow());
            waiter.abort();
            assert!(waiter.await.unwrap_err().is_cancelled());
            assert!(!finished.load(Ordering::SeqCst));
            assert_eq!(runtime.jobs.lock().await.handles.len(), 1);
            assert_eq!(
                runtime
                    .start_tenant_jobs(&ctx)
                    .await
                    .unwrap_err()
                    .to_string(),
                "Organizer runtime is stopping"
            );
            release.notify_one();
            let (logs, subscriber) = capture();
            tokio::time::timeout(
                Duration::from_secs(5),
                runtime.shutdown().with_subscriber(subscriber),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(finished.load(Ordering::SeqCst));
            assert_eq!(
                logs.lock()
                    .unwrap()
                    .iter()
                    .filter(|message| message.as_str() == "Organizer shutdown completed")
                    .count(),
                1
            );
            assert!(runtime.registry.resolve("localhost").await.is_err());
            runtime.shutdown().await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unexpected_job_termination_is_reported_after_pools_close() {
        loco_rs::testing::prelude::request::<App, _, _>(|_, ctx| async move {
            let runtime = ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
            let cancelled = tokio::spawn(std::future::pending::<()>());
            cancelled.abort();
            let panicked = tokio::spawn(async { panic!("synthetic organizer job panic") });
            runtime
                .jobs
                .lock()
                .await
                .handles
                .extend([cancelled, panicked]);
            let (logs, subscriber) = capture();
            let failure = tokio::time::timeout(
                Duration::from_secs(5),
                runtime.shutdown().with_subscriber(subscriber),
            )
            .await
            .unwrap()
            .unwrap_err()
            .to_string();
            assert!(failure.contains("synthetic organizer job panic"));
            assert!(failure.contains("cancelled"));
            assert!(!logs
                .lock()
                .unwrap()
                .iter()
                .any(|message| message == "Organizer shutdown completed"));
            assert!(runtime.registry.resolve("localhost").await.is_err());
            assert_eq!(runtime.shutdown().await.unwrap_err().to_string(), failure);
            assert_eq!(
                runtime
                    .start_tenant_jobs(&ctx)
                    .await
                    .unwrap_err()
                    .to_string(),
                "Organizer runtime is stopping"
            );
        })
        .await;
    }
}
