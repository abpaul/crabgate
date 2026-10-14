use crabgate::{
    admin_orders,
    app::App,
    crypto::MasterKey,
    events::{self, EventInput, TicketTypeInput},
    links,
    members::{self, Role},
    orders,
    organizers::{Edition, Organizer, OrganizerRegistry},
};
use loco_rs::app::AppContext;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

const DAY: i64 = 86_400;

#[derive(Clone, Copy)]
enum Smtp {
    Accept,
    /// Answers every connection with a transient 421.
    Unavailable,
    /// Reads the whole message and never answers.
    StallAcceptance,
    /// Accepts after a delay, so that two senders overlap.
    SlowAccept(Duration),
}

struct Server {
    ctx: AppContext,
    origin: String,
    client: reqwest::Client,
    mode: Arc<Mutex<Smtp>>,
    /// While set, a recipient address that contains `refused` gets a permanent 550.
    refuse_mailbox: Arc<AtomicBool>,
    connections: Arc<AtomicUsize>,
    refused_recipients: Arc<AtomicUsize>,
    accepted: Arc<AtomicUsize>,
    messages: Arc<Mutex<Vec<String>>>,
    smtp: tokio::task::JoinHandle<()>,
    http: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.http.abort();
        self.smtp.abort();
    }
}

impl Server {
    fn set(&self, mode: Smtp) {
        *self.mode.lock().unwrap() = mode;
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn refused_recipients(&self) -> usize {
        self.refused_recipients.load(Ordering::SeqCst)
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    fn messages(&self) -> Vec<String> {
        self.messages.lock().unwrap().clone()
    }
}

async fn server(initial: Smtp) -> Server {
    use loco_rs::{app::Hooks, boot::StartMode, config::WorkerMode, environment::Environment};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mode = Arc::new(Mutex::new(initial));
    let refuse_mailbox = Arc::new(AtomicBool::new(true));
    let connections = Arc::new(AtomicUsize::new(0));
    let refused_recipients = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::new(AtomicUsize::new(0));
    let messages = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let smtp = {
        let mode = mode.clone();
        let refuse_mailbox = refuse_mailbox.clone();
        let connections = connections.clone();
        let refused_recipients = refused_recipients.clone();
        let accepted = accepted.clone();
        let messages = messages.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                connections.fetch_add(1, Ordering::SeqCst);
                let mode = *mode.lock().unwrap();
                let refuse_mailbox = refuse_mailbox.clone();
                let refused_recipients = refused_recipients.clone();
                let accepted = accepted.clone();
                let messages = messages.clone();
                tokio::spawn(async move {
                    if matches!(mode, Smtp::Unavailable) {
                        let _ = socket.write_all(b"421 synthetic outage\r\n").await;
                        return;
                    }
                    if let Smtp::SlowAccept(delay) = mode {
                        tokio::time::sleep(delay).await;
                    }
                    let (read, mut write) = socket.into_split();
                    let mut reader = BufReader::new(read);
                    write.write_all(b"220 synthetic SMTP\r\n").await.unwrap();
                    let mut data = false;
                    let mut message = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let reply: &[u8] = if data {
                            if line != ".\r\n" {
                                message.push_str(&line);
                                continue;
                            }
                            if matches!(mode, Smtp::StallAcceptance) {
                                let _ = reader.read_line(&mut String::new()).await;
                                return;
                            }
                            messages.lock().unwrap().push(std::mem::take(&mut message));
                            accepted.fetch_add(1, Ordering::SeqCst);
                            data = false;
                            b"250 accepted\r\n"
                        } else if line.starts_with("RCPT")
                            && line.contains("refused")
                            && refuse_mailbox.load(Ordering::SeqCst)
                        {
                            refused_recipients.fetch_add(1, Ordering::SeqCst);
                            b"550 5.1.1 synthetic unknown mailbox\r\n"
                        } else if line.starts_with("DATA") {
                            data = true;
                            b"354 continue\r\n"
                        } else if line.starts_with("QUIT") {
                            let _ = write.write_all(b"221 bye\r\n").await;
                            return;
                        } else {
                            b"250 OK\r\n"
                        };
                        if write.write_all(reply).await.is_err() {
                            return;
                        }
                    }
                });
            }
        })
    };
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
    Server {
        ctx: boot.app_context,
        origin,
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap(),
        mode,
        refuse_mailbox,
        connections,
        refused_recipients,
        accepted,
        messages,
        smtp,
        http,
    }
}

struct Fixture {
    registry: OrganizerRegistry,
    organizer: Organizer,
    db: tenant_sqlite_loco::TenantHandle,
    cookie: String,
    kind: String,
    event: String,
}

fn registry(ctx: &AppContext) -> OrganizerRegistry {
    OrganizerRegistry::new(
        ctx.db.clone(),
        ctx.config.database.clone(),
        Edition::SelfHosted,
        "crabgate.com".into(),
        std::env::var("CRABGATE_ORGANIZER_DATABASE_DIR")
            .unwrap()
            .into(),
        MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap(),
    )
    .unwrap()
}

