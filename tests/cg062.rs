use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use axum::{
    extract::{Query, State},
    routing::post,
    Form, Json, Router,
};
use crabgate::{
    admin_orders,
    app::App,
    buyer_messages,
    controllers::admin_orders::{run_refund_work, run_refunds},
    crypto::MasterKey,
    door::{self, CheckinResult},
    events::{self, EventInput, TicketTypeInput},
    initializers::organizers::{
        spawn_tenant_jobs, OrganizerInitializer, OrganizerRuntime, RefundWake,
    },
    inventory::{self, Selection},
    members::{self, Role},
    organizers::{Edition, OrganizerDb, OrganizerRegistry},
    series::{self, Frequency},
    settings,
    tenant_jobs::Budget,
};
use loco_rs::{
    app::{AppContext, Initializer},
    testing::prelude::*,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

#[derive(Default)]
struct Stripe {
    delay: Duration,
    reads: usize,
    posts: Vec<String>,
    refunds: HashMap<String, serde_json::Value>,
}

async fn create_refund(
    State(stripe): State<Arc<Mutex<Stripe>>>,
    headers: axum::http::HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let delay = stripe.lock().unwrap().delay;
    tokio::time::sleep(delay).await;
    let intent = form["payment_intent"].clone();
    let body = json!({"id": format!("re_{intent}"), "status": "succeeded", "amount": 2000, "payment_intent": intent});
    let mut state = stripe.lock().unwrap();
    state
        .posts
        .push(headers["idempotency-key"].to_str().unwrap().to_owned());
    state.refunds.insert(intent, body.clone());
    Json(body)
}

async fn refund_history(
    State(stripe): State<Arc<Mutex<Stripe>>>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let delay = stripe.lock().unwrap().delay;
    tokio::time::sleep(delay).await;
    let mut state = stripe.lock().unwrap();
    state.reads += 1;
    let data: Vec<_> = state
        .refunds
        .get(&query["payment_intent"])
        .cloned()
        .into_iter()
        .collect();
    Json(json!({"data": data, "has_more": false}))
}

/// Serves the refund endpoints and points the Stripe client at them until dropped.
struct StripeFake {
    state: Arc<Mutex<Stripe>>,
    server: tokio::task::JoinHandle<()>,
}

impl StripeFake {
    async fn start(delay: Duration) -> Self {
        let state = Arc::new(Mutex::new(Stripe {
            delay,
            ..Default::default()
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::env::set_var(
            "STRIPE_API_BASE",
            format!("http://{}", listener.local_addr().unwrap()),
        );
        let app = Router::new()
            .route("/v1/refunds", post(create_refund).get(refund_history))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { state, server }
    }
}

impl Drop for StripeFake {
    fn drop(&mut self) {
        self.server.abort();
        std::env::remove_var("STRIPE_API_BASE");
    }
}

#[derive(Default)]
struct Smtp {
    delay_ms: AtomicU64,
    stall: AtomicBool,
    received: Mutex<Vec<String>>,
    accepted: Mutex<Vec<String>>,
    hold_ack: AtomicBool,
    body_arrived: tokio::sync::Notify,
    release_ack: tokio::sync::Notify,
}

impl Smtp {
    fn received(&self) -> Vec<String> {
        self.received.lock().unwrap().clone()
    }

    fn accepted(&self) -> Vec<String> {
        self.accepted.lock().unwrap().clone()
    }
}

/// An SMTP server that records each recipient when the message body arrives and again when
/// it replies. It replies after `delay_ms`, and not at all while `stall` is set.
async fn smtp(ctx: &AppContext) -> (AppContext, Arc<Smtp>, tokio::task::JoinHandle<()>) {
    let state = Arc::new(Smtp::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let shared = state.clone();
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let state = shared.clone();
            connections.spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                if write
                    .write_all(b"220 localhost test SMTP\r\n")
                    .await
                    .is_err()
                {
                    return;
                }
                let mut recipient = String::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    let response = if let Some(address) = line.strip_prefix("RCPT TO:<") {
                        recipient = address.trim_end_matches('>').to_owned();
                        "250 ok\r\n"
                    } else if line == "DATA" {
                        if write.write_all(b"354 send data\r\n").await.is_err() {
                            return;
                        }
                        while let Ok(Some(body)) = lines.next_line().await {
                            if body == "." {
                                break;
                            }
                        }
                        state.received.lock().unwrap().push(recipient.clone());
                        state.body_arrived.notify_one();
                        if state.hold_ack.load(Ordering::SeqCst) {
                            state.release_ack.notified().await;
                        }
                        tokio::time::sleep(Duration::from_millis(
                            state.delay_ms.load(Ordering::SeqCst),
                        ))
                        .await;
                        while state.stall.load(Ordering::SeqCst) {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        state.accepted.lock().unwrap().push(recipient.clone());
                        "250 accepted\r\n"
                    } else if line == "QUIT" {
                        let _ = write.write_all(b"221 bye\r\n").await;
                        return;
                    } else {
                        "250 ok\r\n"
                    };
                    if write.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    let mut config = ctx.config.mailer.clone().unwrap().smtp.unwrap();
    config.host = "127.0.0.1".into();
    config.port = port;
    config.secure = false;
    config.auth = None;
    let mut context = ctx.clone();
    context.mailer = Some(loco_rs::mailer::EmailSender::smtp(&config).unwrap());
    (context, state, server)
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[tokio::test]
#[serial]
async fn shutdown_finishes_an_accepted_notice_before_closing_tenant_pools() {
    request::<App, _, _>(|_, ctx| async move {
        let runtime = ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
        let registry = runtime.registry();
        let scope = registry.resolve("localhost").await.unwrap();
        let (event, kind) = event(&scope.db, "2027-06-15", now()).await;
        let ids = orders(&scope, &registry, (&event, &kind), &["shutdown@owned.invalid".to_owned(), "next-pass@owned.invalid".to_owned()]).await;
        scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO event_change_notices (id,event_id,old_when,new_when,old_place,new_place,created_at) VALUES ('shutdown-notice',?,'Yesterday','Tomorrow','Old hall','New hall',?)", [event.clone().into(),now().into()]
        )).await.unwrap();
        scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO event_change_recipients (notice_id,order_id) VALUES ('shutdown-notice',?)", [ids[0].clone().into()]
        )).await.unwrap();
        scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO event_change_notices (id,event_id,old_when,new_when,old_place,new_place,created_at) VALUES ('next-notice',?,'Yesterday','Tomorrow','Old hall','New hall',?)", [event.into(),(now()+1).into()]
        )).await.unwrap();
        scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO event_change_recipients (notice_id,order_id) VALUES ('next-notice',?)", [ids[1].clone().into()]
        )).await.unwrap();
        let observer = sea_orm::Database::connect(format!("sqlite://{}?mode=rw",scope.organizer.db_path)).await.unwrap();
        drop(scope);
        let (mail_ctx, mail, server) = smtp(&ctx).await;
        mail.hold_ack.store(true, Ordering::SeqCst);
        runtime.start_tenant_jobs(&mail_ctx).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5),mail.body_arrived.notified()).await.unwrap();
        assert_eq!(mail.received(),vec!["shutdown@owned.invalid"]);
        assert_eq!(mail.accepted(),Vec::<String>::new());
        let mut shutdown = Box::pin(runtime.shutdown());
        assert!(tokio::time::timeout(Duration::from_millis(200), &mut shutdown).await.is_err(), "Shutdown cancelled the active provider/receipt pass before SMTP acknowledgement");
        let receipt = observer.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT sent_at FROM event_change_recipients WHERE notice_id='shutdown-notice'"
        )).await.unwrap().unwrap().try_get::<Option<i64>>("","sent_at").unwrap();
        assert_eq!(receipt,None);
        assert_eq!(runtime.start_tenant_jobs(&mail_ctx).await.unwrap_err().to_string(), "Organizer runtime is stopping");
        let mut concurrent_shutdown = Box::pin(runtime.shutdown());
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut concurrent_shutdown).await.is_err());
        mail.hold_ack.store(false,Ordering::SeqCst);
        mail.release_ack.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            let (first, second) = tokio::join!(shutdown, concurrent_shutdown);
            first.unwrap();
            second.unwrap();
        }).await.unwrap();
        runtime.shutdown().await.unwrap();
        assert_eq!(mail.accepted(),vec!["shutdown@owned.invalid"]);
        let receipt = observer.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT sent_at FROM event_change_recipients WHERE notice_id='shutdown-notice'"
        )).await.unwrap().unwrap().try_get::<Option<i64>>("","sent_at").unwrap();
        assert!(receipt.is_some());
        let pending = observer.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT sent_at FROM event_change_recipients WHERE notice_id='next-notice'"
        )).await.unwrap().unwrap().try_get::<Option<i64>>("", "sent_at").unwrap();
        assert_eq!(pending, None);
        let mut restarted_ctx = mail_ctx.clone();
        restarted_ctx.shared_store = Arc::default();
        let _ = OrganizerInitializer.after_routes(Router::new(),&restarted_ctx).await.unwrap();
        let restarted = restarted_ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
        restarted.start_tenant_jobs(&restarted_ctx).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), mail.body_arrived.notified()).await.unwrap();
        restarted.shutdown().await.unwrap();
        assert_eq!(mail.received(),vec!["shutdown@owned.invalid", "next-pass@owned.invalid"]);
        assert_eq!(mail.accepted(),vec!["shutdown@owned.invalid", "next-pass@owned.invalid"]);
        let recorded = observer.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM event_change_recipients WHERE sent_at IS NOT NULL"
        )).await.unwrap().unwrap().try_get::<i64>("", "count").unwrap();
        assert_eq!(recorded, 2);
        observer.close().await.unwrap();
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }).await;
}

