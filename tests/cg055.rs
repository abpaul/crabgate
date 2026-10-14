use crabgate::{
    app::App,
    hosted, hosted_jobs,
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

const DAY: i64 = 86_400;
const LAPSE: i64 = 1_800_000_000;

#[derive(Default)]
struct Capture {
    refused: AtomicBool,
    refuse_second: AtomicBool,
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
        .prefix("cg055-")
        .tempdir_in(root)
        .unwrap()
        .keep();
    println!("CG-055 synthetic evidence: {}", storage.display());
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
                            let response = if data {
                                if line != ".\r\n" { body.push_str(&line); continue; }
                                if state.hold.swap(false, Ordering::SeqCst) {
                                    state.entered.notify_one();
                                    state.release.notified().await;
                                }
                                state.accepted.lock().unwrap().push((recipient.clone(), std::mem::take(&mut body)));
                                state.acceptance.notify_one();
                                data = false;
                                b"250 accepted\r\n".as_slice()
                            } else if line.starts_with("RCPT TO:") {
                                recipient = line.trim().trim_start_matches("RCPT TO:").trim_matches(['<', '>']).to_owned();
                                if state.refuse_second.load(Ordering::SeqCst) && recipient == "b@example.test" {
                                    b"550 synthetic recipient refusal\r\n".as_slice()
                                } else { b"250 OK\r\n".as_slice() }
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
async fn organizer(f: &Fixture) -> crabgate::organizers::Organizer {
    let o = registry(&f.ctx).create("Arts", "arts").await.unwrap();
    for email in ["a@example.test", "b@example.test"] {
        members::add_member(&f.ctx.db, &o.id, email, Role::Admin)
            .await
            .unwrap();
    }
    f.ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO subscriptions (id, organizer_id, status, lapsed_at) VALUES (?, ?, 'Lapsed', ?)",
        [format!("sub-{}", o.id).into(), o.id.clone().into(), LAPSE.into()])).await.unwrap();
    o
}
async fn run(f: &Fixture, now: i64) -> loco_rs::Result<()> {
    hosted_jobs::run(&f.ctx, &registry(&f.ctx), now).await
}
async fn accepted_receipts(f: &Fixture, id: &str) -> i64 {
    f.ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT COUNT(*) AS count FROM hosted_lifecycle_recipients WHERE organizer_id = ? AND sent_at IS NOT NULL",
        [id.into()])).await.unwrap().unwrap().try_get("", "count").unwrap()
}

#[tokio::test]
#[serial_test::serial]
async fn two_lapses_each_warn_all_admins_at_all_three_thresholds_then_delete() {
    let f = fixture().await;
    let o = organizer(&f).await;
    let other = registry(&f.ctx).create("Other", "other").await.unwrap();
    members::add_member(&f.ctx.db, &other.id, "other@example.test", Role::Admin)
        .await
        .unwrap();
    members::add_member(&f.ctx.db, &o.id, "staff@example.test", Role::DoorStaff)
        .await
        .unwrap();
    f.ctx
        .db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO members(id,email) VALUES ('duplicate',' A@EXAMPLE.TEST ');
         INSERT INTO memberships(member_id,organizer_id,role) VALUES ('duplicate',?,'Admin')",
            [o.id.clone().into()],
        ))
        .await
        .unwrap();
    for (lapse, generation) in [(LAPSE, 1), (LAPSE + 400 * DAY, 2)] {
        if generation == 2 {
            hosted::subscription_active(&f.ctx.db, &o.id, "cus", "sub", lapse - DAY, 10)
                .await
                .unwrap();
            assert!(hosted::due_lifecycle(&f.ctx.db, lapse - DAY)
                .await
                .unwrap()
                .is_empty());
            hosted::subscription_cancelled(&f.ctx.db, "sub", lapse, 11)
                .await
                .unwrap();
        }
        for (offset, days) in [(60, 30), (83, 7), (89, 1)] {
            assert_eq!(
                hosted::due_lifecycle(&f.ctx.db, lapse + offset * DAY)
                    .await
                    .unwrap(),
                vec![hosted::LifecycleAction::Warn {
                    organizer_id: o.id.clone(),
                    days_left: days,
                    lapse_generation: generation
                }]
            );
            run(&f, lapse + offset * DAY).await.unwrap();
            run(&f, lapse + offset * DAY).await.unwrap();
            assert!(hosted::due_lifecycle(&f.ctx.db, lapse + offset * DAY)
                .await
                .unwrap()
                .is_empty());
        }
        assert_eq!(accepted_receipts(&f, &o.id).await, generation * 6);
    }
    {
        let mail = f.capture.accepted.lock().unwrap();
        assert_eq!(mail.len(), 12);
        for email in ["a@example.test", "b@example.test"] {
            assert_eq!(mail.iter().filter(|(to, _)| to == email).count(), 6);
            for days in [30, 7, 1] {
                let literal = format!("{days} day");
                assert_eq!(
                    mail.iter()
                        .filter(|(to, body)| to == email && body.contains(&literal))
                        .count(),
                    2
                );
            }
        }
    }
    run(&f, LAPSE + 490 * DAY).await.unwrap();
    assert_eq!(
        hosted::access_state(&f.ctx.db, &o.id)
            .await
            .unwrap()
            .subscription_status,
        "Deleted"
    );
    assert!(!std::path::Path::new(&o.db_path).exists());
    assert!(std::path::Path::new(&other.db_path).exists());
    assert_eq!(f.capture.accepted.lock().unwrap().len(), 12);
}

