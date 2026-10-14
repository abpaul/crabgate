use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE holds ADD COLUMN booking_terms_snapshot TEXT;
             ALTER TABLE holds ADD COLUMN booking_terms_token TEXT;
             ALTER TABLE holds ADD COLUMN booking_terms_submitted_at INTEGER;
             ALTER TABLE holds ADD COLUMN booking_terms_accepted_at INTEGER;
             ALTER TABLE orders ADD COLUMN booking_terms_snapshot TEXT;
             ALTER TABLE orders ADD COLUMN booking_terms_accepted_at INTEGER;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE orders DROP COLUMN booking_terms_accepted_at;
             ALTER TABLE orders DROP COLUMN booking_terms_snapshot;
             ALTER TABLE holds DROP COLUMN booking_terms_accepted_at;
             ALTER TABLE holds DROP COLUMN booking_terms_submitted_at;
             ALTER TABLE holds DROP COLUMN booking_terms_token;
             ALTER TABLE holds DROP COLUMN booking_terms_snapshot;",
            )
            .await?;
        Ok(())
    }
}
