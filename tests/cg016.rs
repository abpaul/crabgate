use std::sync::Arc;

use axum::Router;
use crabgate::{app::App, hosted, initializers::organizers::OrganizerRuntime, members};
use loco_rs::{
    app::{AppContext, Hooks},
    boot::StartMode,
    environment::Environment,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;

struct Harness {
    ctx: AppContext,
    client: reqwest::Client,
    url: String,
    server: tokio::task::JoinHandle<()>,
    environment: [(&'static str, Option<String>); 3],
    _storage: tempfile::TempDir,
}

impl Harness {
    async fn new(smtp_port: Option<u16>) -> Self {
        let environment = [
            "CRABGATE_EDITION",
            "SHOP_BASE_DOMAIN",
            "CRABGATE_ORGANIZER_DATABASE_DIR",
        ]
        .map(|key| (key, std::env::var(key).ok()));
        std::env::set_var("CRABGATE_EDITION", "hosted");
        std::env::set_var("SHOP_BASE_DOMAIN", "crabgate.com");
        let storage = tempfile::Builder::new()
            .prefix("cg016-http-")
            .tempdir_in("/tmp")
            .unwrap();
        std::env::set_var(
            "CRABGATE_ORGANIZER_DATABASE_DIR",
            storage.path().join("organizers"),
        );
        let mut config = App::load_config(&Environment::Test).await.unwrap();
        config.database.uri = format!(
            "sqlite://{}?mode=rwc",
            storage.path().join("control.sqlite").display()
        );
        config.database.max_connections = 5;
        println!(
            "fresh hosted HTTP fixture: {} (5 control connections)",
            storage.path().display()
        );
        if let Some(port) = smtp_port {
            let mailer = config.mailer.as_mut().unwrap();
            mailer.stub = false;
            let smtp = mailer.smtp.as_mut().unwrap();
            smtp.host = "127.0.0.1".to_owned();
            smtp.port = port;
            smtp.secure = false;
            smtp.auth = None;
        }
        let boot = App::boot(StartMode::ServerOnly, &Environment::Test, config)
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router: Router = boot.router.unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            ctx: boot.app_context,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            url,
            server,
            environment,
            _storage: storage,
        }
    }

    async fn get(&self, host: &str, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.url))
            .header("host", host)
            .send()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, form: &[(&str, &str)]) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.url))
            .header("host", "crabgate.com")
            .form(form)
            .send()
            .await
            .unwrap()
    }

    async fn signup(&self, shop: &str, email: &str) -> reqwest::Response {
        self.post(
            "/signup",
            &[
                ("name", "Recovery Arts"),
                ("subdomain", shop),
                ("email", email),
            ],
        )
        .await
    }

    async fn resend(&self, shop: &str, email: &str) -> reqwest::Response {
        self.post("/signup/recover", &[("subdomain", shop), ("email", email)])
            .await
    }

    fn tokens(&self) -> Vec<String> {
        self.ctx
            .mailer
            .as_ref()
            .unwrap()
            .deliveries()
            .messages
            .iter()
            .filter_map(|message| {
                let message = message.replace("=\r\n", "");
                message
                    .split("/verify/")
                    .nth(1)
                    .map(|token| token.chars().take(32).collect())
            })
            .collect()
    }

    async fn count(&self, table: &str) -> i64 {
        let row = self
            .ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT COUNT(*) AS count FROM {table}"),
            ))
            .await
            .unwrap()
            .unwrap();
        row.try_get("", "count").unwrap()
    }

    async fn assert_accounts(&self, organizers: i64, members: i64, subscriptions: i64) {
        assert_eq!(self.count("organizers").await, organizers);
        assert_eq!(self.count("members").await, members);
        assert_eq!(self.count("subscriptions").await, subscriptions);
    }

    async fn close(self) {
        self.server.abort();
        App::on_shutdown(&self.ctx).await;
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
        for (key, value) in &self.environment {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

#[tokio::test]
#[serial]
async fn lost_link_resends_are_bounded_and_complete_on_the_correct_tenant() {
    let h = Harness::new(None).await;
    let invalid = h.signup("recover", "not-an-email").await;
    assert_eq!(invalid.status().as_u16(), 200);
    assert!(invalid.text().await.unwrap().contains("valid email"));
    h.assert_accounts(0, 0, 0).await;
    let sent = h.signup("recover", " ADMIN@example.com ").await;
    assert_eq!(sent.status().as_u16(), 200);
    let html = sent.text().await.unwrap();
    assert!(html.contains("accepted for delivery"));
    assert!(html.contains("href=\"/signup/recover\""));
    assert_eq!(h.tokens().len(), 1);
    h.assert_accounts(1, 1, 1).await;
    assert_eq!(h.count("organizer_verification_links").await, 1);
    let row = h
        .ctx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT status FROM subscriptions",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<String>("", "status").unwrap(), "Pending");
    assert_eq!(h.ctx.mailer.as_ref().unwrap().deliveries().count, 2);
    let duplicate = h.signup("recover", "admin@example.com").await;
    assert!(duplicate
        .text()
        .await
        .unwrap()
        .contains("This Shop address is taken"));
    let invalid = h.resend("recover", "not-an-email").await;
    assert_eq!(invalid.status().as_u16(), 400);
    assert!(invalid.text().await.unwrap().contains("role=\"alert\""));
    let mut generic = String::new();
    for _ in 0..3 {
        let response = h.resend(" RECOVER ", " Admin@example.com ").await;
        assert_eq!(response.status().as_u16(), 200);
        generic = response.text().await.unwrap();
        assert!(generic.contains("role=\"status\""));
        assert!(generic.contains("three requests per hour"));
    }
    let limited = h.resend(" RECOVER ", " Admin@example.com ").await;
    assert_eq!(limited.status().as_u16(), 200);
    assert_eq!(limited.text().await.unwrap(), generic);
    assert_eq!(h.tokens().len(), 4);
    assert_eq!(h.count("organizer_verification_links").await, 4);
    assert_eq!(h.ctx.mailer.as_ref().unwrap().deliveries().count, 5);
    let messages = h.ctx.mailer.as_ref().unwrap().deliveries().messages;
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.lines().any(|line| line == "To: admin@example.com"))
            .count(),
        4
    );
    h.assert_accounts(1, 1, 1).await;
    let token = h.tokens().pop().unwrap();
    let verify = h.get("crabgate.com", &format!("/verify/{token}")).await;
    assert_eq!(verify.status().as_u16(), 303);
    assert_eq!(
        verify.headers()["location"],
        format!("https://recover.crabgate.com/office/complete-signup/{token}")
    );
    let complete = h
        .get(
            "recover.crabgate.com",
            &format!("/office/complete-signup/{token}"),
        )
        .await;
    assert_eq!(complete.status().as_u16(), 303);
    assert_eq!(complete.headers()["location"], "/office/setup");
    let cookie = complete.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let setup = h
        .client
        .get(format!("{}/office/setup", h.url))
        .header("host", "recover.crabgate.com")
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(setup.status().as_u16(), 200);
    assert!(setup.text().await.unwrap().contains("Create a Draft event"));
    for token in h.tokens() {
        let replay = h.get("crabgate.com", &format!("/verify/{token}")).await;
        assert_eq!(replay.status().as_u16(), 400);
        assert!(replay
            .text()
            .await
            .unwrap()
            .contains("Recover your signup link"));
    }
    assert_eq!(
        h.get(
            "recover.crabgate.com",
            &format!("/office/complete-signup/{token}")
        )
        .await
        .status()
        .as_u16(),
        400
    );
    let verified = h.resend(" RECOVER ", " Admin@example.com ").await;
    assert_eq!(verified.status().as_u16(), 200);
    assert_eq!(verified.text().await.unwrap(), generic);
    assert_eq!(h.tokens().len(), 4);
    assert_eq!(h.count("memberships").await, 1);
    assert_eq!(h.count("member_sessions").await, 1);
    let row = h.ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite, "SELECT (SELECT COUNT(*) FROM organizers WHERE verified_at IS NOT NULL) AS verified, (SELECT COUNT(*) FROM organizer_verification_links WHERE used_at IS NOT NULL) AS used")).await.unwrap().unwrap();
    assert_eq!(row.try_get::<i64>("", "verified").unwrap(), 1);
    assert_eq!(row.try_get::<i64>("", "used").unwrap(), 1);
    assert_eq!(
        h.get("recover.crabgate.com", "/office/sign-in")
            .await
            .status()
            .as_u16(),
        200
    );
    h.assert_accounts(1, 1, 1).await;
    h.close().await;
}

