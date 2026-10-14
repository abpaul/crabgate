use crabgate::{
    app::App,
    events::{self, EventInput, TicketTypeInput},
    inventory,
    members::{self, Role},
    operator, orders,
    series::{self, Frequency},
    settings,
};
use loco_rs::{app::AppContext, testing::prelude::*};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

async fn session(ctx: &AppContext, id: &str, email: &str, role: Role) -> String {
    members::add_member(&ctx.db, id, email, role).await.unwrap();
    let token = members::request_magic_link(&ctx.db, id, email, orders::now())
        .await
        .unwrap()
        .unwrap();
    let session = members::claim_magic_link(&ctx.db, id, &token, orders::now())
        .await
        .unwrap()
        .unwrap();
    format!("cg_session={session}")
}

async fn event(db: &DatabaseConnection) -> (String, String) {
    let event = events::create(
        db,
        &EventInput {
            title: "</textarea><script>alert('unsafe title')</script>".into(),
            date: "2027-06-15".into(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Hall".into(),
            address: "Street".into(),
            description: String::new(),
            event_capacity: Some(4),
            max_per_order: 3,
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
            capacity: 4,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(db, &event.id, orders::now()).await.unwrap();
    (event.id, kind.id)
}

async fn counts(db: &DatabaseConnection) -> (i64, i64, i64, i64) {
    let row = db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT (SELECT COUNT(*) FROM events) AS events, (SELECT COUNT(*) FROM holds) AS holds, (SELECT COUNT(*) FROM orders) AS orders, (SELECT COUNT(*) FROM tickets) AS tickets"
    )).await.unwrap().unwrap();
    (
        row.try_get("", "events").unwrap(),
        row.try_get("", "holds").unwrap(),
        row.try_get("", "orders").unwrap(),
        row.try_get("", "tickets").unwrap(),
    )
}

fn textarea(html: &str, id: &str) -> String {
    let value = html
        .split(&format!("id=\"{id}\""))
        .nth(1)
        .unwrap()
        .split_once('>')
        .unwrap()
        .1
        .split("</textarea>")
        .next()
        .unwrap();
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&#x2F;", "/")
        .replace("&amp;", "&")
}

#[tokio::test]
#[serial]
async fn cg026_admin_persists_origins_and_copyable_event_series_fallbacks_without_mutation() {
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let scope = registry.active_dbs().await.unwrap().remove(0);
        let db = &scope.db;
        let admin = session(&ctx, &scope.organizer.id, "embed-admin@example.com", Role::Admin).await;
        let (id, kind) = event(db).await;
        let path = format!("/office/events/{id}");
        let save = format!("{path}/embed-origins");
        let before = counts(db).await;
        let empty = request.get(&path).add_header("cookie", &admin).await;
        assert_eq!(empty.status_code(), 200);
        assert!(empty.text().contains("Embedding is blocked."));
        assert_eq!(textarea(&empty.text(), "embed-origins"), "");
        assert!(!empty.text().contains("id=\"series-embed-snippet\""));
        let saved = request.post(&save).add_header("cookie", &admin).form(&[("origins", "  https://Merchant.Example:443\r\n\nhttps://www.merchant.example\nhttps://Merchant.Example:443  ")]).await;
        assert_eq!(saved.status_code(), 303);
        assert_eq!(saved.header("location"), format!("{path}#embed"));
        let page = request.get(&path).add_header("cookie", &admin).await;
        assert_eq!(page.status_code(), 200);
        assert_eq!(textarea(&page.text(), "embed-origins"), "https://merchant.example\nhttps://www.merchant.example");
        assert!(page.text().contains("organizer-wide setting, not just this Event"));
        assert!(page.text().contains("Embedding is enabled"));
        assert!(page.text().contains("readonly spellcheck=\"false\""));
        assert!(page.text().contains("&lt;iframe"));
        assert!(!page.text().contains("<iframe"));
        assert!(!page.text().contains("</textarea><script>"));
        let snippet = textarea(&page.text(), "event-embed-snippet");
        let origin = ctx.config.server.full_url();
        assert_eq!(snippet,format!("<iframe src=\"{origin}/embed/events/{id}\" title=\"CrabGate event tickets\" width=\"100%\" height=\"600\"></iframe>\n<p><a href=\"{origin}/events/{id}\" target=\"_top\">Choose tickets on CrabGate</a></p>"));
        assert!(!snippet.contains("unsafe title"));
        for target in [format!("/embed/events/{id}"), format!("/events/{id}")] {
            let response = request.get(&target).await;
            assert_eq!(response.status_code(), 200);
            if target.starts_with("/embed/") {
                assert!(!response.headers().contains_key("x-frame-options"));
                assert!(response.header("content-security-policy").to_str().unwrap().ends_with("frame-ancestors https://merchant.example https://www.merchant.example"));
            } else {
                assert_eq!(response.header("x-frame-options"), "DENY");
            }
        }
        assert_eq!(counts(db).await, before);
        assert_eq!(inventory::open_counts(db, &id, orders::now()).await.unwrap(), (0, 0));
        let series = series::start(db, &id, Frequency::Weekly, None, &Ok(()), orders::now()).await.unwrap();
        let before = counts(db).await;
        assert_eq!(before, (8, 0, 0, 0));
        let page = request.get(&path).add_header("cookie", &admin).await;
        assert_eq!(page.status_code(), 200);
        let snippet = textarea(&page.text(), "series-embed-snippet");
        assert_eq!(snippet,format!("<iframe src=\"{origin}/embed/series/{series}\" title=\"CrabGate Series dates and tickets\" width=\"100%\" height=\"600\"></iframe>\n<p><a href=\"{origin}/series/{series}\" target=\"_top\">Choose tickets on CrabGate</a></p>"));
        assert!(!snippet.contains("event_id="));
        std::fs::write(super::evidence_path("cg026-admin-series.html"), page.text()).unwrap();
        std::fs::write(super::evidence_path("cg026-snippet-ids.json"), serde_json::json!({"origin":origin,"event":id,"series":series}).to_string()).unwrap();
        for target in [format!("/embed/series/{series}"), format!("/series/{series}")] {
            let response = request.get(&target).await;
            assert_eq!(response.status_code(), 200);
            assert!(response.text().contains("Choose a date"));
            assert!(!response.text().contains(&format!("name=\"quantity_{kind}\"")));
        }
        assert_eq!(counts(db).await, before);
        let other = series::published_dates(db, &series, orders::now()).await.unwrap().unwrap()[1].id.clone();
        let other_page = request.get(&format!("/office/events/{other}")).add_header("cookie", &admin).await;
        assert_eq!(textarea(&other_page.text(), "embed-origins"), "https://merchant.example\nhttps://www.merchant.example");
        let removed = request.post(&save).add_header("cookie", &admin).form(&[("origins", " \n\t")]).await;
        assert_eq!(removed.status_code(), 303);
        for target in [format!("/embed/events/{id}"), format!("/embed/series/{series}")] {
            let response = request.get(&target).await;
            assert_eq!(response.status_code(), 200);
            assert_eq!(response.header("x-frame-options"), "DENY");
            assert!(response.header("content-security-policy").to_str().unwrap().ends_with("frame-ancestors 'none'"));
        }
        assert_eq!(request.get(&format!("/series/{series}")).await.status_code(), 200);
        assert_eq!(request.get(&format!("/events/{id}")).await.status_code(), 200);
        assert_eq!(counts(db).await, before);
    }).await;
}

