use std::path::Path;

use crabgate::{
    app::App,
    crypto::MasterKey,
    hosted::{self, LifecycleAction},
    hosted_jobs,
    organizers::{Edition, OrganizerRegistry},
};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};
use serial_test::serial;

const LAPSED_AT: i64 = 1_800_000_000;
const DELETE_AT: i64 = LAPSED_AT + 90 * 86_400;

async fn assert_removed(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        assert!(!Path::new(&format!("{}{suffix}", path.display())).exists());
    }
}

#[tokio::test]
#[serial]
async fn public_lifecycle_retries_fault_without_access_or_other_tenant_changes() {
    request::<App, _, _>(|_, ctx| async move {
        let dir = tempfile::Builder::new().prefix("cg054-public-").tempdir_in("/tmp").unwrap();
        let registry = OrganizerRegistry::new(ctx.db.clone(), ctx.config.database.clone(), Edition::Hosted,
            "crabgate.com".to_owned(), dir.path().to_owned(),
            MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap()).unwrap();
        let organizer = registry.create("Expired", "expired").await.unwrap();
        let other = registry.create("Control", "control").await.unwrap();
        let other_bytes = tokio::fs::read(&other.db_path).await.unwrap();
        let other_key = other.data_key.clone().unwrap();
        let control_copy = dir.path().join("control-copy.db");
        let copy_db = sea_orm::Database::connect(format!("sqlite://{}?mode=rwc", control_copy.display())).await.unwrap();
        copy_db.execute_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "CREATE TABLE sentinel (value TEXT); INSERT INTO sentinel VALUES ('untouched control');"
        )).await.unwrap();
        copy_db.close().await.unwrap();
        let control_bytes = tokio::fs::read(&control_copy).await.unwrap();
        ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO subscriptions (id, organizer_id, status, lapsed_at) VALUES ('expired-sub', ?, 'Lapsed', ?)",
            [organizer.id.clone().into(), LAPSED_AT.into()]
        )).await.unwrap();
        let wal = format!("{}-wal", organizer.db_path);
        assert!(!Path::new(&wal).exists());
        tokio::fs::create_dir(&wal).await.unwrap();
        tokio::fs::write(format!("{}-shm", organizer.db_path), b"synthetic shm").await.unwrap();
        assert!(hosted_jobs::run(&ctx, &registry, DELETE_AT).await.is_err());
        assert!(!Path::new(&organizer.db_path).exists());
        assert!(Path::new(&wal).is_dir());
        assert_eq!(tokio::fs::read(format!("{}-shm", organizer.db_path)).await.unwrap(), b"synthetic shm");
        assert_eq!(hosted::access_state(&ctx.db, &organizer.id).await.unwrap().subscription_status, "Deleted");
        assert!(registry.resolve("expired.crabgate.com").await.is_err());
        hosted::subscription_active(&ctx.db, &organizer.id, "customer", "subscription", DELETE_AT + 1, DELETE_AT + 1).await.unwrap();
        assert_eq!(hosted::due_lifecycle(&ctx.db, DELETE_AT + 1).await.unwrap(),
            vec![LifecycleAction::Delete { organizer_id: organizer.id.clone() }]);
        let row = ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT o.data_key, s.status, c.completed_at, c.last_error FROM organizers o JOIN subscriptions s ON s.organizer_id = o.id JOIN hosted_cleanup c ON c.organizer_id = o.id WHERE o.id = ?",
            [organizer.id.clone().into()]
        )).await.unwrap().unwrap();
        assert_eq!(row.try_get::<Option<String>>("", "data_key").unwrap(), None);
        assert_eq!(row.try_get::<String>("", "status").unwrap(), "Deleted");
        assert_eq!(row.try_get::<Option<i64>>("", "completed_at").unwrap(), None);
        let failure = row.try_get::<String>("", "last_error").unwrap();
        assert!(failure.contains("cleanup remove"));
        assert!(failure.contains("-wal"));

        assert!(tokio::fs::read(&other.db_path).await.unwrap() == other_bytes, "other organizer bytes changed during failed cleanup");
        assert!(tokio::fs::read(&control_copy).await.unwrap() == control_bytes, "control fixture bytes changed during failed cleanup");
        tokio::fs::remove_dir(&wal).await.unwrap();
        tokio::fs::write(&wal, b"synthetic wal").await.unwrap();
        hosted_jobs::run(&ctx, &registry, DELETE_AT + 1).await.unwrap();
        hosted_jobs::run(&ctx, &registry, DELETE_AT + 2).await.unwrap();
        assert_removed(Path::new(&organizer.db_path)).await;
        assert!(registry.resolve("expired.crabgate.com").await.is_err());
        let row = ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT completed_at, last_error FROM hosted_cleanup WHERE organizer_id = ?", [organizer.id.clone().into()]
        )).await.unwrap().unwrap();
        assert_eq!(row.try_get::<Option<i64>>("", "completed_at").unwrap(), Some(DELETE_AT + 1));
        assert_eq!(row.try_get::<String>("", "last_error").unwrap(), failure);
        assert!(tokio::fs::read(&other.db_path).await.unwrap() == other_bytes, "other organizer bytes changed during cleanup");
        assert!(tokio::fs::read(&control_copy).await.unwrap() == control_bytes, "control fixture bytes changed during cleanup");
        let other_scope = registry.resolve("control.crabgate.com").await.unwrap();
        assert_eq!(other_scope.organizer.data_key.as_deref(), Some(other_key.as_str()));
        let row = other_scope.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM events"
        )).await.unwrap().unwrap();
        assert_eq!(row.try_get::<i64>("", "count").unwrap(), 0);
    }).await;
}

