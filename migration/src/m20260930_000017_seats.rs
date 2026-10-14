use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(
            "CREATE TABLE seats (
                id TEXT PRIMARY KEY,
                event_id TEXT NOT NULL REFERENCES events(id),
                ticket_type_id TEXT NOT NULL REFERENCES ticket_types(id),
                section TEXT NOT NULL,
                row_label TEXT NOT NULL,
                seat_label TEXT NOT NULL,
                active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
                created_at INTEGER NOT NULL,
                UNIQUE(event_id, section, row_label, seat_label)
            );
            CREATE INDEX seats_type ON seats(ticket_type_id);
            CREATE TABLE hold_seats (
                hold_id TEXT NOT NULL REFERENCES holds(id) ON DELETE CASCADE,
                seat_id TEXT NOT NULL REFERENCES seats(id),
                PRIMARY KEY (hold_id, seat_id)
            );
            CREATE INDEX hold_seats_seat ON hold_seats(seat_id);
            ALTER TABLE tickets ADD COLUMN seat_id TEXT REFERENCES seats(id);
            CREATE UNIQUE INDEX valid_ticket_seat ON tickets(seat_id) WHERE seat_id IS NOT NULL AND status = 'Valid';"
        ).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP INDEX valid_ticket_seat;
             ALTER TABLE tickets DROP COLUMN seat_id;
             DROP TABLE hold_seats;
             DROP TABLE seats;",
            )
            .await?;
        Ok(())
    }
}
