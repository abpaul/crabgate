use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared("CREATE TABLE buyer_messages (
            id TEXT PRIMARY KEY,
            event_id TEXT NOT NULL REFERENCES events(id),
            content_encrypted TEXT NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('Preview', 'Queued')),
            recipient_count INTEGER NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE buyer_message_recipients (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL REFERENCES buyer_messages(id),
            email_encrypted TEXT NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('Pending', 'Sending', 'Accepted', 'Failed', 'Uncertain', 'Skipped')),
            claim_token TEXT,
            claimed_at INTEGER,
            attempts INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX buyer_message_pending ON buyer_message_recipients(state, updated_at);").await?;
        Ok(())
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE buyer_message_recipients; DROP TABLE buyer_messages;")
            .await?;
        Ok(())
    }
}