#[tokio::test]
#[serial]
async fn public_lifecycle_closes_cached_database_and_does_not_recreate_it() {
    request::<App, _, _>(|_, ctx| async move {
        let dir = tempfile::Builder::new().prefix("cg054-cache-").tempdir_in("/tmp").unwrap();
        let registry = OrganizerRegistry::new(ctx.db.clone(), ctx.config.database.clone(), Edition::Hosted,
            "crabgate.com".to_owned(), dir.path().to_owned(),
            MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap()).unwrap();
        let organizer = registry.create("Expired", "expired").await.unwrap();
        let cached = registry.resolve("expired.crabgate.com").await.unwrap();
        cached.db.execute_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "CREATE TABLE cleanup_probe (value TEXT); INSERT INTO cleanup_probe VALUES ('cached data');"
        )).await.unwrap();
        ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO subscriptions (id, organizer_id, status, lapsed_at) VALUES ('expired-sub', ?, 'Lapsed', ?)",
            [organizer.id.clone().into(), LAPSED_AT.into()]
        )).await.unwrap();
        let pool = cached.db.get_sqlite_connection_pool().clone();
        drop(cached);
        let (first, second, _) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(hosted_jobs::run(&ctx, &registry, DELETE_AT),
                hosted_jobs::run(&ctx, &registry, DELETE_AT), async {
                    let lookup = registry.resolve("expired.crabgate.com").await;
                    drop(lookup);
                })
        }).await.unwrap();
        first.unwrap();
        second.unwrap();
        assert!(pool.is_closed());
        assert_removed(Path::new(&organizer.db_path)).await;
        registry.prepare().await.unwrap();
        assert!(registry.resolve("expired.crabgate.com").await.is_err());
        assert_removed(Path::new(&organizer.db_path)).await;
    }).await;
}

