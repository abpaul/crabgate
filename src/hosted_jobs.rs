use loco_rs::app::AppContext;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement, TransactionTrait};

use crate::{
    hosted::{self, LifecycleAction},
    mailers::hosted::HostedMailer,
    organizers::OrganizerRegistry,
};

pub async fn run(ctx: &AppContext, registry: &OrganizerRegistry, now: i64) -> loco_rs::Result<()> {
    let mut failures = Vec::new();
    if registry.background_jobs_stopping() {
        return Ok(());
    }
    for action in hosted::due_lifecycle(&ctx.db, now).await.map_err(error)? {
        if registry.background_jobs_stopping() {
            break;
        }
        let organizer_id = match &action {
            LifecycleAction::Warn { organizer_id, .. }
            | LifecycleAction::Delete { organizer_id } => organizer_id.clone(),
        };
        if let Err(cause) = run_action(ctx, registry, now, action).await {
            tracing::error!(%organizer_id, error = %cause, "Organizer lifecycle action failed");
            failures.push(format!("{organizer_id}: {cause}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(error(format!(
            "{} lifecycle actions failed: {}",
            failures.len(),
            failures.join("; ")
        )))
    }
}

async fn run_action(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    now: i64,
    action: LifecycleAction,
) -> loco_rs::Result<()> {
    match action {
        LifecycleAction::Warn {
            organizer_id,
            days_left,
            lapse_generation,
        } => {
            let recipients = ctx
                .db
                .query_all_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT DISTINCT lower(trim(m.email)) AS email FROM memberships ms
                 JOIN members m ON m.id = ms.member_id
                 WHERE ms.organizer_id = ? AND ms.role = 'Admin' ORDER BY email",
                    [organizer_id.clone().into()],
                ))
                .await
                .map_err(error)?;
            if recipients.is_empty() {
                return Err(error("lifecycle warning has no current Admin recipient"));
            }
            let mut failures = Vec::new();
            for row in recipients {
                if registry.background_jobs_stopping() {
                    break;
                }
                let email: String = row.try_get("", "email").map_err(error)?;
                if let Err(cause) =
                    warn_recipient(ctx, &organizer_id, lapse_generation, days_left, &email, now)
                        .await
                {
                    failures.push(format!("{email}: {cause}"));
                }
            }
            if !failures.is_empty() {
                return Err(error(failures.join("; ")));
            }
        }
        LifecycleAction::Delete { organizer_id } => {
            delete_organizer(&ctx.db, &organizer_id, now, Some(registry)).await?;
        }
    }
    Ok(())
}

const WARNING_CLAIM_SECONDS: i64 = 60;
const WARNING_SEND_SECONDS: i64 = 10;