#[tokio::test]
#[serial_test::serial]
async fn background_async_refusal_has_no_accepted_receipts_and_retries() {
    let f = fixture().await;
    let o = organizer(&f).await;
    f.capture.refused.store(true, Ordering::SeqCst);
    let failure = run(&f, LAPSE + 60 * DAY).await.unwrap_err().to_string();
    assert!(failure.contains("1 lifecycle actions failed"));
    assert_eq!(accepted_receipts(&f, &o.id).await, 0);
    assert!(f.capture.accepted.lock().unwrap().is_empty());
    let row = f.ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT COUNT(*) AS count FROM hosted_lifecycle_recipients WHERE sent_at IS NULL AND attempts = 1 AND last_error IS NOT NULL")).await.unwrap().unwrap();
    assert_eq!(row.try_get::<i64>("", "count").unwrap(), 2);
    f.capture.refused.store(false, Ordering::SeqCst);
    run(&f, LAPSE + 60 * DAY).await.unwrap();
    assert_eq!(accepted_receipts(&f, &o.id).await, 2);
    assert_eq!(f.capture.accepted.lock().unwrap().len(), 2);
}

#[tokio::test]
#[serial_test::serial]
async fn partial_refusal_retries_only_failed_current_admin_and_retains_error() {
    let f = fixture().await;
    let o = organizer(&f).await;
    f.capture.refuse_second.store(true, Ordering::SeqCst);
    assert!(run(&f, LAPSE + 60 * DAY)
        .await
        .unwrap_err()
        .to_string()
        .contains("b@example.test"));
    assert_eq!(accepted_receipts(&f, &o.id).await, 1);
    assert_eq!(f.capture.accepted.lock().unwrap()[0].0, "a@example.test");
    f.capture.refuse_second.store(false, Ordering::SeqCst);
    run(&f, LAPSE + 60 * DAY).await.unwrap();
    assert_eq!(accepted_receipts(&f, &o.id).await, 2);
    let rows = f
        .ctx
        .db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT email, attempts, last_error FROM hosted_lifecycle_recipients ORDER BY email",
        ))
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i64>("", "attempts").unwrap(), 1);
    assert_eq!(rows[1].try_get::<i64>("", "attempts").unwrap(), 2);
    assert!(rows[1]
        .try_get::<String>("", "last_error")
        .unwrap()
        .contains("550"));
    members::add_member(&f.ctx.db, &o.id, "c@example.test", Role::Admin)
        .await
        .unwrap();
    f.ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE memberships SET role = 'Door Staff' WHERE organizer_id = ? AND member_id IN (SELECT id FROM members WHERE email = 'b@example.test')",
        [o.id.clone().into()])).await.unwrap();
    run(&f, LAPSE + 83 * DAY).await.unwrap();
    let mail = f.capture.accepted.lock().unwrap();
    assert_eq!(
        mail.iter().map(|(to, _)| to.as_str()).collect::<Vec<_>>(),
        vec![
            "a@example.test",
            "b@example.test",
            "a@example.test",
            "c@example.test"
        ]
    );
}

