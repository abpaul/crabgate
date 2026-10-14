use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(
            "ALTER TABLE subscriptions ADD COLUMN billing_attempt_key TEXT;
             ALTER TABLE subscriptions ADD COLUMN billing_attempt_started_at INTEGER;
             ALTER TABLE subscriptions ADD COLUMN billing_checkout_url TEXT;
             ALTER TABLE subscriptions ADD COLUMN last_billing_event_created INTEGER NOT NULL DEFAULT 0;"
        ).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE subscriptions DROP COLUMN last_billing_event_created;
             ALTER TABLE subscriptions DROP COLUMN billing_checkout_url;
             ALTER TABLE subscriptions DROP COLUMN billing_attempt_started_at;
             ALTER TABLE subscriptions DROP COLUMN billing_attempt_key;",
            )
            .await?;
        Ok(())
    }
}
