use crabgate::{
    app::App,
    initializers::organizers::OrganizerRuntime,
    members::{self, Role},
};
use loco_rs::{
    app::{AppContext, Hooks},
    boot::StartMode,
    config::WorkerMode,
    environment::Environment,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Default)]
struct Capture {
    refused: AtomicBool,
    reject_after_data: AtomicBool,
    hold: AtomicBool,
    entered: tokio::sync::Notify,
    acceptance: tokio::sync::Notify,
    release: tokio::sync::Notify,
    accepted: Mutex<Vec<(String, String)>>,
}
struct Fixture {
    ctx: AppContext,
    router: axum::Router,
    capture: Arc<Capture>,
    smtp: tokio::task::JoinHandle<()>,
    previous_dir: Option<std::ffi::OsString>,
    previous_edition: Option<std::ffi::OsString>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.smtp.abort();
        for (key, previous) in [
            ("CRABGATE_ORGANIZER_DATABASE_DIR", &self.previous_dir),
            ("CRABGATE_EDITION", &self.previous_edition),
        ] {
            if let Some(value) = previous {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}
async fn fixture() -> Fixture {
    let root = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir);
    let storage = tempfile::Builder::new()
        .prefix("cg056-")
        .tempdir_in(root)
        .unwrap()
        .keep();
    println!("CG-056 synthetic evidence: {}", storage.display());
    let previous_dir = std::env::var_os("CRABGATE_ORGANIZER_DATABASE_DIR");
    let previous_edition = std::env::var_os("CRABGATE_EDITION");
    std::env::set_var(
        "CRABGATE_ORGANIZER_DATABASE_DIR",
        storage.join("organizers"),
    );
    std::env::set_var("CRABGATE_EDITION", "hosted");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let capture = Arc::new(Capture::default());
    let state = capture.clone();
    let smtp = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                connection = listener.accept() => {
                    let (stream, _) = connection.unwrap();
                    let state = state.clone();
                    connections.spawn(async move {
                        let (read, mut write) = stream.into_split();
                        if state.refused.load(Ordering::SeqCst) {
                            let _ = write.write_all(b"421 synthetic refusal\r\n").await;
                            return;
                        }
                        let mut reader = BufReader::new(read);
                        if write.write_all(b"220 loopback SMTP\r\n").await.is_err() { return; }
                        let mut data = false;
                        let mut recipient = String::new();
                        let mut body = String::new();
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).await.unwrap() == 0 { break; }
                            if state.refused.load(Ordering::SeqCst) { let _ = write.write_all(b"421 synthetic refusal\r\n").await; return; }
                            let response = if data {
                                if line != ".\r\n" { body.push_str(&line); continue; }
                                if state.hold.swap(false, Ordering::SeqCst) {
                                    state.entered.notify_one();
                                    state.release.notified().await;
                                }
                                if state.reject_after_data.load(Ordering::SeqCst) {
                                    let _ = write.write_all(b"421 synthetic DATA refusal\r\n").await;
                                    return;
                                }
                                state.accepted.lock().unwrap().push((recipient.clone(), std::mem::take(&mut body)));
                                state.acceptance.notify_one();
                                data = false;
                                b"250 accepted\r\n".as_slice()
                            } else if line.starts_with("RCPT TO:") {
                                recipient = line.trim().trim_start_matches("RCPT TO:").trim_matches(['<', '>']).to_owned();
                                b"250 OK\r\n".as_slice()
                            } else if line.starts_with("DATA") {
                                data = true;
                                b"354 continue\r\n".as_slice()
                            } else if line.starts_with("QUIT") {
                                let _ = write.write_all(b"221 bye\r\n").await; break;
                            } else { b"250 OK\r\n".as_slice() };
                            if write.write_all(response).await.is_err() { break; }
                        }
                    });
                }
                _ = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    });
    let mut config = App::load_config(&Environment::Test).await.unwrap();
    config.database.uri = format!("sqlite://{}?mode=rwc", storage.join("control.db").display());
    config.database.max_connections = 5;
    config.workers.mode = WorkerMode::BackgroundAsync;
    let mailer = config.mailer.as_mut().unwrap();
    mailer.stub = false;
    let transport = mailer.smtp.as_mut().unwrap();
    transport.host = "127.0.0.1".into();
    transport.port = port;
    transport.secure = false;
    transport.auth = None;
    let boot = App::boot(StartMode::ServerOnly, &Environment::Test, config)
        .await
        .unwrap();
    Fixture {
        router: boot.router.unwrap(),
        ctx: boot.app_context,
        capture,
        smtp,
        previous_dir,
        previous_edition,
    }
}
fn registry(ctx: &AppContext) -> Arc<crabgate::organizers::OrganizerRegistry> {
    ctx.shared_store
        .get::<Arc<OrganizerRuntime>>()
        .unwrap()
        .registry()
}
struct Http {
    client: reqwest::Client,
    origin: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Http {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Http {
    async fn new(f: &Fixture) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let router = f.router.clone();
        Self {
            origin,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap(),
            task: tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            }),
        }
    }
    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.origin))
            .header("host", "arts.crabgate.com")
            .header("origin", "https://arts.crabgate.com")
    }
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .get(format!("{}{path}", self.origin))
            .header("host", "arts.crabgate.com")
    }
}
fn token(f: &Fixture, index: usize) -> String {
    let body = f.capture.accepted.lock().unwrap()[index]
        .1
        .replace("=\r\n", "");
    body.split_once("/office/magic-link/")
        .unwrap()
        .1
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}
async fn count(db: &sea_orm::DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite, sql))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "n")
        .unwrap()
}
async fn setup_member(f: &Fixture) -> crabgate::organizers::Organizer {
    let o = registry(&f.ctx).create("Arts", "arts").await.unwrap();
    members::add_member(&f.ctx.db, &o.id, "admin@example.test", Role::Admin)
        .await
        .unwrap();
    o
}
#[tokio::test]
#[serial_test::serial]
async fn member_failure_and_normal_retry_preserve_auth_and_rates() {
    let f = fixture().await;
    let o = setup_member(&f).await;
    registry(&f.ctx).create("Other", "other").await.unwrap();
    let http = Http::new(&f).await;
    assert_eq!(
        http.post("/office/sign-in")
            .form(&[("email", "admin@example.test")])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    f.capture.acceptance.notified().await;
    let claim = http
        .get(&format!("/office/magic-link/{}", token(&f, 0)))
        .send()
        .await
        .unwrap();
    assert_eq!(claim.status(), 303);
    assert_eq!(claim.headers()["location"], "/office/events");
    let cookie = claim.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert_eq!(
        http.get("/office/events")
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let other = http
        .client
        .get(format!("{}/office/events", http.origin))
        .header("host", "other.crabgate.com")
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 303);
    assert_eq!(other.headers()["location"], "/office/sign-in");
    f.capture.refused.store(true, Ordering::SeqCst);
    let invite = http
        .post("/office/door/members")
        .header("cookie", &cookie)
        .form(&[("email", "staff@example.test"), ("role", "Door Staff")])
        .send()
        .await
        .unwrap();
    let status = invite.status().as_u16();
    let body = invite.text().await.unwrap();
    println!("refused invitation: HTTP {status}; {body}");
    assert_eq!(status, 503);
    assert!(body.contains("Member and role saved"));
    assert!(body.contains("failed or is uncertain"));
    assert!(body.contains("send the invite again"));
    assert!(members::list_members(&f.ctx.db, &o.id)
        .await
        .unwrap()
        .iter()
        .any(|m| m.email == "staff@example.test" && m.role == Role::DoorStaff));
    let sign = http
        .post("/office/sign-in")
        .form(&[("email", "staff@example.test")])
        .send()
        .await
        .unwrap();
    assert_eq!(sign.status(), 503);
    let body = sign.text().await.unwrap();
    assert!(body.contains("failed or is uncertain"));
    assert!(!body.contains("sent=1"));
    assert_eq!(
        count(
            &f.ctx.db,
            "SELECT COUNT(*) AS n FROM magic_links WHERE used_at IS NULL"
        )
        .await,
        2
    );
    assert_eq!(f.capture.accepted.lock().unwrap().len(), 1);
    let unknown = http
        .post("/office/sign-in")
        .form(&[("email", "unknown@example.test")])
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 303);
    assert_eq!(unknown.headers()["location"], "/office/sign-in?sent=1");
    assert_eq!(
        http.post("/office/door/members")
            .header("cookie", &cookie)
            .form(&[("email", "admin@example.test"), ("role", "Door Staff")])
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    f.capture.refused.store(false, Ordering::SeqCst);
    assert_eq!(
        http.post("/office/door/members")
            .header("cookie", &cookie)
            .form(&[("email", "staff@example.test"), ("role", "Door Staff")])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    f.capture.acceptance.notified().await;
    let staff = http
        .get(&format!("/office/magic-link/{}", token(&f, 1)))
        .send()
        .await
        .unwrap();
    assert_eq!(staff.status(), 303);
    assert_eq!(staff.headers()["location"], "/door");
    let staff_cookie = staff.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert_eq!(
        http.post("/office/door/members")
            .header("cookie", &staff_cookie)
            .form(&[("email", "bad@example.test"), ("role", "Admin")])
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let limited = http
        .post("/office/door/members")
        .header("cookie", &cookie)
        .form(&[("email", "staff@example.test"), ("role", "Door Staff")])
        .send()
        .await
        .unwrap();
    assert_eq!(limited.status(), 429);
    assert_eq!(limited.text().await.unwrap(),"Member saved, but the invitation email was not sent. Too many tries. Please wait a little and send the invite again.");
    assert_eq!(
        http.post("/office/sign-in")
            .form(&[("email", "staff@example.test")])
            .send()
            .await
            .unwrap()
            .status(),
        429
    );
    assert_eq!(
        http.post("/office/sign-in")
            .form(&[("email", "admin@example.test")])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    f.capture.acceptance.notified().await;
    let retry = http
        .get(&format!("/office/magic-link/{}", token(&f, 2)))
        .send()
        .await
        .unwrap();
    assert_eq!(retry.status(), 303);
    assert_eq!(retry.headers()["location"], "/office/events");
    assert_eq!(f.capture.accepted.lock().unwrap().len(), 3);
    registry(&f.ctx).shutdown().await.unwrap();
}
#[tokio::test]
#[serial_test::serial]
async fn member_timeout_after_data_reports_uncertainty() {
    let f = fixture().await;
    setup_member(&f).await;
    let http = Http::new(&f).await;
    f.capture.hold.store(true, Ordering::SeqCst);
    let start = std::time::Instant::now();
    let response = http
        .post("/office/sign-in")
        .form(&[("email", "admin@example.test")])
        .send()
        .await
        .unwrap();
    let elapsed = start.elapsed();
    assert_eq!(response.status(), 503);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("failed or is uncertain"));
    assert!(
        elapsed >= std::time::Duration::from_secs(9)
            && elapsed < std::time::Duration::from_secs(13),
        "{elapsed:?}"
    );
    println!("DATA acceptance withheld: {elapsed:?}; HTTP 503 uncertainty; token retained");
    assert_eq!(
        count(
            &f.ctx.db,
            "SELECT COUNT(*) AS n FROM magic_links WHERE used_at IS NULL"
        )
        .await,
        1
    );
    f.capture.release.notify_one();
    f.capture.acceptance.notified().await;
    assert_eq!(f.capture.accepted.lock().unwrap().len(), 1);
    let claim = http
        .get(&format!("/office/magic-link/{}", token(&f, 0)))
        .send()
        .await
        .unwrap();
    assert_eq!(claim.status(), 303);
    let cookie = claim.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    f.capture.hold.store(true, Ordering::SeqCst);
    let start = std::time::Instant::now();
    let invite = http
        .post("/office/door/members")
        .header("cookie", &cookie)
        .form(&[("email", "timeout@example.test"), ("role", "Door Staff")])
        .send()
        .await
        .unwrap();
    assert_eq!(invite.status(), 503);
    let body = invite.text().await.unwrap();
    assert!(body.contains("Member and role saved"));
    assert!(body.contains("failed or is uncertain"));
    assert!(start.elapsed() < std::time::Duration::from_secs(13));
    assert_eq!(
        count(
            &f.ctx.db,
            "SELECT COUNT(*) AS n FROM magic_links WHERE used_at IS NULL"
        )
        .await,
        1
    );
    f.capture.release.notify_one();
    f.capture.acceptance.notified().await;
    let claim = http
        .get(&format!("/office/magic-link/{}", token(&f, 1)))
        .send()
        .await
        .unwrap();
    assert_eq!(claim.status(), 303);
    assert_eq!(claim.headers()["location"], "/door");
    println!("invitation DATA timeout: 503 saved-role uncertainty; subsequently accepted token still grants Door Staff only");
    registry(&f.ctx).shutdown().await.unwrap();
}
async fn late_payment_flow(receipt_failure: bool) {
    use crabgate::{
        events::{self, EventInput, TicketTypeInput},
        inventory, orders, settings,
    };
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let f = fixture().await;
    let o = setup_member(&f).await;
    let registry = registry(&f.ctx);
    let db = registry.resolve("arts.crabgate.com").await.unwrap().db;
    let cipher = registry.cipher(&o).unwrap();
    settings::set(&db, &cipher, "stripe_secret_key", "sk_test_synthetic")
        .await
        .unwrap();
    settings::set(&db, &cipher, "webhook_secret", "whsec_cg056")
        .await
        .unwrap();
    settings::set(&db, &cipher, "reply_to", "support@example.test")
        .await
        .unwrap();
    let refunds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = refunds.clone();
    let fake = axum::Router::new().route(
        "/v1/refunds",
        axum::routing::post(move |headers: axum::http::HeaderMap| {
            let calls = calls.clone();
            async move {
                assert_eq!(headers["idempotency-key"], "late-cs_cg056");
                calls.fetch_add(1, Ordering::SeqCst);
                axum::Json(serde_json::json!({"id":"re_cg056","status":"succeeded"}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prior = std::env::var_os("STRIPE_API_BASE");
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let provider = tokio::spawn(async move {
        axum::serve(listener, fake).await.unwrap();
    });
    let event = events::create(
        &db,
        &EventInput {
            title: "Late payment".into(),
            date: (chrono::Utc::now() + chrono::Duration::days(60))
                .format("%Y-%m-%d")
                .to_string(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Hall".into(),
            address: "Street".into(),
            description: String::new(),
            event_capacity: Some(1),
            max_per_order: 1,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let kind = events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "Standard".into(),
            price: "10".into(),
            capacity: 1,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(&db, &event.id, orders::now())
        .await
        .unwrap();
    let selection = [inventory::Selection {
        ticket_type_id: kind.id,
        quantity: 1,
    }];
    let hold = inventory::reserve(&db, &event.id, &selection, orders::now())
        .await
        .unwrap();
    orders::save_details(
        &db,
        &cipher,
        &orders::hold_details(&db, &hold.id).await.unwrap(),
        "buyer@example.test",
        &["Guest".into()],
        false,
        orders::now(),
    )
    .await
    .unwrap();
    orders::attach_session(&db, &hold.id, "cs_cg056", orders::now())
        .await
        .unwrap();
    orders::expire_hold(&db, &hold.id).await.unwrap();
    inventory::reserve(&db, &event.id, &selection, orders::now())
        .await
        .unwrap();
    let body=serde_json::json!({"type":"checkout.session.completed","data":{"object":{"id":"cs_cg056","payment_status":"paid","amount_total":1000,"currency":"eur","payment_intent":"pi_cg056","metadata":{"hold_id":hold.id}}}}).to_string();
    let now = orders::now();
    let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec_cg056").unwrap();
    mac.update(format!("{now}.{body}").as_bytes());
    let signature = format!("t={now},v1={}", hex::encode(mac.finalize().into_bytes()));
    let http = Http::new(&f).await;
    let webhook = || {
        http.post("/webhooks/stripe")
            .header("stripe-signature", &signature)
            .header("content-type", "application/json")
            .body(body.clone())
    };
    if receipt_failure {
        db.execute_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "CREATE TRIGGER cg056_refuse_receipt BEFORE UPDATE OF refund_email_sent_at ON holds BEGIN SELECT RAISE(ABORT, 'synthetic receipt refusal'); END")).await.unwrap();
    }
    f.capture.refused.store(!receipt_failure, Ordering::SeqCst);
    let first = webhook().send().await.unwrap();
    if receipt_failure {
        f.capture.acceptance.notified().await;
    }
    f.capture.refused.store(true, Ordering::SeqCst);
    let status = first.status().as_u16();
    println!(
        "refused late-payment mail: HTTP {status}; receipt {}",
        orders::hold_refund_email_sent(&db, &hold.id).await.unwrap()
    );
    assert_eq!(status, 500);
    assert!(!orders::hold_refund_email_sent(&db, &hold.id).await.unwrap());
    assert_eq!(
        orders::hold_details(&db, &hold.id).await.unwrap().status,
        "Refunded"
    );
    assert_eq!(refunds.load(Ordering::SeqCst), 1);
    assert_eq!(webhook().send().await.unwrap().status(), 500);
    assert_eq!(refunds.load(Ordering::SeqCst), 1);
    if receipt_failure {
        db.execute_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "DROP TRIGGER cg056_refuse_receipt",
        ))
        .await
        .unwrap();
    }
    f.capture.refused.store(false, Ordering::SeqCst);
    assert_eq!(webhook().send().await.unwrap().status(), 200);
    f.capture.acceptance.notified().await;
    assert!(orders::hold_refund_email_sent(&db, &hold.id).await.unwrap());
    assert_eq!(webhook().send().await.unwrap().status(), 200);
    {
        let accepted = f.capture.accepted.lock().unwrap();
        assert_eq!(accepted.len(), if receipt_failure { 2 } else { 1 });
        assert_eq!(accepted[0].0, "buyer@example.test");
        let message = accepted[0].1.replace("=\r\n", "");
        assert!(
            message.contains("Reply-To: support@example.test"),
            "{message}"
        );
        assert!(message
            .contains("No tickets were issued for this payment, so it was refunded in full."));
        assert!(message.contains("Stripe will return it to your original payment method."));
        assert!(message.contains("Arts"));
    }
    assert_eq!(refunds.load(Ordering::SeqCst), 1);
    assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM orders").await, 0);
    assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM tickets").await, 0);
    println!("signed replay recovery: one refund; {} accepted emails; receipt marked; zero Orders/Tickets; receipt_failure={receipt_failure}", if receipt_failure { 2 } else { 1 });
    provider.abort();
    let _ = provider.await;
    if let Some(prior) = prior {
        std::env::set_var("STRIPE_API_BASE", prior);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }
    drop(db);
    registry.shutdown().await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn signed_late_payment_retries_mail_without_refunding_twice() {
    late_payment_flow(false).await;
}
#[tokio::test]
#[serial_test::serial]
async fn signed_late_payment_receipt_failure_remains_retryable_with_duplicate_uncertainty() {
    late_payment_flow(true).await;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeliveryScenario {
    Accepted,
    Refused,
    Expired,
    Cancelled,
    Replaced,
    TimedOut,
}

async fn overlap_flow(charge_callback: bool, scenario: DeliveryScenario) {
    use crabgate::{
        events::{self, EventInput, TicketTypeInput},
        inventory, orders, settings,
    };
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let f = fixture().await;
    let o = setup_member(&f).await;
    let registry = registry(&f.ctx);
    let db = registry.resolve("arts.crabgate.com").await.unwrap().db;
    let cipher = registry.cipher(&o).unwrap();
    settings::set(&db, &cipher, "stripe_secret_key", "sk_test_synthetic")
        .await
        .unwrap();
    settings::set(&db, &cipher, "webhook_secret", "whsec_cg056")
        .await
        .unwrap();
    settings::set(&db, &cipher, "reply_to", "support@example.test")
        .await
        .unwrap();
    let refunds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = refunds.clone();
    let fake = axum::Router::new().route(
        "/v1/refunds",
        axum::routing::post(move |headers: axum::http::HeaderMap| {
            let calls = calls.clone();
            async move {
                assert_eq!(headers["idempotency-key"], "late-cs_cg056");
                calls.fetch_add(1, Ordering::SeqCst);
                axum::Json(serde_json::json!({"id":"re_cg056","status":"succeeded"}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prior = std::env::var_os("STRIPE_API_BASE");
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let provider = tokio::spawn(async move {
        axum::serve(listener, fake).await.unwrap();
    });
    let event = events::create(
        &db,
        &EventInput {
            title: "Late payment".into(),
            date: (chrono::Utc::now() + chrono::Duration::days(60))
                .format("%Y-%m-%d")
                .to_string(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Hall".into(),
            address: "Street".into(),
            description: String::new(),
            event_capacity: Some(1),
            max_per_order: 1,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let kind = events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "Standard".into(),
            price: "10".into(),
            capacity: 1,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(&db, &event.id, orders::now())
        .await
        .unwrap();
    let selection = [inventory::Selection {
        ticket_type_id: kind.id,
        quantity: 1,
    }];
    let hold = inventory::reserve(&db, &event.id, &selection, orders::now())
        .await
        .unwrap();
    orders::save_details(
        &db,
        &cipher,
        &orders::hold_details(&db, &hold.id).await.unwrap(),
        "buyer@example.test",
        &["Guest".into()],
        false,
        orders::now(),
    )
    .await
    .unwrap();
    orders::attach_session(&db, &hold.id, "cs_cg056", orders::now())
        .await
        .unwrap();
    orders::expire_hold(&db, &hold.id).await.unwrap();
    inventory::reserve(&db, &event.id, &selection, orders::now())
        .await
        .unwrap();
    let body=serde_json::json!({"type":"checkout.session.completed","data":{"object":{"id":"cs_cg056","payment_status":"paid","amount_total":1000,"currency":"eur","payment_intent":"pi_cg056","metadata":{"hold_id":hold.id}}}}).to_string();
    let now = orders::now();
    let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec_cg056").unwrap();
    mac.update(format!("{now}.{body}").as_bytes());
    let signature = format!("t={now},v1={}", hex::encode(mac.finalize().into_bytes()));
    let http = Http::new(&f).await;
    let webhook = || {
        http.post("/webhooks/stripe")
            .header("stripe-signature", &signature)
            .header("content-type", "application/json")
            .body(body.clone())
    };

    use tower::ServiceExt;
    f.capture.hold.store(true, Ordering::SeqCst);
    f.capture
        .reject_after_data
        .store(scenario == DeliveryScenario::Refused, Ordering::SeqCst);
    let started = std::time::Instant::now();
    let first_request = webhook();
    let router = f.router.clone();
    let cancelled_request = axum::http::Request::builder()
        .method("POST")
        .uri("/webhooks/stripe")
        .header("host", "arts.crabgate.com")
        .header("stripe-signature", &signature)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.clone()))
        .unwrap();
    let first = tokio::spawn(async move {
        if scenario == DeliveryScenario::Cancelled {
            router
                .oneshot(cancelled_request)
                .await
                .unwrap()
                .status()
                .as_u16()
        } else {
            first_request.send().await.unwrap().status().as_u16()
        }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        f.capture.entered.notified(),
    )
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!orders::hold_refund_email_sent(&db, &hold.id).await.unwrap());
    assert_eq!(refunds.load(Ordering::SeqCst), 1);
    assert_eq!(
        orders::hold_details(&db, &hold.id).await.unwrap().status,
        "Refunded"
    );
    let owner = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT refund_email_claim_token FROM holds WHERE id = ?",
            [hold.id.clone().into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "refund_email_claim_token")
        .unwrap();
    settings::set(&db, &cipher, "reply_to", "support@example.test")
        .await
        .unwrap();
    if scenario == DeliveryScenario::Cancelled {
        first.abort();
    }
    if scenario == DeliveryScenario::Expired || scenario == DeliveryScenario::Replaced {
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE holds SET refund_email_claim_until = unixepoch() - 1 WHERE id = ?",
            [hold.id.clone().into()],
        ))
        .await
        .unwrap();
    }
    let replacement = if scenario == DeliveryScenario::Replaced {
        match orders::claim_hold_refund_email(&db, &hold.id)
            .await
            .unwrap()
        {
            orders::HoldRefundEmailClaim::Owned(token) => Some(token),
            _ => panic!("expired claim must be recoverable"),
        }
    } else {
        None
    };
    let retry_body = if charge_callback {
        serde_json::json!({"type":"charge.refunded","data":{"object":{"id":"ch_cg056","payment_intent":"pi_cg056","refunded":true}}}).to_string()
    } else {
        body.clone()
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec_cg056").unwrap();
    mac.update(format!("{now}.{retry_body}").as_bytes());
    let retry_signature = format!("t={now},v1={}", hex::encode(mac.finalize().into_bytes()));
    let retry_webhook = || {
        http.post("/webhooks/stripe")
            .header("stripe-signature", &retry_signature)
            .header("content-type", "application/json")
            .body(retry_body.clone())
    };
    let second = retry_webhook().send().await.unwrap();
    assert_eq!(
        second.status(),
        if scenario == DeliveryScenario::Expired {
            200
        } else {
            500
        }
    );
    assert_eq!(
        f.capture.accepted.lock().unwrap().len(),
        if scenario == DeliveryScenario::Expired {
            1
        } else {
            0
        }
    );
    assert_eq!(
        orders::hold_refund_email_sent(&db, &hold.id).await.unwrap(),
        scenario == DeliveryScenario::Expired
    );
    if scenario == DeliveryScenario::TimedOut {
        assert_eq!(first.await.unwrap(), 500);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_secs(9)
                && elapsed < std::time::Duration::from_secs(13),
            "{elapsed:?}"
        );
        assert!(!orders::hold_refund_email_sent(&db, &hold.id).await.unwrap());
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) AS n FROM holds WHERE refund_email_claim_token IS NOT NULL"
            )
            .await,
            0
        );
        f.capture.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if f.capture.accepted.lock().unwrap().len() == 1 {
                    break;
                }
                f.capture.acceptance.notified().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(retry_webhook().send().await.unwrap().status(), 200);
    } else if scenario == DeliveryScenario::Cancelled {
        assert!(first.await.unwrap_err().is_cancelled());
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) AS n FROM holds WHERE refund_email_claim_token IS NOT NULL"
            )
            .await,
            1
        );
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE holds SET refund_email_claim_until = unixepoch() - 1 WHERE id = ?",
            [hold.id.clone().into()],
        ))
        .await
        .unwrap();
        assert_eq!(retry_webhook().send().await.unwrap().status(), 200);
        f.capture.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if f.capture.accepted.lock().unwrap().len() == 2 {
                    break;
                }
                f.capture.acceptance.notified().await;
            }
        })
        .await
        .unwrap();
    } else {
        f.capture.release.notify_one();
        assert_eq!(
            first.await.unwrap(),
            if scenario == DeliveryScenario::Accepted {
                200
            } else {
                500
            }
        );
        if let Some(replacement) = replacement {
            let current = db
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT refund_email_claim_token FROM holds WHERE id = ?",
                    [hold.id.clone().into()],
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get::<String>("", "refund_email_claim_token")
                .unwrap();
            assert_ne!(current, owner);
            assert_eq!(current, replacement);
            assert!(!orders::hold_refund_email_sent(&db, &hold.id).await.unwrap());
            assert_eq!(retry_webhook().send().await.unwrap().status(), 500);
            orders::release_hold_refund_email_claim(&db, &hold.id, &replacement)
                .await
                .unwrap();
        }
        if scenario == DeliveryScenario::Refused || scenario == DeliveryScenario::Replaced {
            assert!(!orders::hold_refund_email_sent(&db, &hold.id).await.unwrap());
            f.capture.reject_after_data.store(false, Ordering::SeqCst);
            assert_eq!(retry_webhook().send().await.unwrap().status(), 200);
        }
    }
    let accepted = f.capture.accepted.lock().unwrap().len();
    assert_eq!(
        accepted,
        if matches!(
            scenario,
            DeliveryScenario::Accepted | DeliveryScenario::Refused
        ) {
            1
        } else {
            2
        }
    );
    assert!(orders::hold_refund_email_sent(&db, &hold.id).await.unwrap());
    assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM holds WHERE refund_email_claim_token IS NOT NULL OR refund_email_claim_until IS NOT NULL").await, 0);
    assert_eq!(retry_webhook().send().await.unwrap().status(), 200);
    assert_eq!(f.capture.accepted.lock().unwrap().len(), accepted);
    assert_eq!(refunds.load(Ordering::SeqCst), 1);
    assert_eq!(
        count(&db, "SELECT COUNT(*) AS n FROM external_refunds").await,
        1
    );
    assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM orders").await, 0);
    assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM tickets").await, 0);
    println!("OVERLAP scenario={scenario:?} charge_callback={charge_callback} second_HTTP={} final_accepted={accepted} refund_calls=1 external_refunds=1 orders=0 tickets=0 elapsed={:?}", if scenario == DeliveryScenario::Expired {200} else {500}, started.elapsed());
    provider.abort();
    let _ = provider.await;
    if let Some(prior) = prior {
        std::env::set_var("STRIPE_API_BASE", prior);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }
    drop(db);
    registry.shutdown().await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn overlap_payment_replay() {
    overlap_flow(false, DeliveryScenario::Accepted).await;
}

#[tokio::test]
#[serial_test::serial]
async fn overlap_charge_refunded() {
    overlap_flow(true, DeliveryScenario::Accepted).await;
}

#[tokio::test]
#[serial_test::serial]
async fn overlap_refused_payment_remains_retryable() {
    overlap_flow(false, DeliveryScenario::Refused).await;
}

#[tokio::test]
#[serial_test::serial]
async fn overlap_refused_charge_remains_retryable() {
    overlap_flow(true, DeliveryScenario::Refused).await;
}

#[tokio::test]
#[serial_test::serial]
async fn expired_owner_cannot_store_receipt() {
    overlap_flow(false, DeliveryScenario::Expired).await;
}

#[tokio::test]
#[serial_test::serial]
async fn cancelled_owner_recovers_after_expiry() {
    overlap_flow(false, DeliveryScenario::Cancelled).await;
}

#[tokio::test]
#[serial_test::serial]
async fn stale_owner_cannot_release_replacement() {
    overlap_flow(false, DeliveryScenario::Replaced).await;
}

#[tokio::test]
#[serial_test::serial]
async fn late_payment_timeout_after_data() {
    overlap_flow(false, DeliveryScenario::TimedOut).await;
}