#[tokio::test]
#[serial_test::serial]
async fn duplicate_jobs_claim_once_and_reactivation_commits_during_send() {
    let f = fixture().await;
    let o = organizer(&f).await;
    let (a, b) = tokio::join!(run(&f, LAPSE + 60 * DAY), run(&f, LAPSE + 60 * DAY));
    a.unwrap();
    b.unwrap();
    assert_eq!(accepted_receipts(&f, &o.id).await, 2);
    assert_eq!(f.capture.accepted.lock().unwrap().len(), 2);
    f.capture.hold.store(true, Ordering::SeqCst);
    let (job, ()) = tokio::join!(run(&f, LAPSE + 83 * DAY), async {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            f.capture.entered.notified(),
        )
        .await
        .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            hosted::subscription_active(&f.ctx.db, &o.id, "cus", "sub", LAPSE + 83 * DAY, 10),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(accepted_receipts(&f, &o.id).await, 2);
        assert_eq!(f.capture.accepted.lock().unwrap().len(), 2);
        f.capture.release.notify_one();
    });
    job.unwrap();
    let count = f.capture.accepted.lock().unwrap().len();
    assert_eq!(count, 3);
    assert_eq!(
        hosted::access_state(&f.ctx.db, &o.id)
            .await
            .unwrap()
            .subscription_status,
        "Active"
    );
    run(&f, LAPSE + 89 * DAY).await.unwrap();
    run(&f, LAPSE + 90 * DAY).await.unwrap();
    assert_eq!(f.capture.accepted.lock().unwrap().len(), count);
    assert!(std::path::Path::new(&o.db_path).exists());
}

#[tokio::test]
#[serial_test::serial]
async fn migration_preserves_legacy_evidence_without_suppressing_current_or_new_lapse() {
    use migration::MigratorTrait;
    let f = fixture().await;
    let o = organizer(&f).await;
    let migrations = migration::Migrator::migrations();
    let migration_index = migrations
        .iter()
        .position(|migration| migration.name() == "m20261009_000025_lifecycle_recipients")
        .expect("target migration is registered");
    let rollback_steps = u32::try_from(migrations.len() - migration_index).unwrap();
    migration::Migrator::down(&f.ctx.db, Some(rollback_steps))
        .await
        .unwrap();
    f.ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO hosted_lifecycle_notices(organizer_id,kind,sent_at) VALUES (?,'delete-30',?)",
        [o.id.clone().into(), (LAPSE + 60 * DAY).into()])).await.unwrap();
    migration::Migrator::up(&f.ctx.db, None).await.unwrap();
    run(&f, LAPSE + 60 * DAY).await.unwrap();
    assert_eq!(accepted_receipts(&f, &o.id).await, 2);
    hosted::subscription_active(&f.ctx.db, &o.id, "cus", "sub", LAPSE + 60 * DAY, 1)
        .await
        .unwrap();
    hosted::subscription_lapsed(&f.ctx.db, &o.id, LAPSE)
        .await
        .unwrap();
    run(&f, LAPSE + 60 * DAY).await.unwrap();
    assert_eq!(accepted_receipts(&f, &o.id).await, 4);
    let row = f.ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT sent_at FROM hosted_lifecycle_notices WHERE organizer_id = ? AND kind = 'delete-30'",
        [o.id.clone().into()])).await.unwrap().unwrap();
    assert_eq!(row.try_get::<i64>("", "sent_at").unwrap(), LAPSE + 60 * DAY);
}

