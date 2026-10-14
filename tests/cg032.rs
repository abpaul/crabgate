use std::sync::Arc;

use crabgate::{
    admin_orders,
    app::App,
    crypto::MasterKey,
    data_transfer,
    events::{self, EventInput, TicketTypeInput},
    hosted::{self, LifecycleAction},
    hosted_jobs,
    initializers::organizers::OrganizerRuntime,
    members::{self, Role},
};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;

struct HostedEnvironment(Option<String>);

impl HostedEnvironment {
    fn new() -> Self {
        let prior = std::env::var("CRABGATE_EDITION").ok();
        std::env::set_var("CRABGATE_EDITION", "hosted");
        Self(prior)
    }
}

impl Drop for HostedEnvironment {
    fn drop(&mut self) {
        match &self.0 {
            Some(value) => std::env::set_var("CRABGATE_EDITION", value),
            None => std::env::remove_var("CRABGATE_EDITION"),
        }
    }
}

#[tokio::test]
#[serial]
async fn lapsed_admin_export_restores_orders_tickets_without_control_or_other_tenant_data() {
    let _environment = HostedEnvironment::new();
    request::<App, _, _>(|request, ctx| async move {
        let registry = ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap().registry();
        let organizer = registry.create("CG032 Restore", "cg032-restore").await.unwrap();
        let other = registry.create("CG032 Other", "cg032-other").await.unwrap();
        let scope = registry.resolve("cg032-restore.crabgate.com").await.unwrap();
        let other_scope = registry.resolve("cg032-other.crabgate.com").await.unwrap();
        other_scope.db.execute_unprepared("CREATE TABLE cg032_other (value TEXT); INSERT INTO cg032_other VALUES ('other tenant sentinel')").await.unwrap();
        let cipher = registry.cipher(&organizer).unwrap();
        let now = chrono::Utc::now();
        let event = events::create(&scope.db, &EventInput {
            title: "CG032 retained event".into(),
            date: (now + chrono::Duration::days(2)).date_naive().to_string(),
            start_time: "19:00".into(), end_time: "21:00".into(), timezone: "Europe/Dublin".into(),
            venue: "Synthetic Hall".into(), address: "Synthetic Street".into(), description: String::new(),
            event_capacity: Some(10), max_per_order: 4, attendee_name_required: false,
        }).await.unwrap();
        let kind = events::add_ticket_type(&scope.db, &event, &TicketTypeInput {
            name: "Standard".into(), price: "0".into(), capacity: 10, sales_open: None, sales_close: None,
        }).await.unwrap();
        let now = now.timestamp();
        events::publish(&scope.db, &event.id, now).await.unwrap();
        let order_id = admin_orders::complimentary(&scope.db, &cipher, &event.id, &kind.id,
            "restore@example.test", "Restored Guest", now).await.unwrap();
        members::add_member(&ctx.db, &organizer.id, "restore-admin@example.test", Role::Admin).await.unwrap();
        let token = members::request_magic_link(&ctx.db, &organizer.id, "restore-admin@example.test", now).await.unwrap().unwrap();
        let claim = request.get(&format!("/office/magic-link/{token}"))
            .add_header("host", "cg032-restore.crabgate.com").await;
        assert_eq!(claim.status_code(), 303);
        let cookie = claim.header("set-cookie").to_str().unwrap().split(';').next().unwrap().to_owned();
        let lapse = now - 89 * 86_400;
        ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO subscriptions (id, organizer_id, status, lapsed_at) VALUES ('cg032-sub', ?, 'Lapsed', ?)",
            [organizer.id.clone().into(), lapse.into()])).await.unwrap();
        let deadline = lapse + 90 * 86_400;
        assert!(!hosted::due_lifecycle(&ctx.db, deadline - 1).await.unwrap().iter()
            .any(|action| matches!(action, LifecycleAction::Delete { organizer_id } if organizer_id == &organizer.id)));
        let exported = request.get("/office/data/export").add_header("host", "cg032-restore.crabgate.com")
            .add_header("cookie", &cookie).await;
        assert_eq!(exported.status_code(), 200);
        assert_eq!(exported.header("content-type"), "application/vnd.sqlite3");
        assert_eq!(exported.header("cache-control"), "no-store");
        for filename in ["orders.csv", "tickets.csv"] {
            let csv = request.get(&format!("/office/events/{}/export/{filename}", event.id))
                .add_header("host", "cg032-restore.crabgate.com").add_header("cookie", &cookie).await;
            assert_eq!(csv.status_code(), 200);
            assert!(csv.text().contains(if filename == "orders.csv" { "restore@example.test" } else { "Restored Guest" }));
        }
        let cross = request.get("/office/data/export").add_header("host", "cg032-other.crabgate.com")
            .add_header("cookie", &cookie).await;
        assert_eq!(cross.status_code(), 303);
        let evidence = tempfile::Builder::new().prefix("cg032-export-")
            .tempdir_in(std::env::var_os("CG032_EVIDENCE_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir)).unwrap().keep();
        println!("CG032 synthetic export evidence: {}", evidence.display());
        let source = evidence.join("admin-export.sqlite");
        std::fs::write(&source, exported.as_bytes()).unwrap();
        let snapshot = sea_orm::Database::connect(format!("sqlite://{}?mode=ro", source.display())).await.unwrap();
        let absent = snapshot.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM sqlite_master WHERE name IN ('organizers', 'subscriptions', 'members', 'sessions', 'cg032_other')".to_owned())).await.unwrap().unwrap();
        assert_eq!(absent.try_get::<i64>("", "count").unwrap(), 0);
        snapshot.close().await.unwrap();
        let restored_path = evidence.join("restored.sqlite");
        let restored = sea_orm::Database::connect(format!("sqlite://{}?mode=rwc", restored_path.display())).await.unwrap();
        loco_rs::db::migrate::<migration::OrganizerMigrator>(&restored).await.unwrap();
        restored.close().await.unwrap();
        let restored_cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap()
            .generate_organizer_key("cg032-isolated-restored").unwrap().1;
        data_transfer::import_fresh(&source, &restored_path, &restored_cipher).await.unwrap();
        let restored = sea_orm::Database::connect(format!("sqlite://{}?mode=rw", restored_path.display())).await.unwrap();
        let order = admin_orders::order(&restored, &restored_cipher, &order_id).await.unwrap();
        assert_eq!(order.buyer_email, "restore@example.test");
        assert_eq!(order.tickets.len(), 1);
        assert_eq!(order.tickets[0].attendee_name.as_deref(), Some("Restored Guest"));
        assert_eq!(order.event_id, event.id);
        restored.close().await.unwrap();
        drop(scope);
        hosted_jobs::run(&ctx, &registry, deadline).await.unwrap();
        let deleted = request.get("/office/data/export").add_header("host", "cg032-restore.crabgate.com")
            .add_header("cookie", &cookie).await;
        assert_eq!(deleted.status_code(), 404);
        assert_eq!(hosted::access_state(&ctx.db, &organizer.id).await.unwrap().subscription_status, "Deleted");
        assert!(!std::path::Path::new(&organizer.db_path).exists());
        let other_value = other_scope.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT value FROM cg032_other".to_owned())).await.unwrap().unwrap();
        assert_eq!(other_value.try_get::<String>("", "value").unwrap(), "other tenant sentinel");
        assert_eq!(registry.resolve_organizer(&other).await.unwrap().organizer.data_key, other.data_key);
        assert!(members::find_session(&ctx.db, &organizer.id, cookie.strip_prefix("cg_session=").unwrap(), now).await.unwrap().is_some());
    }).await;
}

