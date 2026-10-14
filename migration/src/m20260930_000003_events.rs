use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE events (
                    id TEXT PRIMARY KEY,
                    title TEXT NOT NULL,
                    description TEXT NOT NULL DEFAULT '',
                    date TEXT NOT NULL,
                    start_time TEXT NOT NULL,
                    end_time TEXT NOT NULL,
                    timezone TEXT NOT NULL,
                    starts_at INTEGER NOT NULL,
                    ends_at INTEGER NOT NULL,
                    venue TEXT NOT NULL,
                    address TEXT NOT NULL,
                    status TEXT NOT NULL CHECK (status IN ('Draft', 'Published', 'Cancelled')),
                    event_capacity INTEGER CHECK (event_capacity IS NULL OR event_capacity >= 0),
                    max_per_order INTEGER NOT NULL DEFAULT 10 CHECK (max_per_order BETWEEN 1 AND 100),
                    attendee_name_required INTEGER NOT NULL DEFAULT 0 CHECK (attendee_name_required IN (0, 1)),
                    cover BLOB,
                    cover_mime TEXT,
                    first_sale_at INTEGER,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                CREATE INDEX events_status_starts_at ON events(status, starts_at);
                CREATE TABLE ticket_types (
                    id TEXT PRIMARY KEY,
                    event_id TEXT NOT NULL REFERENCES events(id) ON DELETE CASCADE,
                    name TEXT NOT NULL,
                    price_cents INTEGER NOT NULL CHECK (price_cents >= 0),
                    capacity INTEGER NOT NULL CHECK (capacity >= 0),
                    sales_open INTEGER,
                    sales_close INTEGER,
                    sort_order INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX ticket_types_event ON ticket_types(event_id, sort_order);",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE ticket_types; DROP TABLE events;")
            .await?;
        Ok(())
    }
}