async fn wait_acceptances(f: &Fixture, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let notified = f.capture.acceptance.notified();
            if f.capture.accepted.lock().unwrap().len() >= count {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn slow_warning_keeps_other_tenant_public_sign_in_available() {
    use tower::ServiceExt;
    let f = fixture().await;
    let o = organizer(&f).await;
    let other = registry(&f.ctx).create("Other", "other").await.unwrap();
    members::add_member(&f.ctx.db, &other.id, "other@example.test", Role::Admin)
        .await
        .unwrap();
    let before = f
        .router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/office/sign-in")
                .header("host", "other.crabgate.com")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(before.status(), 200);
    let busy = f
        .ctx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA busy_timeout",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(busy.try_get::<i64>("", "timeout").unwrap(), 5000);
    f.capture.hold.store(true, Ordering::SeqCst);
    let (job, ()) = tokio::join!(run(&f, LAPSE + 60 * DAY), async {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            f.capture.entered.notified(),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5500)).await;
        let started = std::time::Instant::now();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            f.router.clone().oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/office/sign-in")
                    .header("host", "other.crabgate.com")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(axum::body::Body::from("email=other%40example.test"))
                    .unwrap(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), 303);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(accepted_receipts(&f, &o.id).await, 0);
        assert!(f
            .capture
            .accepted
            .lock()
            .unwrap()
            .iter()
            .all(|(email, _)| email != "a@example.test"));
        let row = f
            .ctx
            .db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM magic_links WHERE organizer_id = ?",
                [other.id.clone().into()],
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "count").unwrap(), 1);
        println!("CORRECTION cross-tenant POST sign-in status=303 elapsed_ms={} links=1; warning DATA held over 5.5s, no warning receipt yet", started.elapsed().as_millis());
        f.capture.release.notify_one();
    });
    job.unwrap();
    assert_eq!(accepted_receipts(&f, &o.id).await, 2);
}

#[tokio::test]
#[serial_test::serial]
async fn membership_writes_during_send_suppress_removed_recipient_and_retry_new_admin() {
    let f = fixture().await;
    let o = organizer(&f).await;
    f.capture.hold.store(true, Ordering::SeqCst);
    let (job, ()) = tokio::join!(run(&f, LAPSE + 60 * DAY), async {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            f.capture.entered.notified(),
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let b = members::list_members(&f.ctx.db, &o.id)
                .await
                .unwrap()
                .into_iter()
                .find(|m| m.email == "b@example.test")
                .unwrap();
            assert!(members::remove_member(&f.ctx.db, &o.id, &b.id)
                .await
                .unwrap());
            members::add_member(&f.ctx.db, &o.id, "c@example.test", Role::Admin)
                .await
                .unwrap();
            let other = registry(&f.ctx)
                .create("Concurrent", "concurrent")
                .await
                .unwrap();
            members::add_member(&f.ctx.db, &other.id, "staff@example.test", Role::DoorStaff)
                .await
                .unwrap();
        })
        .await
        .unwrap();
        assert!(f.capture.accepted.lock().unwrap().is_empty());
        f.capture.release.notify_one();
    });
    job.unwrap();
    assert_eq!(accepted_receipts(&f, &o.id).await, 1);
    run(&f, LAPSE + 60 * DAY).await.unwrap();
    assert_eq!(
        f.capture
            .accepted
            .lock()
            .unwrap()
            .iter()
            .map(|(email, _)| email.clone())
            .collect::<Vec<_>>(),
        vec!["a@example.test", "c@example.test"]
    );
}

