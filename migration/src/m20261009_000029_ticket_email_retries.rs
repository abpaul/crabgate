use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE orders ADD COLUMN email_state TEXT NOT NULL DEFAULT 'Pending';
             ALTER TABLE orders ADD COLUMN email_attempts INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE orders ADD COLUMN email_retry_after INTEGER;
             ALTER TABLE orders ADD COLUMN email_claim_until INTEGER;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE orders DROP COLUMN email_claim_until;
             ALTER TABLE orders DROP COLUMN email_retry_after;
             ALTER TABLE orders DROP COLUMN email_attempts;
             ALTER TABLE orders DROP COLUMN email_state;",
            )
            .await?;
        Ok(())
    }
}
