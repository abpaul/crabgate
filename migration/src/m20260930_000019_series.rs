use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE series (
                id TEXT PRIMARY KEY,
                frequency TEXT NOT NULL CHECK (frequency IN ('Weekly', 'Fortnightly', 'Monthly')),
                first_date TEXT NOT NULL,
                end_date TEXT,
                created_through TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('Active', 'Stopped')),
                publish INTEGER NOT NULL CHECK (publish IN (0, 1)),
                setup_json TEXT NOT NULL,
                cover BLOB,
                cover_mime TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            ALTER TABLE events ADD COLUMN series_id TEXT REFERENCES series(id);
            ALTER TABLE events ADD COLUMN series_date TEXT;
            CREATE UNIQUE INDEX events_series_date ON events(series_id, series_date)
                WHERE series_id IS NOT NULL;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP INDEX events_series_date;
             ALTER TABLE events DROP COLUMN series_date;
             ALTER TABLE events DROP COLUMN series_id;
             DROP TABLE series;",
            )
            .await?;
        Ok(())
    }
}
