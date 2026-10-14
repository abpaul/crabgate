use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(
            "ALTER TABLE orders ADD COLUMN is_complimentary INTEGER NOT NULL DEFAULT 0 CHECK (is_complimentary IN (0, 1));
             ALTER TABLE orders ADD COLUMN refunded_at INTEGER;
             CREATE TABLE organizer_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE INDEX orders_status_created ON orders(status, created_at);"
        ).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP INDEX orders_status_created; DROP TABLE organizer_settings;
             ALTER TABLE orders DROP COLUMN refunded_at;
             ALTER TABLE orders DROP COLUMN is_complimentary;",
            )
            .await?;
        Ok(())
    }
}