async fn warn_recipient(
    ctx: &AppContext,
    organizer_id: &str,
    lapse_generation: i64,
    days_left: i64,
    email: &str,
    now: i64,
) -> loco_rs::Result<()> {
    let token = uuid::Uuid::new_v4().to_string();
    let claim_now = chrono::Utc::now().timestamp();
    let claimed = ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO hosted_lifecycle_recipients
             (organizer_id, lapse_generation, days_left, email, attempts, claim_token, claim_until)
         SELECT s.organizer_id, s.lapse_generation, ?, ?, 1, ?, ? FROM subscriptions s
         WHERE s.organizer_id = ? AND s.status = 'Lapsed' AND s.lapse_generation = ?
         AND s.lapsed_at + 90 * 86400 > ?
         AND CASE WHEN s.lapsed_at + 89 * 86400 <= ? THEN 1
                  WHEN s.lapsed_at + 83 * 86400 <= ? THEN 7
                  WHEN s.lapsed_at + 60 * 86400 <= ? THEN 30 ELSE 0 END = ?
         AND EXISTS (SELECT 1 FROM memberships ms JOIN members m ON m.id = ms.member_id
             WHERE ms.organizer_id = s.organizer_id AND ms.role = 'Admin' AND lower(trim(m.email)) = ?)
         ON CONFLICT (organizer_id, lapse_generation, days_left, email) DO UPDATE SET
             attempts = attempts + 1,
             last_error = CASE WHEN claim_token IS NOT NULL
                 THEN COALESCE(last_error || '; ', '') || 'Previous warning claim expired; SMTP acceptance may be uncertain; retry may duplicate.'
                 ELSE last_error END,
             claim_token = excluded.claim_token, claim_until = excluded.claim_until
         WHERE sent_at IS NULL AND (claim_token IS NULL OR claim_until <= ?)",
        [days_left.into(), email.into(), token.clone().into(), (claim_now + WARNING_CLAIM_SECONDS).into(),
         organizer_id.into(), lapse_generation.into(), now.into(), now.into(), now.into(), now.into(),
         days_left.into(), email.into(), claim_now.into()]
    )).await.map_err(error)?.rows_affected();
    if claimed != 1 {
        return Ok(());
    }
    let current = ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT o.name FROM organizers o JOIN subscriptions s ON s.organizer_id = o.id
         JOIN hosted_lifecycle_recipients r ON r.organizer_id = o.id AND r.lapse_generation = s.lapse_generation
         WHERE o.id = ? AND s.status = 'Lapsed' AND s.lapse_generation = ?
         AND s.lapsed_at + 90 * 86400 > ?
         AND CASE WHEN s.lapsed_at + 89 * 86400 <= ? THEN 1
                  WHEN s.lapsed_at + 83 * 86400 <= ? THEN 7
                  WHEN s.lapsed_at + 60 * 86400 <= ? THEN 30 ELSE 0 END = ?
         AND r.days_left = ? AND r.email = ? AND r.sent_at IS NULL AND r.claim_token = ? AND r.claim_until > ?
         AND EXISTS (SELECT 1 FROM memberships ms JOIN members m ON m.id = ms.member_id
             WHERE ms.organizer_id = o.id AND ms.role = 'Admin' AND lower(trim(m.email)) = ?)",
        [organizer_id.into(), lapse_generation.into(), now.into(), now.into(), now.into(), now.into(),
         days_left.into(), days_left.into(), email.into(), token.clone().into(),
         (chrono::Utc::now().timestamp() + WARNING_SEND_SECONDS).into(), email.into()]
    )).await.map_err(error)?;
    let Some(row) = current else {
        ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "UPDATE hosted_lifecycle_recipients SET claim_token = NULL, claim_until = NULL
             WHERE organizer_id = ? AND lapse_generation = ? AND days_left = ? AND email = ? AND claim_token = ?",
            [organizer_id.into(), lapse_generation.into(), days_left.into(), email.into(), token.into()]
        )).await.map_err(error)?;
        return Ok(());
    };
    let name: String = row.try_get("", "name").map_err(error)?;
    let result = HostedMailer::warning(ctx, email, &name, days_left).await;
    let sent_at = result.as_ref().ok().map(|_| now);
    let last_error = result.as_ref().err().map(ToString::to_string);
    let recorded = ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE hosted_lifecycle_recipients SET sent_at = ?, last_error = COALESCE(?, last_error),
             claim_token = NULL, claim_until = NULL
         WHERE organizer_id = ? AND lapse_generation = ? AND days_left = ? AND email = ?
         AND sent_at IS NULL AND claim_token = ? AND claim_until > ?",
        [sent_at.into(), last_error.into(), organizer_id.into(), lapse_generation.into(), days_left.into(),
         email.into(), token.into(), chrono::Utc::now().timestamp().into()]
    )).await.map_err(|cause| error(format!("warning receipt write failed; SMTP acceptance may be uncertain: {cause}")))?.rows_affected();
    if recorded != 1 {
        return Err(error(
            "warning claim is no longer owned; SMTP acceptance may be uncertain",
        ));
    }
    result
}