async fn lapse(db: &sea_orm::DatabaseConnection, id: &str) {
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "INSERT INTO subscriptions(id, organizer_id,status,lapsed_at) VALUES (?,?,'Lapsed',?)",
        [format!("sub-{id}").into(), id.into(), LAPSED_AT.into()],
    ))
    .await
    .unwrap();
}
#[tokio::test]
#[serial]
async fn concurrent_cold_handles_all_close() {
    request::<App, _, _>(|_, ctx| async move {
        let dir = tempfile::Builder::new()
            .prefix("cg054-review-cold-")
            .tempdir_in("/tmp")
            .unwrap();
        let registry = OrganizerRegistry::new(
            ctx.db.clone(),
            ctx.config.database.clone(),
            Edition::Hosted,
            "crabgate.com".into(),
            dir.path().into(),
            MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap(),
        )
        .unwrap();
        let org = registry.create("Cold", "cold").await.unwrap();
        let start = tokio::sync::Barrier::new(2);
        let (a, b) = tokio::join!(
            async {
                start.wait().await;
                registry.resolve("cold.crabgate.com").await
            },
            async {
                start.wait().await;
                registry.resolve("cold.crabgate.com").await
            }
        );
        let a = a.unwrap();
        let b = b.unwrap();
        lapse(&ctx.db, &org.id).await;
        let pool_a = a.db.get_sqlite_connection_pool().clone();
        let pool_b = b.db.get_sqlite_connection_pool().clone();
        assert_event_count(&a.db).await;
        assert_event_count(&b.db).await;
        drop(a);
        drop(b);
        hosted_jobs::run(&ctx, &registry, DELETE_AT).await.unwrap();
        assert!(pool_a.is_closed());
        assert!(pool_b.is_closed());
        let receipt = ctx
            .db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT completed_at FROM hosted_cleanup WHERE organizer_id=?",
                [org.id.clone().into()],
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            receipt.try_get::<Option<i64>>("", "completed_at").unwrap(),
            Some(DELETE_AT)
        );
        assert_removed(Path::new(&org.db_path)).await;
        registry.prepare().await.unwrap();
        assert!(registry.resolve("cold.crabgate.com").await.is_err());
        assert_removed(Path::new(&org.db_path)).await;
    })
    .await;
}
#[tokio::test]
#[serial]
async fn bad_receipt_does_not_starve_second_receipt() {
    request::<App, _, _>(|_, ctx| async move {
        let dir = tempfile::Builder::new()
            .prefix("cg054-review-starve-")
            .tempdir_in("/tmp")
            .unwrap();
        let registry = OrganizerRegistry::new(
            ctx.db.clone(),
            ctx.config.database.clone(),
            Edition::Hosted,
            "crabgate.com".into(),
            dir.path().into(),
            MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap(),
        )
        .unwrap();
        let a = registry.create("First", "first").await.unwrap();
        let b = registry.create("Second", "second").await.unwrap();
        let other = registry.create("Other", "other").await.unwrap();
        let other_bytes = tokio::fs::read(&other.db_path).await.unwrap();
        for org in [&a, &b] {
            lapse(&ctx.db, &org.id).await;
            assert!(hosted::mark_deleted(&ctx.db, &org.id, DELETE_AT)
                .await
                .unwrap());
        }
        let actions = hosted::due_lifecycle(&ctx.db, DELETE_AT).await.unwrap();
        let crabgate::hosted::LifecycleAction::Delete {
            organizer_id: first,
        } = &actions[0]
        else {
            panic!()
        };
        let (bad, good) = if *first == a.id { (&a, &b) } else { (&b, &a) };
        tokio::fs::create_dir(format!("{}-wal", bad.db_path))
            .await
            .unwrap();
        for pass in 0..3 {
            let failure = hosted_jobs::run(&ctx, &registry, DELETE_AT + pass).await.unwrap_err().to_string();
            assert!(failure.contains("1 lifecycle actions failed"));
            assert!(failure.contains(&bad.id));
            assert!(failure.contains("cleanup remove"));
            for org in [bad, good] {
                let row = ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                    "SELECT o.data_key, s.status, c.completed_at, c.last_error FROM organizers o JOIN subscriptions s ON s.organizer_id=o.id JOIN hosted_cleanup c ON c.organizer_id=o.id WHERE o.id=?",
                    [org.id.clone().into()])).await.unwrap().unwrap();
                assert_eq!(row.try_get::<Option<String>>("", "data_key").unwrap(), None);
                assert_eq!(row.try_get::<String>("", "status").unwrap(), "Deleted");
                if org.id == bad.id {
                    assert_eq!(row.try_get::<Option<i64>>("", "completed_at").unwrap(), None);
                    assert!(row.try_get::<String>("", "last_error").unwrap().contains("-wal"));
                    assert!(Path::new(&format!("{}-wal", bad.db_path)).is_dir());
                } else {
                    assert_eq!(row.try_get::<Option<i64>>("", "completed_at").unwrap(), Some(DELETE_AT));
                    assert_eq!(row.try_get::<Option<String>>("", "last_error").unwrap(), None);
                    assert_removed(Path::new(&good.db_path)).await;
                }
                assert!(registry.resolve(&format!("{}.crabgate.com", org.subdomain)).await.is_err());
            }
            assert!(tokio::fs::read(&other.db_path).await.unwrap() == other_bytes, "other tenant bytes changed");
        }
        let scope = registry.resolve("other.crabgate.com").await.unwrap();
        assert_eq!(scope.organizer.data_key, other.data_key);
        let row = scope.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite, "SELECT COUNT(*) AS count FROM events")).await.unwrap().unwrap();
        assert_eq!(row.try_get::<i64>("", "count").unwrap(), 0);
        drop(scope);
        tokio::fs::remove_dir(format!("{}-wal", bad.db_path)).await.unwrap();
    }).await;
}

