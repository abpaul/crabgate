use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE holds (
                id TEXT PRIMARY KEY,
                event_id TEXT NOT NULL REFERENCES events(id),
                status TEXT NOT NULL CHECK (status IN ('Open', 'Paid', 'Expired', 'RefundPending', 'Refunded')),
                expires_at INTEGER NOT NULL,
                stripe_session_id TEXT UNIQUE,
                buyer_email_encrypted TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX holds_event_status_expiry ON holds(event_id, status, expires_at);
            CREATE TABLE hold_items (
                hold_id TEXT NOT NULL REFERENCES holds(id) ON DELETE CASCADE,
                ticket_type_id TEXT NOT NULL REFERENCES ticket_types(id),
                quantity INTEGER NOT NULL CHECK (quantity > 0),
                price_cents INTEGER NOT NULL CHECK (price_cents >= 0),
                PRIMARY KEY (hold_id, ticket_type_id)
            );
            CREATE TABLE orders (
                id TEXT PRIMARY KEY,
                number TEXT NOT NULL UNIQUE,
                event_id TEXT NOT NULL REFERENCES events(id),
                hold_id TEXT NOT NULL UNIQUE REFERENCES holds(id),
                stripe_session_id TEXT NOT NULL UNIQUE,
                stripe_payment_intent_id TEXT,
                buyer_email_encrypted TEXT NOT NULL,
                total_cents INTEGER NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('Paid', 'RefundPending', 'Refunded')),
                email_sent_at INTEGER,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX orders_event ON orders(event_id, created_at);
            CREATE TABLE hold_attendees (
                hold_id TEXT NOT NULL REFERENCES holds(id) ON DELETE CASCADE,
                ordinal INTEGER NOT NULL,
                attendee_name_encrypted TEXT,
                PRIMARY KEY (hold_id, ordinal)
            );
            CREATE TABLE tickets (
                id TEXT PRIMARY KEY,
                order_id TEXT NOT NULL REFERENCES orders(id),
                event_id TEXT NOT NULL REFERENCES events(id),
                ticket_type_id TEXT NOT NULL REFERENCES ticket_types(id),
                code TEXT NOT NULL UNIQUE,
                attendee_name_encrypted TEXT,
                status TEXT NOT NULL CHECK (status IN ('Valid', 'Void')),
                created_at INTEGER NOT NULL
            );
            CREATE INDEX tickets_event ON tickets(event_id);
            CREATE TABLE payment_events (
                stripe_event_id TEXT PRIMARY KEY,
                stripe_session_id TEXT NOT NULL,
                processed_at INTEGER NOT NULL
            );",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
            "DROP TABLE payment_events; DROP TABLE tickets; DROP TABLE hold_attendees; DROP TABLE orders;
             DROP TABLE hold_items; DROP TABLE holds;",
            )
            .await?;
        Ok(())
    }
}
