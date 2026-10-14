use crabgate::{
    app::App,
    controllers::checkout::send_order_email,
    crypto::MasterKey,
    events::{self, EventInput, TicketTypeInput},
    links,
    members::{self, Role},
    orders,
    organizers::{Edition, Organizer, OrganizerRegistry},
};
use loco_rs::{app::AppContext, testing::prelude::*};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
enum SmtpBehavior {
    Accept,
    Refuse,
    StallGreeting,
    StallAcceptance,
    StallFirstGreeting,
}

struct CorrectionServer {
    ctx: AppContext,
    origin: String,
    client: reqwest::Client,
    behavior: Arc<Mutex<SmtpBehavior>>,
    received: Arc<AtomicUsize>,
    accepted: Arc<AtomicUsize>,
    messages: Arc<Mutex<Vec<String>>>,
    smtp: tokio::task::JoinHandle<()>,
    http: tokio::task::JoinHandle<()>,
}

impl Drop for CorrectionServer {
    fn drop(&mut self) {
        self.http.abort();
        self.smtp.abort();
    }
}

async fn correction_server(initial: SmtpBehavior) -> CorrectionServer {
    use loco_rs::{app::Hooks, boot::StartMode, config::WorkerMode, environment::Environment};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let behavior = Arc::new(Mutex::new(initial));
    let received = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::new(AtomicUsize::new(0));
    let messages = Arc::new(Mutex::new(Vec::new()));
    let captured = messages.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = behavior.clone();
    let data_count = received.clone();
    let accepted_count = accepted.clone();
    let smtp = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mode = {
                let mut state = state.lock().unwrap();
                let mode = *state;
                if matches!(mode, SmtpBehavior::StallFirstGreeting) {
                    *state = SmtpBehavior::Accept;
                }
                mode
            };
            let received = data_count.clone();
            let accepted = accepted_count.clone();
            let captured = captured.clone();
            connections.spawn(async move {
                if matches!(mode, SmtpBehavior::Refuse) {
                    socket
                        .write_all(b"421 synthetic refusal\r\n")
                        .await
                        .unwrap();
                    return;
                }
                if matches!(
                    mode,
                    SmtpBehavior::StallGreeting | SmtpBehavior::StallFirstGreeting
                ) {
                    println!("SMTP TCP connected; withholding greeting");
                    let mut byte = [0];
                    let _ = socket.read(&mut byte).await;
                    return;
                }
                let (read, mut write) = socket.into_split();
                let mut reader = BufReader::new(read);
                write.write_all(b"220 synthetic SMTP\r\n").await.unwrap();
                let mut data = false;
                let mut message = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap() == 0 {
                        return;
                    }
                    if data {
                        if line == ".\r\n" {
                            received.fetch_add(1, Ordering::SeqCst);
                            if matches!(mode, SmtpBehavior::StallAcceptance) {
                                println!(
                                    "SMTP received complete DATA; withholding acceptance response"
                                );
                                let _ = reader.read_line(&mut String::new()).await;
                                return;
                            }
                            captured.lock().unwrap().push(message.clone());
                            accepted.fetch_add(1, Ordering::SeqCst);
                            data = false;
                            write.write_all(b"250 accepted\r\n").await.unwrap();
                        } else {
                            message.push_str(&line);
                        }
                    } else if line.starts_with("DATA") {
                        data = true;
                        write.write_all(b"354 continue\r\n").await.unwrap();
                    } else if line.starts_with("QUIT") {
                        write.write_all(b"221 bye\r\n").await.unwrap();
                        return;
                    } else {
                        write.write_all(b"250 OK\r\n").await.unwrap();
                    }
                }
            });
            while connections.try_join_next().is_some() {}
        }
    });
    let mut config = App::load_config(&Environment::Test).await.unwrap();
    config.workers.mode = WorkerMode::BackgroundAsync;
    let mailer = config.mailer.as_mut().unwrap();
    mailer.stub = false;
    let smtp_config = mailer.smtp.as_mut().unwrap();
    smtp_config.host = "127.0.0.1".into();
    smtp_config.port = port;
    let boot = App::boot(StartMode::ServerOnly, &Environment::Test, config)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let http =
        tokio::spawn(async move { axum::serve(listener, boot.router.unwrap()).await.unwrap() });
    CorrectionServer {
        ctx: boot.app_context,
        origin,
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(13))
            .build()
            .unwrap(),
        behavior,
        received,
        accepted,
        messages,
        smtp,
        http,
    }
}

async fn correction_counts(f: &Fixture) -> (i64, i64) {
    let row = f.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT (SELECT COUNT(*) FROM orders) AS orders, (SELECT COUNT(*) FROM tickets) AS tickets".to_owned())).await.unwrap().unwrap();
    (
        row.try_get("", "orders").unwrap(),
        row.try_get("", "tickets").unwrap(),
    )
}

#[tokio::test]
#[serial]
async fn correction_stalled_public_mailer_has_whole_operation_deadline() {
    let server = correction_server(SmtpBehavior::StallGreeting).await;
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(12),
        crabgate::mailers::ticket::TicketMailer::send_confirmation(
            &server.ctx,
            "buyer@example.com",
            "Organizer",
            "ORDER",
            "https://example.com/orders/test",
            serde_json::json!([]),
            None,
        ),
    )
    .await;
    println!(
        "public TicketMailer elapsed={:?}; completed_before_watchdog={}",
        started.elapsed(),
        result.is_ok()
    );
    let error = result
        .expect("ticket mailer must complete before the 12s watchdog")
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("acceptance is uncertain after timeout"));
    assert!(started.elapsed() >= Duration::from_secs(10));
}