#[tokio::test]
#[serial]
async fn held_transaction_measures_global_resolution_delay() {
    request::<App,_,_>(|_,ctx| async move {
 let dir=tempfile::Builder::new().prefix("cg054-review-held-").tempdir_in("/tmp").unwrap();
 let registry=OrganizerRegistry::new(ctx.db.clone(),ctx.config.database.clone(),Edition::Hosted,"crabgate.com".into(),dir.path().into(),MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap()).unwrap();
 let org=registry.create("Expired","expired").await.unwrap();registry.create("Other","other").await.unwrap();
 let scope=registry.resolve("expired.crabgate.com").await.unwrap();let txn=scope.db.begin().await.unwrap();drop(scope);
 lapse(&ctx.db,&org.id).await;
 let job=hosted_jobs::run(&ctx,&registry,DELETE_AT);tokio::pin!(job);
 assert!(tokio::time::timeout(std::time::Duration::from_millis(200), &mut job).await.is_err());
 assert!(tokio::time::timeout(std::time::Duration::from_millis(200),registry.resolve("other.crabgate.com")).await.is_err());
 txn.rollback().await.unwrap();
 tokio::time::timeout(std::time::Duration::from_secs(5),&mut job).await.unwrap().unwrap();
 registry.resolve("other.crabgate.com").await.unwrap();
 eprintln!("unrelated resolution blocked while deletion waited for tenant transaction; recovered after rollback");
 }).await;
}

#[tokio::test]
#[serial]
async fn warning_failure_returns_error_and_does_not_block_due_deletion() {
    request::<App, _, _>(|_, mut ctx| async move {
        let dir = tempfile::Builder::new().prefix("cg054-warning-").tempdir_in("/tmp").unwrap();
        let registry = OrganizerRegistry::new(ctx.db.clone(), ctx.config.database.clone(), Edition::Hosted,
            "crabgate.com".into(), dir.path().into(),
            MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap()).unwrap();
        let warning = registry.create("Warning", "warning").await.unwrap();
        let due = registry.create("Due", "due").await.unwrap();
        crabgate::members::add_member(&ctx.db, &warning.id, "warning@example.invalid", crabgate::members::Role::Admin).await.unwrap();
        lapse(&ctx.db, &due.id).await;
        ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO subscriptions(id, organizer_id, status, lapsed_at) VALUES (?, ?, 'Lapsed', ?)",
            [format!("sub-{}", warning.id).into(), warning.id.clone().into(), (DELETE_AT - 60 * 86400).into()]
        )).await.unwrap();
        ctx.mailer = None;
        let failure = hosted_jobs::run(&ctx, &registry, DELETE_AT).await.unwrap_err().to_string();
        assert!(failure.contains("1 lifecycle actions failed"));
        assert!(failure.contains(&warning.id));
        assert!(failure.contains("no email sender configured"));
        assert_removed(Path::new(&due.db_path)).await;
        let row = ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT o.data_key, c.completed_at FROM organizers o JOIN hosted_cleanup c ON c.organizer_id=o.id WHERE o.id=?",
            [due.id.clone().into()])).await.unwrap().unwrap();
        assert_eq!(row.try_get::<Option<String>>("", "data_key").unwrap(), None);
        assert_eq!(row.try_get::<Option<i64>>("", "completed_at").unwrap(), Some(DELETE_AT));
        let notice = ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM hosted_lifecycle_recipients WHERE organizer_id=? AND sent_at IS NOT NULL",
            [warning.id.clone().into()])).await.unwrap().unwrap();
        assert_eq!(notice.try_get::<i64>("", "count").unwrap(), 0);
        let scope = registry.resolve("warning.crabgate.com").await.unwrap();
        assert_eq!(scope.organizer.data_key, warning.data_key);
        drop(scope);
    }).await;
}