#[tokio::test]
#[serial]
async fn cg026_invalid_origins_preserve_saved_config_and_storage_corruption_fails_closed() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = super::organizer_registry(&ctx)
            .active_dbs()
            .await
            .unwrap()
            .remove(0);
        let db = &scope.db;
        let admin = session(&ctx, &scope.organizer.id, "bounds@example.com", Role::Admin).await;
        let (id, _) = event(db).await;
        let path = format!("/office/events/{id}");
        let save = format!("{path}/embed-origins");
        assert_eq!(
            request
                .post(&save)
                .add_header("cookie", &admin)
                .form(&[("origins", "https://merchant.example")])
                .await
                .status_code(),
            303
        );
        let before = counts(db).await;
        let seventeen = (0..17)
            .map(|i| format!("https://merchant{i}.example"))
            .collect::<Vec<_>>()
            .join("\n");
        let too_long = format!("https://{}.example", "a".repeat(250));
        for invalid in [
            "http://merchant.example",
            "https://*.example",
            "https://user:pass@merchant.example",
            "https://merchant.example/path",
            "https://merchant.example/",
            "https://merchant.example?x=1",
            "https://merchant.example#x",
            "https://merchant.example https://other.example",
            "https://merchant.example\\evil",
            "https://%65xample.com",
            "https://mé.example",
            "</textarea><script>alert(1)</script>",
            &seventeen,
            &too_long,
        ] {
            let response = request
                .post(&save)
                .add_header("cookie", &admin)
                .form(&[("origins", invalid)])
                .await;
            assert_eq!(response.status_code(), 400, "{invalid}");
            assert!(response.text().contains("aria-invalid=\"true\""));
            assert!(response.text().contains("Saved origins have not changed."));
            assert_eq!(textarea(&response.text(), "embed-origins"), invalid);
            assert!(!response.text().contains("<script>alert(1)</script>"));
            assert_eq!(
                settings::embed_origins(db).await.unwrap().as_slice(),
                &["https://merchant.example"]
            );
            let frame = request.get(&format!("/embed/events/{id}")).await;
            assert_eq!(frame.status_code(), 200);
            assert!(frame
                .header("content-security-policy")
                .to_str()
                .unwrap()
                .ends_with("frame-ancestors https://merchant.example"));
        }
        assert_eq!(counts(db).await, before);
        let sixteen = (0..16)
            .map(|i| format!("https://merchant{i}.example:8443"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            request
                .post(&save)
                .add_header("cookie", &admin)
                .form(&[("origins", &sixteen)])
                .await
                .status_code(),
            303
        );
        let frame = request.get(&format!("/embed/events/{id}")).await;
        assert!(!frame.headers().contains_key("x-frame-options"));
        assert!(frame
            .header("content-security-policy")
            .to_str()
            .unwrap()
            .ends_with(&format!("frame-ancestors {}", sixteen.replace('\n', " "))));
        for invalid in [
            "not json".to_owned(),
            "[\"http://merchant.example\"]".to_owned(),
            "[\"https://merchant.example\",\"https://*.example\"]".to_owned(),
            serde_json::to_string(&seventeen.lines().collect::<Vec<_>>()).unwrap(),
            "x".repeat(8193),
        ] {
            db.execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE organizer_settings SET value = ? WHERE key = 'embed_origins'",
                [invalid.into()],
            ))
            .await
            .unwrap();
            let response = request.get(&format!("/embed/events/{id}")).await;
            assert_eq!(response.status_code(), 200);
            assert_eq!(response.header("x-frame-options"), "DENY");
            let page = request.get(&path).add_header("cookie", &admin).await;
            assert_eq!(page.status_code(), 200);
            assert_eq!(textarea(&page.text(), "embed-origins"), "");
            assert!(page.text().contains("Embedding is blocked."));
        }
        assert_eq!(
            request
                .post(&save)
                .add_header("cookie", &admin)
                .form(&[("origins", "https://repaired.example")])
                .await
                .status_code(),
            303
        );
        assert!(!request
            .get(&format!("/embed/events/{id}"))
            .await
            .headers()
            .contains_key("x-frame-options"));
        assert_eq!(counts(db).await, before);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg026_admin_auth_csrf_and_missing_event_cannot_change_origins() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = super::organizer_registry(&ctx)
            .active_dbs()
            .await
            .unwrap()
            .remove(0);
        let db = &scope.db;
        let admin = session(
            &ctx,
            &scope.organizer.id,
            "permission-admin@example.com",
            Role::Admin,
        )
        .await;
        let door = session(
            &ctx,
            &scope.organizer.id,
            "permission-door@example.com",
            Role::DoorStaff,
        )
        .await;
        let (id, _) = event(db).await;
        let path = format!("/office/events/{id}");
        let save = format!("{path}/embed-origins");
        let form = [("origins", "https://merchant.example")];
        let response = request.post(&save).form(&form).await;
        assert_eq!(response.status_code(), 303);
        assert_eq!(response.header("location"), "/office/sign-in");
        assert_eq!(request.get(&path).await.status_code(), 303);
        assert_eq!(
            request
                .get(&path)
                .add_header("cookie", &door)
                .await
                .status_code(),
            403
        );
        assert_eq!(
            request
                .post(&save)
                .add_header("cookie", &door)
                .form(&form)
                .await
                .status_code(),
            403
        );
        assert_eq!(
            request
                .post(&save)
                .add_header("cookie", &admin)
                .add_header("host", "localhost:5150")
                .add_header("origin", "https://evil.example")
                .form(&form)
                .await
                .status_code(),
            403
        );
        assert_eq!(
            request
                .post(&save)
                .add_header("cookie", &admin)
                .add_header("host", "localhost:5150")
                .add_header("origin", "http://localhost:5150")
                .add_header("sec-fetch-site", "cross-site")
                .form(&form)
                .await
                .status_code(),
            403
        );
        assert_eq!(
            request
                .post(&save)
                .add_header("cookie", &admin)
                .add_header("sec-fetch-site", "cross-site")
                .form(&form)
                .await
                .status_code(),
            403
        );
        assert_eq!(
            request
                .post(&format!(
                    "/office/events/{}/embed-origins",
                    uuid::Uuid::new_v4()
                ))
                .add_header("cookie", &admin)
                .form(&form)
                .await
                .status_code(),
            404
        );
        assert!(settings::embed_origins(db)
            .await
            .unwrap()
            .as_slice()
            .is_empty());
        assert_eq!(
            request
                .get(&format!("/embed/events/{id}"))
                .await
                .header("x-frame-options"),
            "DENY"
        );
        assert_eq!(
            request
                .post(&save)
                .add_header("cookie", &admin)
                .add_header("host", "localhost:5150")
                .add_header("origin", "http://localhost:5150")
                .add_header("sec-fetch-site", "same-origin")
                .form(&form)
                .await
                .status_code(),
            303
        );
        for target in [
            path,
            format!("/events/{id}"),
            format!("/events/{id}/selection"),
            "/office/setup".to_owned(),
        ] {
            let response = request.get(&target).add_header("cookie", &admin).await;
            assert_eq!(response.header("x-frame-options"), "DENY");
            assert!(!response
                .headers()
                .contains_key("access-control-allow-origin"));
        }
        for target in [
            format!("/embed/events/{}/", id),
            format!("/embed/events/{}", id.to_uppercase()),
        ] {
            assert_eq!(request.get(&target).await.header("x-frame-options"), "DENY");
        }
        let unavailable = request
            .get(&format!("/embed/events/{}", uuid::Uuid::new_v4()))
            .await;
        assert_eq!(unavailable.status_code(), 404);
        assert!(!unavailable.headers().contains_key("x-frame-options"));
        assert!(unavailable
            .header("content-security-policy")
            .to_str()
            .unwrap()
            .ends_with("frame-ancestors https://merchant.example"));
        assert_eq!(counts(db).await, (1, 0, 0, 0));
    })
    .await;
}

