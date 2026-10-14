use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
    response::IntoResponse,
    Extension,
};
use chrono::Utc;
use loco_rs::{config::Database, db};
use migration::{MigratorTrait, OrganizerMigrator};
use moka::future::Cache;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, Statement};
use uuid::Uuid;

use crate::{
    crypto::{CryptoError, MasterKey, OrganizerCipher},
    members::{self, Role},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edition {
    SelfHosted,
    Hosted,
}

pub async fn self_hosted_signup_required(control: &DatabaseConnection) -> Result<bool, DbErr> {
    if Edition::from_environment() != Edition::SelfHosted
        || std::env::var("CRABGATE_ADMIN_EMAIL").is_err()
    {
        return Ok(false);
    }
    let row = control
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT onboarding_at FROM organizers WHERE subdomain = 'local'".to_owned(),
        ))
        .await?;
    match row {
        Some(row) => Ok(row.try_get::<Option<i64>>("", "onboarding_at")?.is_none()),
        None => Ok(false),
    }
}

impl Edition {
    pub fn from_environment() -> Self {
        if std::env::var("CRABGATE_EDITION").as_deref() == Ok("hosted") {
            Self::Hosted
        } else {
            Self::SelfHosted
        }
    }
}

#[derive(Clone, Debug)]
pub struct Organizer {
    pub id: String,
    pub name: String,
    pub subdomain: String,
    pub db_path: String,
    pub suspension_reason: Option<String>,
    pub data_key: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum OrganizerError {
    #[error("Organizer not found")]
    NotFound,
    #[error("Organizer is suspended")]
    Suspended,
    #[error("Organizer migrations or schema are not ready")]
    NotReady,
    #[error("Organizer is not suspended by a migration failure")]
    RecoveryNotAllowed,
    #[error("invalid Organizer subdomain")]
    InvalidSubdomain,
    #[error(transparent)]
    Database(#[from] DbErr),
    #[error(transparent)]
    Tenancy(#[from] tenant_sqlite_loco::Error),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Member(#[from] members::MemberError),
}

pub struct OrganizerRegistry {
    control: DatabaseConnection,
    edition: Edition,
    base_domain: String,
    database_dir: PathBuf,
    master_key: MasterKey,
    pools: tenant_sqlite_loco::TenantPools,
    connection_gate: Arc<tokio::sync::RwLock<()>>,
    event_pages: Cache<String, String>,
    background_stop: Arc<AtomicBool>,
}

pub struct OrganizerDb {
    pub organizer: Organizer,
    pub db: tenant_sqlite_loco::TenantHandle,
}

impl OrganizerRegistry {
    pub fn new(
        control: DatabaseConnection,
        database_config: Database,
        edition: Edition,
        base_domain: String,
        database_dir: PathBuf,
        master_key: MasterKey,
    ) -> Result<Self, OrganizerError> {
        let pool_config = tenant_sqlite_loco::config_from_database(&database_dir, &database_config);
        let pools = tenant_sqlite_loco::TenantPools::new::<OrganizerMigrator>(pool_config)?;
        Ok(Self {
            control,
            edition,
            base_domain,
            database_dir,
            master_key,
            connection_gate: Arc::default(),
            pools,
            background_stop: Arc::default(),
            event_pages: Cache::builder()
                .max_capacity(1000)
                .time_to_live(Duration::from_secs(1))
                .build(),
        })
    }

    pub(crate) fn pool_service(&self) -> &tenant_sqlite_loco::TenantPools {
        &self.pools
    }

    pub(crate) fn stop_background_jobs(&self) {
        self.background_stop.store(true, Ordering::SeqCst);
    }

    pub(crate) fn background_jobs_stopping(&self) -> bool {
        self.background_stop.load(Ordering::SeqCst)
    }

    pub(crate) fn background_stop(&self) -> Arc<AtomicBool> {
        self.background_stop.clone()
    }

    pub fn edition(&self) -> Edition {
        self.edition
    }

    pub async fn event_page(&self, organizer_id: &str, event_id: &str) -> Option<String> {
        self.event_pages
            .get(&format!("{organizer_id}/{event_id}"))
            .await
    }

    pub async fn cache_event_page(&self, organizer_id: &str, event_id: &str, html: String) {
        self.event_pages
            .insert(format!("{organizer_id}/{event_id}"), html)
            .await;
    }

    pub async fn prepare(&self) -> Result<(), OrganizerError> {
        if self.edition == Edition::SelfHosted && self.all().await?.is_empty() {
            let name = std::env::var("CRABGATE_ORGANIZER_NAME")
                .unwrap_or_else(|_| "My Organizer".to_owned());
            self.create(&name, "local").await?;
        }
        if self.edition == Edition::SelfHosted {
            if let Ok(email) = std::env::var("CRABGATE_ADMIN_EMAIL") {
                if let Some(organizer) = self.all().await?.into_iter().next() {
                    let count = self
                        .control
                        .query_one_raw(Statement::from_sql_and_values(
                            DatabaseBackend::Sqlite,
                            "SELECT COUNT(*) AS count FROM memberships WHERE organizer_id = ?",
                            [organizer.id.clone().into()],
                        ))
                        .await?
                        .ok_or(OrganizerError::NotFound)?;
                    if count.try_get::<i64>("", "count")? == 0 {
                        members::add_member(&self.control, &organizer.id, &email, Role::Admin)
                            .await?;
                    }
                }
            }
        }
        let _gate = self.connection_gate.read().await;
        for organizer in self.all().await? {
            if organizer.data_key.is_none() {
                continue;
            }
            let migration = async {
                let pool = self.open(&organizer).await?;
                db::migrate::<OrganizerMigrator>(&pool).await?;
                Ok::<_, OrganizerError>(())
            };
            if let Err(error) = migration.await {
                tracing::error!(organizer_id = %organizer.id, %error, "Organizer migration failed");
                self.control
                    .execute_raw(Statement::from_sql_and_values(
                        DatabaseBackend::Sqlite,
                        "UPDATE organizers SET suspension_reason = 'migration failed' WHERE id = ? AND suspension_reason IS NULL AND data_key IS NOT NULL",
                        [organizer.id.into()],
                    ))
                    .await?;
            }
        }
        Ok(())
    }

    async fn target(&self, id: &str) -> Result<Organizer, OrganizerError> {
        let row = self.control.query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT o.id, o.name, o.subdomain, o.db_path, o.suspension_reason, o.data_key FROM organizers o WHERE o.id = ? AND o.data_key IS NOT NULL AND NOT EXISTS (SELECT 1 FROM subscriptions s WHERE s.organizer_id = o.id AND s.status = 'Deleted')",
            [id.into()],
        )).await?.ok_or(OrganizerError::NotFound)?;
        organizer_from_row(row)
    }

    async fn verify_schema(
        &self,
        organizer: &Organizer,
        db: &DatabaseConnection,
    ) -> Result<String, OrganizerError> {
        self.cipher(organizer)?;
        if !OrganizerMigrator::get_pending_migrations_read_only(db)
            .await?
            .is_empty()
        {
            return Err(OrganizerError::NotReady);
        }
        db.query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT e.id, e.series_review_reason, x.reason FROM events e LEFT JOIN series_exceptions x ON x.series_id = e.series_id LIMIT 1",
        )).await?;
        OrganizerMigrator::migrations()
            .last()
            .map(|migration| migration.name().to_owned())
            .ok_or(OrganizerError::NotReady)
    }

    pub async fn tenant_readiness(&self, id: &str) -> Result<String, OrganizerError> {
        let _gate = self.connection_gate.read().await;
        let organizer = self.target(id).await?;
        if organizer.suspension_reason.is_some() {
            return Err(OrganizerError::Suspended);
        }
        let db = self.open(&organizer).await?;
        self.verify_schema(&organizer, &db).await
    }

    pub async fn retry_migration(&self, id: &str) -> Result<String, OrganizerError> {
        let _gate = self.connection_gate.write().await;
        let organizer = self.target(id).await?;
        if organizer.suspension_reason.as_deref() != Some("migration failed") {
            return Err(OrganizerError::RecoveryNotAllowed);
        }
        let db = self.open(&organizer).await?;
        db::migrate::<OrganizerMigrator>(&db).await?;
        let latest = self.verify_schema(&organizer, &db).await?;
        let changed = self.control.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE organizers SET suspension_reason = NULL WHERE id = ? AND suspension_reason = 'migration failed' AND data_key = ? AND NOT EXISTS (SELECT 1 FROM subscriptions s WHERE s.organizer_id = organizers.id AND s.status = 'Deleted')",
            [id.into(), organizer.data_key.into()],
        )).await?;
        if changed.rows_affected() != 1 {
            return Err(OrganizerError::RecoveryNotAllowed);
        }
        Ok(latest)
    }

    pub async fn create(&self, name: &str, subdomain: &str) -> Result<Organizer, OrganizerError> {
        let _gate = self.connection_gate.read().await;
        if !valid_subdomain(subdomain) {
            return Err(OrganizerError::InvalidSubdomain);
        }
        let id = Uuid::new_v4().to_string();
        let path = self.database_dir.join(format!("{id}.db"));
        let db_path = path.to_string_lossy().into_owned();
        let (data_key, _) = self.master_key.generate_organizer_key(&id)?;
        let organizer = Organizer {
            id,
            name: name.to_owned(),
            subdomain: subdomain.to_owned(),
            db_path,
            suspension_reason: None,
            data_key: Some(data_key),
        };
        let tenant_id = tenant_sqlite_loco::TenantId::new(&organizer.id)?;
        drop(self.pools.registry().provision(&tenant_id).await?);
        self.pools.registry().evict(&tenant_id).await?;
        self.control
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "INSERT INTO organizers (id, name, subdomain, db_path, data_key, created_at) VALUES (?, ?, ?, ?, ?, ?)",
                [
                    organizer.id.clone().into(),
                    organizer.name.clone().into(),
                    organizer.subdomain.clone().into(),
                    organizer.db_path.clone().into(),
                    organizer.data_key.clone().into(),
                    Utc::now().timestamp().into(),
                ],
            ))
            .await?;
        Ok(organizer)
    }

    pub async fn resolve(&self, host: &str) -> Result<OrganizerDb, OrganizerError> {
        let _gate = self.connection_gate.read().await;
        let organizer = match self.edition {
            Edition::SelfHosted => self.all().await?.into_iter().next(),
            Edition::Hosted => {
                let hostname = host
                    .split(':')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let suffix = format!(".{}", self.base_domain);
                let subdomain = hostname
                    .strip_suffix(&suffix)
                    .ok_or(OrganizerError::NotFound)?;
                if !valid_subdomain(subdomain) {
                    return Err(OrganizerError::NotFound);
                }
                self.by_subdomain(subdomain).await?
            }
        }
        .ok_or(OrganizerError::NotFound)?;
        if organizer.suspension_reason.is_some() {
            return Err(OrganizerError::Suspended);
        }
        if organizer.data_key.is_none() {
            return Err(OrganizerError::NotFound);
        }
        let db = self.open(&organizer).await?;
        Ok(OrganizerDb { organizer, db })
    }

    pub(crate) async fn cleanup_guard(&self) -> tokio::sync::RwLockWriteGuard<'_, ()> {
        self.connection_gate.write().await
    }

    pub(crate) async fn close_deleted(
        &self,
        organizer_id: &str,
        path: &std::path::Path,
        _guard: &tokio::sync::RwLockWriteGuard<'_, ()>,
    ) -> Result<(), OrganizerError> {
        let expected = tokio::fs::canonicalize(&self.database_dir)
            .await?
            .join(format!("{organizer_id}.db"));
        if expected != path {
            return Err(OrganizerError::NotFound);
        }
        self.pools
            .registry()
            .close_for_deletion(&tenant_sqlite_loco::TenantId::new(organizer_id)?)
            .await?;
        Ok(())
    }

    pub fn cipher(&self, organizer: &Organizer) -> Result<OrganizerCipher, OrganizerError> {
        let wrapped = organizer
            .data_key
            .as_deref()
            .ok_or(CryptoError::InvalidData)?;
        Ok(self.master_key.open_organizer_key(&organizer.id, wrapped)?)
    }

    pub async fn active_dbs(&self) -> Result<Vec<OrganizerDb>, OrganizerError> {
        let mut scopes = Vec::new();
        for organizer in self.all().await? {
            if organizer.suspension_reason.is_some() || organizer.data_key.is_none() {
                continue;
            }
            let host = match self.edition {
                Edition::SelfHosted => self.base_domain.clone(),
                Edition::Hosted => format!("{}.{}", organizer.subdomain, self.base_domain),
            };
            scopes.push(self.resolve(&host).await?);
        }
        Ok(scopes)
    }

    async fn open(
        &self,
        organizer: &Organizer,
    ) -> Result<tenant_sqlite_loco::TenantHandle, OrganizerError> {
        let id = tenant_sqlite_loco::TenantId::new(&organizer.id)?;
        if !tenant_sqlite_loco::matches_path(
            self.pools.registry(),
            &id,
            std::path::Path::new(&organizer.db_path),
        )
        .await?
        {
            return Err(OrganizerError::NotFound);
        }
        Ok(self.pools.registry().open_existing(&id).await?)
    }

    pub async fn active_organizers(&self) -> Result<Vec<Organizer>, OrganizerError> {
        Ok(self
            .all()
            .await?
            .into_iter()
            .filter(|organizer| {
                organizer.suspension_reason.is_none() && organizer.data_key.is_some()
            })
            .collect())
    }

    pub async fn resolve_organizer(
        &self,
        organizer: &Organizer,
    ) -> Result<OrganizerDb, OrganizerError> {
        let host = match self.edition {
            Edition::SelfHosted => self.base_domain.clone(),
            Edition::Hosted => format!("{}.{}", organizer.subdomain, self.base_domain),
        };
        self.resolve(&host).await
    }

    pub async fn shutdown(&self) -> Result<(), OrganizerError> {
        let _gate = self.connection_gate.write().await;
        Ok(self.pools.registry().shutdown().await?)
    }

    async fn by_subdomain(&self, subdomain: &str) -> Result<Option<Organizer>, OrganizerError> {
        let row = self
            .control
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT id, name, subdomain, db_path, suspension_reason, data_key FROM organizers WHERE subdomain = ?",
                [subdomain.into()],
            ))
            .await?;
        row.map(organizer_from_row).transpose()
    }

    async fn all(&self) -> Result<Vec<Organizer>, OrganizerError> {
        self.control
            .query_all_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT id, name, subdomain, db_path, suspension_reason, data_key FROM organizers ORDER BY id",
            ))
            .await?
            .into_iter()
            .map(organizer_from_row)
            .collect()
    }
}