#[tokio::test]
#[serial]
async fn expired_unknown_and_non_signup_admin_inputs_do_not_enumerate_or_change_accounts() {
    let h = Harness::new(None).await;
    assert_eq!(
        h.signup("expired", "signup@example.com")
            .await
            .status()
            .as_u16(),
        200
    );
    assert_eq!(
        h.signup("other", "signup@example.com")
            .await
            .status()
            .as_u16(),
        200
    );
    h.assert_accounts(2, 1, 2).await;
    let runtime = h.ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
    let org = runtime
        .registry()
        .resolve("expired.crabgate.com")
        .await
        .unwrap()
        .organizer;
    members::add_member(
        &h.ctx.db,
        &org.id,
        "later@example.com",
        members::Role::Admin,
    )
    .await
    .unwrap();
    let old = h.tokens()[0].clone();
    h.ctx
        .db
        .execute_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            format!(
                "UPDATE organizer_verification_links SET expires_at = {}",
                hosted::now() - 1
            ),
        ))
        .await
        .unwrap();
    for (host, path) in [
        ("crabgate.com", format!("/verify/{old}")),
        (
            "expired.crabgate.com",
            format!("/office/complete-signup/{old}"),
        ),
        ("crabgate.com", "/verify/not-a-token".to_owned()),
    ] {
        let response = h.get(host, &path).await;
        assert_eq!(response.status().as_u16(), 400);
        let html = response.text().await.unwrap();
        assert!(html.contains("invalid, expired, or already used"));
        assert!(html.contains("href=\"https://crabgate.com/signup/recover\""));
        assert!(html.contains("Sign in to your Back Office"));
    }
    for (shop, email) in [
        ("unknown", "signup@example.com"),
        ("expired", "unknown@example.com"),
        ("expired", "later@example.com"),
    ] {
        let response = h.resend(shop, email).await;
        assert_eq!(response.status().as_u16(), 200);
        assert!(response
            .text()
            .await
            .unwrap()
            .contains("If these details match an unfinished signup"));
    }
    assert_eq!(h.tokens().len(), 2);
    assert_eq!(h.count("magic_link_rate_limits").await, 0);
    assert_eq!(
        h.resend("expired", "signup@example.com")
            .await
            .status()
            .as_u16(),
        200
    );
    let new = h.tokens().pop().unwrap();
    let wrong = h
        .get(
            "other.crabgate.com",
            &format!("/office/complete-signup/{new}"),
        )
        .await;
    assert_eq!(wrong.status().as_u16(), 404);
    let complete = h
        .get(
            "expired.crabgate.com",
            &format!("/office/complete-signup/{new}"),
        )
        .await;
    assert_eq!(complete.status().as_u16(), 303);
    assert_eq!(h.count("organizer_verification_links").await, 3);
    h.assert_accounts(2, 2, 2).await;
    assert_eq!(h.count("memberships").await, 3);
    assert_eq!(h.count("member_sessions").await, 1);
    h.close().await;
}

