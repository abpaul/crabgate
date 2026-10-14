use loco_rs::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

use crate::{
    controllers::checkout::{send_order_email, ticket_email_stopped},
    orders,
    organizers::{OrganizerDb, OrganizerRegistry},
    tenant_jobs::{Admission, Budget, Pass, Turn},
};

async fn deliver_turn(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    scope: &OrganizerDb,
    turn: &mut Turn<'_>,
    now: i64,
) -> Result<()> {
    for order_id in orders::stop_abandoned_emails(&scope.db, now)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
    {
        ticket_email_stopped(&scope.organizer.id, &order_id);
    }
    let rows = scope
        .db
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id FROM orders WHERE status = 'Paid' AND email_sent_at IS NULL AND email_state = 'Pending'
             AND (email_retry_after IS NULL OR email_retry_after <= ?)
             AND (email_claim_until IS NULL OR email_claim_until <= ?)
             AND event_id NOT IN (SELECT id FROM events WHERE status = 'Cancelled')
             ORDER BY email_attempts, created_at, id LIMIT 200",
            [now.into(), now.into()],
        ))
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
    for row in rows {
        let order_id: String = row
            .try_get("", "id")
            .map_err(|error| loco_rs::Error::string(&error.to_string()))?;
        match turn.admit(&order_id) {
            Admission::Granted => {}
            Admission::AlreadyAttempted => continue,
            Admission::OutOfBudget => break,
        }
        if let Err(error) =
            send_order_email(ctx, registry, &scope.organizer, &scope.db, &order_id).await
        {
            tracing::error!(%error, organizer_id = %scope.organizer.id, order_id, "Ticket email was not accepted");
        }
    }
    Ok(())
}

pub async fn run(ctx: &AppContext, registry: &OrganizerRegistry) -> Result<()> {
    run_at(ctx, registry, orders::now()).await
}

/// One pass over the Orders whose retry time is at or before `now`. Orders with the fewest
/// attempts come first, so an Order that keeps failing cannot hold back a newer one.
pub async fn run_at(ctx: &AppContext, registry: &OrganizerRegistry, now: i64) -> Result<()> {
    let mut pass = Pass::new(Budget::MAIL);
    while let Some(organizers) = pass
        .next_round(registry)
        .await
        .map_err(|error| loco_rs::Error::string(&error.to_string()))?
    {
        for organizer in organizers {
            let Some((scope, mut turn)) = pass.turn(registry, &organizer).await else {
                continue;
            };
            if let Err(error) = deliver_turn(ctx, registry, &scope, &mut turn, now).await {
                tracing::error!(%error, organizer_id = %organizer.id, "Ticket email retry scan failed");
            }
        }
    }
    Ok(())
}