struct EditionGuard(Option<std::ffi::OsString>);

impl Drop for EditionGuard {
    fn drop(&mut self) {
        match &self.0 {
            Some(value) => std::env::set_var("CRABGATE_EDITION", value),
            None => std::env::remove_var("CRABGATE_EDITION"),
        }
    }
}

#[tokio::test]
#[serial]
async fn shutdown_finishes_a_hosted_warning_before_restart() {
    request::<App, _, _>(|_, ctx| async move {
        let initial = ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
        let scope = initial.registry().resolve("localhost").await.unwrap();
        let organizer_id = scope.organizer.id.clone();
        members::add_member(&ctx.db, &organizer_id, "hosted-shutdown@owned.invalid", Role::Admin).await.unwrap();
        members::add_member(&ctx.db, &organizer_id, "z-next-hosted@owned.invalid", Role::Admin).await.unwrap();
        ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO subscriptions(id,organizer_id,status,lapsed_at) VALUES ('shutdown-hosted',?,'Lapsed',?)",
            [organizer_id.clone().into(), (now()-60*86400).into()]
        )).await.unwrap();
        drop(scope);
        initial.shutdown().await.unwrap();
        let _edition = EditionGuard(std::env::var_os("CRABGATE_EDITION"));
        std::env::set_var("CRABGATE_EDITION", "hosted");
        let (mut mail_ctx, mail, server) = smtp(&ctx).await;
        mail_ctx.environment = loco_rs::environment::Environment::Production;
        mail_ctx.shared_store = Arc::default();
        mail.hold_ack.store(true, Ordering::SeqCst);
        let _ = OrganizerInitializer.after_routes(Router::new(), &mail_ctx).await.unwrap();
        let runtime = mail_ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
        tokio::time::timeout(Duration::from_secs(5), mail.body_arrived.notified()).await.unwrap();
        assert_eq!(mail.received(), vec!["hosted-shutdown@owned.invalid"]);
        let mut shutdown = Box::pin(runtime.shutdown());
        assert!(tokio::time::timeout(Duration::from_millis(200), &mut shutdown).await.is_err());
        let receipt = ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT sent_at FROM hosted_lifecycle_recipients WHERE email='hosted-shutdown@owned.invalid'"
        )).await.unwrap().unwrap().try_get::<Option<i64>>("", "sent_at").unwrap();
        assert_eq!(receipt, None);
        mail.hold_ack.store(false, Ordering::SeqCst);
        mail.release_ack.notify_one();
        tokio::time::timeout(Duration::from_secs(5), shutdown).await.unwrap().unwrap();
        let receipt = ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT sent_at FROM hosted_lifecycle_recipients WHERE email='hosted-shutdown@owned.invalid'"
        )).await.unwrap().unwrap().try_get::<Option<i64>>("", "sent_at").unwrap();
        assert!(receipt.is_some());
        assert_eq!(mail.accepted(), vec!["hosted-shutdown@owned.invalid"]);
        let unsent = ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM hosted_lifecycle_recipients WHERE email='z-next-hosted@owned.invalid' AND sent_at IS NOT NULL"
        )).await.unwrap().unwrap().try_get::<i64>("", "count").unwrap();
        assert_eq!(unsent, 0);
        let mut restarted_ctx = mail_ctx.clone();
        restarted_ctx.shared_store = Arc::default();
        let _ = OrganizerInitializer.after_routes(Router::new(), &restarted_ctx).await.unwrap();
        let restarted = restarted_ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
        tokio::time::timeout(Duration::from_secs(5), mail.body_arrived.notified()).await.unwrap();
        restarted.shutdown().await.unwrap();
        assert_eq!(mail.received(), vec!["hosted-shutdown@owned.invalid", "z-next-hosted@owned.invalid"]);
        assert_eq!(mail.accepted(), vec!["hosted-shutdown@owned.invalid", "z-next-hosted@owned.invalid"]);
        let recorded = ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM hosted_lifecycle_recipients WHERE sent_at IS NOT NULL"
        )).await.unwrap().unwrap().try_get::<i64>("", "count").unwrap();
        assert_eq!(recorded, 2);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }).await;
}