struct HostedEnvironment(Option<String>);

impl HostedEnvironment {
    fn set() -> Self {
        let prior = std::env::var("CRABGATE_EDITION").ok();
        std::env::set_var("CRABGATE_EDITION", "hosted");
        Self(prior)
    }
}

impl Drop for HostedEnvironment {
    fn drop(&mut self) {
        if let Some(prior) = &self.0 {
            std::env::set_var("CRABGATE_EDITION", prior);
        } else {
            std::env::remove_var("CRABGATE_EDITION");
        }
    }
}

#[tokio::test]
#[serial]
async fn cg026_hosted_tenant_isolation_trusted_urls_and_read_only_support_grants() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let a = registry.create("A", "cg026-a").await.unwrap();
        let b = registry.create("B", "cg026-b").await.unwrap();
        let a_db = registry.resolve_organizer(&a).await.unwrap();
        let b_db = registry.resolve_organizer(&b).await.unwrap();
        let (id, _) = event(&a_db.db).await;
        let series = series::start(&a_db.db, &id, Frequency::Weekly, None, &Ok(()), orders::now()).await.unwrap();
        let (other, _) = event(&b_db.db).await;
        let admin_a = session(&ctx, &a.id, "tenant-a@example.com", Role::Admin).await;
        let admin_b = session(&ctx, &b.id, "tenant-b@example.com", Role::Admin).await;
        let path = format!("/office/events/{id}");
        let save = format!("{path}/embed-origins");
        let form = [("origins", "https://a-merchant.example")];
        let cipher = registry.cipher(&a).unwrap();
        settings::set(&a_db.db, &cipher, "stripe_secret_key", "sk_test_CG026_PRIVATE").await.unwrap();
        let before_a = counts(&a_db.db).await;
        let before_b = counts(&b_db.db).await;
        assert_eq!(request.post(&save).add_header("cookie", &admin_a).add_header("host", "cg026-a.crabgate.com").form(&form).await.status_code(), 303);
        let page = request.get(&path).add_header("cookie", &admin_a).add_header("host", "cg026-a.crabgate.com").add_header("x-forwarded-host", "evil.example").await;
        assert_eq!(page.status_code(), 200);
        let event_snippet = textarea(&page.text(), "event-embed-snippet");
        assert!(event_snippet.contains(&format!("src=\"https://cg026-a.crabgate.com/embed/events/{id}\"")));
        assert!(event_snippet.contains(&format!("href=\"https://cg026-a.crabgate.com/events/{id}\" target=\"_top\"")));
        let series_snippet = textarea(&page.text(), "series-embed-snippet");
        assert!(series_snippet.contains(&format!("src=\"https://cg026-a.crabgate.com/embed/series/{series}\"")));
        assert!(series_snippet.contains(&format!("href=\"https://cg026-a.crabgate.com/series/{series}\" target=\"_top\"")));
        assert!(!page.text().contains("sk_test_CG026_PRIVATE"));
        assert!(!event_snippet.contains("evil.example"));
        assert_eq!(request.post(&save).add_header("cookie", &admin_a).add_header("host", "cg026-b.crabgate.com").form(&form).await.status_code(), 303);
        assert_eq!(request.post(&save).add_header("cookie", &admin_b).add_header("host", "cg026-b.crabgate.com").form(&form).await.status_code(), 404);
        assert_eq!(request.get(&path).add_header("cookie", &admin_b).add_header("host", "cg026-b.crabgate.com").await.status_code(), 404);
        assert_eq!(request.post(&format!("/office/events/{other}/embed-origins")).add_header("cookie", &admin_a).add_header("host", "cg026-a.crabgate.com").form(&form).await.status_code(), 404);
        assert!(settings::embed_origins(&b_db.db).await.unwrap().as_slice().is_empty());
        assert_eq!(settings::embed_origins(&a_db.db).await.unwrap().as_slice(), &["https://a-merchant.example"]);
        ctx.db.execute_raw(Statement::from_string(DatabaseBackend::Sqlite,"INSERT INTO operators (id,email) VALUES ('cg026-operator','cg026-operator@example.com')")).await.unwrap();
        let now = orders::now();
        let token = operator::request_link(&ctx.db, "cg026-operator@example.com", now).await.unwrap().unwrap();
        let support = operator::claim_link(&ctx.db, &token, now).await.unwrap().unwrap();
        let cookie = format!("cg_operator_session={support}");
        assert_eq!(request.get(&path).add_header("cookie", &cookie).add_header("host", "cg026-a.crabgate.com").await.status_code(), 403);
        let member = members::add_member(&ctx.db, &a.id, "grant-admin@example.com", Role::Admin).await.unwrap();
        operator::grant(&ctx.db, &a.id, &member, 24, now).await.unwrap();
        let page = request.get(&path).add_header("cookie", &cookie).add_header("host", "cg026-a.crabgate.com").await;
        assert_eq!(page.status_code(), 200);
        assert_eq!(textarea(&page.text(), "embed-origins"), "https://a-merchant.example");
        assert!(!page.text().contains("sk_test_CG026_PRIVATE"));
        assert_eq!(request.post(&save).add_header("cookie", &cookie).add_header("host", "cg026-a.crabgate.com").form(&[("origins", "https://support-write.example")]).await.status_code(), 403);
        assert_eq!(request.get("/office/setup").add_header("cookie", &cookie).add_header("host", "cg026-a.crabgate.com").await.status_code(), 403);
        assert_eq!(settings::embed_origins(&a_db.db).await.unwrap().as_slice(), &["https://a-merchant.example"]);
        assert_eq!(counts(&a_db.db).await, before_a);
        assert_eq!(counts(&b_db.db).await, before_b);
    }).await;
}