async fn delete_organizer(
    control: &DatabaseConnection,
    organizer_id: &str,
    now: i64,
    registry: Option<&OrganizerRegistry>,
) -> loco_rs::Result<()> {
    hosted::mark_deleted(control, organizer_id, now)
        .await
        .map_err(error)?;
    let guard = match registry {
        Some(registry) => Some(registry.cleanup_guard().await),
        None => None,
    };
    let transaction = control.begin().await.map_err(error)?;
    // Keep eligibility and path identity stable through unlink and receipt commit.
    let locked = transaction.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE hosted_cleanup SET last_error = last_error WHERE organizer_id = ? AND completed_at IS NULL",
        [organizer_id.into()],
    )).await.map_err(error)?;
    if locked.rows_affected() == 0 {
        transaction.rollback().await.map_err(error)?;
        return Ok(());
    }
    let result = async {
        let row = transaction
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT c.db_path FROM hosted_cleanup c
             JOIN organizers o ON o.id = c.organizer_id
             JOIN subscriptions s ON s.organizer_id = o.id
             WHERE c.organizer_id = ? AND c.completed_at IS NULL
               AND s.status = 'Deleted' AND o.data_key IS NULL AND o.db_path = c.db_path",
                [organizer_id.into()],
            ))
            .await
            .map_err(error)?
            .ok_or_else(|| error("cleanup eligibility or database identity changed"))?;
        let path: String = row.try_get("", "db_path").map_err(error)?;
        let path = std::path::PathBuf::from(path);
        if path.file_name().and_then(|name| name.to_str()) != Some(&format!("{organizer_id}.db")) {
            return Err(error(
                "cleanup database filename does not match organizer identity",
            ));
        }
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir().map_err(error)?.join(&path)
        };
        let parent = absolute
            .parent()
            .ok_or_else(|| error("cleanup database has no parent"))?;
        if tokio::fs::symlink_metadata(parent)
            .await
            .map_err(error)?
            .file_type()
            .is_symlink()
        {
            return Err(error("cleanup database parent is a symlink"));
        }
        let absolute = tokio::fs::canonicalize(parent).await.map_err(error)?.join(
            path.file_name()
                .ok_or_else(|| error("cleanup database has no filename"))?,
        );
        let other_paths = transaction
            .query_all_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT db_path FROM organizers WHERE id != ?",
                [organizer_id.into()],
            ))
            .await
            .map_err(error)?;
        for other in other_paths {
            let other =
                std::path::PathBuf::from(other.try_get::<String>("", "db_path").map_err(error)?);
            let Some(parent) = other.parent() else {
                continue;
            };
            let Some(name) = other.file_name() else {
                continue;
            };
            let normalized = match tokio::fs::canonicalize(parent).await {
                Ok(parent) => parent.join(name),
                Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => continue,
                Err(cause) => return Err(error(cause)),
            };
            for suffix in ["", "-wal", "-shm"] {
                if normalized == std::path::Path::new(&format!("{}{suffix}", absolute.display())) {
                    return Err(error("cleanup path belongs to another organizer"));
                }
            }
        }
        let databases = transaction
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "PRAGMA database_list",
            ))
            .await
            .map_err(error)?;
        for database in databases {
            let file: String = database.try_get("", "file").map_err(error)?;
            if !file.is_empty() && tokio::fs::canonicalize(&file).await.map_err(error)? == absolute
            {
                return Err(error("cleanup path belongs to the control database"));
            }
        }
        if let (Some(registry), Some(guard)) = (registry, guard.as_ref()) {
            registry
                .close_deleted(organizer_id, &absolute, guard)
                .await
                .map_err(error)?;
        }
        for suffix in ["", "-wal", "-shm"] {
            let file = format!("{}{suffix}", absolute.display());
            match tokio::fs::symlink_metadata(&file).await {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(error(format!("cleanup refuses symlink: {file}")));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(cause) => return Err(error(format!("cleanup metadata {file}: {cause}"))),
            }
            match tokio::fs::remove_file(&file).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(cause) => return Err(error(format!("cleanup remove {file}: {cause}"))),
            }
        }
        Ok(())
    }
    .await;
    let failure = result.as_ref().err().map(ToString::to_string);
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE hosted_cleanup SET completed_at = ?, last_error = COALESCE(?, last_error) WHERE organizer_id = ?",
            [
                if result.is_ok() { Some(now) } else { None }.into(),
                failure.clone().into(),
                organizer_id.into(),
            ],
        ))
        .await
        .map_err(error)?;
    transaction.commit().await.map_err(error)?;
    if let Some(failure) = failure {
        tracing::error!(%organizer_id, error = %failure, "Organizer file cleanup remains pending");
    }
    result
}