async fn event(db: &DatabaseConnection, date: &str, published_at: i64) -> (String, String) {
    let event = events::create(
        db,
        &EventInput {
            title: "CG062".into(),
            date: date.into(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Hall".into(),
            address: "Main Street".into(),
            description: String::new(),
            event_capacity: Some(50),
            max_per_order: 10,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let kind = events::add_ticket_type(
        db,
        &event,
        &TicketTypeInput {
            name: "Standard".into(),
            price: "20".into(),
            capacity: 50,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(db, &event.id, published_at).await.unwrap();
    (event.id, kind.id)
}

/// Creates Orders whose ticket email is already recorded, so only the work under test is due.
async fn orders(
    scope: &OrganizerDb,
    registry: &OrganizerRegistry,
    (event, kind): (&str, &str),
    emails: &[String],
) -> Vec<String> {
    let cipher = registry.cipher(&scope.organizer).unwrap();
    let mut ids = Vec::new();
    for (n, email) in emails.iter().enumerate() {
        let id = admin_orders::complimentary(
            &scope.db,
            &cipher,
            event,
            kind,
            email,
            "Guest",
            now() + n as i64,
        )
        .await
        .unwrap();
        scope
            .db
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE orders SET email_sent_at = 1 WHERE id = ?",
                [id.clone().into()],
            ))
            .await
            .unwrap();
        ids.push(id);
    }
    ids
}

async fn make_paid(db: &DatabaseConnection, ids: &[String], tag: &str) {
    for (n, id) in ids.iter().enumerate() {
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE orders SET total_cents = 2000, is_complimentary = 0, stripe_payment_intent_id = ? WHERE id = ?",
            [format!("pi_{tag}_{n}").into(), id.clone().into()],
        ))
        .await
        .unwrap();
    }
}

fn addresses(tag: &str, count: usize) -> Vec<String> {
    (0..count)
        .map(|n| format!("{tag}{n}@example.com"))
        .collect()
}

async fn queue_message(
    scope: &OrganizerDb,
    registry: &OrganizerRegistry,
    event: &str,
    subject: &str,
) -> String {
    let cipher = registry.cipher(&scope.organizer).unwrap();
    let id = buyer_messages::preview(&scope.db, &cipher, event, subject, "Come at six", now())
        .await
        .unwrap();
    let snapshot = buyer_messages::messages(&scope.db, &cipher, event)
        .await
        .unwrap()
        .into_iter()
        .find(|message| message.id == id)
        .unwrap()
        .snapshot;
    buyer_messages::queue(&scope.db, &cipher, event, &id, &snapshot, now())
        .await
        .unwrap();
    id
}

async fn recipient_states(db: &DatabaseConnection, message: &str) -> Vec<String> {
    column(
        db,
        "SELECT state AS value FROM buyer_message_recipients WHERE message_id = ? ORDER BY state",
        message,
    )
    .await
}

async fn column(db: &DatabaseConnection, sql: &str, value: &str) -> Vec<String> {
    db.query_all_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        sql,
        [value.into()],
    ))
    .await
    .unwrap()
    .into_iter()
    .map(|row| row.try_get("", "value").unwrap())
    .collect()
}

async fn order_states(db: &DatabaseConnection, event: &str) -> Vec<String> {
    column(db, "SELECT status || '/' || refund_work_state || '/' || cancel_notice_state || '/' || refund_notice_state AS value FROM orders WHERE event_id = ? ORDER BY value", event).await
}

async fn ticket_states(db: &DatabaseConnection, event: &str) -> Vec<String> {
    column(
        db,
        "SELECT status AS value FROM tickets WHERE event_id = ? ORDER BY rowid",
        event,
    )
    .await
}

