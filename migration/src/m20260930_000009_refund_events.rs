use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(
            "ALTER TABLE holds ADD COLUMN stripe_payment_intent_id TEXT;
             ALTER TABLE holds ADD COLUMN refund_email_sent_at INTEGER;
             CREATE UNIQUE INDEX holds_payment_intent ON holds(stripe_payment_intent_id);
             CREATE TABLE external_refunds (payment_intent_id TEXT PRIMARY KEY, created_at INTEGER NOT NULL);"
        ).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP TABLE external_refunds; DROP INDEX holds_payment_intent;
             ALTER TABLE holds DROP COLUMN refund_email_sent_at;
             ALTER TABLE holds DROP COLUMN stripe_payment_intent_id;",
            )
            .await?;
        Ok(())
    }
}