#[tokio::test]
#[serial]
async fn concurrent_http_resends_and_claims_keep_literal_bounds_and_rate_boundary() {
    let h = Harness::new(None).await;
    assert_eq!(
        h.signup("parallel", "admin@example.com")
            .await
            .status()
            .as_u16(),
        200
    );
    let mut requests = Vec::new();
    for _ in 0..8 {
        let client = h.client.clone();
        let url = h.url.clone();
        requests.push(tokio::spawn(async move {
            client
                .post(format!("{url}/signup/recover"))
                .header("host", "crabgate.com")
                .form(&[("subdomain", "parallel"), ("email", "admin@example.com")])
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }));
    }
    for request in requests {
        assert_eq!(request.await.unwrap(), 200);
    }
    assert_eq!(h.tokens().len(), 4);
    assert_eq!(h.count("organizer_verification_links").await, 4);
    let row = h
        .ctx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT count FROM magic_link_rate_limits",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<i64>("", "count").unwrap(), 3);
    let org = h
        .ctx
        .shared_store
        .get::<Arc<OrganizerRuntime>>()
        .unwrap()
        .registry()
        .resolve("parallel.crabgate.com")
        .await
        .unwrap()
        .organizer;
    let boundary = (hosted::now() / 3600 + 1) * 3600;
    assert!(
        hosted::recover_verification(&h.ctx.db, "parallel", "admin@example.com", boundary - 1)
            .await
            .unwrap()
            .is_none()
    );
    let next = hosted::recover_verification(&h.ctx.db, "parallel", "admin@example.com", boundary)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.email, "admin@example.com");
    assert_eq!(next.organizer_id, org.id);
    assert_eq!(h.count("organizer_verification_links").await, 5);
    let a = h.tokens()[0].clone();
    let b = h.tokens()[1].clone();
    let a_path = format!("/office/complete-signup/{a}");
    let b_path = format!("/office/complete-signup/{b}");
    let (one, two, resend) = tokio::join!(
        h.get("parallel.crabgate.com", &a_path),
        h.get("parallel.crabgate.com", &b_path),
        h.resend("parallel", "admin@example.com")
    );
    let mut statuses = [one.status().as_u16(), two.status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, [303, 400]);
    assert_eq!(resend.status().as_u16(), 200);
    assert_eq!(h.count("member_sessions").await, 1);
    assert_eq!(h.count("organizer_verification_links").await, 5);
    assert!(
        hosted::pending_verification(&h.ctx.db, &next.token, boundary)
            .await
            .is_err()
    );
    h.assert_accounts(1, 1, 1).await;
    h.close().await;
}