async fn wait_for(what: &str, mut ready: impl AsyncFnMut() -> bool) {
    let started = Instant::now();
    while !ready().await {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stop(jobs: Vec<tokio::task::JoinHandle<()>>) {
    for job in &jobs {
        job.abort();
    }
    for job in jobs {
        assert!(job.await.unwrap_err().is_cancelled());
    }
}

/// A Hosted registry with two new Organizers, returned in the order the jobs serve them.
async fn two_tenants(
    ctx: &AppContext,
    tag: &str,
) -> (Arc<OrganizerRegistry>, OrganizerDb, OrganizerDb) {
    let registry = Arc::new(
        OrganizerRegistry::new(
            ctx.db.clone(),
            ctx.config.database.clone(),
            Edition::Hosted,
            "crabgate.com".into(),
            std::env::var("CRABGATE_ORGANIZER_DATABASE_DIR")
                .unwrap()
                .into(),
            MasterKey::from_base64(KEY).unwrap(),
        )
        .unwrap(),
    );
    let mut scopes = Vec::new();
    for name in ["one", "two"] {
        let subdomain = format!("cg062-{tag}-{name}");
        registry.create(&subdomain, &subdomain).await.unwrap();
        scopes.push(
            registry
                .resolve(&format!("{subdomain}.crabgate.com"))
                .await
                .unwrap(),
        );
    }
    scopes.sort_by(|a, b| a.organizer.id.cmp(&b.organizer.id));
    let second = scopes.pop().unwrap();
    (registry, scopes.pop().unwrap(), second)
}

#[tokio::test]
#[serial]
async fn cancellation_commits_and_redirects_before_any_provider_call() {
    let stripe = StripeFake::start(Duration::from_millis(300)).await;
    let provider = stripe.state.clone();
    request::<App, _, _>(|request, ctx| async move {
        let runtime = ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
        let registry = runtime.registry();
        let scope = registry.resolve("localhost").await.unwrap();
        let cipher = registry.cipher(&scope.organizer).unwrap();
        members::add_member(&ctx.db, &scope.organizer.id, "cg062@example.com", Role::Admin)
            .await
            .unwrap();
        let link = members::request_magic_link(&ctx.db, &scope.organizer.id, "cg062@example.com", now())
            .await
            .unwrap()
            .unwrap();
        let session = members::claim_magic_link(&ctx.db, &scope.organizer.id, &link, now())
            .await
            .unwrap()
            .unwrap();
        let cookie = format!("cg_session={session}");
        settings::set(&scope.db, &cipher, "stripe_secret_key", "sk_test_cg062_synthetic")
            .await
            .unwrap();
        let (event, kind) = event(&scope.db, "2027-06-15", now()).await;
        let paid = orders(&scope, &registry, (&event, &kind), &addresses("paid", 6)).await;
        make_paid(&scope.db, &paid, "cancel").await;
        orders(&scope, &registry, (&event, &kind), &addresses("free", 1)).await;
        let code = admin_orders::order(&scope.db, &cipher, &paid[0]).await.unwrap().tickets[0].code.clone();
        let route = format!("/office/events/{event}/cancel");

        let started = Instant::now();
        let response = request.post(&route).add_header("cookie", &cookie).await;
        let elapsed = started.elapsed();
        println!("CG062 cancel request elapsed={elapsed:?} for 6 paid orders at 300 ms per provider call");
        assert_eq!(response.status_code(), 303);
        assert_eq!(response.headers()["location"], format!("/office/events/{event}"));
        assert!(elapsed < Duration::from_millis(1500), "the request must not wait for 12 provider calls of 300 ms");
        assert_eq!(provider.lock().unwrap().reads, 0);
        assert_eq!(provider.lock().unwrap().posts.len(), 0);
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 0);
        assert_eq!(events::get(&scope.db, &event).await.unwrap().unwrap().status, "Cancelled");
        let mut queued = vec!["Paid/Pending/Pending/Not requested"; 7];
        assert_eq!(order_states(&scope.db, &event).await, queued);
        let selection = [Selection { ticket_type_id: kind.clone(), quantity: 1 }];
        assert!(inventory::reserve(&scope.db, &event, &selection, now()).await.is_err());
        assert!(matches!(
            door::check_in(&scope.db, &cipher, &event, &code, "Member", "cg062", now()).await.unwrap(),
            CheckinResult::VoidTicket(_)
        ));
        let page = request.get(&format!("/office/events/{event}")).add_header("cookie", &cookie).await;
        assert!(page.text().contains("Refunds and cancellation notices run in the background and can take several minutes."));

        run_refund_work(&ctx, &registry).await.unwrap();
        queued = vec!["Refunded/Completed/Completed/Completed"; 6];
        queued.push("Refunded/Completed/Completed/Not requested");
        assert_eq!(order_states(&scope.db, &event).await, queued);
        assert_eq!(ticket_states(&scope.db, &event).await, vec!["Void"; 7]);
        let mut expected_keys: Vec<_> = paid.iter().map(|id| format!("order-refund-{id}")).collect();
        expected_keys.sort();
        let mut keys = provider.lock().unwrap().posts.clone();
        keys.sort();
        assert_eq!(keys, expected_keys);
        let mail = ctx.mailer.as_ref().unwrap().deliveries();
        assert_eq!(mail.count, 13);
        assert_eq!(mail.messages.iter().filter(|m| m.contains("was refunded in full")).count(), 6);

        assert_eq!(request.post(&route).add_header("cookie", &cookie).await.status_code(), 303);
        run_refund_work(&ctx, &registry).await.unwrap();
        assert_eq!(provider.lock().unwrap().posts.len(), 6);
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 13);
        assert_eq!(events::get(&scope.db, &event).await.unwrap().unwrap().status, "Cancelled");
        assert_eq!(ticket_states(&scope.db, &event).await, vec!["Void"; 7]);
        assert_eq!(order_states(&scope.db, &event).await, queued);
        assert!(inventory::reserve(&scope.db, &event, &selection, now()).await.is_err());
        assert!(matches!(
            door::check_in(&scope.db, &cipher, &event, &code, "Member", "cg062", now()).await.unwrap(),
            CheckinResult::VoidTicket(_)
        ));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cancellation_wakes_the_refund_jobs_before_their_five_minute_period() {
    let stripe = StripeFake::start(Duration::from_millis(300)).await;
    let provider = stripe.state.clone();
    request::<App, _, _>(|request, ctx| async move {
        let runtime = ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
        let registry = runtime.registry();
        let scope = registry.resolve("localhost").await.unwrap();
        members::add_member(
            &ctx.db,
            &scope.organizer.id,
            "cg062-wake@example.com",
            Role::Admin,
        )
        .await
        .unwrap();
        let link = members::request_magic_link(
            &ctx.db,
            &scope.organizer.id,
            "cg062-wake@example.com",
            now(),
        )
        .await
        .unwrap()
        .unwrap();
        let session = members::claim_magic_link(&ctx.db, &scope.organizer.id, &link, now())
            .await
            .unwrap()
            .unwrap();
        let cipher = registry.cipher(&scope.organizer).unwrap();
        settings::set(
            &scope.db,
            &cipher,
            "stripe_secret_key",
            "sk_test_cg062_synthetic",
        )
        .await
        .unwrap();
        let (event, kind) = event(&scope.db, "2027-06-15", now()).await;
        let ids = orders(&scope, &registry, (&event, &kind), &addresses("wake", 2)).await;
        make_paid(&scope.db, &ids[..1], "wake").await;

        runtime.start_tenant_jobs(&ctx).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            order_states(&scope.db, &event).await,
            vec!["Paid/Not requested/Not requested/Not requested"; 2]
        );

        let response = request
            .post(&format!("/office/events/{event}/cancel"))
            .add_header("cookie", format!("cg_session={session}"))
            .await;
        assert_eq!(response.status_code(), 303);
        // The paid refund ends after the woken notice pass has looked at it. Its confirmation
        // needs the notice pass that the refund pass wakes.
        let done = vec![
            "Refunded/Completed/Completed/Completed",
            "Refunded/Completed/Completed/Not requested",
        ];
        wait_for("woken refund jobs", async || {
            order_states(&scope.db, &event).await == done
        })
        .await;
        assert_eq!(ticket_states(&scope.db, &event).await, vec!["Void"; 2]);
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 3);
        assert_eq!(
            provider.lock().unwrap().posts,
            vec![format!("order-refund-{}", ids[0])]
        );
        drop(scope);
        runtime.shutdown().await.unwrap();
    })
    .await;
}

/// A cancelled Event with one paid and one free Order. Nothing wakes the refund jobs.
async fn cancelled_without_wake(
    scope: &OrganizerDb,
    registry: &OrganizerRegistry,
    tag: &str,
) -> (String, String) {
    let cipher = registry.cipher(&scope.organizer).unwrap();
    settings::set(
        &scope.db,
        &cipher,
        "stripe_secret_key",
        "sk_test_cg062_synthetic",
    )
    .await
    .unwrap();
    let (event, kind) = event(&scope.db, "2027-06-15", now()).await;
    let ids = orders(scope, registry, (&event, &kind), &addresses(tag, 2)).await;
    make_paid(&scope.db, &ids[..1], tag).await;
    admin_orders::cancel_event(&scope.db, &event, now())
        .await
        .unwrap();
    (event, ids[0].clone())
}

/// Waits for the paid Order's refund and returns how long its confirmation took after it.
/// The notice pass must see the Order before the refund ends, so the confirmation needs a
/// later notice pass.
async fn confirmation_lag(db: &DatabaseConnection, paid: &str) -> Duration {
    let work = async || {
        column(db, "SELECT refund_work_state || '/' || cancel_notice_state || '/' || refund_notice_state AS value FROM orders WHERE id = ?", paid).await.remove(0)
    };
    wait_for("the cancellation notice before the refund", async || {
        work().await == "Pending/Completed/Not requested"
    })
    .await;
    wait_for("the refund", async || {
        work().await.starts_with("Completed/")
    })
    .await;
    let refunded = Instant::now();
    wait_for("the refund confirmation", async || {
        work().await == "Completed/Completed/Completed"
    })
    .await;
    refunded.elapsed()
}

async fn assert_one_refund_and_one_confirmation(
    ctx: &AppContext,
    scope: &OrganizerDb,
    provider: &Arc<Mutex<Stripe>>,
    (event, paid): (&str, &str),
) {
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        order_states(&scope.db, event).await,
        vec![
            "Refunded/Completed/Completed/Completed",
            "Refunded/Completed/Completed/Not requested",
        ]
    );
    assert_eq!(
        provider.lock().unwrap().posts,
        vec![format!("order-refund-{paid}")]
    );
    let mail = ctx.mailer.as_ref().unwrap().deliveries();
    assert_eq!(mail.count, 3);
    assert_eq!(
        mail.messages
            .iter()
            .filter(|message| message.contains("was refunded in full"))
            .count(),
        1
    );
}

