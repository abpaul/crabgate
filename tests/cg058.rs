use crabgate::{app::App, hosted, initializers::organizers::OrganizerRuntime};
use loco_rs::{
    app::{AppContext, Hooks},
    boot::StartMode,
    config::WorkerMode,
    environment::Environment,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Default)]
struct Capture {
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
        .prefix("cg058-")
        .tempdir_in(root)
        .unwrap()
        .keep();
    println!("CG-058 synthetic evidence: {}", storage.display());
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
                        let mut reader = BufReader::new(read);
                        if write.write_all(b"220 loopback SMTP\r\n").await.is_err() { return; }
                        let mut data = false;
                        let mut recipient = String::new();
                        let mut body = String::new();
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).await.unwrap() == 0 { break; }
                            let response = if data {
                                if line != ".\r\n" { body.push_str(&line); continue; }
                                state.accepted.lock().unwrap().push((recipient.clone(), std::mem::take(&mut body)));
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

use tower::ServiceExt;
async fn post(f: &Fixture, path: &str, body: String, spoof: usize) -> (u16, String) {
    let response = f
        .router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(path)
                .header("host", "crabgate.com")
                .header("origin", "https://crabgate.com")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("x-forwarded-for", format!("192.0.2.{}", spoof))
                .header("forwarded", format!("for=192.0.2.{}", spoof))
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}
async fn count(f: &Fixture, table: &str) -> i64 {
    f.ctx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            format!("SELECT COUNT(*) AS n FROM {table}"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "n")
        .unwrap()
}

fn files() -> usize {
    std::fs::read_dir(std::env::var_os("CRABGATE_ORGANIZER_DATABASE_DIR").unwrap())
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|v| v == "db")
        })
        .count()
}
async fn state(f: &Fixture, shops: i64, emails: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while f.capture.accepted.lock().unwrap().len() < emails {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    for table in ["organizers", "subscriptions", "memberships"] {
        assert_eq!(count(f, table).await, shops, "{table}");
    }
    assert_eq!(files(), shops as usize);
    assert_eq!(f.capture.accepted.lock().unwrap().len(), emails);
}
#[tokio::test]
#[serial_test::serial]
async fn twelve_public_attempts_share_normalized_recipient_limit_and_recover() {
    let f = fixture().await;
    for i in 0..12 {
        let email = if i % 2 == 0 {
            "%20Victim%40EXAMPLE.test%20"
        } else {
            "victim%40example.test"
        };
        let (status, body) = post(
            &f,
            "/signup",
            format!("name=Arts&subdomain=flood-{i}&email={email}"),
            i,
        )
        .await;
        assert_eq!(status, if i < 3 { 200 } else { 429 });
        assert!(body.contains(if i < 3 {
            "Check your inbox"
        } else {
            "Too many signup attempts"
        }));
        assert!(body.contains("/signup/recover"));
    }
    state(&f, 3, 6).await;
    assert_eq!(count(&f, "members").await, 1);
    assert_eq!(count(&f, "organizer_verification_links").await, 3);
    let (status, body) = post(
        &f,
        "/signup",
        "name=Other&subdomain=other&email=other%40example.test".into(),
        99,
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("Check your inbox"));
    state(&f, 4, 8).await;
    assert_eq!(count(&f, "members").await, 2);
    for _ in 0..4 {
        assert_eq!(
            post(
                &f,
                "/signup/recover",
                "subdomain=flood-0&email=%20VICTIM%40example.test%20".into(),
                100
            )
            .await
            .0,
            200
        );
    }
    state(&f, 4, 11).await;
    assert_eq!(count(&f, "organizer_verification_links").await, 7);
    let message = f.capture.accepted.lock().unwrap()[0].1.clone();
    let token: String = message
        .split("/verify/")
        .nth(1)
        .unwrap()
        .chars()
        .take(32)
        .collect();
    let verification = f
        .router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/verify/{token}"))
                .header("host", "crabgate.com")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(verification.status(), 303);
    let completed = f
        .router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/office/complete-signup/{token}"))
                .header("host", "flood-0.crabgate.com")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(completed.status(), 303);
    assert_eq!(completed.headers()["location"], "/office/setup");
    println!("FINAL original flood: 3 HTTP200 + 9 HTTP429, 3 Shops/files, 6 SMTP; distinct normal recipient accepted; recovery 3 deliveries; verification HTTP303");
}
#[tokio::test]
#[serial_test::serial]
async fn aggregate_budget_bounds_rotating_recipients_without_blocking_recovery() {
    let f = fixture().await;
    for i in 0..20 {
        let body = format!("name=Arts&subdomain=normal-{i}&email=normal-{i}%40example.test");
        let (status, text) = post(&f, "/signup", body, i).await;
        assert_eq!(status, 200);
        assert!(text.contains("Check your inbox"));
    }
    state(&f, 20, 40).await;
    let (status, body) = post(
        &f,
        "/signup",
        "name=Blocked&subdomain=blocked&email=fresh%40example.test".into(),
        201,
    )
    .await;
    assert_eq!(status, 429);
    assert!(body.contains("Too many signup attempts"));
    state(&f, 20, 40).await;
    assert_eq!(
        post(
            &f,
            "/signup/recover",
            "subdomain=normal-0&email=normal-0%40example.test".into(),
            202
        )
        .await
        .0,
        200
    );
    state(&f, 20, 41).await;
    println!("FINAL aggregate: 20 attempts admitted, 21st HTTP429; exhausted signup still allows recovery SMTP");
}
async fn independent(f: &Fixture) -> sea_orm::DatabaseConnection {
    let row = f
        .ctx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA database_list",
        ))
        .await
        .unwrap()
        .unwrap();
    let file: String = row.try_get("", "file").unwrap();
    let mut options = sea_orm::ConnectOptions::new(format!("sqlite://{file}?mode=rw"));
    options.max_connections(1).sqlx_logging(false);
    sea_orm::Database::connect(options).await.unwrap()
}
#[tokio::test]
#[serial_test::serial]
async fn independent_workers_atomically_bound_recipient_before_creation() {
    let f = fixture().await;
    let mut workers = tokio::task::JoinSet::new();
    let gate = Arc::new(tokio::sync::Barrier::new(24));
    for i in 0..24 {
        let db = independent(&f).await;
        let registry = registry(&f.ctx);
        let gate = gate.clone();
        workers.spawn(async move {
            gate.wait().await;
            hosted::public_signup(
                &registry,
                &db,
                "Arts",
                &format!("worker-{i}"),
                if i % 2 == 0 {
                    " Worker@EXAMPLE.test "
                } else {
                    "worker@example.test"
                },
                1_800_000_000,
            )
            .await
        });
    }
    let mut accepted = 0;
    let mut rejected = 0;
    while let Some(result) = workers.join_next().await {
        match result.unwrap() {
            Ok(_) => accepted += 1,
            Err(hosted::HostedError::TooManySignups) => rejected += 1,
            Err(error) => panic!("unexpected worker error: {error}"),
        }
    }
    assert_eq!((accepted, rejected), (3, 21));
    state(&f, 3, 0).await;
    assert_eq!(count(&f, "organizer_verification_links").await, 3);
    println!("FINAL independent connections: 24 contenders, 3 created + 21 denied");
}
#[tokio::test]
#[serial_test::serial]
async fn independent_workers_atomically_bound_invalid_attempts_and_persist_restart() {
    let f = fixture().await;
    let mut workers = tokio::task::JoinSet::new();
    let gate = Arc::new(tokio::sync::Barrier::new(24));
    for _ in 0..24 {
        let db = independent(&f).await;
        let registry = registry(&f.ctx);
        let gate = gate.clone();
        workers.spawn(async move {
            gate.wait().await;
            hosted::public_signup(&registry, &db, "", "invalid", "invalid", 1_800_000_000).await
        });
    }
    let mut invalid = 0;
    let mut limited = 0;
    while let Some(result) = workers.join_next().await {
        match result.unwrap() {
            Err(hosted::HostedError::InvalidSignup) => invalid += 1,
            Err(hosted::HostedError::TooManySignups) => limited += 1,
            _ => panic!("unexpected outcome"),
        }
    }
    assert_eq!((invalid, limited), (20, 4));
    state(&f, 0, 0).await;
    let db = independent(&f).await;
    assert!(matches!(
        hosted::public_signup(
            &registry(&f.ctx),
            &db,
            "Arts",
            "restart",
            "restart@example.test",
            1_800_003_599
        )
        .await,
        Err(hosted::HostedError::TooManySignups)
    ));
    db.close().await.unwrap();
    let db = independent(&f).await;
    assert!(hosted::public_signup(
        &registry(&f.ctx),
        &db,
        "Arts",
        "restart",
        "restart@example.test",
        1_800_003_600
    )
    .await
    .is_ok());
    state(&f, 1, 0).await;
    println!("FINAL invalid contention: 20 invalid + 4 limited; reopened connection denies at 3599s, permits at 3600s");
}
#[tokio::test]
#[serial_test::serial]
async fn recipient_window_survives_reopen_and_expires_at_exact_boundary() {
    let f = fixture().await;
    for i in 0..3 {
        hosted::public_signup(
            &registry(&f.ctx),
            &f.ctx.db,
            "Arts",
            &format!("first-{i}"),
            "same@example.test",
            1_800_000_000,
        )
        .await
        .unwrap();
    }
    process_probe(&f, "denied", 1_800_003_599).await;
    state(&f, 3, 0).await;
    process_probe(&f, "accepted", 1_800_003_600).await;
    state(&f, 4, 0).await;
}
async fn process_probe(f: &Fixture, outcome: &str, now: i64) {
    let row = f
        .ctx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA database_list",
        ))
        .await
        .unwrap()
        .unwrap();
    let file: String = row.try_get("", "file").unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "restart_process_probe",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("CG058_RESTART_DB", &file)
        .env("CG058_RESTART_OUTCOME", outcome)
        .env("CG058_RESTART_NOW", now.to_string())
        .output()
        .unwrap();
    let log = std::path::Path::new(&file)
        .parent()
        .unwrap()
        .join(format!("restart-{outcome}.log"));
    std::fs::write(&log, [&result.stdout[..], &result.stderr[..]].concat()).unwrap();
    assert!(
        result.status.success(),
        "restart child failed; see {}",
        log.display()
    );
}
#[tokio::test]
#[ignore = "invoked in a fresh process by recipient_window_survives_reopen_and_expires_at_exact_boundary"]
async fn restart_process_probe() {
    let file = std::env::var("CG058_RESTART_DB").unwrap();
    let now: i64 = std::env::var("CG058_RESTART_NOW").unwrap().parse().unwrap();
    let outcome = std::env::var("CG058_RESTART_OUTCOME").unwrap();
    let mut config = App::load_config(&Environment::Test).await.unwrap().database;
    config.uri = format!("sqlite://{file}?mode=rw");
    config.min_connections = 1;
    config.max_connections = 1;
    let db = loco_rs::db::connect(&config).await.unwrap();
    let registry = crabgate::organizers::OrganizerRegistry::new(
        db.clone(),
        config,
        crabgate::organizers::Edition::Hosted,
        "crabgate.com".into(),
        std::env::var_os("CRABGATE_ORGANIZER_DATABASE_DIR")
            .unwrap()
            .into(),
        crabgate::crypto::MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .unwrap(),
    )
    .unwrap();
    let result = hosted::public_signup(
        &registry,
        &db,
        "Arts",
        "restart-process",
        " SAME@EXAMPLE.test ",
        now,
    )
    .await;
    match outcome.as_str() {
        "denied" => assert!(matches!(result, Err(hosted::HostedError::TooManySignups))),
        "accepted" => assert_eq!(result.unwrap().organizer.subdomain, "restart-process"),
        _ => panic!("unknown probe outcome"),
    }
    println!("Fresh process persisted recipient window: {outcome} at {now}");
}

