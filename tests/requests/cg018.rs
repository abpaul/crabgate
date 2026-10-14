use crabgate::{app::App, hosted, operator};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;

async fn smoke(origin: String, id: String, session: String) -> std::process::Output {
    smoke_on_host(origin, "cg018-bad.crabgate.com", id, session).await
}

async fn smoke_on_host(
    origin: String,
    host: &str,
    id: String,
    session: String,
) -> std::process::Output {
    let host = host.to_owned();
    tokio::task::spawn_blocking(move || {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".curlrc"), "verbose\nheader = \"Host: ignored.invalid\"\n").unwrap();
        let curl = dir.path().join("curl");
        std::fs::write(&curl, format!("#!/bin/sh\nfirst=$1\nshift\nexec /usr/bin/curl \"$first\" --header 'Host: {host}' --noproxy '*' \"$@\"\n")).unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cookies = dir.path().join("cookies");
        std::fs::write(&cookies, format!("# Netscape HTTP Cookie File\n.crabgate.com\tTRUE\t/\tFALSE\t0\tcg_operator_session\t{session}\n")).unwrap();
        let output = std::process::Command::new("sh")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/tenant-smoke.sh"))
            .args([origin.trim_end_matches('/').to_owned(), id])
            .arg(cookies)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
            .env("CURL_HOME", dir.path())
            .output().unwrap();
        assert!(!String::from_utf8_lossy(&output.stdout).contains(&session), "operator cookie leaked to stdout");
        assert!(!String::from_utf8_lossy(&output.stderr).contains(&session), "operator cookie leaked to stderr");
        output
    }).await.unwrap()
}

