use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(
            "ALTER TABLE subscriptions ADD COLUMN lapse_generation INTEGER NOT NULL DEFAULT 0;
             UPDATE subscriptions SET lapse_generation = 1 WHERE status = 'Lapsed';
             CREATE TRIGGER subscription_lapse_insert AFTER INSERT ON subscriptions
             WHEN NEW.status = 'Lapsed'
             BEGIN UPDATE subscriptions SET lapse_generation = 1 WHERE id = NEW.id; END;
             CREATE TRIGGER subscription_lapse_transition AFTER UPDATE OF status, lapsed_at ON subscriptions
             WHEN NEW.status = 'Lapsed' AND (OLD.status != 'Lapsed' OR OLD.lapsed_at IS NOT NEW.lapsed_at)
             BEGIN UPDATE subscriptions SET lapse_generation = OLD.lapse_generation + 1 WHERE id = NEW.id; END;
             CREATE TABLE hosted_lifecycle_recipients (
                 organizer_id TEXT NOT NULL REFERENCES organizers(id),
                 lapse_generation INTEGER NOT NULL,
                 days_left INTEGER NOT NULL CHECK (days_left IN (30, 7, 1)),
                 email TEXT NOT NULL,
                 attempts INTEGER NOT NULL DEFAULT 0,
                 sent_at INTEGER,
                 last_error TEXT,
                 PRIMARY KEY (organizer_id, lapse_generation, days_left, email)
             );"
        ).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP TABLE hosted_lifecycle_recipients;
             DROP TRIGGER subscription_lapse_transition;
             DROP TRIGGER subscription_lapse_insert;
             ALTER TABLE subscriptions DROP COLUMN lapse_generation;",
            )
            .await?;
        Ok(())
    }
}
