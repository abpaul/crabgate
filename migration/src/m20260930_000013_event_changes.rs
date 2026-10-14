use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE event_change_notices (
                id TEXT PRIMARY KEY,
                event_id TEXT NOT NULL REFERENCES events(id),
                old_when TEXT NOT NULL,
                new_when TEXT NOT NULL,
                old_place TEXT NOT NULL,
                new_place TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE event_change_recipients (
                notice_id TEXT NOT NULL REFERENCES event_change_notices(id),
                order_id TEXT NOT NULL REFERENCES orders(id),
                sent_at INTEGER,
                PRIMARY KEY (notice_id, order_id)
            );
            CREATE INDEX event_change_pending ON event_change_recipients(sent_at, notice_id);",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP TABLE event_change_recipients; DROP TABLE event_change_notices;",
            )
            .await?;
        Ok(())
    }
}
