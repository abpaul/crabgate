use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE hosted_cleanup (
                organizer_id TEXT PRIMARY KEY REFERENCES organizers(id),
                db_path TEXT NOT NULL,
                requested_at INTEGER NOT NULL,
                completed_at INTEGER,
                last_error TEXT
             );
             INSERT INTO hosted_cleanup (organizer_id, db_path, requested_at)
             SELECT o.id, o.db_path, s.updated_at FROM organizers o
             JOIN subscriptions s ON s.organizer_id = o.id
             WHERE s.status = 'Deleted' AND o.data_key IS NULL;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE hosted_cleanup;")
            .await?;
        Ok(())
    }
}
