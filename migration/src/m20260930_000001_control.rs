use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE organizers (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    subdomain TEXT NOT NULL UNIQUE,
                    db_path TEXT NOT NULL UNIQUE,
                    suspension_reason TEXT,
                    data_key TEXT
                );
                CREATE TABLE members (
                    id TEXT PRIMARY KEY,
                    email TEXT NOT NULL UNIQUE
                );
                CREATE TABLE memberships (
                    member_id TEXT NOT NULL REFERENCES members(id),
                    organizer_id TEXT NOT NULL REFERENCES organizers(id),
                    role TEXT NOT NULL CHECK (role IN ('Admin', 'Door Staff')),
                    PRIMARY KEY (member_id, organizer_id)
                );
                CREATE TABLE magic_links (
                    token_hash TEXT PRIMARY KEY,
                    member_id TEXT NOT NULL REFERENCES members(id),
                    organizer_id TEXT NOT NULL REFERENCES organizers(id),
                    expires_at INTEGER NOT NULL,
                    used_at INTEGER,
                    created_at INTEGER NOT NULL
                );
                CREATE INDEX magic_links_member_created ON magic_links(member_id, created_at);
                CREATE TABLE magic_link_rate_limits (
                    organizer_id TEXT NOT NULL REFERENCES organizers(id),
                    email_hash TEXT NOT NULL,
                    hour_bucket INTEGER NOT NULL,
                    count INTEGER NOT NULL,
                    PRIMARY KEY (organizer_id, email_hash, hour_bucket)
                );
                CREATE TABLE member_sessions (
                    token_hash TEXT PRIMARY KEY,
                    member_id TEXT NOT NULL REFERENCES members(id),
                    organizer_id TEXT NOT NULL REFERENCES organizers(id),
                    expires_at INTEGER NOT NULL
                );
                CREATE TABLE subscriptions (
                    id TEXT PRIMARY KEY,
                    organizer_id TEXT NOT NULL REFERENCES organizers(id),
                    status TEXT NOT NULL,
                    grace_until INTEGER,
                    lapsed_at INTEGER
                );
                CREATE TABLE access_grants (
                    id TEXT PRIMARY KEY,
                    organizer_id TEXT NOT NULL REFERENCES organizers(id),
                    member_id TEXT NOT NULL REFERENCES members(id),
                    expires_at INTEGER NOT NULL,
                    revoked_at INTEGER
                );
                CREATE TABLE operators (
                    id TEXT PRIMARY KEY,
                    email TEXT NOT NULL UNIQUE
                );
                CREATE TABLE audit_log (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    organizer_id TEXT NOT NULL REFERENCES organizers(id),
                    operator_id TEXT NOT NULL REFERENCES operators(id),
                    method TEXT NOT NULL,
                    path TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP TABLE audit_log;
                 DROP TABLE operators;
                 DROP TABLE access_grants;
                 DROP TABLE subscriptions;
                 DROP TABLE member_sessions;
                 DROP TABLE magic_link_rate_limits;
                 DROP TABLE magic_links;
                 DROP TABLE memberships;
                 DROP TABLE members;
                 DROP TABLE organizers;",
            )
            .await?;
        Ok(())
    }
}
