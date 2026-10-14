use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE orders ADD COLUMN refund_notice_sent_at INTEGER;
             ALTER TABLE orders ADD COLUMN cancel_notice_sent_at INTEGER;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE orders DROP COLUMN cancel_notice_sent_at;
             ALTER TABLE orders DROP COLUMN refund_notice_sent_at;",
            )
            .await?;
        Ok(())
    }
}