async fn correction_creation(mode: SmtpBehavior) {
    let server = correction_server(mode).await;
    let f = fixture(&server.ctx).await;
    let started = Instant::now();
    let created = server
        .client
        .post(format!("{}/office/orders/complimentary", server.origin))
        .header("cookie", &f.cookie)
        .form(&[
            ("ticket_type_id", f.kind.as_str()),
            ("email", "buyer@example.com"),
            ("name", "Guest"),
        ])
        .send()
        .await
        .unwrap();
    println!(
        "complimentary {mode:?}: status={}, location={:?}, counts={:?}, elapsed={:?}",
        created.status(),
        created.headers().get("location"),
        correction_counts(&f).await,
        started.elapsed()
    );
    assert_eq!(created.status(), 303);
    let detail = created.headers()["location"].to_str().unwrap().to_owned();
    assert!(detail.ends_with("?ticket_email=unconfirmed"));
    let route = detail.split('?').next().unwrap();
    let id = route.strip_prefix("/office/orders/").unwrap();
    assert_eq!(correction_counts(&f).await, (1, 1));
    assert!(!orders::email_sent(&f.db, id).await.unwrap());
    for _ in 0..2 {
        let page = server
            .client
            .get(format!("{}{detail}", server.origin))
            .header("cookie", &f.cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        let body = page.text().await.unwrap();
        assert!(body.contains("Tickets created, but mail acceptance could not be confirmed."));
        assert!(body.contains("Do not create another booking"));
        assert!(body.contains(&format!("action=\"{route}/resend\"")));
        assert!(!body.contains("action=\"/office/orders/complimentary\""));
        std::fs::write(super::evidence_path("cg006-correction-office.html"), body).unwrap();
    }
    assert_eq!(correction_counts(&f).await, (1, 1));
    *server.behavior.lock().unwrap() = SmtpBehavior::Refuse;
    let failed = server
        .client
        .post(format!("{}{route}/resend", server.origin))
        .header("cookie", &f.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), 502);
    assert!(failed
        .text()
        .await
        .unwrap()
        .contains("another attempt may deliver a duplicate"));
    assert_eq!(correction_counts(&f).await, (1, 1));
    let buyer = server
        .client
        .get(format!(
            "{}/orders/{id}?{}",
            server.origin,
            buyer_query(&f, id)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(buyer.status(), 200);
    let body = buyer.text().await.unwrap();
    assert!(body.contains("Mail acceptance has not been confirmed"));
    assert!(!body.contains("sent to <strong>"));
    std::fs::write(super::evidence_path("cg006-correction-buyer.html"), body).unwrap();
    *server.behavior.lock().unwrap() = SmtpBehavior::Accept;
    let retry = server
        .client
        .post(format!("{}{route}/resend", server.origin))
        .header("cookie", &f.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(retry.status(), 303);
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
    assert_eq!(correction_counts(&f).await, (1, 1));
    assert!(!orders::email_sent(&f.db, id).await.unwrap());
    let page = server
        .client
        .get(format!(
            "{}{route}?ticket_email=%3Cscript%3Ecg006_probe_alert%3C%2Fscript%3E",
            server.origin
        ))
        .header("cookie", &f.cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!page.contains("Tickets created, but"));
    assert!(!page.contains("cg006_probe_alert"));
    println!("refresh and failed retry retain exactly 1 Order/1 Ticket; same-order retry SMTP accepted=1; initial receipt remains absent");
}

#[tokio::test]
#[serial]
async fn correction_complimentary_refusal_preserves_committed_order() {
    correction_creation(SmtpBehavior::Refuse).await;
}

#[tokio::test]
#[serial]
async fn correction_complimentary_stalled_greeting_preserves_committed_order() {
    correction_creation(SmtpBehavior::StallGreeting).await;
}

#[tokio::test]
#[serial]
async fn correction_stalled_acceptance_http_is_uncertain_and_receipt_absent() {
    let server = correction_server(SmtpBehavior::StallAcceptance).await;
    let f = fixture(&server.ctx).await;
    let cipher = f.registry.cipher(&f.organizer).unwrap();
    let id = crabgate::admin_orders::complimentary(
        &f.db,
        &cipher,
        &f.event,
        &f.kind,
        "buyer@example.com",
        "Guest",
        orders::now(),
    )
    .await
    .unwrap();
    let started = Instant::now();
    let result = server
        .client
        .post(format!("{}/office/orders/{id}/resend", server.origin))
        .header("cookie", &f.cookie)
        .send()
        .await
        .unwrap();
    println!(
        "HTTP resend withheld final SMTP response elapsed={:?}; status={}",
        started.elapsed(),
        result.status()
    );
    assert_eq!(result.status(), 502);
    assert!(started.elapsed() >= Duration::from_secs(10));
    assert!(result
        .text()
        .await
        .unwrap()
        .contains("another attempt may deliver a duplicate"));
    assert_eq!(server.received.load(Ordering::SeqCst), 1);
    assert!(!orders::email_sent(&f.db, &id).await.unwrap());
}

#[tokio::test]
#[serial]
async fn correction_stalled_first_scheduled_send_allows_later_order() {
    let server = correction_server(SmtpBehavior::StallFirstGreeting).await;
    let f = fixture(&server.ctx).await;
    let cipher = f.registry.cipher(&f.organizer).unwrap();
    let mut ids = Vec::new();
    for now in [orders::now() - 2, orders::now() - 1] {
        ids.push(
            crabgate::admin_orders::complimentary(
                &f.db,
                &cipher,
                &f.event,
                &f.kind,
                "buyer@example.com",
                "Guest",
                now,
            )
            .await
            .unwrap(),
        );
    }
    let started = Instant::now();
    tokio::time::timeout(
        Duration::from_secs(12),
        crabgate::order_notices::run(&server.ctx, &f.registry),
    )
    .await
    .expect("scheduled scan must continue past stalled SMTP")
    .unwrap();
    assert!(!orders::email_sent(&f.db, &ids[0]).await.unwrap());
    assert!(orders::email_sent(&f.db, &ids[1]).await.unwrap());
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
    println!(
        "scheduled scan elapsed={:?}; first receipt absent; second receipt present; accepted=1",
        started.elapsed()
    );
}

#[tokio::test]
#[serial]
async fn correction_free_checkout_refusal_keeps_confirmation_and_existing_tickets() {
    let server = correction_server(SmtpBehavior::Refuse).await;
    let f = fixture(&server.ctx).await;
    let reserved = server
        .client
        .post(format!("{}/checkout", server.origin))
        .form(&[
            ("event_id".to_owned(), f.event.clone()),
            (format!("quantity_{}", f.kind), "1".to_owned()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(reserved.status(), 303);
    let checkout = reserved.headers()["location"].to_str().unwrap().to_owned();
    let result = server
        .client
        .post(format!("{}{checkout}", server.origin))
        .form(&[("email", "buyer@example.com"), ("name_0", "Guest")])
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 303);
    let confirmation = result.headers()["location"].to_str().unwrap().to_owned();
    assert!(confirmation.ends_with("/confirmation"));
    let page = server
        .client
        .get(format!("{}{confirmation}", server.origin))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let body = page.text().await.unwrap();
    assert!(body.contains("Mail acceptance has not been confirmed"));
    assert!(body.contains("available below"));
    assert!(!body.contains("sent to <strong>"));
    assert_eq!(correction_counts(&f).await, (1, 1));
    let id = orders::order_id_for_hold(&f.db, checkout.trim_start_matches("/checkout/"))
        .await
        .unwrap()
        .unwrap();
    assert!(!orders::email_sent(&f.db, &id).await.unwrap());
    println!("free checkout SMTP refusal: 303 confirmation; rendered available tickets/unconfirmed email; counts=(1,1); receipt absent");
}

#[tokio::test]
#[serial]
async fn correction_paid_callback_refusal_retries_email_without_another_order() {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let server = correction_server(SmtpBehavior::Refuse).await;
    let f = fixture(&server.ctx).await;
    let cipher = f.registry.cipher(&f.organizer).unwrap();
    f.db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE ticket_types SET price_cents = 1000 WHERE id = ?",
        [f.kind.clone().into()],
    ))
    .await
    .unwrap();
    let now = orders::now();
    let hold = crabgate::inventory::reserve(
        &f.db,
        &f.event,
        &[crabgate::inventory::Selection {
            ticket_type_id: f.kind.clone(),
            quantity: 1,
        }],
        now,
    )
    .await
    .unwrap();
    let details = orders::hold_details(&f.db, &hold.id).await.unwrap();
    orders::save_details(
        &f.db,
        &cipher,
        &details,
        "buyer@example.com",
        &["Guest".into()],
        false,
        now,
    )
    .await
    .unwrap();
    orders::attach_session(&f.db, &hold.id, "cs_cg006_correction", now)
        .await
        .unwrap();
    let secret = "whsec_cg006_synthetic";
    crabgate::settings::set(&f.db, &cipher, "webhook_secret", secret)
        .await
        .unwrap();
    let body = serde_json::json!({"type":"checkout.session.completed", "data":{"object":{"id":"cs_cg006_correction", "payment_status":"paid", "payment_intent":"pi_cg006_correction", "amount_total":1000, "currency":"eur", "metadata":{"hold_id":hold.id}}}}).to_string();
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    let timestamp = orders::now();
    mac.update(format!("{timestamp}.{body}").as_bytes());
    let signature = format!(
        "t={timestamp},v1={}",
        hex::encode(mac.finalize().into_bytes())
    );
    for expected in [500, 200, 200] {
        let result = server
            .client
            .post(format!("{}/webhooks/stripe", server.origin))
            .header("Stripe-Signature", &signature)
            .header("content-type", "application/json")
            .body(body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), expected);
        assert_eq!(correction_counts(&f).await, (1, 1));
        let id = orders::order_id_for_hold(&f.db, &hold.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            orders::email_sent(&f.db, &id).await.unwrap(),
            expected == 200
        );
        *server.behavior.lock().unwrap() = SmtpBehavior::Accept;
    }
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
    println!("paid callback refused SMTP=500; provider retries=200/200; counts remain (1,1); accepted=1; receipt after acceptance");
}

struct Fixture {
    registry: OrganizerRegistry,
    organizer: Organizer,
    db: tenant_sqlite_loco::TenantHandle,
    cookie: String,
    member: String,
    kind: String,
    event: String,
}

async fn fixture(ctx: &AppContext) -> Fixture {
    let registry = OrganizerRegistry::new(
        ctx.db.clone(),
        ctx.config.database.clone(),
        Edition::SelfHosted,
        "crabgate.com".into(),
        std::env::var("CRABGATE_ORGANIZER_DATABASE_DIR")
            .unwrap()
            .into(),
        MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap(),
    )
    .unwrap();
    let scope = registry.active_dbs().await.unwrap().remove(0);
    let now = orders::now();
    let member = members::add_member(
        &ctx.db,
        &scope.organizer.id,
        "cg006@example.com",
        Role::Admin,
    )
    .await
    .unwrap();
    let token = members::request_magic_link(&ctx.db, &scope.organizer.id, "cg006@example.com", now)
        .await
        .unwrap()
        .unwrap();
    let session = members::claim_magic_link(&ctx.db, &scope.organizer.id, &token, now)
        .await
        .unwrap()
        .unwrap();
    let event = events::create(
        &scope.db,
        &EventInput {
            title: "Resend Test".into(),
            date: "2027-06-15".into(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Hall".into(),
            address: "Main Street".into(),
            description: String::new(),
            event_capacity: Some(10),
            max_per_order: 10,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let kind = events::add_ticket_type(
        &scope.db,
        &event,
        &TicketTypeInput {
            name: "Standard".into(),
            price: "0".into(),
            capacity: 10,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(&scope.db, &event.id, now).await.unwrap();
    crabgate::settings::set_contact(&scope.db, "", "", "reply@example.com")
        .await
        .unwrap();
    Fixture {
        registry,
        organizer: scope.organizer,
        db: scope.db,
        cookie: format!("cg_session={session}"),
        member,
        kind: kind.id,
        event: event.id,
    }
}

fn buyer_query(f: &Fixture, id: &str) -> String {
    let expires = orders::now() + 3600;
    format!(
        "expires_at={expires}&sig={}",
        links::sign_order(&f.organizer.id, id, expires).unwrap()
    )
}

async fn receipt(db: &DatabaseConnection, id: &str) -> i64 {
    db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT email_sent_at FROM orders WHERE id = ?",
        [id.into()],
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get("", "email_sent_at")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn organiser_explicit_resend_delivers_again_without_resetting_initial_receipt() {
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        let created = request
            .post("/office/orders/complimentary")
            .add_header("cookie", &f.cookie)
            .form(&[
                ("ticket_type_id", f.kind.as_str()),
                ("email", "buyer@example.com"),
                ("name", "Guest"),
            ])
            .await;
        assert_eq!(created.status_code(), 303);
        let detail = created.header("location").to_str().unwrap().to_owned();
        let id = detail.strip_prefix("/office/orders/").unwrap();
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 1);
        let initial_receipt = receipt(&f.db, id).await;
        let response = request
            .post(&format!("{detail}/resend"))
            .add_header("cookie", &f.cookie)
            .await;
        assert_eq!(response.status_code(), 303);
        let deliveries = ctx.mailer.as_ref().unwrap().deliveries();
        println!(
            "initial confirmation + authorised HTTP resend: deliveries={}",
            deliveries.count
        );
        assert_eq!(
            deliveries.count, 2,
            "explicit organiser resend must deliver a second email"
        );
        assert_eq!(receipt(&f.db, id).await, initial_receipt);
        assert!(deliveries
            .messages
            .iter()
            .all(|message| message.contains("buyer@example.com")
                && message.contains("Reply-To: reply@example.com")
                && message.contains("Standard")
                && message.contains(&format!("/orders/{id}"))));
        send_order_email(&ctx, &f.registry, &f.organizer, &f.db, id)
            .await
            .unwrap();
        crabgate::order_notices::run(&ctx, &f.registry)
            .await
            .unwrap();
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 2);
        let feedback = request
            .get(response.header("location").to_str().unwrap())
            .add_header("cookie", &f.cookie)
            .await;
        assert_eq!(feedback.status_code(), 200);
        assert!(feedback
            .text()
            .contains("Ticket email accepted by the mail server."));
        std::fs::write(
            super::evidence_path("cg006-office-order.html"),
            feedback.text(),
        )
        .unwrap();
        let second = request
            .post(&format!("{detail}/resend"))
            .add_header("cookie", &f.cookie)
            .await;
        assert_eq!(second.status_code(), 303);
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 3);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn background_mailer_smtp_failure_is_not_success_or_an_initial_receipt() {
    use loco_rs::{app::Hooks, boot::StartMode, config::WorkerMode, environment::Environment};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let refused = Arc::new(AtomicBool::new(true));
    let messages = Arc::new(Mutex::new(Vec::<String>::new()));
    let smtp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let smtp_port = smtp_listener.local_addr().unwrap().port();
    let reject = refused.clone();
    let captured = messages.clone();
    let smtp = tokio::spawn(async move {
        loop {
            let (mut socket, _) = smtp_listener.accept().await.unwrap();
            if reject.load(Ordering::SeqCst) {
                socket
                    .write_all(b"421 synthetic refusal\r\n")
                    .await
                    .unwrap();
                continue;
            }
            let captured = captured.clone();
            let reject = reject.clone();
            tokio::spawn(async move {
                let (read, mut write) = socket.into_split();
                let mut reader = BufReader::new(read);
                write.write_all(b"220 synthetic SMTP\r\n").await.unwrap();
                let mut data = false;
                let mut message = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap() == 0 {
                        break;
                    }
                    if data {
                        if line == ".\r\n" {
                            captured.lock().unwrap().push(message.clone());
                            message.clear();
                            data = false;
                            write.write_all(b"250 accepted\r\n").await.unwrap();
                        } else {
                            message.push_str(&line);
                        }
                    } else if reject.load(Ordering::SeqCst) {
                        write.write_all(b"421 synthetic refusal\r\n").await.unwrap();
                        break;
                    } else if line.starts_with("DATA") {
                        data = true;
                        write.write_all(b"354 continue\r\n").await.unwrap();
                    } else if line.starts_with("QUIT") {
                        write.write_all(b"221 bye\r\n").await.unwrap();
                        break;
                    } else {
                        write.write_all(b"250 OK\r\n").await.unwrap();
                    }
                }
            });
        }
    });
    let mut config = App::load_config(&Environment::Test).await.unwrap();
    config.workers.mode = WorkerMode::BackgroundAsync;
    let mailer = config.mailer.as_mut().unwrap();
    mailer.stub = false;
    let smtp_config = mailer.smtp.as_mut().unwrap();
    smtp_config.host = "127.0.0.1".into();
    smtp_config.port = smtp_port;
    let boot = App::boot(StartMode::ServerOnly, &Environment::Test, config)
        .await
        .unwrap();
    let ctx = boot.app_context;
    let f = fixture(&ctx).await;
    let cipher = f.registry.cipher(&f.organizer).unwrap();
    let id = crabgate::admin_orders::complimentary(
        &f.db,
        &cipher,
        &f.event,
        &f.kind,
        "buyer@example.com",
        "Guest",
        orders::now(),
    )
    .await
    .unwrap();
    let result = send_order_email(&ctx, &f.registry, &f.organizer, &f.db, &id).await;
    assert!(
        result.is_err(),
        "SMTP refusal must reach the caller even with BackgroundAsync workers"
    );
    assert!(!orders::email_sent(&f.db, &id).await.unwrap());
    assert_eq!(messages.lock().unwrap().len(), 0);
    refused.store(false, Ordering::SeqCst);
    send_order_email(&ctx, &f.registry, &f.organizer, &f.db, &id)
        .await
        .unwrap();
    assert_eq!(messages.lock().unwrap().len(), 1);
    let initial_receipt = receipt(&f.db, &id).await;
    refused.store(true, Ordering::SeqCst);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server =
        tokio::spawn(async move { axum::serve(listener, boot.router.unwrap()).await.unwrap() });
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for (route, cookie) in [
        (
            format!("/office/orders/{id}/resend"),
            Some(f.cookie.as_str()),
        ),
        (
            format!("/orders/{id}/resend?{}", buyer_query(&f, &id)),
            None,
        ),
    ] {
        let mut request = client.post(format!("{origin}{route}"));
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 502);
        assert!(response.headers().get("location").is_none());
        let body = response.text().await.unwrap();
        assert!(body.contains("Ticket email acceptance could not be confirmed."));
        assert!(body.contains("Check with the organizer before retrying"));
        std::fs::write(super::evidence_path("cg006-resend-failure.txt"), body).unwrap();
        assert_eq!(receipt(&f.db, &id).await, initial_receipt);
        assert_eq!(messages.lock().unwrap().len(), 1);
    }
    refused.store(false, Ordering::SeqCst);
    for (index, (route, cookie)) in [
        (
            format!("/office/orders/{id}/resend"),
            Some(f.cookie.as_str()),
        ),
        (
            format!("/orders/{id}/resend?{}", buyer_query(&f, &id)),
            None,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut request = client.post(format!("{origin}{route}"));
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 303);
        assert!(response.headers()["location"]
            .to_str()
            .unwrap()
            .contains("tickets_resent=1"));
        assert_eq!(messages.lock().unwrap().len(), index + 2);
        assert_eq!(receipt(&f.db, &id).await, initial_receipt);
    }
    send_order_email(&ctx, &f.registry, &f.organizer, &f.db, &id)
        .await
        .unwrap();
    let accepted = messages.lock().unwrap();
    assert_eq!(accepted.len(), 3);
    assert!(accepted
        .iter()
        .all(|message| message.contains("To: buyer@example.com")
            && message.contains("Reply-To: reply@example.com")
            && message.contains("Standard")
            && message.contains(&format!("/orders/{id}"))));
    println!("BackgroundAsync + synthetic SMTP: failed initial receipt absent; both HTTP refusals=502; accepted delivery counts=1->2->3; repeat automatic count=3");
    server.abort();
    smtp.abort();
}

#[tokio::test]
#[serial]
async fn buyer_resend_delivers_once_per_action_and_preserves_signed_link_limits() {
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        let cipher = f.registry.cipher(&f.organizer).unwrap();
        let id = crabgate::admin_orders::complimentary(
            &f.db,
            &cipher,
            &f.event,
            &f.kind,
            "buyer@example.com",
            "Guest",
            orders::now(),
        )
        .await
        .unwrap();
        send_order_email(&ctx, &f.registry, &f.organizer, &f.db, &id)
            .await
            .unwrap();
        let initial = receipt(&f.db, &id).await;
        let query = buyer_query(&f, &id);
        assert_eq!(
            request
                .post(&format!("/orders/{id}/resend?expires_at=1&sig=invalid"))
                .await
                .status_code(),
            403
        );
        assert_eq!(
            request
                .post(&format!("/orders/{id}/resend?{query}"))
                .add_header("sec-fetch-site", "cross-site")
                .await
                .status_code(),
            403
        );
        let expired = orders::now() - 1;
        let signature = links::sign_order(&f.organizer.id, &id, expired).unwrap();
        assert_eq!(
            request
                .post(&format!(
                    "/orders/{id}/resend?expires_at={expired}&sig={signature}"
                ))
                .await
                .status_code(),
            403
        );
        for expected in 2..=4 {
            let response = request.post(&format!("/orders/{id}/resend?{query}")).await;
            assert_eq!(response.status_code(), 303);
            assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, expected);
            let feedback = request
                .get(response.header("location").to_str().unwrap())
                .await;
            assert_eq!(feedback.status_code(), 200);
            assert!(feedback
                .text()
                .contains("Ticket email accepted by the mail server."));
            std::fs::write(
                super::evidence_path("cg006-buyer-order.html"),
                feedback.text(),
            )
            .unwrap();
        }
        let limited = request.post(&format!("/orders/{id}/resend?{query}")).await;
        assert_eq!(limited.status_code(), 429);
        assert!(limited.text().contains("up to three resends per hour"));
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 4);
        assert_eq!(receipt(&f.db, &id).await, initial);
        send_order_email(&ctx, &f.registry, &f.organizer, &f.db, &id)
            .await
            .unwrap();
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 4);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn resend_rejects_door_staff_support_refunded_cancelled_and_anonymized_orders() {
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        let cipher = f.registry.cipher(&f.organizer).unwrap();
        let id = crabgate::admin_orders::complimentary(
            &f.db,
            &cipher,
            &f.event,
            &f.kind,
            "buyer@example.com",
            "Guest",
            orders::now(),
        )
        .await
        .unwrap();
        send_order_email(&ctx, &f.registry, &f.organizer, &f.db, &id)
            .await
            .unwrap();
        let route = format!("/office/orders/{id}/resend");
        assert_eq!(request.post(&route).await.status_code(), 303);
        assert_eq!(
            request
                .get(&route)
                .add_header("cookie", &f.cookie)
                .await
                .status_code(),
            405
        );
        assert_eq!(
            request
                .post(&route)
                .add_header("cookie", &f.cookie)
                .add_header("sec-fetch-site", "cross-site")
                .await
                .status_code(),
            403
        );
        let now = orders::now();
        members::add_member(
            &ctx.db,
            &f.organizer.id,
            "door@example.com",
            Role::DoorStaff,
        )
        .await
        .unwrap();
        let token = members::request_magic_link(&ctx.db, &f.organizer.id, "door@example.com", now)
            .await
            .unwrap()
            .unwrap();
        let session = members::claim_magic_link(&ctx.db, &f.organizer.id, &token, now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            request
                .post(&route)
                .add_header("cookie", &format!("cg_session={session}"))
                .await
                .status_code(),
            403
        );
        ctx.db
            .execute_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "INSERT INTO operators (id, email) VALUES ('cg006-support', 'support@example.com')",
            ))
            .await
            .unwrap();
        let token = crabgate::operator::request_link(&ctx.db, "support@example.com", now)
            .await
            .unwrap()
            .unwrap();
        let session = crabgate::operator::claim_link(&ctx.db, &token, now)
            .await
            .unwrap()
            .unwrap();
        crabgate::operator::grant(&ctx.db, &f.organizer.id, &f.member, 1, now)
            .await
            .unwrap();
        assert_eq!(
            request
                .post(&route)
                .add_header("cookie", &format!("cg_operator_session={session}"))
                .await
                .status_code(),
            403
        );
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 1);
        let buyer = format!("/orders/{id}/resend?{}", buyer_query(&f, &id));
        for status in ["RefundPending", "Refunded"] {
            f.db.execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE orders SET status = ? WHERE id = ?",
                [status.into(), id.clone().into()],
            ))
            .await
            .unwrap();
            assert_eq!(
                request
                    .post(&route)
                    .add_header("cookie", &f.cookie)
                    .await
                    .status_code(),
                400
            );
            assert_eq!(request.post(&buyer).await.status_code(), 403);
        }
        f.db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE orders SET status = 'Paid' WHERE id = ?",
            [id.clone().into()],
        ))
        .await
        .unwrap();
        f.db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET status = 'Cancelled' WHERE id = ?",
            [f.event.clone().into()],
        ))
        .await
        .unwrap();
        assert_eq!(
            request
                .post(&route)
                .add_header("cookie", &f.cookie)
                .await
                .status_code(),
            400
        );
        assert_eq!(request.post(&buyer).await.status_code(), 403);
        let page = request
            .get(&format!("/office/orders/{id}"))
            .add_header("cookie", &f.cookie)
            .await;
        assert_eq!(page.status_code(), 200);
        assert!(!page.text().contains("Resend tickets</button>"));
        let page = request
            .get(&format!("/orders/{id}?{}", buyer_query(&f, &id)))
            .await;
        assert_eq!(page.status_code(), 200);
        assert!(!page.text().contains("Resend email</button>"));
        f.db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET status = 'Published' WHERE id = ?",
            [f.event.clone().into()],
        ))
        .await
        .unwrap();
        crabgate::admin_orders::anonymize(&f.db, &cipher, &id)
            .await
            .unwrap();
        assert_eq!(
            request
                .post(&route)
                .add_header("cookie", &f.cookie)
                .await
                .status_code(),
            400
        );
        assert_eq!(request.post(&buyer).await.status_code(), 403);
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 1);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn repeated_signed_payment_callbacks_deduplicate_initial_confirmation_after_resend() {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        let cipher = f.registry.cipher(&f.organizer).unwrap();
        f.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "UPDATE ticket_types SET price_cents = 1000 WHERE id = ?", [f.kind.clone().into()])).await.unwrap();
        let now = orders::now();
        let hold = crabgate::inventory::reserve(&f.db, &f.event, &[crabgate::inventory::Selection { ticket_type_id: f.kind.clone(), quantity: 1 }], now).await.unwrap();
        let details = orders::hold_details(&f.db, &hold.id).await.unwrap();
        orders::save_details(&f.db, &cipher, &details, "buyer@example.com", &["Guest".into()], false, now).await.unwrap();
        orders::attach_session(&f.db, &hold.id, "cs_cg006", now).await.unwrap();
        let secret = "whsec_cg006_synthetic";
        crabgate::settings::set(&f.db, &cipher, "webhook_secret", secret).await.unwrap();
        let mut id = String::new();
        let mut initial = 0;
        for (index, expected) in [1, 1, 2].into_iter().enumerate() {
            let body = serde_json::json!({"type":"checkout.session.completed", "data":{"object":{"id":"cs_cg006", "payment_status":"paid", "payment_intent":"pi_cg006", "amount_total":1000, "currency":"eur", "metadata":{"hold_id":hold.id}}}}).to_string();
            let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
            let timestamp = orders::now();
            mac.update(format!("{timestamp}.{body}").as_bytes());
            let signature = format!("t={timestamp},v1={}", hex::encode(mac.finalize().into_bytes()));
            let response = request.post("/webhooks/stripe").text(&body).add_header("Stripe-Signature", signature).await;
            assert_eq!(response.status_code(), 200);
            assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, expected);
            if index == 0 {
                id = orders::order_id_for_hold(&f.db, &hold.id).await.unwrap().unwrap();
                initial = receipt(&f.db, &id).await;
            }
            assert_eq!(receipt(&f.db, &id).await, initial);
            if index == 1 {
                let response = request.post(&format!("/office/orders/{id}/resend")).add_header("cookie", &f.cookie).await;
                assert_eq!(response.status_code(), 303);
                assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 2);
            }
        }
        let order = orders::get_order(&f.db, &cipher, &id).await.unwrap();
        assert_eq!(order.tickets.len(), 1);
        assert_eq!(order.status, "Paid");
        println!("signed payment HTTP callback counts: 1->1; explicit resend 1->2; later callback remains 2; tickets=1");
    }).await;
}

