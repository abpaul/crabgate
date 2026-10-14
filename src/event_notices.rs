use chrono::Utc;
use loco_rs::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

use crate::{
    mailers::event_change::{Details, EventChangeMailer},
    organizers::{Organizer, OrganizerRegistry},
    settings,
    tenant_jobs::{Admission, Budget, Pass, Turn},
};

fn error(error: impl std::fmt::Display) -> loco_rs::Error {
    loco_rs::Error::string(&error.to_string())
}

pub async fn deliver_pending(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    organizer: &Organizer,
    db: &DatabaseConnection,
) -> Result<()> {
    let mut pass = Pass::new(Budget::single_turn(200));
    let mut turn = pass.single_turn(&organizer.id);
    deliver_turn(ctx, registry, organizer, db, &mut turn).await
}

async fn deliver_turn(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    organizer: &Organizer,
    db: &DatabaseConnection,
    turn: &mut Turn<'_>,
) -> Result<()> {
    let cipher = registry.cipher(organizer).map_err(error)?;
    let contact = settings::contact(db).await.map_err(error)?;
    let rows = db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT r.notice_id, r.order_id, o.number, e.title,
                n.old_when, n.new_when, n.old_place, n.new_place
         FROM event_change_recipients r
         JOIN event_change_notices n ON n.id = r.notice_id
         JOIN orders o ON o.id = r.order_id
         JOIN events e ON e.id = n.event_id
         WHERE r.sent_at IS NULL AND o.status != 'Refunded' AND e.status != 'Cancelled'
         AND EXISTS (SELECT 1 FROM tickets t WHERE t.order_id = o.id AND t.event_id = n.event_id AND t.status = 'Valid')
         ORDER BY n.created_at, r.order_id LIMIT 200"
                .to_owned(),
        ))
        .await
        .map_err(error)?;
    for row in rows {
        let notice_id: String = row.try_get("", "notice_id").map_err(error)?;
        let order_id: String = row.try_get("", "order_id").map_err(error)?;
        match turn.admit(&format!("{notice_id}:{order_id}")) {
            Admission::Granted => {}
            Admission::AlreadyAttempted => continue,
            Admission::OutOfBudget => break,
        }
        let Some(recipient) = db.query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT o.buyer_email_encrypted FROM orders o
             JOIN event_change_notices n ON n.id = ? AND n.event_id = o.event_id
             JOIN events e ON e.id = n.event_id
             WHERE o.id = ? AND o.status != 'Refunded' AND e.status != 'Cancelled'
             AND EXISTS (SELECT 1 FROM tickets t WHERE t.order_id = o.id AND t.event_id = n.event_id AND t.status = 'Valid')",
            [notice_id.clone().into(), order_id.clone().into()],
        )).await.map_err(error)? else {
            continue;
        };
        let encrypted: String = recipient
            .try_get("", "buyer_email_encrypted")
            .map_err(error)?;
        let email = cipher.decrypt("buyer_email", &encrypted).map_err(error)?;
        if email.is_empty() {
            db.execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "DELETE FROM event_change_recipients WHERE notice_id = ? AND order_id = ? AND sent_at IS NULL",
                [notice_id.into(), order_id.into()],
            )).await.map_err(error)?;
            continue;
        }
        let result = EventChangeMailer::send(
            ctx,
            &email,
            &organizer.name,
            Details {
                event_title: &row.try_get::<String>("", "title").map_err(error)?,
                order_number: &row.try_get::<String>("", "number").map_err(error)?,
                old_when: &row.try_get::<String>("", "old_when").map_err(error)?,
                new_when: &row.try_get::<String>("", "new_when").map_err(error)?,
                old_place: &row.try_get::<String>("", "old_place").map_err(error)?,
                new_place: &row.try_get::<String>("", "new_place").map_err(error)?,
            },
            (!contact.reply_to.is_empty()).then_some(contact.reply_to.as_str()),
        )
        .await;
        if let Err(error) = result {
            tracing::error!(%error, organizer_id = %organizer.id, notice_id, order_id, "Event change notice failed");
            continue;
        }
        db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "UPDATE event_change_recipients SET sent_at = ? WHERE notice_id = ? AND order_id = ? AND sent_at IS NULL",
            [Utc::now().timestamp().into(), notice_id.into(), order_id.into()]
        )).await.map_err(error)?;
    }
    Ok(())
}

pub async fn run(ctx: &AppContext, registry: &OrganizerRegistry) -> Result<()> {
    let mut pass = Pass::new(Budget::MAIL);
    while let Some(organizers) = pass.next_round(registry).await.map_err(error)? {
        for organizer in organizers {
            let Some((scope, mut turn)) = pass.turn(registry, &organizer).await else {
                continue;
            };
            if let Err(error) =
                deliver_turn(ctx, registry, &scope.organizer, &scope.db, &mut turn).await
            {
                tracing::error!(%error, organizer_id = %organizer.id, "Event notices failed");
            }
        }
    }
    Ok(())
}