#[tokio::test]
#[serial_test::serial]
async fn invalid_public_attempts_are_bounded_without_provisioning() {
    let f = fixture().await;
    for i in 0..21 {
        let (status, body) = post(
            &f,
            "/signup",
            format!("name=&subdomain=invalid-{i}&email=invalid-{i}"),
            i,
        )
        .await;
        assert_eq!(status, if i < 20 { 200 } else { 429 });
        assert!(body.contains(if i < 20 {
            "Enter an organizer name"
        } else {
            "Too many signup attempts"
        }));
    }
    state(&f, 0, 0).await;
    assert_eq!(count(&f, "members").await, 0);
    assert_eq!(count(&f, "organizer_verification_links").await, 0);
}
#[tokio::test]
#[serial_test::serial]
async fn control_upgrade_preserves_existing_shop_and_missing_limits_fail_closed() {
    use migration::MigratorTrait;
    let f = fixture().await;
    hosted::signup(
        &registry(&f.ctx),
        &f.ctx.db,
        "Existing",
        "existing",
        "existing@example.test",
        1_800_000_000,
    )
    .await
    .unwrap();
    let migrations = migration::Migrator::migrations();
    let migration_index = migrations
        .iter()
        .position(|migration| migration.name() == "m20261009_000028_signup_rate_limits")
        .expect("target migration is registered");
    let rollback_steps = u32::try_from(migrations.len() - migration_index).unwrap();
    migration::Migrator::down(&f.ctx.db, Some(rollback_steps))
        .await
        .unwrap();
    assert!(matches!(
        hosted::public_signup(
            &registry(&f.ctx),
            &f.ctx.db,
            "Blocked",
            "blocked",
            "blocked@example.test",
            1_800_000_001
        )
        .await,
        Err(hosted::HostedError::Database(_))
    ));
    state(&f, 1, 0).await;
    migration::Migrator::up(&f.ctx.db, None).await.unwrap();
    let (status, body) = post(
        &f,
        "/signup",
        "name=New&subdomain=new&email=new%40example.test".into(),
        1,
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("Check your inbox"));
    state(&f, 2, 2).await;
    assert_eq!(count(&f, "organizer_verification_links").await, 2);
}