async fn fixture(ctx: &AppContext) -> Fixture {
    let registry = registry(ctx);
    let scope = registry.active_dbs().await.unwrap().remove(0);
    let now = orders::now();
    members::add_member(
        &ctx.db,
        &scope.organizer.id,
        "cg063@example.com",
        Role::Admin,
    )
    .await
    .unwrap();
    let token = members::request_magic_link(&ctx.db, &scope.organizer.id, "cg063@example.com", now)
        .await
        .unwrap()
        .unwrap();
    let session = members::claim_magic_link(&ctx.db, &scope.organizer.id, &token, now)
        .await
        .unwrap()
        .unwrap();
    let (event, kind) = event(&scope.db, "Retry Test").await;
    Fixture {
        registry,
        organizer: scope.organizer,
        db: scope.db,
        cookie: format!("cg_session={session}"),
        kind,
        event,
    }
}

async fn event(db: &tenant_sqlite_loco::TenantHandle, title: &str) -> (String, String) {
    let event = events::create(
        db,
        &EventInput {
            title: title.into(),
            date: "2027-06-15".into(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Hall".into(),
            address: "Main Street".into(),
            description: String::new(),
            event_capacity: Some(400),
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
            price: "0".into(),
            capacity: 400,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(db, &event.id, orders::now()).await.unwrap();
    (event.id, kind.id)
}

/// One pass of the Ticket email retry job, as it runs `seconds` from now.
async fn pass(server: &Server, registry: &OrganizerRegistry, seconds: i64) {
    crabgate::order_notices::run_at(&server.ctx, registry, orders::now() + seconds)
        .await
        .unwrap();
}

/// A paid Order that never had a Ticket email attempt.
async fn unsent_order(f: &Fixture, email: &str, created: i64) -> String {
    let cipher = f.registry.cipher(&f.organizer).unwrap();
    admin_orders::complimentary(&f.db, &cipher, &f.event, &f.kind, email, "Guest", created)
        .await
        .unwrap()
}

/// The Buyer books a free Ticket in the Shop. Returns the Order and its confirmation page.
async fn free_checkout(server: &Server, f: &Fixture, email: &str) -> (String, String) {
    free_checkout_for(server, f, &f.event, &f.kind, email).await
}

async fn free_checkout_for(
    server: &Server,
    f: &Fixture,
    event: &str,
    kind: &str,
    email: &str,
) -> (String, String) {
    let reserved = server
        .client
        .post(format!("{}/checkout", server.origin))
        .form(&[
            ("event_id".to_owned(), event.to_owned()),
            (format!("quantity_{kind}"), "1".to_owned()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(reserved.status(), 303);
    let checkout = reserved.headers()["location"].to_str().unwrap().to_owned();
    let submitted = server
        .client
        .post(format!("{}{checkout}", server.origin))
        .form(&[("email", email), ("name_0", "Guest")])
        .send()
        .await
        .unwrap();
    assert_eq!(submitted.status(), 303);
    let confirmation = submitted.headers()["location"].to_str().unwrap().to_owned();
    let id = orders::order_id_for_hold(&f.db, checkout.trim_start_matches("/checkout/"))
        .await
        .unwrap()
        .unwrap();
    let page = server
        .client
        .get(format!("{}{confirmation}", server.origin))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    (id, page.text().await.unwrap())
}

/// An Admin creates a complimentary Order in the Office. Returns the Order and the redirect.
async fn complimentary(server: &Server, f: &Fixture, email: &str) -> (String, String) {
    let created = server
        .client
        .post(format!("{}/office/orders/complimentary", server.origin))
        .header("cookie", &f.cookie)
        .form(&[
            ("ticket_type_id", f.kind.as_str()),
            ("email", email),
            ("name", "Guest"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 303);
    let location = created.headers()["location"].to_str().unwrap().to_owned();
    let id = location
        .split('?')
        .next()
        .unwrap()
        .strip_prefix("/office/orders/")
        .unwrap()
        .to_owned();
    (id, location)
}

async fn office_post(server: &Server, f: &Fixture, id: &str, action: &str) -> reqwest::Response {
    server
        .client
        .post(format!("{}/office/orders/{id}/{action}", server.origin))
        .header("cookie", &f.cookie)
        .send()
        .await
        .unwrap()
}

async fn office_page(server: &Server, f: &Fixture, id: &str) -> String {
    let page = server
        .client
        .get(format!("{}/office/orders/{id}", server.origin))
        .header("cookie", &f.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    page.text().await.unwrap()
}

fn buyer_query(f: &Fixture, id: &str) -> String {
    let expires = orders::now() + 3600;
    format!(
        "expires_at={expires}&sig={}",
        links::sign_order(&f.organizer.id, id, expires).unwrap()
    )
}

async fn buyer_page(server: &Server, f: &Fixture, id: &str) -> String {
    let page = server
        .client
        .get(format!(
            "{}/orders/{id}?{}",
            server.origin,
            buyer_query(f, id)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    page.text().await.unwrap()
}

async fn buyer_resend(server: &Server, f: &Fixture, id: &str) -> reqwest::StatusCode {
    server
        .client
        .post(format!(
            "{}/orders/{id}/resend?{}",
            server.origin,
            buyer_query(f, id)
        ))
        .send()
        .await
        .unwrap()
        .status()
}

async fn receipt(f: &Fixture, id: &str) -> Option<i64> {
    f.db.query_one_raw(Statement::from_sql_and_values(
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

/// The text of the "Ticket email" line on the Office Order page, or "" when the page has none.
fn ticket_line(page: &str) -> String {
    page.split("<dt>Ticket email</dt><dd>")
        .nth(1)
        .and_then(|rest| rest.split("</dd>").next())
        .unwrap_or_default()
        .to_owned()
}

struct EmailRow {
    state: String,
    attempts: i64,
    retry_after: Option<i64>,
    claim_until: Option<i64>,
}

async fn email_row(f: &Fixture, id: &str) -> EmailRow {
    let row = f
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT email_state, email_attempts, email_retry_after, email_claim_until FROM orders WHERE id = ?",
            [id.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    EmailRow {
        state: row.try_get("", "email_state").unwrap(),
        attempts: row.try_get("", "email_attempts").unwrap(),
        retry_after: row.try_get("", "email_retry_after").unwrap(),
        claim_until: row.try_get("", "email_claim_until").unwrap(),
    }
}

async fn sql(f: &Fixture, text: &str, id: &str) {
    f.db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        text,
        [id.into()],
    ))
    .await
    .unwrap();
}

/// Clock shortcut: the stored claim and retry time of one Order move one day into the past.
async fn a_day_later(f: &Fixture, id: &str) {
    sql(
        f,
        "UPDATE orders SET email_claim_until = email_claim_until - 86400, email_retry_after = email_retry_after - 86400 WHERE id = ?",
        id,
    )
    .await;
}

/// One pass of the Ticket email retry job with the wall clock.
async fn real_pass(server: &Server, registry: &OrganizerRegistry) {
    crabgate::order_notices::run(&server.ctx, registry)
        .await
        .unwrap();
}

async fn connections_reach(server: &Server, count: usize) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while server.connections() < count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

/// One real pass that is dropped while its send is in flight, like a stop or a crash.
/// Returns false when the pass ended without a new SMTP connection.
async fn interrupted_pass(server: &Server, registry: &OrganizerRegistry) -> bool {
    let before = server.connections();
    tokio::select! {
        result = crabgate::order_notices::run(&server.ctx, registry) => {
            result.unwrap();
            assert_eq!(server.connections(), before);
            false
        }
        () = connections_reach(server, before + 1) => true,
    }
}

/// A real pass with its own registry, as a second process runs it.
fn spawned_pass(server: &Server) -> tokio::task::JoinHandle<()> {
    let ctx = server.ctx.clone();
    tokio::spawn(async move {
        let registry = registry(&ctx);
        crabgate::order_notices::run(&ctx, &registry).await.unwrap();
    })
}

/// Collects the messages of log events while the returned guard lives.
fn captured_logs() -> (Arc<Mutex<Vec<String>>>, tracing::subscriber::DefaultGuard) {
    use tracing_subscriber::layer::SubscriberExt;
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
    let guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(Capture(logs.clone())),
    );
    (logs, guard)
}

const STOPPED_LINE: &str = "Automatic attempts stopped after 8 tries. Mail acceptance was not confirmed. Check the mail server, then use Resend tickets.";
const STOPPED_LOG: &str = "Ticket email automatic attempts stopped; use Resend tickets";

#[tokio::test]
#[serial]
async fn interrupted_attempts_stop_at_eight_and_the_office_page_says_so() {
    let server = server(Smtp::StallAcceptance).await;
    let f = fixture(&server.ctx).await;

    // Attempt 1: the Admin's browser gives up while the send is in flight.
    let impatient = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let gave_up = impatient
        .post(format!("{}/office/orders/complimentary", server.origin))
        .header("cookie", &f.cookie)
        .form(&[
            ("ticket_type_id", f.kind.as_str()),
            ("email", "buyer@example.com"),
            ("name", "Guest"),
        ])
        .send()
        .await;
    assert!(gave_up.unwrap_err().is_timeout());
    assert_eq!(server.connections(), 1);
    let id: String =
        f.db.query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT id FROM orders ORDER BY rowid DESC LIMIT 1",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "id")
        .unwrap();
    // Longer than the 10-second send bound: no result of this attempt was written.
    tokio::time::sleep(Duration::from_secs(11)).await;
    let row = email_row(&f, &id).await;
    assert_eq!((row.state.as_str(), row.attempts), ("Pending", 1));
    assert!(row.claim_until.unwrap() > orders::now());
    assert_eq!(
        ticket_line(&office_page(&server, &f, &id).await),
        "Mail acceptance not confirmed after 1 of 8 automatic attempts. CrabGate will try again."
    );
    // A second sender inside the claim sends nothing.
    real_pass(&server, &f.registry).await;
    crabgate::controllers::checkout::send_order_email(
        &server.ctx,
        &f.registry,
        &f.organizer,
        &f.db,
        &id,
    )
    .await
    .unwrap();
    assert_eq!(server.connections(), 1);

    // Attempts 2 to 8: the job pass is dropped during each send.
    let (logs, _guard) = captured_logs();
    for attempt in 2..=8 {
        a_day_later(&f, &id).await;
        assert!(
            interrupted_pass(&server, &f.registry).await,
            "attempt {attempt}"
        );
    }
    assert_eq!(server.connections(), 8);
    let row = email_row(&f, &id).await;
    assert_eq!((row.state.as_str(), row.attempts), ("Pending", 8));
    assert_eq!(
        ticket_line(&office_page(&server, &f, &id).await),
        "The last of 8 automatic attempts is in progress."
    );

    // The claim of attempt 8 ends without a result.
    a_day_later(&f, &id).await;
    assert_eq!(
        ticket_line(&office_page(&server, &f, &id).await),
        STOPPED_LINE
    );
    for pass in 9..=10 {
        assert!(
            !interrupted_pass(&server, &f.registry).await,
            "pass {pass} opened an SMTP connection"
        );
        a_day_later(&f, &id).await;
    }
    assert_eq!(server.connections(), 8);
    let row = email_row(&f, &id).await;
    assert_eq!((row.state.as_str(), row.attempts), ("Stopped", 8));
    assert_eq!(receipt(&f, &id).await, None);
    assert_eq!(
        ticket_line(&office_page(&server, &f, &id).await),
        STOPPED_LINE
    );
    let stops = logs
        .lock()
        .unwrap()
        .iter()
        .filter(|message| message.as_str() == STOPPED_LOG)
        .count();
    assert_eq!(stops, 1);

    server.set(Smtp::Accept);
    real_pass(&server, &f.registry).await;
    assert_eq!(server.accepted(), 0);
    assert_eq!(office_post(&server, &f, &id, "resend").await.status(), 303);
    assert_eq!(server.accepted(), 1);
    assert!(receipt(&f, &id).await.is_some());
    assert_eq!(
        ticket_line(&office_page(&server, &f, &id).await),
        "Accepted by the mail server."
    );
}

#[tokio::test]
#[serial]
async fn a_stale_sender_does_not_release_the_claim_of_the_next_sender() {
    let server = server(Smtp::StallAcceptance).await;
    let f = fixture(&server.ctx).await;
    let id = unsent_order(&f, "buyer@example.com", orders::now()).await;

    // Sender A stalls in SMTP. Its claim ends (clock shortcut) and sender B takes the next one.
    let a = spawned_pass(&server);
    connections_reach(&server, 1).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    a_day_later(&f, &id).await;
    let b = spawned_pass(&server);
    connections_reach(&server, 2).await;
    assert_eq!(email_row(&f, &id).await.attempts, 2);

    // A reaches its send bound and writes its result while B is still inside its claim.
    a.await.unwrap();
    assert!(!b.is_finished());
    let row = email_row(&f, &id).await;
    assert_eq!((row.state.as_str(), row.attempts), ("Pending", 2));
    assert!(row.claim_until.unwrap() > orders::now());
    // A third sender, as a repeated payment webhook calls it, gets no claim and sends nothing.
    tokio::time::timeout(
        Duration::from_secs(1),
        crabgate::controllers::checkout::send_order_email(
            &server.ctx,
            &f.registry,
            &f.organizer,
            &f.db,
            &id,
        ),
    )
    .await
    .expect("a third sender started a send inside the claim of B")
    .unwrap();
    assert_eq!(server.connections(), 2);

    b.await.unwrap();
    let row = email_row(&f, &id).await;
    assert_eq!(
        (row.state.as_str(), row.attempts, row.claim_until),
        ("Pending", 2, None)
    );
    assert_eq!(
        ticket_line(&office_page(&server, &f, &id).await),
        "Mail acceptance not confirmed after 2 of 8 automatic attempts. CrabGate will try again."
    );
}

#[tokio::test]
#[serial]
async fn acceptance_after_the_claim_ended_is_still_recorded() {
    let server = server(Smtp::SlowAccept(Duration::from_secs(3))).await;
    let f = fixture(&server.ctx).await;
    let id = unsent_order(&f, "buyer@example.com", orders::now()).await;

    // Sender A is slow. Its claim ends (clock shortcut); sender B tries and fails at once.
    let a = spawned_pass(&server);
    connections_reach(&server, 1).await;
    a_day_later(&f, &id).await;
    server.set(Smtp::Unavailable);
    real_pass(&server, &f.registry).await;
    assert_eq!(server.connections(), 2);
    assert!(!a.is_finished());
    let row = email_row(&f, &id).await;
    assert_eq!(
        (row.state.as_str(), row.attempts, row.claim_until),
        ("Pending", 2, None)
    );
    assert_eq!(receipt(&f, &id).await, None);

    // The mail server accepts the message of A after that.
    a.await.unwrap();
    assert_eq!(server.accepted(), 1);
    assert!(receipt(&f, &id).await.is_some());
    assert_eq!(
        ticket_line(&office_page(&server, &f, &id).await),
        "Accepted by the mail server."
    );
    server.set(Smtp::Accept);
    a_day_later(&f, &id).await;
    real_pass(&server, &f.registry).await;
    assert_eq!(server.connections(), 2);
}

#[tokio::test]
#[serial]
async fn a_sender_that_is_slow_before_smtp_does_not_send_on_a_claim_that_is_about_to_end() {
    let server = server(Smtp::Accept).await;
    let f = fixture(&server.ctx).await;
    // Clock shortcut: 55 of the 60 claim seconds pass between the claim and the send.
    f.db.execute_unprepared(
        "CREATE TRIGGER cg063_slow_sender AFTER UPDATE OF email_attempts ON orders BEGIN
         UPDATE orders SET email_claim_until = email_claim_until - 55 WHERE id = NEW.id; END",
    )
    .await
    .unwrap();
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
    f.db.execute_unprepared("DROP TRIGGER cg063_slow_sender")
        .await
        .unwrap();
    assert_eq!(created.status(), 303);
    let location = created.headers()["location"].to_str().unwrap();
    assert!(
        location.ends_with("?ticket_email=unconfirmed"),
        "{location}"
    );
    assert_eq!(server.connections(), 0);
    let id = location
        .split('?')
        .next()
        .unwrap()
        .strip_prefix("/office/orders/")
        .unwrap()
        .to_owned();
    assert_eq!(receipt(&f, &id).await, None);
    assert_eq!(
        ticket_line(&office_page(&server, &f, &id).await),
        "Mail acceptance not confirmed after 1 of 8 automatic attempts. CrabGate will try again."
    );

    real_pass(&server, &f.registry).await;
    assert_eq!(server.connections(), 0);
    a_day_later(&f, &id).await;
    real_pass(&server, &f.registry).await;
    assert_eq!(server.accepted(), 1);
    assert!(receipt(&f, &id).await.is_some());
}

#[tokio::test]
#[serial]
async fn retry_waits_are_stored_and_a_stopped_order_recovers_with_a_manual_resend() {
    let server = server(Smtp::Unavailable).await;
    let f = fixture(&server.ctx).await;
    let id = unsent_order(&f, "buyer@example.com", orders::now()).await;
    let other = registry(&server.ctx);
    let (logs, _guard) = captured_logs();
    for (index, wait) in [300, 1200, 4800, 19200, 76800, 86400, 86400, 86400]
        .into_iter()
        .enumerate()
    {
        let registry = if index % 2 == 0 { &f.registry } else { &other };
        let before = orders::now();
        real_pass(&server, registry).await;
        let after = orders::now();
        assert_eq!(server.connections(), index + 1);
        let row = email_row(&f, &id).await;
        assert_eq!(row.attempts, index as i64 + 1);
        let retry_after = row.retry_after.unwrap();
        assert!(
            (before + wait..=after + wait).contains(&retry_after),
            "attempt {}: stored wait {}",
            index + 1,
            retry_after - before
        );
        assert_eq!(row.claim_until, None);
        // No retry at once, and none 2 seconds before the retry time, with either registry.
        real_pass(&server, &other).await;
        sql(
            &f,
            &format!(
                "UPDATE orders SET email_retry_after = {} WHERE id = ?",
                orders::now() + 2
            ),
            &id,
        )
        .await;
        real_pass(&server, &f.registry).await;
        assert_eq!(server.connections(), index + 1);
        sql(
            &f,
            "UPDATE orders SET email_retry_after = email_retry_after - 2 WHERE id = ?",
            &id,
        )
        .await;
    }
    assert_eq!(email_row(&f, &id).await.state, "Stopped");
    assert_eq!(
        ticket_line(&office_page(&server, &f, &id).await),
        STOPPED_LINE
    );
    let stops = logs
        .lock()
        .unwrap()
        .iter()
        .filter(|message| message.as_str() == STOPPED_LOG)
        .count();
    assert_eq!(stops, 1);

    server.set(Smtp::Accept);
    real_pass(&server, &other).await;
    assert_eq!(server.connections(), 8);
    assert!(buyer_page(&server, &f, &id)
        .await
        .contains("Mail acceptance has not been confirmed"));
    assert_eq!(buyer_resend(&server, &f, &id).await, 303);
    assert_eq!(server.accepted(), 1);
    assert!(receipt(&f, &id).await.is_some());
    assert_eq!(
        ticket_line(&office_page(&server, &f, &id).await),
        "Accepted by the mail server."
    );
    assert!(!buyer_page(&server, &f, &id)
        .await
        .contains("Mail acceptance has not been confirmed"));
    real_pass(&server, &f.registry).await;
    assert_eq!(server.accepted(), 1);
}

#[tokio::test]
#[serial]
async fn office_line_before_an_attempt_and_for_orders_that_get_no_ticket_email() {
    let server = server(Smtp::Unavailable).await;
    let f = fixture(&server.ctx).await;
    let waiting = unsent_order(&f, "waiting@example.com", orders::now()).await;
    assert_eq!(
        ticket_line(&office_page(&server, &f, &waiting).await),
        "Not sent yet. CrabGate will try automatically."
    );

    let (refunded, _) = free_checkout(&server, &f, "refunded@buyer.example").await;
    admin_orders::refund(&f.db, &refunded, None, orders::now())
        .await
        .unwrap();
    let (cancelled_event, kind) = event(&f.db, "Cancelled Test").await;
    let (cancelled, _) = free_checkout_for(
        &server,
        &f,
        &cancelled_event,
        &kind,
        "cancelled@buyer.example",
    )
    .await;
    admin_orders::cancel_event(&f.db, &cancelled_event, orders::now())
        .await
        .unwrap();
    for id in [&refunded, &cancelled] {
        let page = office_page(&server, &f, id).await;
        assert!(!page.contains("<dt>Ticket email</dt>"), "{id}");
    }

    // A resend of an Order that still has automatic attempts left records no receipt.
    let (pending, _) = complimentary(&server, &f, "pending@example.com").await;
    server.set(Smtp::Accept);
    assert_eq!(
        office_post(&server, &f, &pending, "resend").await.status(),
        303
    );
    assert_eq!(server.accepted(), 1);
    assert_eq!(receipt(&f, &pending).await, None);
    assert_eq!(
        ticket_line(&office_page(&server, &f, &pending).await),
        "Mail acceptance not confirmed after 1 of 8 automatic attempts. CrabGate will try again."
    );
}

#[tokio::test]
#[serial]
async fn new_buyer_gets_the_ticket_email_behind_200_old_undeliverable_orders() {
    let server = server(Smtp::Accept).await;
    let f = fixture(&server.ctx).await;
    let cipher = f.registry.cipher(&f.organizer).unwrap();
    let old = orders::now() - 30 * DAY;
    for index in 0..100 {
        unsent_order(&f, &format!("refused-{index}@example.com"), old + index).await;
    }
    for index in 100..200 {
        let id = unsent_order(&f, &format!("gone-{index}@example.com"), old + index).await;
        // The stored form of an Order that was anonymized before its Ticket email was accepted.
        f.db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE orders SET buyer_email_encrypted = ? WHERE id = ?",
            [cipher.encrypt("buyer_email", "").unwrap().into(), id.into()],
        ))
        .await
        .unwrap();
    }

    server.set(Smtp::Unavailable);
    let (id, confirmation) = free_checkout(&server, &f, "buyer@example.com").await;
    assert!(confirmation.contains("Mail acceptance has not been confirmed"));
    assert_eq!(receipt(&f, &id).await, None);
    assert_eq!(server.accepted(), 0);

    server.set(Smtp::Accept);
    pass(&server, &f.registry, 300).await;
    pass(&server, &f.registry, 600).await;
    let messages = server.messages();
    println!(
        "two passes after recovery: accepted={}, refused recipients={}",
        messages.len(),
        server.refused_recipients()
    );
    assert_eq!(messages.len(), 1);
    assert!(messages[0].contains("To: buyer@example.com"));
    assert!(messages[0].contains(&format!("/orders/{id}")));
    assert!(receipt(&f, &id).await.is_some());
    assert!(!buyer_page(&server, &f, &id)
        .await
        .contains("Mail acceptance has not been confirmed"));

    for _ in 0..10 {
        pass(&server, &f.registry, 30 * DAY).await;
    }
    assert_eq!(server.refused_recipients(), 800);
    assert_eq!(server.accepted(), 1);
    let connections = server.connections();
    pass(&server, &f.registry, 60 * DAY).await;
    assert_eq!(server.connections(), connections);
    assert!(server
        .messages()
        .iter()
        .all(|message| !message.contains("gone-")));
    println!("100 refused Orders stopped after 8 attempts each; 100 anonymized Orders got no email; new Buyer accepted=1");
}

#[tokio::test]
#[serial]
async fn refused_recipient_stops_after_eight_attempts_and_manual_resend_still_works() {
    let server = server(Smtp::Accept).await;
    let f = fixture(&server.ctx).await;
    let (id, location) = complimentary(&server, &f, "refused@example.com").await;
    assert!(location.ends_with("?ticket_email=unconfirmed"));
    assert_eq!(server.refused_recipients(), 1);
    assert!(office_page(&server, &f, &id)
        .await
        .contains("Mail acceptance not confirmed after 1 of 8 automatic attempts."));

    for _ in 0..10 {
        pass(&server, &f.registry, 30 * DAY).await;
    }
    assert_eq!(server.refused_recipients(), 8);
    assert_eq!(server.accepted(), 0);
    assert_eq!(receipt(&f, &id).await, None);
    let office = office_page(&server, &f, &id).await;
    assert!(office.contains(
        "Automatic attempts stopped after 8 tries. The mail server refused the last attempt."
    ));
    assert!(!office.contains("Accepted by the mail server."));
    assert!(buyer_page(&server, &f, &id)
        .await
        .contains("Mail acceptance has not been confirmed"));

    server.refuse_mailbox.store(false, Ordering::SeqCst);
    let resent = office_post(&server, &f, &id, "resend").await;
    assert_eq!(resent.status(), 303);
    assert_eq!(
        resent.headers()["location"],
        format!("/office/orders/{id}?tickets_resent=1").as_str()
    );
    assert_eq!(server.accepted(), 1);
    assert!(server.messages()[0].contains("To: refused@example.com"));
    assert!(receipt(&f, &id).await.is_some());
    assert_eq!(
        ticket_line(&office_page(&server, &f, &id).await),
        "Accepted by the mail server."
    );
    for expected in [303, 303, 303, 429] {
        assert_eq!(buyer_resend(&server, &f, &id).await, expected);
    }
    assert_eq!(server.accepted(), 4);
    pass(&server, &f.registry, 60 * DAY).await;
    assert_eq!(server.accepted(), 4);
}

#[tokio::test]
#[serial]
async fn transient_failure_waits_for_its_retry_time_and_recovers_with_a_new_registry() {
    let server = server(Smtp::Unavailable).await;
    let f = fixture(&server.ctx).await;
    let (id, _) = free_checkout(&server, &f, "buyer@example.com").await;
    assert_eq!(server.connections(), 1);
    assert_eq!(receipt(&f, &id).await, None);

    pass(&server, &f.registry, 0).await;
    pass(&server, &f.registry, 240).await;
    assert_eq!(server.connections(), 1);
    pass(&server, &f.registry, 301).await;
    assert_eq!(server.connections(), 2);
    pass(&server, &f.registry, 900).await;
    assert_eq!(server.connections(), 2);
    assert!(office_page(&server, &f, &id)
        .await
        .contains("Mail acceptance not confirmed after 2 of 8 automatic attempts."));

    server.set(Smtp::Accept);
    let reopened = registry(&server.ctx);
    pass(&server, &reopened, 1260).await;
    assert_eq!(server.connections(), 3);
    assert_eq!(server.accepted(), 1);
    assert!(server.messages()[0].contains("To: buyer@example.com"));
    assert!(receipt(&f, &id).await.is_some());
    pass(&server, &reopened, 30 * DAY).await;
    assert_eq!(server.accepted(), 1);
    assert!(office_page(&server, &f, &id)
        .await
        .contains("Accepted by the mail server."));
}

#[tokio::test]
#[serial]
async fn mail_outage_stops_after_eight_attempts_without_a_receipt() {
    let server = server(Smtp::Unavailable).await;
    let f = fixture(&server.ctx).await;
    let (id, _) = complimentary(&server, &f, "buyer@example.com").await;
    for _ in 0..10 {
        pass(&server, &f.registry, 30 * DAY).await;
    }
    assert_eq!(server.connections(), 8);
    assert_eq!(receipt(&f, &id).await, None);
    assert!(office_page(&server, &f, &id)
        .await
        .contains("Automatic attempts stopped after 8 tries. Mail acceptance was not confirmed."));

    server.set(Smtp::Accept);
    pass(&server, &f.registry, 60 * DAY).await;
    assert_eq!(server.accepted(), 0);
    assert_eq!(office_post(&server, &f, &id, "resend").await.status(), 303);
    assert_eq!(server.accepted(), 1);
}

#[tokio::test]
#[serial]
async fn anonymized_order_gets_no_ticket_email_and_keeps_an_earlier_receipt() {
    let server = server(Smtp::Accept).await;
    let f = fixture(&server.ctx).await;
    let (accepted, _) = complimentary(&server, &f, "kept@example.com").await;
    let accepted_at = receipt(&f, &accepted).await;
    assert!(accepted_at.is_some());
    server.set(Smtp::Unavailable);
    let (unsent, _) = complimentary(&server, &f, "erase@example.com").await;
    for id in [&accepted, &unsent] {
        assert_eq!(
            office_post(&server, &f, id, "anonymize").await.status(),
            303
        );
    }

    server.set(Smtp::Accept);
    let connections = server.connections();
    pass(&server, &f.registry, 30 * DAY).await;
    pass(&server, &f.registry, 60 * DAY).await;
    assert_eq!(server.connections(), connections);
    assert_eq!(server.accepted(), 1);
    assert_eq!(receipt(&f, &accepted).await, accepted_at);
    assert_eq!(receipt(&f, &unsent).await, None);
    let office = office_page(&server, &f, &unsent).await;
    assert!(office.contains("Not sent. This order has no recipient email."));
    assert!(!office.contains("erase@example.com"));
    assert!(office_page(&server, &f, &accepted)
        .await
        .contains("Accepted by the mail server."));
    assert_eq!(
        office_post(&server, &f, &unsent, "resend").await.status(),
        400
    );
    assert_eq!(server.connections(), connections);
}

#[tokio::test]
#[serial]
async fn refunded_or_cancelled_orders_get_no_ticket_email_after_recovery() {
    let server = server(Smtp::Unavailable).await;
    let f = fixture(&server.ctx).await;
    let (refunded, _) = free_checkout(&server, &f, "refunded@buyer.example").await;
    admin_orders::refund(&f.db, &refunded, None, orders::now())
        .await
        .unwrap();
    let (cancelled_event, kind) = event(&f.db, "Cancelled Test").await;
    free_checkout_for(
        &server,
        &f,
        &cancelled_event,
        &kind,
        "cancelled@buyer.example",
    )
    .await;
    admin_orders::cancel_event(&f.db, &cancelled_event, orders::now())
        .await
        .unwrap();
    let (paid, _) = free_checkout(&server, &f, "paid@buyer.example").await;

    server.set(Smtp::Accept);
    let connections = server.connections();
    pass(&server, &f.registry, 301).await;
    pass(&server, &f.registry, 30 * DAY).await;
    assert_eq!(server.connections(), connections + 1);
    let messages = server.messages();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].contains("To: paid@buyer.example"));
    assert!(messages[0].contains(&format!("/orders/{paid}")));
    assert_eq!(receipt(&f, &refunded).await, None);
}

#[tokio::test]
#[serial]
async fn stalled_acceptance_is_shown_as_unconfirmed_and_retried() {
    let server = server(Smtp::StallAcceptance).await;
    let f = fixture(&server.ctx).await;
    let (id, location) = complimentary(&server, &f, "buyer@example.com").await;
    assert!(location.ends_with("?ticket_email=unconfirmed"));
    assert_eq!(server.accepted(), 0);
    assert_eq!(receipt(&f, &id).await, None);
    let office = office_page(&server, &f, &id).await;
    assert!(office.contains("Mail acceptance not confirmed after 1 of 8 automatic attempts."));
    assert!(!office.contains("Accepted by the mail server."));
    assert!(buyer_page(&server, &f, &id)
        .await
        .contains("Mail acceptance has not been confirmed"));

    server.set(Smtp::Accept);
    pass(&server, &f.registry, 0).await;
    assert_eq!(server.accepted(), 0);
    pass(&server, &f.registry, 301).await;
    assert_eq!(server.accepted(), 1);
    assert!(receipt(&f, &id).await.is_some());
}

#[tokio::test]
#[serial]
async fn two_passes_at_the_same_time_send_one_ticket_email() {
    let server = server(Smtp::SlowAccept(Duration::from_millis(500))).await;
    let f = fixture(&server.ctx).await;
    let id = unsent_order(&f, "buyer@example.com", orders::now()).await;
    let other = registry(&server.ctx);
    tokio::join!(pass(&server, &f.registry, 0), pass(&server, &other, 0));
    assert_eq!(server.accepted(), 1);
    assert!(receipt(&f, &id).await.is_some());
    pass(&server, &other, 30 * DAY).await;
    assert_eq!(server.accepted(), 1);
}

#[tokio::test]
#[serial]
async fn paid_order_webhook_repeats_count_as_attempts_and_stop_at_the_limit() {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let server = server(Smtp::Accept).await;
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
        "refused@example.com",
        &["Guest".into()],
        false,
        now,
    )
    .await
    .unwrap();
    orders::attach_session(&f.db, &hold.id, "cs_cg063", now)
        .await
        .unwrap();
    let secret = "whsec_cg063_synthetic";
    crabgate::settings::set(&f.db, &cipher, "webhook_secret", secret)
        .await
        .unwrap();
    let body = serde_json::json!({"type":"checkout.session.completed", "data":{"object":{"id":"cs_cg063", "payment_status":"paid", "payment_intent":"pi_cg063", "amount_total":1000, "currency":"eur", "metadata":{"hold_id":hold.id}}}}).to_string();
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    let timestamp = orders::now();
    mac.update(format!("{timestamp}.{body}").as_bytes());
    let signature = format!(
        "t={timestamp},v1={}",
        hex::encode(mac.finalize().into_bytes())
    );
    let mut statuses = Vec::new();
    for _ in 0..10 {
        let response = server
            .client
            .post(format!("{}/webhooks/stripe", server.origin))
            .header("Stripe-Signature", &signature)
            .header("content-type", "application/json")
            .body(body.clone())
            .send()
            .await
            .unwrap();
        statuses.push(response.status().as_u16());
    }
    assert_eq!(statuses, [500, 500, 500, 500, 500, 500, 500, 500, 200, 200]);
    assert_eq!(server.refused_recipients(), 8);
    let id = orders::order_id_for_hold(&f.db, &hold.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt(&f, &id).await, None);
    assert!(office_page(&server, &f, &id).await.contains(
        "Automatic attempts stopped after 8 tries. The mail server refused the last attempt."
    ));
    pass(&server, &f.registry, 30 * DAY).await;
    assert_eq!(server.refused_recipients(), 8);

    server.refuse_mailbox.store(false, Ordering::SeqCst);
    assert_eq!(buyer_resend(&server, &f, &id).await, 303);
    assert_eq!(server.accepted(), 1);
    assert!(server.messages()[0].contains("To: refused@example.com"));
    assert!(receipt(&f, &id).await.is_some());
    assert_eq!(
        ticket_line(&office_page(&server, &f, &id).await),
        "Accepted by the mail server."
    );
    assert!(!buyer_page(&server, &f, &id)
        .await
        .contains("Mail acceptance has not been confirmed"));
}