async fn cg008_callback(server: &CorrectionServer, hold: &str, kind: &str) -> reqwest::Response {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let body = serde_json::json!({"type":kind,"data":{"object":{"id":"cs_cg007_1","payment_status":"paid","payment_intent":"pi_cg008","amount_total":1000,"currency":"eur","metadata":{"hold_id":hold}}}}).to_string();
    let timestamp = orders::now();
    let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec_cg007").unwrap();
    mac.update(format!("{timestamp}.{body}").as_bytes());
    server
        .client
        .post(format!("{}/webhooks/stripe", server.origin))
        .header(
            "Stripe-Signature",
            format!(
                "t={timestamp},v1={}",
                hex::encode(mac.finalize().into_bytes())
            ),
        )
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn cg008_begin(server: &CorrectionServer, f: &Fixture, paid: bool) -> String {
    if paid {
        let cipher = f.registry.cipher(&f.organizer).unwrap();
        f.db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE ticket_types SET price_cents = 1000 WHERE id = ?",
            [f.kind.clone().into()],
        ))
        .await
        .unwrap();
        crabgate::settings::set(&f.db, &cipher, "stripe_secret_key", "sk_test_synthetic")
            .await
            .unwrap();
        crabgate::settings::set(&f.db, &cipher, "webhook_secret", "whsec_cg007")
            .await
            .unwrap();
    }
    let start = server
        .client
        .post(format!("{}/checkout", server.origin))
        .form(&[
            ("event_id".into(), f.event.clone()),
            (format!("quantity_{}", f.kind), "1".into()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(start.status(), 303, "{}", start.text().await.unwrap());
    start.headers()["location"].to_str().unwrap().to_owned()
}

#[tokio::test]
#[serial]
async fn cg008_nullable_name_public_mailer_renders_without_error() {
    let server = correction_server(SmtpBehavior::Accept).await;
    let result = crabgate::mailers::ticket::TicketMailer::send_confirmation(&server.ctx,
        "buyer@example.com", "Organizer", "ORDER", "https://example.com/orders/test",
        serde_json::json!([{"code":"TESTCODE", "ticket_type":"Standard", "attendee_name":null, "seat":null, "qr_url":"https://example.com/qr"}]), None).await;
    println!(
        "nullable-name public mailer result={result:?}; SMTP accepted={}",
        server.accepted.load(Ordering::SeqCst)
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
    assert!(server.messages.lock().unwrap()[0].contains("TESTCODE"));
}

async fn cg008_blank_flow(paid: bool) {
    let _stripe = super::checkout_expiry::mock().await;
    let server = correction_server(SmtpBehavior::Accept).await;
    let f = fixture(&server.ctx).await;
    let path = cg008_begin(&server, &f, paid).await;
    let id = path.trim_start_matches("/checkout/");
    let page = server
        .client
        .get(format!("{}{path}", server.origin))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains("Optional"));
    let submit = server
        .client
        .post(format!("{}{path}", server.origin))
        .form(&[("email", "buyer@example.com"), ("name_0", "")])
        .send()
        .await
        .unwrap();
    assert_eq!(submit.status(), 303);
    if paid {
        let callback = cg008_callback(&server, id, "checkout.session.completed").await;
        println!(
            "blank paid callback status={}; counts={:?}; SMTP accepted={}",
            callback.status(),
            correction_counts(&f).await,
            server.accepted.load(Ordering::SeqCst)
        );
        assert_eq!(callback.status(), 200, "{}", callback.text().await.unwrap());
    }
    let order = orders::order_id_for_hold(&f.db, id).await.unwrap().unwrap();
    println!(
        "blank paid={paid}: counts={:?}; email_sent={}; SMTP accepted={}",
        correction_counts(&f).await,
        orders::email_sent(&f.db, &order).await.unwrap(),
        server.accepted.load(Ordering::SeqCst)
    );
    assert_eq!(correction_counts(&f).await, (1, 1));
    assert!(orders::email_sent(&f.db, &order).await.unwrap());
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
    let cipher = f.registry.cipher(&f.organizer).unwrap();
    assert_eq!(
        orders::get_order(&f.db, &cipher, &order)
            .await
            .unwrap()
            .tickets[0]
            .attendee_name,
        None
    );
    let confirmation = server
        .client
        .get(format!("{}{path}/confirmation", server.origin))
        .send()
        .await
        .unwrap();
    assert_eq!(confirmation.status(), 200);
    let html = confirmation.text().await.unwrap();
    assert!(html.contains("Tue 15 June 2027"), "{html}");
    assert!(html.contains("Standard"));
    assert!(html.contains(if paid { "Paid €10" } else { ">Free<" }));
    std::fs::write(
        if paid {
            super::evidence_path("cg008-paid.html")
        } else {
            super::evidence_path("cg008-free.html")
        },
        html,
    )
    .unwrap();
    if paid {
        assert_eq!(
            cg008_callback(&server, id, "checkout.session.completed")
                .await
                .status(),
            200
        );
    } else {
        let retry = server
            .client
            .post(format!("{}{path}", server.origin))
            .form(&[("email", "buyer@example.com")])
            .send()
            .await
            .unwrap();
        assert_eq!(retry.status(), 303);
    }
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
    let resend = server
        .client
        .post(format!(
            "{}/orders/{order}/resend?{}",
            server.origin,
            buyer_query(&f, &order)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resend.status(), 303, "{}", resend.text().await.unwrap());
    assert_eq!(server.accepted.load(Ordering::SeqCst), 2);
    assert_eq!(correction_counts(&f).await, (1, 1));
    let messages = server.messages.lock().unwrap();
    for message in messages.iter() {
        assert!(message.contains("Standard"));
        assert!(message.contains("QR code for ticket"));
        assert!(message.contains("Show each ticket code at the door"));
        assert!(!message.contains("null"));
    }
    std::fs::write(
        if paid {
            super::evidence_path("cg008-paid-mail.txt")
        } else {
            super::evidence_path("cg008-free-mail.txt")
        },
        messages.join("\n"),
    )
    .unwrap();
}

#[tokio::test]
#[serial]
async fn cg008_blank_free_checkout_and_resend_deliver_once_each() {
    cg008_blank_flow(false).await;
}
#[tokio::test]
#[serial]
async fn cg008_blank_paid_checkout_and_resend_deliver_once_each() {
    cg008_blank_flow(true).await;
}

#[tokio::test]
#[serial]
async fn cg008_blank_explicit_resend_preserves_committed_booking() {
    let server = correction_server(SmtpBehavior::Refuse).await;
    let f = fixture(&server.ctx).await;
    let path = cg008_begin(&server, &f, false).await;
    let submit = server
        .client
        .post(format!("{}{path}", server.origin))
        .form(&[("email", "buyer@example.com"), ("name_0", "")])
        .send()
        .await
        .unwrap();
    assert_eq!(submit.status(), 303);
    let order = orders::order_id_for_hold(&f.db, path.trim_start_matches("/checkout/"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(correction_counts(&f).await, (1, 1));
    assert!(!orders::email_sent(&f.db, &order).await.unwrap());
    *server.behavior.lock().unwrap() = SmtpBehavior::Accept;
    let resend = server
        .client
        .post(format!(
            "{}/orders/{order}/resend?{}",
            server.origin,
            buyer_query(&f, &order)
        ))
        .send()
        .await
        .unwrap();
    println!(
        "blank explicit resend status={}; counts={:?}; SMTP accepted={}",
        resend.status(),
        correction_counts(&f).await,
        server.accepted.load(Ordering::SeqCst)
    );
    assert_eq!(resend.status(), 303, "{}", resend.text().await.unwrap());
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
    assert!(!orders::email_sent(&f.db, &order).await.unwrap());
    assert_eq!(correction_counts(&f).await, (1, 1));
}

#[tokio::test]
#[serial]
async fn cg008_delayed_failed_and_retried_signed_callbacks_are_safe() {
    let _stripe = super::checkout_expiry::mock().await;
    let server = correction_server(SmtpBehavior::Accept).await;
    let f = fixture(&server.ctx).await;
    let path = cg008_begin(&server, &f, true).await;
    let id = path.trim_start_matches("/checkout/");
    for _ in 0..2 {
        let submit = server
            .client
            .post(format!("{}{path}", server.origin))
            .form(&[("email", "buyer@example.com"), ("name_0", "")])
            .send()
            .await
            .unwrap();
        assert_eq!(submit.status(), 303);
        assert_eq!(
            submit.headers()["location"],
            "https://checkout.stripe.com/cs_cg007_1"
        );
        let page = server
            .client
            .get(format!(
                "{}{path}/confirmation?session_id=cs_forged",
                server.origin
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 202);
        let html = page.text().await.unwrap();
        assert!(html.contains("Payment not confirmed"));
        std::fs::write(super::evidence_path("cg008-correction-pending.html"), &html).unwrap();
        assert!(html.contains(&format!("href=\"{path}\"")));
        assert!(html.contains("If you have not paid: continue checkout"));
        assert!(html.contains("We have not confirmed your payment yet"));
        let continuation = server
            .client
            .get(format!("{}{path}", server.origin))
            .send()
            .await
            .unwrap();
        assert_eq!(continuation.status(), 200);
        assert!(html.contains("do not book again"));
        assert!(html.contains(&format!("href=\"{path}/confirmation\"")));
        assert!(!html.contains("You're in"));
        std::fs::write(super::evidence_path("cg008-pending.html"), html).unwrap();
        assert_eq!(correction_counts(&f).await, (0, 0));
    }
    let hold =
        f.db.query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count, MAX(stripe_session_id) AS session FROM holds",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hold.try_get::<i64>("", "count").unwrap(), 1);
    assert_eq!(hold.try_get::<String>("", "session").unwrap(), "cs_cg007_1");
    let invalid = server
        .client
        .post(format!("{}/webhooks/stripe", server.origin))
        .header("Stripe-Signature", "t=1,v1=forged")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    assert_eq!(correction_counts(&f).await, (0, 0));
    assert_eq!(
        cg008_callback(&server, id, "checkout.session.async_payment_failed")
            .await
            .status(),
        200
    );
    let failed = server
        .client
        .get(format!("{}{path}/confirmation", server.origin))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), 410);
    let html = failed.text().await.unwrap();
    assert!(html.contains("Checkout closed"));
    assert!(!html.contains("Payment not confirmed"));
    assert_eq!(correction_counts(&f).await, (0, 0));
    assert_eq!(
        crabgate::inventory::open_counts(&f.db, &f.event, orders::now())
            .await
            .unwrap(),
        (0, 0)
    );
    for _ in 0..2 {
        assert_eq!(
            cg008_callback(&server, id, "checkout.session.async_payment_succeeded")
                .await
                .status(),
            200
        );
    }
    assert_eq!(correction_counts(&f).await, (1, 1));
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
    let committed = server
        .client
        .get(format!("{}{path}", server.origin))
        .send()
        .await
        .unwrap();
    assert_eq!(committed.status(), 303);
    assert_eq!(
        committed.headers()["location"],
        format!("{path}/confirmation")
    );
    for suffix in ["", "/cancel"] {
        let retry = server
            .client
            .post(format!("{}{path}{suffix}", server.origin))
            .form(&[("email", "buyer@example.com")])
            .send()
            .await
            .unwrap();
        assert_eq!(retry.status(), 303);
        assert_eq!(retry.headers()["location"], format!("{path}/confirmation"));
    }
    println!("redirect-only=(0,0); failed callback=(0,0); delayed paid/retry=(1,1); SMTP accepted=1; committed retry returns existing confirmation");
}

#[tokio::test]
#[serial]
async fn cg008_order_links_and_payment_ticket_states_are_truthful() {
    let server = correction_server(SmtpBehavior::Accept).await;
    let f = fixture(&server.ctx).await;
    let path = cg008_begin(&server, &f, false).await;
    assert_eq!(
        server
            .client
            .post(format!("{}{path}", server.origin))
            .form(&[("email", "buyer@example.com"), ("name_0", "")])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    let order = orders::order_id_for_hold(&f.db, path.trim_start_matches("/checkout/"))
        .await
        .unwrap()
        .unwrap();
    for (query, status) in [
        ("", 400),
        ("expires_at=no&sig=x", 400),
        ("expires_at=1&sig=x", 403),
    ] {
        let page = server
            .client
            .get(format!("{}/orders/{order}?{query}", server.origin))
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), status);
        let html = page.text().await.unwrap();
        assert!(html.contains("order link"));
        assert!(html.contains("Contact the organizer"));
        assert!(!html.contains("You're in"));
        std::fs::write(super::evidence_path("cg008-order-link.html"), html).unwrap();
    }
    for (status, expected) in [
        ("RefundPending", "Your refund has not been confirmed."),
        ("Refunded", "Your refund has not been confirmed."),
    ] {
        f.db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE orders SET status = ? WHERE id = ?",
            [status.into(), order.clone().into()],
        ))
        .await
        .unwrap();
        let page = server
            .client
            .get(format!("{}{path}/confirmation", server.origin))
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        let html = page.text().await.unwrap();
        assert!(html.contains(expected));
        assert!(!html.contains("You're in"));
        assert!(!html.contains("<img src=\"/orders/"));
        if status == "RefundPending" {
            assert!(!html.contains("This order was refunded"));
        }
        std::fs::write(super::evidence_path(&format!("cg008-{status}.html")), html).unwrap();
    }
    f.db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE orders SET status = 'Paid' WHERE id = ?",
        [order.clone().into()],
    ))
    .await
    .unwrap();
    f.db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE events SET status = 'Cancelled' WHERE id = ?",
        [f.event.clone().into()],
    ))
    .await
    .unwrap();
    let html = server
        .client
        .get(format!("{}{path}/confirmation", server.origin))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("This event is cancelled"));
    assert!(!html.contains("You're in"));
    assert!(!html.contains("<img src=\"/orders/"));
    assert!(!html.contains("Resend email"));
    std::fs::write(super::evidence_path("cg008-cancelled.html"), html).unwrap();
    assert_eq!(correction_counts(&f).await, (1, 1));
}