impl<S> FromRequestParts<S> for OrganizerDb
where
    S: Send + Sync,
{
    type Rejection = axum::response::Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Extension(registry): Extension<Arc<OrganizerRegistry>> =
            Extension::from_request_parts(parts, state)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
        let host = parts
            .headers
            .get(axum::http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| StatusCode::BAD_REQUEST.into_response())?;
        registry.resolve(host).await.map_err(|error| match error {
            OrganizerError::NotFound | OrganizerError::InvalidSubdomain => crate::app::not_found(),
            OrganizerError::Suspended => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        })
    }
}

fn organizer_from_row(row: sea_orm::QueryResult) -> Result<Organizer, OrganizerError> {
    Ok(Organizer {
        id: row.try_get("", "id")?,
        name: row.try_get("", "name")?,
        subdomain: row.try_get("", "subdomain")?,
        db_path: row.try_get("", "db_path")?,
        suspension_reason: row.try_get("", "suspension_reason")?,
        data_key: row.try_get("", "data_key")?,
    })
}

const RESERVED_SUBDOMAINS: [&str; 8] = [
    "admin", "api", "app", "help", "mail", "operator", "status", "www",
];

pub fn reserved_subdomain(value: &str) -> bool {
    RESERVED_SUBDOMAINS.contains(&value)
}

