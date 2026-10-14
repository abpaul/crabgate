use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(
            "CREATE TABLE door_links (
                id TEXT PRIMARY KEY,
                event_id TEXT NOT NULL REFERENCES events(id),
                label TEXT NOT NULL,
                token_hash TEXT NOT NULL UNIQUE,
                token_encrypted TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                revoked_at INTEGER
            );
            CREATE INDEX door_links_event ON door_links(event_id);
            CREATE TABLE checkins (
                id TEXT PRIMARY KEY,
                ticket_id TEXT NOT NULL REFERENCES tickets(id),
                event_id TEXT NOT NULL REFERENCES events(id),
                checked_at INTEGER NOT NULL,
                actor_type TEXT NOT NULL CHECK (actor_type IN ('Member', 'Door Link')),
                actor_id TEXT NOT NULL,
                undone_at INTEGER,
                undo_member_id TEXT
            );
            CREATE UNIQUE INDEX checkins_one_active_per_ticket ON checkins(ticket_id) WHERE undone_at IS NULL;
            CREATE INDEX checkins_event_active ON checkins(event_id, undone_at);"
        ).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE checkins; DROP TABLE door_links;")
            .await?;
        Ok(())
    }
}
