use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE order_resend_attempts (
                    order_id TEXT NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
                    hour_bucket INTEGER NOT NULL,
                    count INTEGER NOT NULL,
                    PRIMARY KEY (order_id, hour_bucket)
                );",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE order_resend_attempts;")
            .await?;
        Ok(())
    }
}
