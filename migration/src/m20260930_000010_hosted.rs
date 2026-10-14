use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE organizers ADD COLUMN verified_at INTEGER;
             ALTER TABLE organizers ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE subscriptions ADD COLUMN stripe_customer_id TEXT;
             ALTER TABLE subscriptions ADD COLUMN stripe_subscription_id TEXT;
             ALTER TABLE subscriptions ADD COLUMN stripe_checkout_session_id TEXT;
             ALTER TABLE subscriptions ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;
             CREATE UNIQUE INDEX subscriptions_organizer ON subscriptions(organizer_id);
             CREATE UNIQUE INDEX subscriptions_stripe_id ON subscriptions(stripe_subscription_id);
             CREATE TABLE organizer_verification_links (
                 token_hash TEXT PRIMARY KEY,
                 organizer_id TEXT NOT NULL REFERENCES organizers(id),
                 email TEXT NOT NULL,
                 expires_at INTEGER NOT NULL,
                 used_at INTEGER,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE operator_magic_links (
                 token_hash TEXT PRIMARY KEY,
                 operator_id TEXT NOT NULL REFERENCES operators(id),
                 expires_at INTEGER NOT NULL,
                 used_at INTEGER,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE operator_sessions (
                 token_hash TEXT PRIMARY KEY,
                 operator_id TEXT NOT NULL REFERENCES operators(id),
                 expires_at INTEGER NOT NULL
             );
             CREATE TABLE hosted_lifecycle_notices (
                 organizer_id TEXT NOT NULL REFERENCES organizers(id),
                 kind TEXT NOT NULL,
                 sent_at INTEGER NOT NULL,
                 PRIMARY KEY (organizer_id, kind)
             );",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP TABLE hosted_lifecycle_notices;
             DROP TABLE operator_sessions;
             DROP TABLE operator_magic_links;
             DROP TABLE organizer_verification_links;
             DROP INDEX subscriptions_stripe_id;
             DROP INDEX subscriptions_organizer;
             ALTER TABLE subscriptions DROP COLUMN updated_at;
             ALTER TABLE subscriptions DROP COLUMN stripe_checkout_session_id;
             ALTER TABLE subscriptions DROP COLUMN stripe_subscription_id;
             ALTER TABLE subscriptions DROP COLUMN stripe_customer_id;
             ALTER TABLE organizers DROP COLUMN created_at;
             ALTER TABLE organizers DROP COLUMN verified_at;",
            )
            .await?;
        Ok(())
    }
}
