use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE holds ADD COLUMN refund_email_claim_token TEXT;
             ALTER TABLE holds ADD COLUMN refund_email_claim_until INTEGER;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE holds DROP COLUMN refund_email_claim_until;
             ALTER TABLE holds DROP COLUMN refund_email_claim_token;",
            )
            .await?;
        Ok(())
    }
}
