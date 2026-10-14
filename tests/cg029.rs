use crabgate::{
    app::App,
    initializers::organizers::OrganizerRuntime,
    members::{self, Role},
    settings,
};
use loco_rs::{app::Hooks, boot::StartMode, config::WorkerMode, environment::Environment};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
#[serial_test::serial]
async fn setup_test_waits_for_smtp_acceptance_and_can_retry() {
    let refused = Arc::new(AtomicBool::new(true));
    let accepted = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let refusal = refused.clone();
    let count = accepted.clone();
    let smtp = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            if refusal.load(Ordering::SeqCst) {
                stream
                    .write_all(b"421 synthetic refusal\r\n")
                    .await
                    .unwrap();
                continue;
            }
            let count = count.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut reader = BufReader::new(read);
                write.write_all(b"220 loopback SMTP\r\n").await.unwrap();
                let mut data = false;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap() == 0 {
                        break;
                    }
                    let response = if data {
                        if line != ".\r\n" {
                            continue;
                        }
                        count.fetch_add(1, Ordering::SeqCst);
                        data = false;
                        b"250 accepted\r\n".as_slice()
                    } else if line.starts_with("DATA") {
                        data = true;
                        b"354 continue\r\n".as_slice()
                    } else if line.starts_with("QUIT") {
                        let _ = write.write_all(b"221 bye\r\n").await;
                        break;
                    } else {
                        b"250 OK\r\n".as_slice()
                    };
                    if write.write_all(response).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    let evidence_root = std::env::var_os("CARGO_TARGET_DIR")
        .and_then(|path| {
            std::path::PathBuf::from(path)
                .parent()
                .map(std::path::Path::to_path_buf)
        })
        .unwrap_or_else(std::env::temp_dir);
    let storage = tempfile::Builder::new()
        .prefix("cg029-setup-")
        .tempdir_in(evidence_root)
        .unwrap()
        .keep();
    println!("synthetic setup evidence retained at {}", storage.display());
    let _environment = OrganizerDirectory(std::env::var_os("CRABGATE_ORGANIZER_DATABASE_DIR"));
    std::env::set_var(
        "CRABGATE_ORGANIZER_DATABASE_DIR",
        storage.join("organizers"),
    );
    let mut config = App::load_config(&Environment::Test).await.unwrap();
    config.database.uri = format!("sqlite://{}?mode=rwc", storage.join("control.db").display());
    config.workers.mode = WorkerMode::BackgroundAsync;
    let mailer = config.mailer.as_mut().unwrap();
    mailer.stub = false;
    let smtp_config = mailer.smtp.as_mut().unwrap();
    smtp_config.host = "127.0.0.1".into();
    smtp_config.port = port;
    smtp_config.secure = false;
    smtp_config.auth = None;
    let boot = App::boot(StartMode::ServerOnly, &Environment::Test, config)
        .await
        .unwrap();
    let ctx = boot.app_context;
    let runtime = ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
    let registry = runtime.registry();
    let scope = registry.resolve("localhost").await.unwrap();
    let cipher = registry.cipher(&scope.organizer).unwrap();
    let now = crabgate::orders::now();
    members::add_member(
        &ctx.db,
        &scope.organizer.id,
        "setup-test@example.com",
        Role::Admin,
    )
    .await
    .unwrap();
    let token =
        members::request_magic_link(&ctx.db, &scope.organizer.id, "setup-test@example.com", now)
            .await
            .unwrap()
            .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server =
        tokio::spawn(async move { axum::serve(listener, boot.router.unwrap()).await.unwrap() });
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();
    let login = client
        .get(format!("{origin}/office/magic-link/{token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status().as_u16(), 303);
    let cookie = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|h| h.to_str().unwrap())
        .find(|h| h.starts_with("cg_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let response = client
        .post(format!("{origin}/office/setup/email"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let receipt = settings::get(&scope.db, &cipher, "smtp_tested_at")
        .await
        .unwrap();
    println!(
        "SMTP refusal: HTTP {}, setup receipt present={}, local accepted={}",
        response.status().as_u16(),
        receipt.is_some(),
        accepted.load(Ordering::SeqCst)
    );
    assert_eq!(
        receipt, None,
        "SMTP refusal must not complete the setup email test"
    );
    assert_eq!(response.status().as_u16(), 303);
    assert_eq!(
        response.headers().get("location").unwrap(),
        "/office/setup?step=email&error=smtp_unconfirmed"
    );
    let failure = client
        .get(format!(
            "{origin}/office/setup?step=email&error=smtp_unconfirmed"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(failure.status().as_u16(), 200);
    let html = failure.text().await.unwrap();
    assert!(html.contains("Test email acceptance could not be confirmed"));
    assert!(html.contains("Send me a test email"));
    assert_eq!(accepted.load(Ordering::SeqCst), 0);
    refused.store(false, Ordering::SeqCst);
    let retry = client
        .post(format!("{origin}/office/setup/email"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(retry.status().as_u16(), 303);
    assert_eq!(retry.headers().get("location").unwrap(), "/office/setup");
    assert!(settings::get(&scope.db, &cipher, "smtp_tested_at")
        .await
        .unwrap()
        .is_some());
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    let success = client
        .get(format!("{origin}/office/setup?step=email"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(success.status().as_u16(), 200);
    let html = success.text().await.unwrap();
    assert!(html.contains("Accepted by the mail server"));
    assert!(html.contains("does not confirm inbox delivery"));
    server.abort();
    smtp.abort();
    let _ = server.await;
    let _ = smtp.await;
    drop(scope);
    tokio::time::timeout(std::time::Duration::from_secs(5), App::on_shutdown(&ctx))
        .await
        .expect("test runtime must release its tenant scope before shutdown");
}

struct OrganizerDirectory(Option<std::ffi::OsString>);

impl Drop for OrganizerDirectory {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            std::env::set_var("CRABGATE_ORGANIZER_DATABASE_DIR", value);
        } else {
            std::env::remove_var("CRABGATE_ORGANIZER_DATABASE_DIR");
        }
    }
}

mod correction {
    use crabgate::{
        app::App,
        initializers::organizers::OrganizerRuntime,
        members::{self, Role},
        settings,
    };
    use loco_rs::{app::Hooks, boot::StartMode, config::WorkerMode, environment::Environment};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    #[serial_test::serial]
    async fn setup_deadline_history_and_receipt_fault_feedback() {
        use sea_orm::ConnectionTrait;
        let mode = Arc::new(AtomicUsize::new(1));
        let accepted = Arc::new(AtomicUsize::new(0));
        let consumed = Arc::new(AtomicUsize::new(0));
        let recipients = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = mode.clone();
        let count = accepted.clone();
        let data_count = consumed.clone();
        let rcpts = recipients.clone();
        let smtp = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mode = state.clone();
                let count = count.clone();
                let data_count = data_count.clone();
                let rcpts = rcpts.clone();
                clients.spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut reader = BufReader::new(read);
                    write.write_all(b"220 loopback SMTP\r\n").await.unwrap();
                    let mut data = false;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            break;
                        }
                        let response = if data {
                            if line != ".\r\n" {
                                continue;
                            }
                            data_count.fetch_add(1, Ordering::SeqCst);
                            data = false;
                            if mode.load(Ordering::SeqCst) == 3 {
                                break;
                            }
                            count.fetch_add(1, Ordering::SeqCst);
                            b"250 accepted\r\n".as_slice()
                        } else if line.starts_with("MAIL FROM") && mode.load(Ordering::SeqCst) == 0
                        {
                            b"550 synthetic refusal\r\n".as_slice()
                        } else if line.starts_with("MAIL FROM") && mode.load(Ordering::SeqCst) == 2
                        {
                            std::future::pending::<()>().await;
                            b"250 OK\r\n".as_slice()
                        } else if line.starts_with("RCPT TO") {
                            rcpts.lock().unwrap().push(line.trim().to_owned());
                            b"250 OK\r\n".as_slice()
                        } else if line.starts_with("DATA") {
                            data = true;
                            b"354 continue\r\n".as_slice()
                        } else if line.starts_with("QUIT") {
                            let _ = write.write_all(b"221 bye\r\n").await;
                            break;
                        } else {
                            b"250 OK\r\n".as_slice()
                        };
                        if write.write_all(response).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let evidence_root = std::env::var_os("CARGO_TARGET_DIR")
            .and_then(|path| {
                std::path::PathBuf::from(path)
                    .parent()
                    .map(std::path::Path::to_path_buf)
            })
            .unwrap_or_else(std::env::temp_dir);
        let storage = tempfile::Builder::new()
            .prefix("cg029-setup-")
            .tempdir_in(evidence_root)
            .unwrap()
            .keep();
        println!("synthetic setup evidence retained at {}", storage.display());
        let _environment = OrganizerDirectory(std::env::var_os("CRABGATE_ORGANIZER_DATABASE_DIR"));
        std::env::set_var(
            "CRABGATE_ORGANIZER_DATABASE_DIR",
            storage.join("organizers"),
        );
        let mut config = App::load_config(&Environment::Test).await.unwrap();
        config.database.uri = format!("sqlite://{}?mode=rwc", storage.join("control.db").display());
        config.workers.mode = WorkerMode::BackgroundAsync;
        let mailer = config.mailer.as_mut().unwrap();
        mailer.stub = false;
        let smtp_config = mailer.smtp.as_mut().unwrap();
        smtp_config.host = "127.0.0.1".into();
        smtp_config.port = port;
        smtp_config.secure = false;
        smtp_config.auth = None;
        let boot = App::boot(StartMode::ServerOnly, &Environment::Test, config)
            .await
            .unwrap();
        let ctx = boot.app_context;
        let runtime = ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
        let registry = runtime.registry();
        let scope = registry.resolve("localhost").await.unwrap();
        let cipher = registry.cipher(&scope.organizer).unwrap();
        let now = crabgate::orders::now();
        members::add_member(
            &ctx.db,
            &scope.organizer.id,
            "setup-test@example.com",
            Role::Admin,
        )
        .await
        .unwrap();
        let token = members::request_magic_link(
            &ctx.db,
            &scope.organizer.id,
            "setup-test@example.com",
            now,
        )
        .await
        .unwrap()
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server =
            tokio::spawn(async move { axum::serve(listener, boot.router.unwrap()).await.unwrap() });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap();
        let login = client
            .get(format!("{origin}/office/magic-link/{token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(login.status().as_u16(), 303);
        let cookie = login
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|h| h.to_str().unwrap())
            .find(|h| h.starts_with("cg_session="))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let mut observations = serde_json::Map::new();
        for (email, role, expected) in [("door-review@example.com", Role::DoorStaff, 403)] {
            members::add_member(&ctx.db, &scope.organizer.id, email, role)
                .await
                .unwrap();
            let token = members::request_magic_link(&ctx.db, &scope.organizer.id, email, now)
                .await
                .unwrap()
                .unwrap();
            let login = client
                .get(format!("{origin}/office/magic-link/{token}"))
                .send()
                .await
                .unwrap();
            assert_eq!(login.status().as_u16(), 303);
            let denied_cookie = login
                .headers()
                .get_all("set-cookie")
                .iter()
                .map(|h| h.to_str().unwrap())
                .find(|h| h.starts_with("cg_session="))
                .unwrap()
                .split(';')
                .next()
                .unwrap();
            let denied = client
                .post(format!("{origin}/office/setup/email"))
                .header("cookie", denied_cookie)
                .send()
                .await
                .unwrap();
            assert_eq!(denied.status().as_u16(), expected);
        }
        let anonymous = client
            .post(format!("{origin}/office/setup/email"))
            .send()
            .await
            .unwrap();
        assert_eq!(anonymous.status().as_u16(), 303);
        assert_eq!(
            anonymous.headers().get("location").unwrap(),
            "/office/sign-in"
        );
        let csrf = client
            .post(format!("{origin}/office/setup/email"))
            .header("cookie", &cookie)
            .header("origin", "https://other.example.com")
            .send()
            .await
            .unwrap();
        assert_eq!(csrf.status().as_u16(), 403);
        ctx.db.execute_unprepared("INSERT INTO operators (id, email) VALUES ('review-support', 'support-review@example.com')").await.unwrap();
        let token = crabgate::operator::request_link(&ctx.db, "support-review@example.com", now)
            .await
            .unwrap()
            .unwrap();
        let support = crabgate::operator::claim_link(&ctx.db, &token, now)
            .await
            .unwrap()
            .unwrap();
        let member_id = members::add_member(
            &ctx.db,
            &scope.organizer.id,
            "setup-test@example.com",
            Role::Admin,
        )
        .await
        .unwrap();
        crabgate::operator::grant(&ctx.db, &scope.organizer.id, &member_id, 1, now)
            .await
            .unwrap();
        let support_result = client
            .post(format!("{origin}/office/setup/email"))
            .header("cookie", format!("cg_operator_session={support}"))
            .send()
            .await
            .unwrap();
        assert_eq!(support_result.status().as_u16(), 403);
        assert_eq!(accepted.load(Ordering::SeqCst), 0);
        observations.insert("access".into(), serde_json::json!({"anonymous":303,"door":403,"support":403,"cross_origin":403,"accepted":0}));
        settings::set(&scope.db, &cipher, "smtp_tested_at", "1234567890")
            .await
            .unwrap();
        let legacy = client
            .get(format!("{origin}/office/setup?step=email"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!legacy.contains("Accepted by the mail server"));
        assert!(!legacy.contains("stamp stamp-ink\">Done"));
        assert!(!legacy.contains("Check ticket emails<span class=\"sr-only\"> (done)"));
        assert!(
            legacy.contains("A previous test was recorded without mail-server acceptance proof")
        );
        assert_eq!(accepted.load(Ordering::SeqCst), 0);
        scope
            .db
            .execute_unprepared("DELETE FROM organizer_settings WHERE key = 'smtp_tested_at'")
            .await
            .unwrap();
        observations.insert("legacy_marker".into(), serde_json::json!({"current_acceptance_claim":false,"current_done":false,"new_smtp_acceptances":0}));
        let first = client
            .post(format!("{origin}/office/setup/email"))
            .header("cookie", &cookie)
            .body("email=attacker@example.com")
            .send()
            .await
            .unwrap();
        assert_eq!(first.status().as_u16(), 303);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
        assert_eq!(
            *recipients.lock().unwrap(),
            vec!["RCPT TO:<setup-test@example.com>"]
        );
        let old = settings::get(&scope.db, &cipher, "smtp_tested_at")
            .await
            .unwrap();
        assert!(old.as_ref().unwrap().starts_with("accepted-v1:"));
        assert_eq!(first.headers().get("location").unwrap(), "/office/setup");
        mode.store(0, Ordering::SeqCst);
        let failed = client
            .post(format!("{origin}/office/setup/email"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let preserved = settings::get(&scope.db, &cipher, "smtp_tested_at")
            .await
            .unwrap();
        assert_eq!(preserved, old);
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
        let page = client
            .get(format!(
                "{origin}/office/setup?step=email&error=smtp_unconfirmed"
            ))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        std::fs::write(storage.join("failed-retry.html"), &page).unwrap();
        assert_eq!(
            failed.headers().get("location").unwrap(),
            "/office/setup?step=email&error=smtp_unconfirmed"
        );
        assert!(page.contains("Test email acceptance could not be confirmed"));
        assert!(page.contains("Another attempt may deliver a duplicate"));
        assert!(!page.contains("Accepted by the mail server"));
        assert!(!page.contains("stamp stamp-ink\">Done"));
        assert!(!page.contains("Check ticket emails<span class=\"sr-only\"> (done)"));
        assert!(page.contains("Historical test: the mail server accepted a previous message"));
        observations.insert("failed_after_success".into(), serde_json::json!({"http":failed.status().as_u16(),"location":failed.headers().get("location").unwrap().to_str().unwrap(),"old_receipt_preserved":true,"accepted":1,"failure_text":page.contains("Test email acceptance could not be confirmed"),"success_text":page.contains("Accepted by the mail server"),"legacy_sent_text":page.contains("<b>Sent.</b>")}));
        scope
            .db
            .execute_unprepared("DELETE FROM organizer_settings WHERE key = 'smtp_tested_at'")
            .await
            .unwrap();
        scope.db.execute_unprepared("CREATE TRIGGER review_block_receipt BEFORE INSERT ON organizer_settings WHEN NEW.key = 'smtp_tested_at' BEGIN SELECT RAISE(FAIL, 'synthetic receipt failure'); END").await.unwrap();
        mode.store(1, Ordering::SeqCst);
        let persisted = client
            .post(format!("{origin}/office/setup/email"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        let persisted_status = persisted.status().as_u16();
        assert_eq!(
            persisted.headers().get("location").unwrap(),
            "/office/setup?step=email&error=smtp_receipt_unconfirmed"
        );
        let persisted_body = client
            .get(format!(
                "{origin}/office/setup?step=email&error=smtp_receipt_unconfirmed"
            ))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        std::fs::write(storage.join("receipt-write-failure.txt"), &persisted_body).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(persisted_status, 303);
        assert!(persisted_body.contains(
            "The mail server accepted the test email, but its receipt could not be saved"
        ));
        assert!(persisted_body.contains("Another attempt may deliver a duplicate"));
        assert!(!persisted_body.contains("stamp stamp-ink\">Done"));
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        assert_eq!(
            settings::get(&scope.db, &cipher, "smtp_tested_at")
                .await
                .unwrap(),
            None
        );
        observations.insert("accepted_then_receipt_failure".into(), serde_json::json!({"http":persisted_status,"accepted":2,"receipt_present":false,"duplicate_feedback":persisted_body.contains("duplicate"),"feedback_file":"receipt-write-failure.txt"}));
        scope
            .db
            .execute_unprepared("DROP TRIGGER review_block_receipt")
            .await
            .unwrap();
        mode.store(3, Ordering::SeqCst);
        let ambiguous = client
            .post(format!("{origin}/office/setup/email"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        assert_eq!(consumed.load(Ordering::SeqCst), 3);
        assert_eq!(
            ambiguous.headers().get("location").unwrap(),
            "/office/setup?step=email&error=smtp_unconfirmed"
        );
        assert_eq!(
            settings::get(&scope.db, &cipher, "smtp_tested_at")
                .await
                .unwrap(),
            None
        );
        observations.insert("lost_acceptance_reply".into(), serde_json::json!({"http":ambiguous.status().as_u16(),"location":ambiguous.headers().get("location").unwrap().to_str().unwrap(),"server_received_data":3,"receipt_present":settings::get(&scope.db, &cipher, "smtp_tested_at").await.unwrap().is_some()}));
        mode.store(2, Ordering::SeqCst);
        let started = std::time::Instant::now();
        let stalled = client
            .post(format!("{origin}/office/setup/email"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        let elapsed = started.elapsed().as_secs_f64();
        assert_eq!(stalled.status().as_u16(), 303);
        assert_eq!(
            stalled.headers().get("location").unwrap(),
            "/office/setup?step=email&error=smtp_unconfirmed"
        );
        assert!(
            (9.5..13.0).contains(&elapsed),
            "whole-send deadline elapsed {elapsed}"
        );
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        assert_eq!(
            settings::get(&scope.db, &cipher, "smtp_tested_at")
                .await
                .unwrap(),
            None
        );
        observations.insert("stall".into(), serde_json::json!({"seconds":elapsed,"http":303,"location":"/office/setup?step=email&error=smtp_unconfirmed","receipt_present":false,"acknowledged_messages":2,"consumed_messages":3}));
        mode.store(1, Ordering::SeqCst);
        let retry = client
            .post(format!("{origin}/office/setup/email"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(retry.status().as_u16(), 303);
        assert_eq!(retry.headers().get("location").unwrap(), "/office/setup");
        assert_eq!(accepted.load(Ordering::SeqCst), 3);
        assert_eq!(consumed.load(Ordering::SeqCst), 4);
        assert!(settings::get(&scope.db, &cipher, "smtp_tested_at")
            .await
            .unwrap()
            .unwrap()
            .starts_with("accepted-v1:"));
        let successful = client
            .get(format!("{origin}/office/setup?step=email"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(successful.contains("Accepted by the mail server"));
        assert!(successful.contains("does not confirm inbox delivery"));
        assert!(successful.contains("stamp stamp-ink\">Done"));
        observations.insert("deadline_retry".into(), serde_json::json!({"http":303,"receipt_present":true,"acknowledged_messages":3,"consumed_messages":4}));
        std::fs::write(
            storage.join("observations.json"),
            serde_json::to_string_pretty(&observations).unwrap(),
        )
        .unwrap();
        println!(
            "EDGE_OBSERVATIONS {}",
            serde_json::to_string(&observations).unwrap()
        );
        server.abort();
        smtp.abort();
        let _ = server.await;
        let _ = smtp.await;
        drop(scope);
        tokio::time::timeout(std::time::Duration::from_secs(5), App::on_shutdown(&ctx))
            .await
            .expect("test runtime must release its tenant scope before shutdown");
    }

    struct OrganizerDirectory(Option<std::ffi::OsString>);

    impl Drop for OrganizerDirectory {
        fn drop(&mut self) {
            if let Some(value) = self.0.take() {
                std::env::set_var("CRABGATE_ORGANIZER_DATABASE_DIR", value);
            } else {
                std::env::remove_var("CRABGATE_ORGANIZER_DATABASE_DIR");
            }
        }
    }
}