fn valid_subdomain(value: &str) -> bool {
    !value.is_empty()
        && !reserved_subdomain(value)
        && value.len() <= 63
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (tempfile::TempDir, OrganizerRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let config: Database = serde_json::from_value(serde_json::json!({
            "uri": format!("sqlite://{}?mode=rwc", dir.path().join("control.db").display()),
            "enable_logging": false, "min_connections": 1, "max_connections": 1,
            "connect_timeout": 500, "idle_timeout": 60_000
        }))
        .unwrap();
        let control = db::connect(&config).await.unwrap();
        db::migrate::<migration::Migrator>(&control).await.unwrap();
        let registry = OrganizerRegistry::new(
            control,
            config,
            Edition::Hosted,
            "crabgate.com".into(),
            dir.path().join("organizers"),
            MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap(),
        )
        .unwrap();
        (dir, registry)
    }

    #[tokio::test]
    async fn missing_tenant_file_is_never_recreated_by_request_or_prepare() {
        let (_dir, registry) = fixture().await;
        let organizer = registry.create("Missing", "missing").await.unwrap();
        tokio::fs::remove_file(&organizer.db_path).await.unwrap();
        assert!(matches!(
            registry.resolve("missing.crabgate.com").await,
            Err(OrganizerError::Tenancy(tenant_sqlite_loco::Error::Missing(
                _
            )))
        ));
        registry.prepare().await.unwrap();
        assert!(!std::path::Path::new(&organizer.db_path).exists());
        assert!(matches!(
            registry.resolve("missing.crabgate.com").await,
            Err(OrganizerError::Suspended)
        ));
        registry.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn migration_failure_suspends_only_the_affected_organizer() {
        let (_dir, registry) = fixture().await;
        let bad = registry.create("Bad", "bad").await.unwrap();
        registry.create("Good", "good").await.unwrap();
        let db = sea_orm::Database::connect(format!("sqlite://{}?mode=rw", bad.db_path))
            .await
            .unwrap();
        db.execute_unprepared("DROP TABLE events; DELETE FROM seaql_migrations WHERE version = 'm20260930_000003_events'; CREATE TABLE events (broken TEXT)").await.unwrap();
        db.close().await.unwrap();
        registry.prepare().await.unwrap();
        assert!(matches!(
            registry.resolve("bad.crabgate.com").await,
            Err(OrganizerError::Suspended)
        ));
        let good = registry.resolve("good.crabgate.com").await.unwrap();
        let row = good
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM events",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "count").unwrap(), 0);
        drop(good);
        registry.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cg018_concurrent_and_canceled_retries_keep_the_tracked_pool() {
        let (dir, registry) = fixture().await;
        let registry = Arc::new(registry);
        let organizer = registry.create("Retry", "retry").await.unwrap();
        let scope = registry.resolve("retry.crabgate.com").await.unwrap();
        registry
            .control
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE organizers SET suspension_reason = 'migration failed' WHERE id = ?",
                [organizer.id.clone().into()],
            ))
            .await
            .unwrap();
        let guard = registry.cleanup_guard().await;
        let waiting_registry = registry.clone();
        let waiting_id = organizer.id.clone();
        let waiting =
            tokio::spawn(async move { waiting_registry.retry_migration(&waiting_id).await });
        tokio::task::yield_now().await;
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        drop(guard);
        let (first, second) = tokio::join!(
            registry.retry_migration(&organizer.id),
            registry.retry_migration(&organizer.id),
        );
        assert_eq!(first.unwrap(), "m20261011_000030_read_indexes");
        assert!(matches!(second, Err(OrganizerError::RecoveryNotAllowed)));
        let row = scope
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM events",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "count").unwrap(), 0);
        assert_eq!(registry.pools.registry().stats().await.retained_pools, 1);
        assert_eq!(
            registry.tenant_readiness(&organizer.id).await.unwrap(),
            "m20261011_000030_read_indexes"
        );
        drop(scope);
        registry.shutdown().await.unwrap();
        assert_eq!(registry.pools.registry().stats().await.retained_pools, 0);
        println!("CG018 retry database evidence: {}", dir.keep().display());
    }

    #[tokio::test]
    async fn background_selection_visits_more_tenants_than_retained_limit() {
        let (_dir, registry) = fixture().await;
        for n in 0..105 {
            registry
                .create("Tenant", &format!("tenant{n}"))
                .await
                .unwrap();
        }
        let organizers = registry.active_organizers().await.unwrap();
        assert_eq!(organizers.len(), 105);
        for organizer in organizers {
            let scope = registry.resolve_organizer(&organizer).await.unwrap();
            let row = scope
                .db
                .query_one_raw(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    "SELECT COUNT(*) AS count FROM events",
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.try_get::<i64>("", "count").unwrap(), 0);
            assert!(registry.pools.registry().stats().await.retained_pools <= 100);
        }
        registry.shutdown().await.unwrap();
        assert_eq!(registry.pools.registry().stats().await.retained_pools, 0);
    }
}
