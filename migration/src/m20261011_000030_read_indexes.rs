use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE INDEX tickets_order ON tickets(order_id);
                 CREATE INDEX tickets_type_status ON tickets(ticket_type_id, status);
                 CREATE INDEX tickets_code_nocase ON tickets(code COLLATE NOCASE);
                 CREATE INDEX checkins_ticket_time ON checkins(ticket_id, checked_at);
                 CREATE INDEX holds_status_expiry ON holds(status, expires_at);",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP INDEX holds_status_expiry;
                 DROP INDEX checkins_ticket_time;
                 DROP INDEX tickets_code_nocase;
                 DROP INDEX tickets_type_status;
                 DROP INDEX tickets_order;",
            )
            .await?;
        Ok(())
    }
}
