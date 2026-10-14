use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE hosted_lifecycle_recipients ADD COLUMN claim_token TEXT;
             ALTER TABLE hosted_lifecycle_recipients ADD COLUMN claim_until INTEGER;",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE hosted_lifecycle_recipients DROP COLUMN claim_until;
             ALTER TABLE hosted_lifecycle_recipients DROP COLUMN claim_token;",
            )
            .await?;
        Ok(())
    }
}