fn error(value: impl std::fmt::Display) -> loco_rs::Error {
    loco_rs::Error::string(&value.to_string())
}

#[cfg(test)]
mod tests {
    use sea_orm::{DatabaseConnection, QueryResult};

    use super::*;

    const DAY: i64 = 86_400;
    const LAPSED_AT: i64 = 1_700_000_000;
    const DELETE_AT: i64 = LAPSED_AT + 90 * DAY;

    async fn fixture() -> (tempfile::TempDir, DatabaseConnection) {
        let dir = tempfile::Builder::new()
            .prefix("cg054-")
            .tempdir_in("/tmp")
            .unwrap();
        let control = sea_orm::Database::connect(format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("control.db").display()
        ))
        .await
        .unwrap();
        loco_rs::db::migrate::<migration::Migrator>(&control)
            .await
            .unwrap();
        let path = dir.path().join("organizer.db").to_str().unwrap().to_owned();
        for suffix in ["", "-wal", "-shm"] {
            tokio::fs::write(format!("{path}{suffix}"), b"retained organizer data")
                .await
                .unwrap();
        }
        control.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO organizers (id, name, subdomain, db_path, data_key) VALUES ('organizer', 'Arts', 'arts', ?, 'wrapped-key')",
            [path.into()],
        )).await.unwrap();
        control.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO subscriptions (id, organizer_id, status, lapsed_at, stripe_checkout_session_id) VALUES ('subscription', 'organizer', 'Lapsed', ?, 'checkout')",
            [LAPSED_AT.into()],
        )).await.unwrap();
        (dir, control)
    }

    async fn state(control: &DatabaseConnection) -> QueryResult {
        control.query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT s.status, o.data_key FROM subscriptions s JOIN organizers o ON o.id = s.organizer_id WHERE o.id = 'organizer'",
        )).await.unwrap().unwrap()
    }

    async fn receipt(control: &DatabaseConnection) -> QueryResult {
        control.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT completed_at, last_error, db_path FROM hosted_cleanup WHERE organizer_id = 'organizer'"
        )).await.unwrap().unwrap()
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn stale_warning_actions_cannot_send_or_receipt_a_reactivated_or_new_lapse() {
        use crate::{
            app::App,
            crypto::MasterKey,
            members::{self, Role},
            organizers::Edition,
        };
        loco_rs::testing::prelude::request::<App, _, _>(|_, ctx| async move {
            let root = std::env::var_os("CARGO_TARGET_DIR").map(std::path::PathBuf::from)
                .and_then(|p| p.parent().map(std::path::Path::to_path_buf)).unwrap_or_else(std::env::temp_dir);
            let storage = tempfile::Builder::new().prefix("cg055-stale-").tempdir_in(root).unwrap().keep();
            println!("CG-055 stale action evidence: {}", storage.display());
            let registry = OrganizerRegistry::new(ctx.db.clone(), ctx.config.database.clone(), Edition::Hosted,
                "crabgate.com".to_owned(), storage,
                MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap()).unwrap();
            let organizer = registry.create("Stale", "stale").await.unwrap();
            members::add_member(&ctx.db, &organizer.id, "admin@example.test", Role::Admin).await.unwrap();
            ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "INSERT INTO subscriptions(id,organizer_id,status,lapsed_at) VALUES ('stale-sub',?,'Lapsed',?)",
                [organizer.id.clone().into(), LAPSED_AT.into()])).await.unwrap();
            let action = hosted::due_lifecycle(&ctx.db, LAPSED_AT + 60 * DAY).await.unwrap().remove(0);
            hosted::subscription_active(&ctx.db, &organizer.id, "cus", "sub", LAPSED_AT + 60 * DAY, 1).await.unwrap();
            run_action(&ctx, &registry, LAPSED_AT + 60 * DAY, action).await.unwrap();
            assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 0);
            hosted::subscription_lapsed(&ctx.db, &organizer.id, LAPSED_AT).await.unwrap();
            run_action(&ctx, &registry, LAPSED_AT + 60 * DAY, LifecycleAction::Warn {
                organizer_id: organizer.id.clone(), days_left: 30, lapse_generation: 1,
            }).await.unwrap();
            assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 0);
            let row = ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM hosted_lifecycle_recipients")).await.unwrap().unwrap();
            assert_eq!(row.try_get::<i64>("", "count").unwrap(), 0);
            run(&ctx, &registry, LAPSED_AT + 60 * DAY).await.unwrap();
            assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 1);
            let row = ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
                "SELECT lapse_generation, sent_at FROM hosted_lifecycle_recipients")).await.unwrap().unwrap();
            assert_eq!(row.try_get::<i64>("", "lapse_generation").unwrap(), 2);
            assert_eq!(row.try_get::<i64>("", "sent_at").unwrap(), LAPSED_AT + 60 * DAY);
        }).await;
    }

    #[tokio::test]
    async fn migration_recovers_existing_deleted_rows_without_keys() {
        use migration::MigratorTrait;
        let (dir, control) = fixture().await;
        let migrations = migration::Migrator::migrations();
        let migration_index = migrations
            .iter()
            .position(|migration| migration.name() == "m20261007_000021_hosted_cleanup")
            .expect("target migration is registered");
        let rollback_steps = u32::try_from(migrations.len() - migration_index).unwrap();
        migration::Migrator::down(&control, Some(rollback_steps))
            .await
            .unwrap();
        control.execute_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "UPDATE subscriptions SET status = 'Deleted'; UPDATE organizers SET data_key = NULL;"
        )).await.unwrap();
        loco_rs::db::migrate::<migration::Migrator>(&control)
            .await
            .unwrap();
        assert_eq!(
            receipt(&control)
                .await
                .try_get::<Option<i64>>("", "completed_at")
                .unwrap(),
            None
        );
        assert_eq!(
            hosted::due_lifecycle(&control, DELETE_AT).await.unwrap(),
            vec![LifecycleAction::Delete {
                organizer_id: "organizer".to_owned()
            }]
        );
        delete_organizer(&control, "organizer", DELETE_AT, None)
            .await
            .unwrap();
        assert_files(&dir, false).await;
    }

    #[tokio::test]
    async fn interrupted_cleanup_survives_reopen_and_concurrent_retries() {
        for removed in [0, 1, 2] {
            let (dir, control) = fixture().await;
            assert!(hosted::mark_deleted(&control, "organizer", DELETE_AT)
                .await
                .unwrap());
            for suffix in ["", "-wal", "-shm"].into_iter().take(removed) {
                tokio::fs::remove_file(dir.path().join(format!("organizer.db{suffix}")))
                    .await
                    .unwrap();
            }
            control.close().await.unwrap();
            let control = sea_orm::Database::connect(format!(
                "sqlite://{}?mode=rw",
                dir.path().join("control.db").display()
            ))
            .await
            .unwrap();
            assert_eq!(
                receipt(&control)
                    .await
                    .try_get::<Option<i64>>("", "completed_at")
                    .unwrap(),
                None
            );
            assert_eq!(
                hosted::due_lifecycle(&control, DELETE_AT + 1)
                    .await
                    .unwrap(),
                vec![LifecycleAction::Delete {
                    organizer_id: "organizer".to_owned()
                }]
            );
            let (first, second) = tokio::join!(
                delete_organizer(&control, "organizer", DELETE_AT + 1, None),
                delete_organizer(&control, "organizer", DELETE_AT + 1, None)
            );
            first.unwrap();
            second.unwrap();
            assert_files(&dir, false).await;
            assert_eq!(
                receipt(&control)
                    .await
                    .try_get::<Option<i64>>("", "completed_at")
                    .unwrap(),
                Some(DELETE_AT + 1)
            );
            assert!(hosted::due_lifecycle(&control, DELETE_AT + 2)
                .await
                .unwrap()
                .is_empty());
            let row = state(&control).await;
            assert_eq!(row.try_get::<String>("", "status").unwrap(), "Deleted");
            assert_eq!(row.try_get::<Option<String>>("", "data_key").unwrap(), None);
        }
    }

    #[tokio::test]
    async fn pending_cleanup_rechecks_state_key_and_path() {
        for change in [
            "UPDATE subscriptions SET status = 'Active' WHERE organizer_id = 'organizer'",
            "UPDATE organizers SET data_key = 'restored-key' WHERE id = 'organizer'",
            "UPDATE organizers SET db_path = db_path || '.different' WHERE id = 'organizer'",
        ] {
            let (dir, control) = fixture().await;
            hosted::mark_deleted(&control, "organizer", DELETE_AT)
                .await
                .unwrap();
            control
                .execute_raw(Statement::from_string(DatabaseBackend::Sqlite, change))
                .await
                .unwrap();
            assert!(delete_organizer(&control, "organizer", DELETE_AT + 1, None)
                .await
                .is_err());
            assert_files(&dir, true).await;
            let receipt = receipt(&control).await;
            assert_eq!(
                receipt.try_get::<Option<i64>>("", "completed_at").unwrap(),
                None
            );
            assert_eq!(
                receipt.try_get::<String>("", "last_error").unwrap(),
                "cleanup eligibility or database identity changed"
            );
        }
    }

    #[tokio::test]
    async fn cleanup_cannot_unlink_its_control_database() {
        let dir = tempfile::Builder::new()
            .prefix("cg054-control-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("organizer.db");
        let control = sea_orm::Database::connect(format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        loco_rs::db::migrate::<migration::Migrator>(&control)
            .await
            .unwrap();
        control.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO organizers (id, name, subdomain, db_path, data_key) VALUES ('organizer', 'Arts', 'arts', ?, 'wrapped-key');",
            [path.to_str().unwrap().into()]
        )).await.unwrap();
        control.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO subscriptions (id, organizer_id, status, lapsed_at) VALUES ('subscription', 'organizer', 'Lapsed', ?)",
            [LAPSED_AT.into()]
        )).await.unwrap();
        assert!(delete_organizer(&control, "organizer", DELETE_AT, None)
            .await
            .is_err());
        assert!(path.is_file());
        assert_eq!(
            receipt(&control)
                .await
                .try_get::<String>("", "last_error")
                .unwrap(),
            "cleanup path belongs to the control database"
        );
        assert_eq!(
            state(&control)
                .await
                .try_get::<String>("", "status")
                .unwrap(),
            "Deleted"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_refuses_a_symlink_parent() {
        let (dir, control) = fixture().await;
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
        control
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE organizers SET db_path = ? WHERE id = 'organizer'",
                [alias.join("organizer.db").to_str().unwrap().into()],
            ))
            .await
            .unwrap();
        assert!(delete_organizer(&control, "organizer", DELETE_AT, None)
            .await
            .is_err());
        assert_files(&dir, true).await;
        assert_eq!(
            receipt(&control)
                .await
                .try_get::<String>("", "last_error")
                .unwrap(),
            "cleanup database parent is a symlink"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_refuses_symlink_and_shared_tenant_path() {
        let (dir, control) = fixture().await;
        let other = dir.path().join("other.db");
        tokio::fs::write(&other, b"other organizer bytes")
            .await
            .unwrap();
        tokio::fs::remove_file(dir.path().join("organizer.db"))
            .await
            .unwrap();
        std::os::unix::fs::symlink(&other, dir.path().join("organizer.db")).unwrap();
        assert!(delete_organizer(&control, "organizer", DELETE_AT, None)
            .await
            .is_err());
        assert_eq!(
            tokio::fs::read(&other).await.unwrap(),
            b"other organizer bytes"
        );
        assert!(dir.path().join("organizer.db").is_symlink());
        tokio::fs::remove_file(dir.path().join("organizer.db"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("organizer.db"), b"retained organizer data")
            .await
            .unwrap();
        control.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO organizers (id, name, subdomain, db_path, data_key) VALUES ('other', 'Other', 'other', ?, 'other-key')",
            [format!("{}/./organizer.db", dir.path().display()).into()]
        )).await.unwrap();
        assert!(delete_organizer(&control, "organizer", DELETE_AT + 1, None)
            .await
            .is_err());
        assert_files(&dir, true).await;
        assert_eq!(
            receipt(&control)
                .await
                .try_get::<String>("", "last_error")
                .unwrap(),
            "cleanup path belongs to another organizer"
        );
    }

    #[tokio::test]
    async fn removal_failure_is_selected_again_and_recovers() {
        let (dir, control) = fixture().await;
        let wal = dir.path().join("organizer.db-wal");
        tokio::fs::remove_file(&wal).await.unwrap();
        tokio::fs::create_dir(&wal).await.unwrap();
        assert!(delete_organizer(&control, "organizer", DELETE_AT, None)
            .await
            .is_err());
        assert!(!dir.path().join("organizer.db").exists());
        assert!(wal.is_dir());
        assert!(dir.path().join("organizer.db-shm").exists());
        let pending = receipt(&control).await;
        assert_eq!(
            pending.try_get::<Option<i64>>("", "completed_at").unwrap(),
            None
        );
        let failure = pending.try_get::<String>("", "last_error").unwrap();
        assert!(failure.contains("cleanup remove"));
        assert!(failure.contains("organizer.db-wal"));
        let row = state(&control).await;
        assert_eq!(row.try_get::<String>("", "status").unwrap(), "Deleted");
        assert_eq!(row.try_get::<Option<String>>("", "data_key").unwrap(), None);
        assert_eq!(
            hosted::due_lifecycle(&control, DELETE_AT + 1)
                .await
                .unwrap(),
            vec![LifecycleAction::Delete {
                organizer_id: "organizer".to_owned()
            }]
        );
        tokio::fs::remove_dir(&wal).await.unwrap();
        tokio::fs::write(&wal, b"retained organizer data")
            .await
            .unwrap();
        delete_organizer(&control, "organizer", DELETE_AT + 1, None)
            .await
            .unwrap();
        assert_files(&dir, false).await;
        let completed = receipt(&control).await;
        assert_eq!(
            completed
                .try_get::<Option<i64>>("", "completed_at")
                .unwrap(),
            Some(DELETE_AT + 1)
        );
        assert_eq!(
            completed.try_get::<String>("", "last_error").unwrap(),
            failure
        );
    }

    #[tokio::test]
    async fn selected_deletion_cannot_destroy_a_reactivated_key() {
        let (_dir, control) = fixture().await;
        assert_eq!(
            hosted::due_lifecycle(&control, DELETE_AT).await.unwrap(),
            vec![LifecycleAction::Delete {
                organizer_id: "organizer".to_owned()
            }]
        );
        hosted::subscription_active(
            &control,
            "organizer",
            "customer",
            "subscription",
            DELETE_AT,
            DELETE_AT,
        )
        .await
        .unwrap();
        assert!(!hosted::mark_deleted(&control, "organizer", DELETE_AT)
            .await
            .unwrap());
        delete_organizer(&control, "organizer", DELETE_AT, None)
            .await
            .unwrap();
        assert_files(&_dir, true).await;
        let row = state(&control).await;
        assert_eq!(row.try_get::<String>("", "status").unwrap(), "Active");
        assert_eq!(
            row.try_get::<Option<String>>("", "data_key")
                .unwrap()
                .as_deref(),
            Some("wrapped-key")
        );
    }

    async fn assert_files(dir: &tempfile::TempDir, present: bool) {
        for suffix in ["", "-wal", "-shm"] {
            let file = dir.path().join(format!("organizer.db{suffix}"));
            assert_eq!(file.exists(), present, "{}", file.display());
            if present {
                assert_eq!(
                    tokio::fs::read(&file).await.unwrap(),
                    b"retained organizer data"
                );
            }
        }
    }

    #[tokio::test]
    async fn deletion_requires_the_current_ninety_day_deadline() {
        let (dir, control) = fixture().await;
        delete_organizer(&control, "organizer", DELETE_AT - 1, None)
            .await
            .unwrap();
        assert_files(&dir, true).await;
        assert_eq!(
            state(&control)
                .await
                .try_get::<String>("", "status")
                .unwrap(),
            "Lapsed"
        );
        assert_eq!(
            hosted::due_lifecycle(&control, DELETE_AT).await.unwrap(),
            vec![LifecycleAction::Delete {
                organizer_id: "organizer".to_owned()
            }]
        );
        hosted::subscription_active(
            &control,
            "organizer",
            "customer",
            "subscription",
            DELETE_AT,
            DELETE_AT,
        )
        .await
        .unwrap();
        hosted::subscription_lapsed(&control, "organizer", DELETE_AT)
            .await
            .unwrap();
        delete_organizer(&control, "organizer", DELETE_AT, None)
            .await
            .unwrap();
        assert_files(&dir, true).await;
        assert_eq!(
            state(&control)
                .await
                .try_get::<Option<String>>("", "data_key")
                .unwrap()
                .as_deref(),
            Some("wrapped-key")
        );
        delete_organizer(&control, "organizer", DELETE_AT + 90 * DAY, None)
            .await
            .unwrap();
        assert_files(&dir, false).await;
        let row = state(&control).await;
        assert_eq!(row.try_get::<String>("", "status").unwrap(), "Deleted");
        assert_eq!(row.try_get::<Option<String>>("", "data_key").unwrap(), None);
    }

    #[tokio::test]
    async fn successful_deletion_removes_files_and_cannot_be_reactivated() {
        let (dir, control) = fixture().await;
        delete_organizer(&control, "organizer", DELETE_AT, None)
            .await
            .unwrap();
        assert_files(&dir, false).await;
        assert!(hosted::due_lifecycle(&control, DELETE_AT)
            .await
            .unwrap()
            .is_empty());
        assert!(!hosted::mark_deleted(&control, "organizer", DELETE_AT)
            .await
            .unwrap());
        delete_organizer(&control, "organizer", DELETE_AT, None)
            .await
            .unwrap();
        hosted::subscription_active(
            &control,
            "organizer",
            "customer",
            "subscription",
            DELETE_AT + 1,
            DELETE_AT + 1,
        )
        .await
        .unwrap();
        assert!(!hosted::subscription_checkout_complete(
            &control,
            "organizer",
            "checkout",
            "customer",
            "subscription",
            DELETE_AT + 1,
            DELETE_AT + 1
        )
        .await
        .unwrap());
        hosted::subscription_lapsed(&control, "organizer", DELETE_AT + 1)
            .await
            .unwrap();
        let row = state(&control).await;
        assert_eq!(row.try_get::<String>("", "status").unwrap(), "Deleted");
        assert_eq!(row.try_get::<Option<String>>("", "data_key").unwrap(), None);
    }

    #[tokio::test]
    async fn ineligible_and_unknown_organizers_do_not_lose_keys_or_files() {
        let (dir, control) = fixture().await;
        for status in ["Pending", "Active", "Grace", "Deleted"] {
            control
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "UPDATE subscriptions SET status = ? WHERE organizer_id = 'organizer'",
                    [status.into()],
                ))
                .await
                .unwrap();
            delete_organizer(&control, "organizer", DELETE_AT, None)
                .await
                .unwrap();
            assert_files(&dir, true).await;
            let row = state(&control).await;
            assert_eq!(row.try_get::<String>("", "status").unwrap(), status);
            assert_eq!(
                row.try_get::<Option<String>>("", "data_key")
                    .unwrap()
                    .as_deref(),
                Some("wrapped-key")
            );
        }
        control.execute_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "UPDATE subscriptions SET status = 'Lapsed', lapsed_at = NULL WHERE organizer_id = 'organizer'"
        )).await.unwrap();
        delete_organizer(&control, "organizer", DELETE_AT, None)
            .await
            .unwrap();
        delete_organizer(&control, "unknown", DELETE_AT, None)
            .await
            .unwrap();
        assert_files(&dir, true).await;
        assert_eq!(
            state(&control)
                .await
                .try_get::<Option<String>>("", "data_key")
                .unwrap()
                .as_deref(),
            Some("wrapped-key")
        );
    }

    #[tokio::test]
    async fn concurrent_deletion_attempts_claim_once() {
        let (_dir, control) = fixture().await;
        let (first, second) = tokio::join!(
            hosted::mark_deleted(&control, "organizer", DELETE_AT),
            hosted::mark_deleted(&control, "organizer", DELETE_AT),
        );
        assert_eq!(
            usize::from(first.unwrap()) + usize::from(second.unwrap()),
            1
        );
        assert_eq!(
            state(&control)
                .await
                .try_get::<String>("", "status")
                .unwrap(),
            "Deleted"
        );
        assert_eq!(
            state(&control)
                .await
                .try_get::<Option<String>>("", "data_key")
                .unwrap(),
            None
        );
    }
}