#[tokio::test]
#[serial]
async fn rendered_retention_copy_has_finite_export_and_an_open_backup_limit() {
    let _environment = HostedEnvironment::new();
    request::<App, _, _>(|request, _| async move {
        let evidence = tempfile::Builder::new()
            .prefix("cg032-copy-")
            .tempdir_in(
                std::env::var_os("CG032_EVIDENCE_DIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(std::env::temp_dir),
            )
            .unwrap()
            .keep();
        println!("CG032 rendered copy evidence: {}", evidence.display());
        for (path, expected) in [
            (
                "/pricing",
                "until deletion starts 90 days after the subscription lapses",
            ),
            (
                "/privacy",
                "We have not yet verified a maximum retention period for production backup copies",
            ),
            ("/terms", "Export access ends when deletion starts"),
        ] {
            let response = request.get(path).add_header("host", "crabgate.com").await;
            assert_eq!(response.status_code(), 200);
            let html = response.text();
            assert!(html.contains(expected), "{path}");
            assert!(!html.contains("Export it whenever you like"), "{path}");
            assert!(!html.contains("You can always check people in"), "{path}");
            assert!(!html.contains("Backups are kept for 30 days"), "{path}");
            if path == "/terms" {
                assert!(html.contains("at least 90 days' notice"));
                assert!(html.contains("separate 90-day lapse rule"));
                assert!(!html.contains("keep 30 days of backups"));
            }
            std::fs::write(evidence.join(format!("{}.html", &path[1..])), html).unwrap();
        }
    })
    .await;
}