async fn assert_event_count(db: &sea_orm::DatabaseConnection) {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM events",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<i64>("", "count").unwrap(), 0);
}

#[tokio::test]
#[serial]
async fn actual_capacity_turnover_preserves_scoped_handles_until_hosted_deletion() {
    request::<App, _, _>(|_, ctx| async move {
        let dir = tempfile::Builder::new().prefix("cg054-stable-capacity-").tempdir_in("/tmp").unwrap();
        let registry = OrganizerRegistry::new(ctx.db.clone(), ctx.config.database.clone(), Edition::Hosted,
            "crabgate.com".into(), dir.path().into(), MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap()).unwrap();
        let selected = registry.create("Selected", "live0").await.unwrap();
        let (first, second) = tokio::join!(registry.resolve("live0.crabgate.com"), registry.resolve("live0.crabgate.com"));
        let first = first.unwrap().db;
        let second = second.unwrap().db;
        let raw_clone = first.clone();
        for n in 1..130 {
            registry.create("Live", &format!("live{n}")).await.unwrap();
        }
        let mut selected_handles = vec![first, second, raw_clone];
        for _ in 0..3 {
            let organizers: Vec<_> = registry.active_organizers().await.unwrap().into_iter().filter(|organizer| organizer.subdomain.starts_with("live")).collect();
            assert_eq!(organizers.len(), 130);
            for organizer in organizers {
                let scope = registry.resolve_organizer(&organizer).await.unwrap();
                assert_event_count(&scope.db).await;
                if scope.organizer.id == selected.id {
                    selected_handles.push(scope.db);
                }
            }
            let reinserted = registry.resolve("live0.crabgate.com").await.unwrap().db;
            assert_event_count(&reinserted).await;
            selected_handles.push(reinserted);
            for handle in &selected_handles { assert_event_count(handle).await; }
        }
        let other = registry.resolve("live1.crabgate.com").await.unwrap();
        let other_key = other.organizer.data_key.clone();
        let other_bytes = tokio::fs::read(&other.organizer.db_path).await.unwrap();
        let keys = ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM organizers WHERE data_key IS NOT NULL AND suspension_reason IS NULL AND subdomain GLOB 'live*'"
        )).await.unwrap().unwrap();
        assert_eq!(keys.try_get::<i64>("", "count").unwrap(), 130);
        let txn = selected_handles[0].begin().await.unwrap();
        lapse(&ctx.db, &selected.id).await;
        let job = hosted_jobs::run(&ctx, &registry, DELETE_AT);
        tokio::pin!(job);
        assert!(tokio::time::timeout(std::time::Duration::from_millis(100), &mut job).await.is_err());
        for handle in &selected_handles { assert_event_count(handle).await; }
        let pool = selected_handles[0].get_sqlite_connection_pool().clone();
        drop(selected_handles);
        tokio::select! {
            _ = pool.close_event() => {},
            result = &mut job => panic!("cleanup completed with a held transaction: {result:?}"),
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => panic!("cleanup never started closing"),
        }
        assert!(Path::new(&selected.db_path).exists());
        let row = txn.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite, "SELECT COUNT(*) AS count FROM events")).await.unwrap().unwrap();
        assert_eq!(row.try_get::<i64>("", "count").unwrap(), 0);
        txn.rollback().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), &mut job).await.unwrap().unwrap();
        assert_removed(Path::new(&selected.db_path)).await;
        let receipt = ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT completed_at FROM hosted_cleanup WHERE organizer_id=?", [selected.id.clone().into()]
        )).await.unwrap().unwrap();
        assert_eq!(receipt.try_get::<Option<i64>>("", "completed_at").unwrap(), Some(DELETE_AT));
        registry.prepare().await.unwrap();
        assert!(registry.resolve("live0.crabgate.com").await.is_err());
        assert_removed(Path::new(&selected.db_path)).await;
        assert_eq!(other.organizer.data_key, other_key);
        assert!(tokio::fs::read(&other.organizer.db_path).await.unwrap() == other_bytes);
        assert_event_count(&other.db).await;
        assert_eq!(registry.resolve("live1.crabgate.com").await.unwrap().organizer.data_key, other_key);
    }).await;
}