#[tokio::test]
#[serial]
async fn smtp_failure_preserves_recovery_and_does_not_disclose_account_state() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = attempts.clone();
    let smtp = tokio::spawn(async move {
        loop {
            let (mut connection, _) = listener.accept().await.unwrap();
            observed.fetch_add(1, Ordering::SeqCst);
            connection
                .write_all(b"421 Local test mail unavailable\r\n")
                .await
                .unwrap();
            connection.shutdown().await.unwrap();
        }
    });
    let h = Harness::new(Some(port)).await;
    let unknown = h.resend("mail-failure", "admin@example.com").await;
    assert_eq!(unknown.status().as_u16(), 200);
    let generic = unknown.text().await.unwrap();
    let failed = h.signup("mail-failure", "admin@example.com").await;
    assert_eq!(failed.status().as_u16(), 503);
    let html = failed.text().await.unwrap();
    assert!(html.contains("could not send the verification email"));
    assert!(html.contains("/signup/recover"));
    h.assert_accounts(1, 1, 1).await;
    assert_eq!(h.count("organizer_verification_links").await, 1);
    let org = h
        .ctx
        .shared_store
        .get::<Arc<OrganizerRuntime>>()
        .unwrap()
        .registry()
        .resolve("mail-failure.crabgate.com")
        .await
        .unwrap()
        .organizer;
    let old = hosted::request_verification(&h.ctx.db, &org.id, "admin@example.com", hosted::now())
        .await
        .unwrap();
    for _ in 0..3 {
        let response = h.resend("mail-failure", "admin@example.com").await;
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(response.text().await.unwrap(), generic);
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    assert_eq!(h.count("organizer_verification_links").await, 5);
    assert!(hosted::pending_verification(&h.ctx.db, &old, hosted::now())
        .await
        .is_ok());
    for (shop, email) in [
        ("mail-failure", "admin@example.com"),
        ("unknown", "admin@example.com"),
        ("mail-failure", "other@example.com"),
    ] {
        assert_eq!(h.resend(shop, email).await.status().as_u16(), 200);
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    let complete = h
        .get(
            "mail-failure.crabgate.com",
            &format!("/office/complete-signup/{old}"),
        )
        .await;
    assert_eq!(complete.status().as_u16(), 303);
    assert_eq!(h.count("member_sessions").await, 1);
    assert_eq!(h.count("organizer_verification_links").await, 5);
    h.assert_accounts(1, 1, 1).await;
    smtp.abort();
    h.close().await;
}

#[tokio::test]
#[serial]
async fn recovery_is_hosted_only_and_marketing_host_only() {
    let h = Harness::new(None).await;
    assert_eq!(
        h.signup("tenant", "admin@example.com")
            .await
            .status()
            .as_u16(),
        200
    );
    assert_eq!(
        h.get("tenant.crabgate.com", "/signup/recover")
            .await
            .status()
            .as_u16(),
        404
    );
    std::env::set_var("CRABGATE_EDITION", "self-hosted");
    assert_eq!(
        h.get("crabgate.com", "/signup/recover")
            .await
            .status()
            .as_u16(),
        404
    );
    assert_eq!(
        h.resend("tenant", "admin@example.com")
            .await
            .status()
            .as_u16(),
        404
    );
    assert_eq!(h.tokens().len(), 1);
    h.close().await;
}