async fn expire_claims(f: &Fixture) {
    f.ctx
        .db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE hosted_lifecycle_recipients SET claim_until = ? WHERE claim_token IS NOT NULL",
            [(chrono::Utc::now().timestamp() - 1).into()],
        ))
        .await
        .unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn dropped_send_retains_claim_and_attempt_until_expiry_then_recovers_with_uncertainty() {
    let f = fixture().await;
    let o = organizer(&f).await;
    f.capture.hold.store(true, Ordering::SeqCst);
    {
        let job = run(&f, LAPSE + 60 * DAY);
        tokio::pin!(job);
        tokio::select! { _ = &mut job => panic!("warning must wait"), _ = f.capture.entered.notified() => {} }
    }
    members::add_member(&f.ctx.db, &o.id, "staff@example.test", Role::DoorStaff)
        .await
        .unwrap();
    let row = f.ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT attempts, claim_token, claim_until, sent_at FROM hosted_lifecycle_recipients WHERE email = 'a@example.test'")).await.unwrap().unwrap();
    assert_eq!(row.try_get::<i64>("", "attempts").unwrap(), 1);
    assert!(row.try_get::<String>("", "claim_token").unwrap().len() > 10);
    assert!(row.try_get::<i64>("", "claim_until").unwrap() >= chrono::Utc::now().timestamp() + 50);
    assert_eq!(row.try_get::<Option<i64>>("", "sent_at").unwrap(), None);
    run(&f, LAPSE + 60 * DAY).await.unwrap();
    assert_eq!(accepted_receipts(&f, &o.id).await, 1);
    assert_eq!(
        f.capture
            .accepted
            .lock()
            .unwrap()
            .iter()
            .map(|(to, _)| to.clone())
            .collect::<Vec<_>>(),
        vec!["b@example.test"]
    );
    f.capture.release.notify_one();
    wait_acceptances(&f, 2).await;
    expire_claims(&f).await;
    run(&f, LAPSE + 60 * DAY).await.unwrap();
    let row = f.ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT attempts, last_error, claim_token FROM hosted_lifecycle_recipients WHERE email = 'a@example.test'")).await.unwrap().unwrap();
    assert_eq!(row.try_get::<i64>("", "attempts").unwrap(), 2);
    assert!(row
        .try_get::<String>("", "last_error")
        .unwrap()
        .contains("claim expired; SMTP acceptance may be uncertain"));
    assert_eq!(
        row.try_get::<Option<String>>("", "claim_token").unwrap(),
        None
    );
    assert_eq!(accepted_receipts(&f, &o.id).await, 2);
    assert_eq!(f.capture.accepted.lock().unwrap().len(), 3);
}

#[tokio::test]
#[serial_test::serial]
async fn superseded_token_cannot_record_old_send_completion() {
    let f = fixture().await;
    let o = organizer(&f).await;
    f.capture.hold.store(true, Ordering::SeqCst);
    let (old_job, ()) = tokio::join!(run(&f, LAPSE + 60 * DAY), async {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            f.capture.entered.notified(),
        )
        .await
        .unwrap();
        expire_claims(&f).await;
        run(&f, LAPSE + 60 * DAY).await.unwrap();
        assert_eq!(accepted_receipts(&f, &o.id).await, 2);
        f.capture.release.notify_one();
    });
    assert!(old_job
        .unwrap_err()
        .to_string()
        .contains("claim is no longer owned; SMTP acceptance may be uncertain"));
    assert_eq!(accepted_receipts(&f, &o.id).await, 2);
    assert_eq!(f.capture.accepted.lock().unwrap().len(), 3);
    let row = f
        .ctx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT attempts FROM hosted_lifecycle_recipients WHERE email = 'a@example.test'",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<i64>("", "attempts").unwrap(), 2);
}

#[tokio::test]
#[serial_test::serial]
async fn in_flight_old_lapse_receipt_cannot_suppress_new_lapse() {
    let f = fixture().await;
    let o = organizer(&f).await;
    f.capture.hold.store(true, Ordering::SeqCst);
    let (old_job, ()) = tokio::join!(run(&f, LAPSE + 60 * DAY), async {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            f.capture.entered.notified(),
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            hosted::subscription_active(&f.ctx.db, &o.id, "cus", "sub", LAPSE + 60 * DAY, 1)
                .await
                .unwrap();
            hosted::subscription_lapsed(&f.ctx.db, &o.id, LAPSE)
                .await
                .unwrap();
        })
        .await
        .unwrap();
        run(&f, LAPSE + 60 * DAY).await.unwrap();
        assert_eq!(accepted_receipts(&f, &o.id).await, 2);
        f.capture.release.notify_one();
    });
    old_job.unwrap();
    assert_eq!(accepted_receipts(&f, &o.id).await, 3);
    let row = f.ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT COUNT(*) AS count FROM hosted_lifecycle_recipients WHERE lapse_generation = 1 AND email = 'b@example.test'")).await.unwrap().unwrap();
    assert_eq!(row.try_get::<i64>("", "count").unwrap(), 0);
    run(&f, LAPSE + 60 * DAY).await.unwrap();
    assert_eq!(f.capture.accepted.lock().unwrap().len(), 3);
}

