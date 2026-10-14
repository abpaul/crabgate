use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE series_exceptions (
                series_id TEXT NOT NULL REFERENCES series(id),
                date TEXT NOT NULL,
                reason TEXT NOT NULL,
                setup_json TEXT NOT NULL,
                cover BLOB,
                cover_mime TEXT,
                PRIMARY KEY (series_id, date)
            );
            ALTER TABLE events ADD COLUMN series_review_reason TEXT;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE events DROP COLUMN series_review_reason;
             DROP TABLE series_exceptions;",
            )
            .await?;
        Ok(())
    }
}
