use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE event_questions (
                    id TEXT PRIMARY KEY,
                    event_id TEXT NOT NULL REFERENCES events(id),
                    scope TEXT NOT NULL CHECK (scope IN ('Order', 'Attendee')),
                    kind TEXT NOT NULL CHECK (kind IN ('Text', 'Choice', 'Checkbox')),
                    label TEXT NOT NULL,
                    options_json TEXT NOT NULL DEFAULT '[]',
                    required INTEGER NOT NULL CHECK (required IN (0, 1)),
                    sort_order INTEGER NOT NULL
                );
                CREATE INDEX event_questions_event ON event_questions(event_id, sort_order);
                CREATE TABLE hold_questions (
                    hold_id TEXT NOT NULL REFERENCES holds(id) ON DELETE CASCADE,
                    question_id TEXT NOT NULL,
                    scope TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    label TEXT NOT NULL,
                    options_json TEXT NOT NULL,
                    required INTEGER NOT NULL,
                    sort_order INTEGER NOT NULL,
                    PRIMARY KEY (hold_id, question_id)
                );
                CREATE TABLE hold_answers (
                    hold_id TEXT NOT NULL REFERENCES holds(id) ON DELETE CASCADE,
                    question_id TEXT NOT NULL,
                    ordinal INTEGER NOT NULL,
                    question_snapshot_json TEXT NOT NULL,
                    answer_encrypted TEXT NOT NULL,
                    PRIMARY KEY (hold_id, question_id, ordinal)
                );",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP TABLE hold_answers; DROP TABLE hold_questions; DROP TABLE event_questions;",
            )
            .await?;
        Ok(())
    }
}
