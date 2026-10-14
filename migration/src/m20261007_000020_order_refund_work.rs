use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(
            "ALTER TABLE orders ADD COLUMN refund_work_state TEXT NOT NULL DEFAULT 'Not requested';
             ALTER TABLE orders ADD COLUMN refund_claim_until INTEGER;
             ALTER TABLE orders ADD COLUMN refund_claim_token TEXT;
             ALTER TABLE orders ADD COLUMN stripe_refund_id TEXT;
             ALTER TABLE orders ADD COLUMN refund_retry_after TEXT;
             ALTER TABLE orders ADD COLUMN refund_notice_state TEXT NOT NULL DEFAULT 'Not requested';
             ALTER TABLE orders ADD COLUMN cancel_notice_state TEXT NOT NULL DEFAULT 'Not requested';
             ALTER TABLE orders ADD COLUMN cancel_notice_state_claimed_at INTEGER;
             ALTER TABLE orders ADD COLUMN refund_notice_state_claimed_at INTEGER;
             UPDATE orders SET refund_work_state = CASE WHEN status = 'Refunded' THEN 'Completed' ELSE 'Pending' END
               WHERE status IN ('RefundPending', 'Refunded') OR (status = 'Paid' AND event_id IN (SELECT id FROM events WHERE status = 'Cancelled'));
             UPDATE orders SET cancel_notice_state = CASE WHEN cancel_notice_sent_at IS NOT NULL THEN 'Uncertain' ELSE 'Pending' END
               WHERE status IN ('Paid', 'RefundPending', 'Refunded') AND event_id IN (SELECT id FROM events WHERE status = 'Cancelled');
             UPDATE orders SET refund_notice_state = CASE WHEN refund_notice_sent_at IS NOT NULL THEN 'Uncertain' ELSE 'Pending' END
               WHERE refund_notice_sent_at IS NOT NULL OR (status = 'Refunded' AND event_id NOT IN (SELECT id FROM events WHERE status = 'Cancelled'));"
        ).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE orders DROP COLUMN cancel_notice_state_claimed_at;
             ALTER TABLE orders DROP COLUMN refund_notice_state_claimed_at;
             ALTER TABLE orders DROP COLUMN cancel_notice_state;
             ALTER TABLE orders DROP COLUMN refund_notice_state;
             ALTER TABLE orders DROP COLUMN refund_retry_after;
             ALTER TABLE orders DROP COLUMN stripe_refund_id;
             ALTER TABLE orders DROP COLUMN refund_claim_token;
             ALTER TABLE orders DROP COLUMN refund_claim_until;
             ALTER TABLE orders DROP COLUMN refund_work_state;",
            )
            .await?;
        Ok(())
    }
}