#[tokio::test]
#[serial_test::serial]
async fn timeout_after_data_retains_error_and_retry_can_duplicate() {
    let f = fixture().await;
    let o = organizer(&f).await;
    f.capture.hold.store(true, Ordering::SeqCst);
    let start = std::time::Instant::now();
    assert!(run(&f, LAPSE + 60 * DAY)
        .await
        .unwrap_err()
        .to_string()
        .contains("acceptance is uncertain after timeout"));
    assert!((9500..13000).contains(&start.elapsed().as_millis()));
    assert_eq!(accepted_receipts(&f, &o.id).await, 1);
    let row = f.ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT last_error, claim_token FROM hosted_lifecycle_recipients WHERE email = 'a@example.test'")).await.unwrap().unwrap();
    assert!(row
        .try_get::<String>("", "last_error")
        .unwrap()
        .contains("acceptance is uncertain after timeout"));
    assert_eq!(
        row.try_get::<Option<String>>("", "claim_token").unwrap(),
        None
    );
    f.capture.release.notify_one();
    wait_acceptances(&f, 2).await;
    run(&f, LAPSE + 60 * DAY).await.unwrap();
    assert_eq!(accepted_receipts(&f, &o.id).await, 2);
    assert_eq!(f.capture.accepted.lock().unwrap().len(), 3);
}

#[tokio::test]
#[serial_test::serial]
async fn receipt_write_failure_after_250_retains_claim_and_recovers_after_expiry() {
    let f = fixture().await;
    let o = organizer(&f).await;
    f.ctx.db.execute_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "CREATE TRIGGER refuse_receipt BEFORE UPDATE ON hosted_lifecycle_recipients WHEN NEW.sent_at IS NOT NULL BEGIN SELECT RAISE(ABORT,'synthetic receipt failure'); END")).await.unwrap();
    assert!(run(&f, LAPSE + 60 * DAY)
        .await
        .unwrap_err()
        .to_string()
        .contains("warning receipt write failed; SMTP acceptance may be uncertain"));
    assert_eq!(accepted_receipts(&f, &o.id).await, 0);
    assert_eq!(f.capture.accepted.lock().unwrap().len(), 2);
    let row = f.ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT COUNT(*) AS count FROM hosted_lifecycle_recipients WHERE attempts = 1 AND claim_token IS NOT NULL AND sent_at IS NULL")).await.unwrap().unwrap();
    assert_eq!(row.try_get::<i64>("", "count").unwrap(), 2);
    f.ctx
        .db
        .execute_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "DROP TRIGGER refuse_receipt",
        ))
        .await
        .unwrap();
    run(&f, LAPSE + 60 * DAY).await.unwrap();
    assert_eq!(f.capture.accepted.lock().unwrap().len(), 2);
    expire_claims(&f).await;
    run(&f, LAPSE + 60 * DAY).await.unwrap();
    assert_eq!(accepted_receipts(&f, &o.id).await, 2);
    assert_eq!(f.capture.accepted.lock().unwrap().len(), 4);
    let row = f.ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT COUNT(*) AS count FROM hosted_lifecycle_recipients WHERE attempts = 2 AND last_error LIKE '%claim expired%' AND claim_token IS NULL")).await.unwrap().unwrap();
    assert_eq!(row.try_get::<i64>("", "count").unwrap(), 2);
}