#[tokio::test]
#[serial]
async fn cg008_fully_discounted_optional_name_is_free_and_escaped_on_resend() {
    let server = correction_server(SmtpBehavior::Accept).await;
    let f = fixture(&server.ctx).await;
    f.db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE ticket_types SET price_cents = 1000 WHERE id = ?",
        [f.kind.clone().into()],
    ))
    .await
    .unwrap();
    crabgate::discounts::create(
        &f.db,
        crabgate::discounts::NewDiscount {
            event_id: &f.event,
            code: "FREE",
            kind: "Percent",
            value: 100,
            starts_at: None,
            ends_at: None,
            max_redemptions: Some(1),
        },
        orders::now(),
    )
    .await
    .unwrap();
    let start = server
        .client
        .post(format!("{}/checkout", server.origin))
        .form(&[
            ("event_id".into(), f.event.clone()),
            (format!("quantity_{}", f.kind), "1".into()),
            ("discount_code".into(), "FREE".into()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(start.status(), 303);
    let path = start.headers()["location"].to_str().unwrap();
    let details = server
        .client
        .get(format!("{}{path}", server.origin))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(details.contains("Get tickets"));
    assert!(!details.contains("Continue to payment"));
    assert_eq!(
        server
            .client
            .post(format!("{}{path}", server.origin))
            .form(&[("email", "buyer@example.com"), ("name_0", "")])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    let id = orders::order_id_for_hold(&f.db, path.trim_start_matches("/checkout/"))
        .await
        .unwrap()
        .unwrap();
    let html = server
        .client
        .get(format!("{}{path}/confirmation", server.origin))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains(">Free<"));
    assert!(!html.contains("Paid €10"));
    let cipher = f.registry.cipher(&f.organizer).unwrap();
    let ticket = orders::get_order(&f.db, &cipher, &id)
        .await
        .unwrap()
        .tickets
        .remove(0);
    let changed = server
        .client
        .post(format!(
            "{}/orders/{id}/names?{}",
            server.origin,
            buyer_query(&f, &id)
        ))
        .form(&[(
            format!("name_{}", ticket.id),
            "Élodie <script> & \"Guest\"".to_owned(),
        )])
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), 303);
    let resent = server
        .client
        .post(format!(
            "{}/orders/{id}/resend?{}",
            server.origin,
            buyer_query(&f, &id)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resent.status(), 303);
    assert_eq!(server.accepted.load(Ordering::SeqCst), 2);
    let raw = server.messages.lock().unwrap()[1].replace("=\r\n", "");
    assert!(
        raw.contains("=C3=89lodie &lt;script&gt; &amp; &quot;Guest&quot;"),
        "{raw}"
    );
    assert!(!raw.contains("<script>"));
    assert!(raw.contains(&ticket.code));
    assert!(raw.contains("QR code for ticket"));
    assert_eq!(correction_counts(&f).await, (1, 1));
    std::fs::write(super::evidence_path("cg008-escaped-mail.txt"), raw).unwrap();
}

#[tokio::test]
#[serial]
async fn cg008_cancelled_checkout_blank_name_late_callback_keeps_one_booking() {
    let _stripe = super::checkout_expiry::mock().await;
    let server = correction_server(SmtpBehavior::Accept).await;
    let f = fixture(&server.ctx).await;
    let path = cg008_begin(&server, &f, true).await;
    let id = path.trim_start_matches("/checkout/");
    let submitted = server
        .client
        .post(format!("{}{path}", server.origin))
        .form(&[("email", "buyer@example.com"), ("name_0", "")])
        .send()
        .await
        .unwrap();
    assert_eq!(submitted.status(), 303);
    assert_eq!(
        server
            .client
            .post(format!("{}{path}/cancel", server.origin))
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    assert_eq!(
        orders::hold_details(&f.db, id).await.unwrap().status,
        "Expired"
    );
    assert_eq!(correction_counts(&f).await, (0, 0));
    for _ in 0..2 {
        let callback = cg008_callback(&server, id, "checkout.session.completed").await;
        assert_eq!(callback.status(), 200, "{}", callback.text().await.unwrap());
        assert_eq!(correction_counts(&f).await, (1, 1));
    }
    let order = orders::order_id_for_hold(&f.db, id).await.unwrap().unwrap();
    assert!(orders::email_sent(&f.db, &order).await.unwrap());
    assert_eq!(server.accepted.load(Ordering::SeqCst), 1);
    let cipher = f.registry.cipher(&f.organizer).unwrap();
    let ticket = orders::get_order(&f.db, &cipher, &order)
        .await
        .unwrap()
        .tickets
        .remove(0);
    assert_eq!(ticket.attendee_name, None);
    assert!(server.messages.lock().unwrap()[0].contains(&ticket.code));
    println!("checkout cancelled before blank-name paid callback: callback/retry=200; one Order/Ticket; email receipt set; SMTP accepted=1");
}
