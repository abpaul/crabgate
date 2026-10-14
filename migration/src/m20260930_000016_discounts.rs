use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE discount_codes (
                id TEXT PRIMARY KEY,
                event_id TEXT NOT NULL REFERENCES events(id),
                code TEXT NOT NULL,
                kind TEXT NOT NULL CHECK (kind IN ('Percent', 'Fixed')),
                value INTEGER NOT NULL CHECK (value > 0),
                starts_at INTEGER,
                ends_at INTEGER,
                max_redemptions INTEGER CHECK (max_redemptions > 0),
                active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
                created_at INTEGER NOT NULL,
                UNIQUE(event_id, code)
            );
            CREATE TABLE hold_discounts (
                hold_id TEXT PRIMARY KEY REFERENCES holds(id) ON DELETE CASCADE,
                code_id TEXT NOT NULL REFERENCES discount_codes(id),
                code_snapshot TEXT NOT NULL,
                subtotal_cents INTEGER NOT NULL,
                discount_cents INTEGER NOT NULL CHECK (discount_cents >= 0)
            );
            CREATE INDEX hold_discounts_code ON hold_discounts(code_id);",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE hold_discounts; DROP TABLE discount_codes;")
            .await?;
        Ok(())
    }
}