#[tokio::test]
#[serial_test::serial]
async fn claim_migration_preserves_existing_accepted_and_failed_receipts() {
    use migration::MigratorTrait;
    let f = fixture().await;
    let o = organizer(&f).await;
    let migrations = migration::Migrator::migrations();
    let migration_index = migrations
        .iter()
        .position(|migration| migration.name() == "m20261009_000026_lifecycle_claims")
        .expect("target migration is registered");
    let rollback_steps = u32::try_from(migrations.len() - migration_index).unwrap();
    migration::Migrator::down(&f.ctx.db, Some(rollback_steps))
        .await
        .unwrap();
    f.ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO hosted_lifecycle_recipients(organizer_id,lapse_generation,days_left,email,attempts,sent_at,last_error)
         VALUES (?,1,30,'a@example.test',3,?,'retained failure'), (?,1,30,'b@example.test',2,NULL,'550 historical failure')",
        [o.id.clone().into(), (LAPSE + 60 * DAY).into(), o.id.clone().into()])).await.unwrap();
    migration::Migrator::up(&f.ctx.db, None).await.unwrap();
    run(&f, LAPSE + 60 * DAY).await.unwrap();
    assert_eq!(
        f.capture
            .accepted
            .lock()
            .unwrap()
            .iter()
            .map(|(to, _)| to.clone())
            .collect::<Vec<_>>(),
        vec!["b@example.test"]
    );
    let rows = f.ctx.db.query_all_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT attempts, last_error, claim_token FROM hosted_lifecycle_recipients ORDER BY email")).await.unwrap();
    assert_eq!(rows[0].try_get::<i64>("", "attempts").unwrap(), 3);
    assert_eq!(
        rows[0].try_get::<String>("", "last_error").unwrap(),
        "retained failure"
    );
    assert_eq!(rows[1].try_get::<i64>("", "attempts").unwrap(), 3);
    assert_eq!(
        rows[1].try_get::<String>("", "last_error").unwrap(),
        "550 historical failure"
    );
    assert_eq!(
        rows[1]
            .try_get::<Option<String>>("", "claim_token")
            .unwrap(),
        None
    );
    assert!(f
        .ctx
        .db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA foreign_key_check"
        ))
        .await
        .unwrap()
        .is_empty());
    let scope = registry(&f.ctx).resolve("arts.crabgate.com").await.unwrap();
    assert!(scope.db.query_all_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT name FROM sqlite_master WHERE name IN ('subscriptions', 'hosted_lifecycle_recipients')")).await.unwrap().is_empty());
}

#[tokio::test]
#[serial_test::serial]
async fn eligibility_changes_after_claim_are_rechecked_before_network_delivery() {
    for mutation in [
        "UPDATE subscriptions SET status = 'Active' WHERE organizer_id = NEW.organizer_id;",
        "UPDATE subscriptions SET lapsed_at = lapsed_at + 1 WHERE organizer_id = NEW.organizer_id;",
        "UPDATE memberships SET role = 'Door Staff' WHERE organizer_id = NEW.organizer_id;",
    ] {
        let f = fixture().await;
        let o = organizer(&f).await;
        f.ctx.db.execute_raw(Statement::from_string(DatabaseBackend::Sqlite,
            format!("CREATE TRIGGER change_eligibility AFTER INSERT ON hosted_lifecycle_recipients BEGIN {mutation} END"))).await.unwrap();
        run(&f, LAPSE + 60 * DAY).await.unwrap();
        assert!(f.capture.accepted.lock().unwrap().is_empty());
        let row = f.ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM hosted_lifecycle_recipients WHERE attempts = 1 AND sent_at IS NULL AND claim_token IS NULL AND claim_until IS NULL")).await.unwrap().unwrap();
        assert_eq!(row.try_get::<i64>("", "count").unwrap(), 1);
        f.ctx
            .db
            .execute_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "DROP TRIGGER change_eligibility",
            ))
            .await
            .unwrap();
        hosted::subscription_lapsed(&f.ctx.db, &o.id, LAPSE)
            .await
            .unwrap();
        members::add_member(&f.ctx.db, &o.id, "a@example.test", Role::Admin)
            .await
            .unwrap();
        run(&f, LAPSE + 61 * DAY).await.unwrap();
        assert!(f
            .capture
            .accepted
            .lock()
            .unwrap()
            .iter()
            .any(|(to, _)| to == "a@example.test"));
    }
}