#[tokio::test]
#[serial]
async fn cg018_migration_smoke_and_targeted_recovery() {
    let prior = std::env::var("CRABGATE_EDITION").ok();
    std::env::set_var("CRABGATE_EDITION", "hosted");
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let bad = registry.create("Bad", "cg018-bad").await.unwrap();
        registry.create("Good", "cg018-good").await.unwrap();
        let held = registry.create("Held", "cg018-held").await.unwrap();
        let gone = registry.create("Gone", "cg018-gone").await.unwrap();
        let missing = registry.create("Missing", "cg018-missing").await.unwrap();
        let deleted = registry.create("Deleted", "cg018-deleted").await.unwrap();
        ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "UPDATE organizers SET suspension_reason = 'support hold' WHERE id = ?", [held.id.clone().into()])).await.unwrap();
        ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "UPDATE organizers SET data_key = NULL WHERE id = ?", [gone.id.clone().into()])).await.unwrap();
        ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO subscriptions (organizer_id, status) VALUES (?, 'Deleted')", [deleted.id.clone().into()])).await.unwrap();
        tokio::fs::remove_file(&missing.db_path).await.unwrap();
        for organizer in [&bad, &held] {
            let db = sea_orm::Database::connect(format!("sqlite://{}?mode=rw", organizer.db_path)).await.unwrap();
            db.execute_unprepared("DROP TABLE series_exceptions; ALTER TABLE events DROP COLUMN series_review_reason; DELETE FROM seaql_migrations WHERE version = 'm20261007_000022_series_exceptions'; CREATE TABLE series_exceptions (broken TEXT)").await.unwrap();
            db.close().await.unwrap();
        }
        registry.prepare().await.unwrap();
        assert_eq!(request.get("/").add_header("host", "crabgate.com").await.status_code(), 200);
        assert_eq!(request.get("/").add_header("host", "cg018-bad.crabgate.com").await.status_code(), 503);
        assert_eq!(request.get("/").add_header("host", "cg018-good.crabgate.com").await.status_code(), 200);
        ctx.db.execute_unprepared("INSERT INTO operators (id, email) VALUES ('cg018-operator', 'operator@example.com')").await.unwrap();
        let link = operator::request_link(&ctx.db, "operator@example.com", hosted::now()).await.unwrap().unwrap();
        let claim = request.get(&format!("/operator/magic-link/{link}")).add_header("host", "crabgate.com").await;
        assert_eq!(claim.status_code(), 303);
        let set_cookie = claim.header("set-cookie").to_str().unwrap().to_owned();
        assert!(set_cookie.contains("Domain=crabgate.com"));
        assert!(set_cookie.contains("Path=/"));
        let session = set_cookie.split(';').next().unwrap().strip_prefix("cg_operator_session=").unwrap().to_owned();
        let cookie = format!("cg_operator_session={session}");
        let check = format!("/operator/organizers/{}/readiness", bad.id);
        let retry = format!("/operator/organizers/{}/migration/retry", bad.id);
        assert_eq!(request.get(&check).add_header("host", "cg018-bad.crabgate.com").await.status_code(), 403);
        assert_eq!(request.get(&check).add_header("cookie", "cg_operator_session=invalid").await.status_code(), 403);
        let failed = request.get(&check).add_header("host", "cg018-bad.crabgate.com").add_header("cookie", &cookie).await;
        assert_eq!(failed.status_code(), 503);
        assert_eq!(failed.json::<serde_json::Value>(), serde_json::json!({"organizer_id": bad.id, "ready": false}));
        assert!(!failed.text().contains(&bad.db_path));
        assert!(!failed.text().contains(bad.data_key.as_ref().unwrap()));
        assert!(!failed.text().contains(&session));
        let address = request.server_address().unwrap().to_string();
        let query_smoke = smoke_on_host(
            format!("{}/?", address.trim_end_matches('/')),
            "crabgate.com", bad.id.clone(), session.clone(),
        ).await;
        assert_eq!(query_smoke.status.code(), Some(2));
        assert!(!String::from_utf8_lossy(&query_smoke.stdout).contains("passed"));
        let state = ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT suspension_reason FROM organizers WHERE id = ?", [bad.id.clone().into()])).await.unwrap().unwrap();
        assert_eq!(state.try_get::<String>("", "suspension_reason").unwrap(), "migration failed");
        let failed_smoke = smoke(address.clone(), bad.id.clone(), session.clone()).await;
        assert_eq!(failed_smoke.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&failed_smoke.stderr).contains("HTTP 503"));
        assert!(!String::from_utf8_lossy(&failed_smoke.stderr).contains(&session));
        assert_eq!(request.post(&retry).add_header("host", "crabgate.com").add_header("cookie", &cookie).await.status_code(), 403);
        assert_eq!(request.post(&retry).add_header("host", "crabgate.com").add_header("origin", "https://evil.example").add_header("cookie", &cookie).await.status_code(), 403);
        assert_eq!(request.post(&retry).add_header("host", "crabgate.com").add_header("origin", &hosted::origin(None)).add_header("cookie", &cookie).await.status_code(), 503);
        let db = sea_orm::Database::connect(format!("sqlite://{}?mode=rw", bad.db_path)).await.unwrap();
        db.execute_unprepared("DROP TABLE series_exceptions").await.unwrap();
        db.close().await.unwrap();
        let recovered = request.post(&retry).add_header("host", "crabgate.com").add_header("origin", &hosted::origin(None)).add_header("cookie", &cookie).await;
        assert_eq!(recovered.status_code(), 200);
        assert_eq!(recovered.json::<serde_json::Value>(), serde_json::json!({"organizer_id": bad.id, "ready": true, "latest_migration": "m20261011_000030_read_indexes"}));
        let ready = request.get(&check).add_header("host", "cg018-bad.crabgate.com").add_header("cookie", &cookie).await;
        assert_eq!(ready.status_code(), 200);
        assert_eq!(ready.json::<serde_json::Value>(), recovered.json::<serde_json::Value>());
        assert_eq!(ready.header("cache-control"), "no-store");
        let passed_smoke = smoke(address, bad.id.clone(), session.clone()).await;
        assert_eq!(passed_smoke.status.code(), Some(0), "{}", String::from_utf8_lossy(&passed_smoke.stderr));
        assert_eq!(String::from_utf8_lossy(&passed_smoke.stdout).trim(), format!("Organizer {} readiness and tenant route passed", bad.id));
        assert_eq!(request.get("/").add_header("host", "cg018-bad.crabgate.com").await.status_code(), 200);
        for host in ["crabgate.com", "cg018-good.crabgate.com", "unknown.crabgate.com"] {
            assert_eq!(request.get(&check).add_header("host", host).add_header("cookie", &cookie).await.status_code(), 404);
        }
        assert_eq!(request.post(&retry).add_header("host", "crabgate.com").add_header("origin", &hosted::origin(None)).add_header("cookie", &cookie).await.status_code(), 409);
        assert_eq!(request.post("/operator/organizers/unknown/migration/retry").add_header("host", "crabgate.com").add_header("origin", &hosted::origin(None)).add_header("cookie", &cookie).await.status_code(), 404);
        for (organizer, expected) in [(&held, 409), (&gone, 404), (&deleted, 404), (&missing, 503)] {
            let path = format!("/operator/organizers/{}/migration/retry", organizer.id);
            assert_eq!(request.post(&path).add_header("host", "crabgate.com").add_header("origin", &hosted::origin(None)).add_header("cookie", &cookie).await.status_code(), expected);
        }
        for (id, host, expected) in [(&gone.id[..], "cg018-gone.crabgate.com", 404), (&deleted.id[..], "cg018-deleted.crabgate.com", 404), (&missing.id[..], "cg018-missing.crabgate.com", 503), ("unknown", "unknown.crabgate.com", 404)] {
            let path = format!("/operator/organizers/{id}/readiness");
            assert_eq!(request.get(&path).add_header("host", host).add_header("cookie", &cookie).await.status_code(), expected);
        }
        let row=ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT suspension_reason, data_key FROM organizers WHERE id = ?", [held.id.into()])).await.unwrap().unwrap();
        assert_eq!(row.try_get::<String>("", "suspension_reason").unwrap(), "support hold");
        assert_eq!(row.try_get::<String>("", "data_key").unwrap(), held.data_key.unwrap());
        let row = ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT suspension_reason, data_key FROM organizers WHERE id = ?", [bad.id.clone().into()])).await.unwrap().unwrap();
        assert_eq!(row.try_get::<Option<String>>("", "suspension_reason").unwrap(), None);
        assert_eq!(row.try_get::<String>("", "data_key").unwrap(), bad.data_key.unwrap());
        let scope = registry.resolve("cg018-bad.crabgate.com").await.unwrap();
        let applied = scope.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT version FROM seaql_migrations WHERE version = 'm20261007_000022_series_exceptions'")).await.unwrap().unwrap();
        assert_eq!(applied.try_get::<String>("", "version").unwrap(), "m20261007_000022_series_exceptions");
        scope.db.execute_unprepared("ALTER TABLE events DROP COLUMN series_review_reason").await.unwrap();
        drop(scope);
        assert_eq!(request.get(&check).add_header("host", "cg018-bad.crabgate.com").add_header("cookie", &cookie).await.status_code(), 503);
        let scope = registry.resolve("cg018-good.crabgate.com").await.unwrap();
        scope.db.execute_unprepared("DELETE FROM seaql_migrations WHERE version = 'm20261007_000022_series_exceptions'").await.unwrap();
        let good_id = scope.organizer.id.clone();
        drop(scope);
        assert_eq!(request.get("/").add_header("host", "cg018-good.crabgate.com").await.status_code(), 200);
        assert_eq!(request.get(&format!("/operator/organizers/{good_id}/readiness")).add_header("host", "cg018-good.crabgate.com").add_header("cookie", &cookie).await.status_code(), 503);
        assert!(!std::path::Path::new(&missing.db_path).exists());
        ctx.shared_store.get::<std::sync::Arc<crabgate::initializers::organizers::OrganizerRuntime>>().unwrap().shutdown().await.unwrap();
    }).await;
    match prior {
        Some(value) => std::env::set_var("CRABGATE_EDITION", value),
        None => std::env::remove_var("CRABGATE_EDITION"),
    }
}