#[tokio::test]
#[serial]
async fn canceled_cleanup_keeps_pending_receipt_until_commit_rollback_or_drop_drains() {
    request::<App, _, _>(|_, ctx| async move {
        for release in ["commit", "rollback", "drop"] {
            let dir = tempfile::Builder::new().prefix("cg054-canceled-drain-").tempdir_in("/tmp").unwrap();
            let registry = std::sync::Arc::new(OrganizerRegistry::new(ctx.db.clone(), ctx.config.database.clone(), Edition::Hosted,
                "crabgate.com".into(), dir.path().into(), MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap()).unwrap());
            let org = registry.create("Expired", release).await.unwrap();
            let scope = registry.resolve(&format!("{release}.crabgate.com")).await.unwrap();
            let txn = scope.db.begin().await.unwrap();
            let pool = scope.db.get_sqlite_connection_pool().clone();
            drop(scope);
            lapse(&ctx.db, &org.id).await;
            let job = {
                let ctx = ctx.clone();
                let registry = std::sync::Arc::clone(&registry);
                tokio::spawn(async move { hosted_jobs::run(&ctx, &registry, DELETE_AT).await })
            };
            tokio::time::timeout(std::time::Duration::from_secs(10), pool.close_event()).await.unwrap();
            assert!(!job.is_finished());
            job.abort();
            assert!(job.await.unwrap_err().is_cancelled());
            let row = ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT o.data_key, s.status, c.completed_at FROM organizers o JOIN subscriptions s ON s.organizer_id=o.id JOIN hosted_cleanup c ON c.organizer_id=o.id WHERE o.id=?",
                [org.id.clone().into()])).await.unwrap().unwrap();
            assert_eq!(row.try_get::<Option<String>>("", "data_key").unwrap(), None);
            assert_eq!(row.try_get::<String>("", "status").unwrap(), "Deleted");
            assert_eq!(row.try_get::<Option<i64>>("", "completed_at").unwrap(), None);
            assert!(Path::new(&org.db_path).exists());
            assert!(registry.resolve(&format!("{release}.crabgate.com")).await.is_err());
            assert!(pool.is_closed());
            let row = txn.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite, "SELECT COUNT(*) AS count FROM events")).await.unwrap().unwrap();
            assert_eq!(row.try_get::<i64>("", "count").unwrap(), 0);
            let retry = hosted_jobs::run(&ctx, &registry, DELETE_AT + 1);
            tokio::pin!(retry);
            assert!(tokio::time::timeout(std::time::Duration::from_millis(100), &mut retry).await.is_err());
            assert!(Path::new(&org.db_path).exists());
            match release {
                "commit" => txn.commit().await.unwrap(),
                "rollback" => txn.rollback().await.unwrap(),
                "drop" => drop(txn),
                _ => unreachable!(),
            }
            tokio::time::timeout(std::time::Duration::from_secs(10), &mut retry).await.unwrap().unwrap();
            assert_removed(Path::new(&org.db_path)).await;
            let row = ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT completed_at FROM hosted_cleanup WHERE organizer_id=?", [org.id.clone().into()])).await.unwrap().unwrap();
            assert_eq!(row.try_get::<Option<i64>>("", "completed_at").unwrap(), Some(DELETE_AT + 1));
            registry.prepare().await.unwrap();
            assert!(registry.resolve(&format!("{release}.crabgate.com")).await.is_err());
            assert_removed(Path::new(&org.db_path)).await;
        }
    }).await;
}