#[tokio::test]
#[serial]
async fn refund_confirmation_follows_the_first_refund_pass_after_a_start() {
    let stripe = StripeFake::start(Duration::from_millis(300)).await;
    let provider = stripe.state.clone();
    request::<App, _, _>(|_, ctx| async move {
        let runtime = ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
        let registry = runtime.registry();
        let scope = registry.resolve("localhost").await.unwrap();
        let (event, paid) = cancelled_without_wake(&scope, &registry, "start").await;

        runtime.start_tenant_jobs(&ctx).await.unwrap();
        let lag = confirmation_lag(&scope.db, &paid).await;
        println!("CG062 refund confirmation lag after the first pass={lag:?}, period 300 s");
        assert!(lag < Duration::from_secs(5));
        assert_one_refund_and_one_confirmation(&ctx, &scope, &provider, (&event, &paid)).await;
        drop(scope);
        runtime.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
#[serial]
async fn refund_confirmation_follows_a_periodic_refund_pass() {
    let stripe = StripeFake::start(Duration::from_millis(300)).await;
    let provider = stripe.state.clone();
    request::<App, _, _>(|_, ctx| async move {
        let runtime = ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
        let registry = runtime.registry();
        let scope = registry.resolve("localhost").await.unwrap();
        let period = Duration::from_secs(4);

        let (_stop, stopped) = tokio::sync::watch::channel(false);
        let jobs = spawn_tenant_jobs(&ctx, &registry, &RefundWake::default(), period, stopped);
        // The first passes end with no work. The refund is left to the pass one period later.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let (event, paid) = cancelled_without_wake(&scope, &registry, "period").await;
        let lag = confirmation_lag(&scope.db, &paid).await;
        println!("CG062 refund confirmation lag after a periodic pass={lag:?}, period {period:?}");
        assert!(lag < period / 2);
        assert_one_refund_and_one_confirmation(&ctx, &scope, &provider, (&event, &paid)).await;
        stop(jobs).await;
    })
    .await;
}

#[tokio::test]
#[serial]
async fn stalled_mail_for_one_tenant_does_not_delay_refunds_or_series_of_another() {
    request::<App, _, _>(|_, ctx| async move {
        let (registry, slow, other) = two_tenants(&ctx, "stall").await;
        let (mail_ctx, mail, mail_server) = smtp(&ctx).await;
        mail.stall.store(true, Ordering::SeqCst);

        let (slow_event, slow_kind) = event(&slow.db, "2027-06-15", now()).await;
        orders(&slow, &registry, (&slow_event, &slow_kind), &addresses("slow", 3)).await;
        let message = queue_message(&slow, &registry, &slow_event, "Arrival").await;
        let (other_event, other_kind) = event(&other.db, "2027-06-15", now()).await;
        orders(&other, &registry, (&other_event, &other_kind), &addresses("other", 2)).await;

        let wake = RefundWake::default();
        let (_stop, stopped) = tokio::sync::watch::channel(false);
        let jobs = spawn_tenant_jobs(&mail_ctx, &registry, &wake, Duration::from_millis(200), stopped);
        wait_for("the stalled buyer message", async || mail.received().len() == 1).await;
        let stalled = mail.received().remove(0);
        assert!(stalled.starts_with("slow"));

        // Work that becomes due only after the mail job is stuck.
        admin_orders::cancel_event(&other.db, &other_event, now()).await.unwrap();
        wake.wake();
        let january = chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap().and_hms_opt(12, 0, 0).unwrap().and_utc().timestamp();
        let (series_event, _) = event(&other.db, "2026-01-08", january).await;
        let series_id = series::start(&other.db, &series_event, Frequency::Weekly, None, &Ok(()), january)
            .await
            .unwrap();
        let series_dates = async || {
            column(&other.db, "SELECT date AS value FROM events WHERE series_id = ? ORDER BY series_date", &series_id).await
        };
        let started_with = series_dates().await;
        assert_eq!(started_with.len(), 8);
        assert_eq!(started_with[7], "2026-02-26");

        wait_for("the other tenant's refunds", async || {
            order_states(&other.db, &other_event).await.iter().all(|state| state.starts_with("Refunded/Completed/"))
        })
        .await;
        wait_for("the Series top-up", async || series_dates().await.len() == 16).await;
        assert_eq!(ticket_states(&other.db, &other_event).await, vec!["Void"; 2]);
        assert!(series_dates().await[8].as_str() > "2026-10-01");
        // The slow tenant's turn still waits for its one row in flight. Nothing was skipped.
        assert_eq!(recipient_states(&slow.db, &message).await, vec!["Pending", "Pending", "Sending"]);
        assert_eq!(mail.accepted(), Vec::<String>::new());

        // A stop keeps every claim: the uncertain send is not marked failed or sent again.
        stop(jobs).await;
        assert_eq!(recipient_states(&slow.db, &message).await, vec!["Pending", "Pending", "Sending"]);
        buyer_messages::recover(&slow.db, now() + 301).await.unwrap();
        assert_eq!(recipient_states(&slow.db, &message).await, vec!["Pending", "Pending", "Uncertain"]);
        let cancel_notices = column(&other.db, "SELECT cancel_notice_state AS value FROM orders WHERE event_id = ? ORDER BY created_at, id", &other_event).await;
        assert_eq!(cancel_notices, vec!["Sending", "Pending"]);

        // After a restart the durable states decide what is due.
        mail.stall.store(false, Ordering::SeqCst);
        let before = mail.received().len();
        buyer_messages::run(&mail_ctx, &registry).await.unwrap();
        run_refund_work(&mail_ctx, &registry).await.unwrap();
        assert_eq!(recipient_states(&slow.db, &message).await, vec!["Accepted", "Accepted", "Uncertain"]);
        let mut resumed = mail.received().split_off(before);
        resumed.sort();
        let mut expected = addresses("slow", 3);
        expected.retain(|address| *address != stalled);
        expected.insert(0, "other1@example.com".into());
        assert_eq!(resumed, expected);
        mail_server.abort();
    })
    .await;
}

#[tokio::test]
#[serial]
async fn tenant_turns_are_bounded_and_leave_durable_work_for_the_next_pass() {
    request::<App, _, _>(|_, ctx| async move {
        let (registry, first, second) = two_tenants(&ctx, "turns").await;
        let (mail_ctx, mail, mail_server) = smtp(&ctx).await;
        let (first_event, first_kind) = event(&first.db, "2027-06-15", now()).await;
        orders(
            &first,
            &registry,
            (&first_event, &first_kind),
            &addresses("first", 5),
        )
        .await;
        let (second_event, second_kind) = event(&second.db, "2027-06-15", now()).await;
        orders(
            &second,
            &registry,
            (&second_event, &second_kind),
            &addresses("second", 2),
        )
        .await;
        let tenant = |sent: Vec<String>| {
            sent.iter()
                .map(|email| {
                    email
                        .split(|c: char| c.is_ascii_digit())
                        .next()
                        .unwrap()
                        .to_owned()
                })
                .collect::<Vec<_>>()
        };

        // Row budget: two rows a turn, in rounds, until the pass has no work left.
        queue_message(&first, &registry, &first_event, "Rows").await;
        queue_message(&second, &registry, &second_event, "Rows").await;
        let rows = Budget {
            turn_rows: 2,
            turn_time: Duration::from_secs(30),
            pass_rows: 200,
        };
        buyer_messages::run_bounded(&mail_ctx, &registry, rows)
            .await
            .unwrap();
        assert_eq!(
            tenant(mail.accepted()),
            vec!["first", "first", "second", "second", "first", "first", "first"]
        );

        // Time budget: a turn stops before the next row once its time is spent, and the pass
        // stops at its row limit. The rest stays Pending.
        mail.accepted.lock().unwrap().clear();
        mail.delay_ms.store(300, Ordering::SeqCst);
        let slow_message = queue_message(&first, &registry, &first_event, "Time").await;
        let quick_message = queue_message(&second, &registry, &second_event, "Time").await;
        let time = Budget {
            turn_rows: 100,
            turn_time: Duration::from_millis(450),
            pass_rows: 3,
        };
        let started = Instant::now();
        buyer_messages::run_bounded(&mail_ctx, &registry, time)
            .await
            .unwrap();
        let elapsed = started.elapsed();
        let sent = tenant(mail.accepted());
        println!("CG062 time-bounded pass elapsed={elapsed:?} order={sent:?}");
        assert_eq!(sent.iter().filter(|name| *name == "first").count(), 3);
        assert_eq!(sent.iter().filter(|name| *name == "second").count(), 2);
        assert!(
            sent.iter().position(|name| name == "second").unwrap() <= 2,
            "the second tenant is served before the first tenant's third row"
        );
        assert_eq!(
            recipient_states(&first.db, &slow_message).await,
            vec!["Accepted", "Accepted", "Accepted", "Pending", "Pending"]
        );
        assert_eq!(
            recipient_states(&second.db, &quick_message).await,
            vec!["Accepted", "Accepted"]
        );

        // The next pass continues from the durable rows and sends nothing twice.
        mail.delay_ms.store(0, Ordering::SeqCst);
        buyer_messages::run(&mail_ctx, &registry).await.unwrap();
        assert_eq!(
            recipient_states(&first.db, &slow_message).await,
            vec!["Accepted"; 5]
        );
        let mut all = mail.accepted();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 7);
        assert_eq!(mail.accepted().len(), 7);
        mail_server.abort();
    })
    .await;
}

#[tokio::test]
#[serial]
async fn failing_refund_is_attempted_once_a_pass_and_does_not_block_later_orders() {
    let stripe = StripeFake::start(Duration::ZERO).await;
    let provider = stripe.state.clone();
    request::<App, _, _>(|_, ctx| async move {
        let (registry, tenant, _) = two_tenants(&ctx, "retry").await;
        let cipher = registry.cipher(&tenant.organizer).unwrap();
        settings::set(&tenant.db, &cipher, "stripe_secret_key", "sk_test_cg062_synthetic")
            .await
            .unwrap();
        let (event, kind) = event(&tenant.db, "2027-06-15", now()).await;
        let paid = orders(&tenant, &registry, (&event, &kind), &addresses("retry", 4)).await;
        make_paid(&tenant.db, &paid, "retry").await;
        // The oldest Order cannot be refunded: it has no payment to refund.
        tenant.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "UPDATE orders SET stripe_payment_intent_id = NULL WHERE id = ?", [paid[0].clone().into()])).await.unwrap();
        admin_orders::cancel_event(&tenant.db, &event, now()).await.unwrap();

        let one_row_turns = Budget { turn_rows: 1, turn_time: Duration::from_secs(30), pass_rows: usize::MAX };
        run_refunds(&ctx, &registry, one_row_turns).await.unwrap();
        let refunds = async || column(&tenant.db, "SELECT refund_work_state AS value FROM orders WHERE event_id = ? ORDER BY created_at, id", &event).await;
        assert_eq!(refunds().await, vec!["Failed", "Completed", "Completed", "Completed"]);
        assert_eq!(provider.lock().unwrap().posts.len(), 3);
        run_refunds(&ctx, &registry, one_row_turns).await.unwrap();
        assert_eq!(refunds().await, vec!["Failed", "Completed", "Completed", "Completed"]);
        assert_eq!(provider.lock().unwrap().posts.len(), 3);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn duplicate_workers_refund_and_mail_each_row_once() {
    let stripe = StripeFake::start(Duration::from_millis(100)).await;
    let provider = stripe.state.clone();
    request::<App, _, _>(|_, ctx| async move {
        let (registry, tenant, _) = two_tenants(&ctx, "workers").await;
        let (mail_ctx, mail, mail_server) = smtp(&ctx).await;
        mail.delay_ms.store(100, Ordering::SeqCst);
        let cipher = registry.cipher(&tenant.organizer).unwrap();
        settings::set(
            &tenant.db,
            &cipher,
            "stripe_secret_key",
            "sk_test_cg062_synthetic",
        )
        .await
        .unwrap();
        let (kept_event, kept_kind) = event(&tenant.db, "2027-06-15", now()).await;
        orders(
            &tenant,
            &registry,
            (&kept_event, &kept_kind),
            &addresses("message", 3),
        )
        .await;
        let message = queue_message(&tenant, &registry, &kept_event, "Arrival").await;
        let (event, kind) = event(&tenant.db, "2027-06-16", now()).await;
        let paid = orders(&tenant, &registry, (&event, &kind), &addresses("refund", 3)).await;
        make_paid(&tenant.db, &paid, "workers").await;
        admin_orders::cancel_event(&tenant.db, &event, now())
            .await
            .unwrap();

        let small = Budget {
            turn_rows: 1,
            turn_time: Duration::from_secs(30),
            pass_rows: 200,
        };
        let (a, b, c, d) = tokio::join!(
            run_refund_work(&mail_ctx, &registry),
            run_refund_work(&mail_ctx, &registry),
            buyer_messages::run_bounded(&mail_ctx, &registry, small),
            buyer_messages::run_bounded(&mail_ctx, &registry, small),
        );
        for result in [a, b, c, d] {
            result.unwrap();
        }
        // A worker that found a refund still claimed by the other leaves its notice queued.
        run_refund_work(&mail_ctx, &registry).await.unwrap();

        assert_eq!(
            order_states(&tenant.db, &event).await,
            vec!["Refunded/Completed/Completed/Completed"; 3]
        );
        assert_eq!(ticket_states(&tenant.db, &event).await, vec!["Void"; 3]);
        assert_eq!(
            events::get(&tenant.db, &event)
                .await
                .unwrap()
                .unwrap()
                .status,
            "Cancelled"
        );
        let mut keys = provider.lock().unwrap().posts.clone();
        keys.sort();
        let mut expected: Vec<_> = paid.iter().map(|id| format!("order-refund-{id}")).collect();
        expected.sort();
        assert_eq!(keys, expected);
        assert_eq!(
            recipient_states(&tenant.db, &message).await,
            vec!["Accepted"; 3]
        );
        let mut received = mail.received();
        received.sort();
        assert_eq!(
            received,
            vec![
                "message0@example.com",
                "message1@example.com",
                "message2@example.com",
                "refund0@example.com",
                "refund0@example.com",
                "refund1@example.com",
                "refund1@example.com",
                "refund2@example.com",
                "refund2@example.com",
            ]
        );
        mail_server.abort();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[serial]
async fn refund_completion_between_read_and_claim_does_not_reclaim_completed_work() {
    completed_refund_race("SELECT refund_work_state, cancel_notice_state, refund_notice_state")
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[serial]
async fn completed_work_after_stale_status_read_remains_an_idempotent_create_request() {
    completed_refund_race("SELECT status, stripe_payment_intent_id, total_cents, event_id").await;
}

async fn completed_refund_race(statement: &'static str) {
    let stripe = StripeFake::start(Duration::ZERO).await;
    let provider = stripe.state.clone();
    request::<App, _, _>(|_, ctx| async move {
        let (registry, tenant, _) = two_tenants(&ctx, "completion-race").await;
        let cipher = registry.cipher(&tenant.organizer).unwrap();
        settings::set(&tenant.db, &cipher, "stripe_secret_key", "sk_test_cg062_synthetic")
            .await
            .unwrap();
        let (event, kind) = event(&tenant.db, "2027-06-15", now()).await;
        let paid = orders(&tenant, &registry, (&event, &kind), &addresses("race", 1)).await;
        make_paid(&tenant.db, &paid, "completion-race").await;
        admin_orders::cancel_event(&tenant.db, &event, now()).await.unwrap();
        let client = settings::stripe_client(&tenant.db, &cipher).await.unwrap();
        let mut duplicate_db: DatabaseConnection = (*tenant.db).clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let entered_tx = Mutex::new(Some(entered_tx));
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        duplicate_db.set_metric_callback(move |info| {
            if info.statement.sql.starts_with(statement) {
                if let Some(entered) = entered_tx.lock().unwrap().take() {
                    entered.send(()).unwrap();
                    release_rx.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
                }
            }
        });
        let duplicate_id = paid[0].clone();
        let duplicate_client = client.clone();
        let runtime = tokio::runtime::Handle::current();
        let duplicate = tokio::task::spawn_blocking(move || {
            runtime.block_on(async move {
                admin_orders::refund(&duplicate_db, &duplicate_id, Some(&duplicate_client), now())
                    .await
            })
        });
        tokio::time::timeout(Duration::from_secs(5), entered_rx).await.unwrap().unwrap();
        admin_orders::refund(&tenant.db, &paid[0], Some(&client), now()).await.unwrap();
        let completed = admin_orders::work_progress(&tenant.db, &paid[0]).await.unwrap();
        assert_eq!(completed.refund, "Completed");
        assert!(!completed.refund_claimed);
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), duplicate).await.unwrap().unwrap().unwrap();
        let row = tenant.db.query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT refund_claim_token, refund_claim_until, refund_work_state, refund_notice_state FROM orders WHERE id = ?",
            [paid[0].clone().into()],
        )).await.unwrap().unwrap();
        assert_eq!(row.try_get::<Option<String>>("", "refund_claim_token").unwrap(), None);
        assert_eq!(row.try_get::<Option<i64>>("", "refund_claim_until").unwrap(), None);
        assert_eq!(row.try_get::<String>("", "refund_work_state").unwrap(), "Completed");
        assert_eq!(row.try_get::<String>("", "refund_notice_state").unwrap(), "Pending");
        assert_eq!(provider.lock().unwrap().reads, 1);
        assert_eq!(provider.lock().unwrap().posts, [format!("order-refund-{}", paid[0])]);
        assert!(admin_orders::claim_notice(&tenant.db, &paid[0], false).await.unwrap());
        assert!(!admin_orders::claim_notice(&tenant.db, &paid[0], false).await.unwrap());
    }).await;
}
