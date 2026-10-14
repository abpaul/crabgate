use std::collections::BTreeSet;

use loco_rs::app::AppContext;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    crypto::OrganizerCipher,
    mailers::buyer_message::{self, Outcome},
    organizers::{Organizer, OrganizerRegistry},
    settings,
    tenant_jobs::{Admission, Budget, Pass, Turn},
};

#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    #[error("Message service unavailable")]
    Unavailable,
    #[error("Event or message unavailable")]
    Missing,
    #[error("Enter a subject of 1 to 120 characters without line breaks, and a plain text message of 1 to 10,000 characters")]
    Invalid,
    #[error("Recipients changed. Check the updated preview before sending")]
    Changed,
    #[error("No eligible Buyers. No message was queued")]
    Empty,
    #[error("Message storage unavailable")]
    Database(#[from] sea_orm::DbErr),
    #[error("Message data unavailable")]
    Crypto(#[from] crate::crypto::CryptoError),
    #[error("Message data unavailable")]
    Json(#[from] serde_json::Error),
}

#[derive(Serialize, Deserialize)]
pub struct Content {
    pub subject: String,
    pub body: String,
    recipients: BTreeSet<String>,
}

#[derive(Serialize)]
pub struct Message {
    pub id: String,
    pub snapshot: String,
    pub state: String,
    pub subject: String,
    pub body: String,
    pub recipient_count: i64,
    pub pending: i64,
    pub sending: i64,
    pub accepted: i64,
    pub failed: i64,
    pub uncertain: i64,
    pub skipped: i64,
}

fn sql(query: &str, values: impl IntoIterator<Item = sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DatabaseBackend::Sqlite, query, values)
}

pub async fn recipients<C: ConnectionTrait>(
    db: &C,
    cipher: &OrganizerCipher,
    event_id: &str,
) -> Result<BTreeSet<String>, MessageError> {
    let event = db
        .query_one_raw(sql(
            "SELECT status FROM events WHERE id = ?",
            [event_id.into()],
        ))
        .await?
        .ok_or(MessageError::Missing)?;
    if event.try_get::<String>("", "status")? == "Cancelled" {
        return Ok(BTreeSet::new());
    }
    let rows = db.query_all_raw(sql("SELECT o.buyer_email_encrypted FROM orders o WHERE o.event_id = ? AND o.status IN ('Paid', 'RefundPending') AND EXISTS (SELECT 1 FROM tickets t WHERE t.order_id = o.id AND t.event_id = o.event_id AND t.status = 'Valid') ORDER BY o.created_at, o.id", [event_id.into()])).await?;
    let mut emails = BTreeSet::new();
    for row in rows {
        let email = cipher.decrypt(
            "buyer_email",
            &row.try_get::<String>("", "buyer_email_encrypted")?,
        )?;
        let email = email.trim();
        if !email.is_empty() {
            emails.insert(email.to_lowercase());
        }
    }
    Ok(emails)
}

pub async fn preview(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: &str,
    subject: &str,
    body: &str,
    now: i64,
) -> Result<String, MessageError> {
    let subject = subject.trim();
    let body = body.trim().replace("\r\n", "\n");
    if subject.is_empty()
        || subject.chars().count() > 120
        || subject.chars().any(char::is_control)
        || body.is_empty()
        || body.chars().count() > 10_000
        || body
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(MessageError::Invalid);
    }
    let recipients = recipients(db, cipher, event_id).await?;
    let count = recipients.len() as i64;
    let content = cipher.encrypt(
        "buyer_message",
        &serde_json::to_string(&Content {
            subject: subject.to_owned(),
            body,
            recipients,
        })?,
    )?;
    let id = Uuid::new_v4().to_string();
    db.execute_raw(sql("INSERT INTO buyer_messages (id, event_id, content_encrypted, state, recipient_count, created_at) VALUES (?, ?, ?, 'Preview', ?, ?)", [id.clone().into(), event_id.into(), content.into(), count.into(), now.into()])).await?;
    Ok(id)
}

pub async fn queue(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: &str,
    id: &str,
    confirmed_snapshot: &str,
    now: i64,
) -> Result<(), MessageError> {
    let tx = db.begin().await?;
    tx.execute_raw(sql(
        "UPDATE buyer_messages SET state = state WHERE id = ? AND event_id = ?",
        [id.into(), event_id.into()],
    ))
    .await?;
    let row = tx
        .query_one_raw(sql(
            "SELECT content_encrypted, state FROM buyer_messages WHERE id = ? AND event_id = ?",
            [id.into(), event_id.into()],
        ))
        .await?
        .ok_or(MessageError::Missing)?;
    if row.try_get::<String>("", "state")? == "Queued" {
        tx.commit().await?;
        return Ok(());
    }
    let encrypted: String = row.try_get("", "content_encrypted")?;
    let mut content: Content = serde_json::from_str(&cipher.decrypt("buyer_message", &encrypted)?)?;
    let current = recipients(&tx, cipher, event_id).await?;
    if content.recipients != current {
        content.recipients = current;
        let encrypted = cipher.encrypt("buyer_message", &serde_json::to_string(&content)?)?;
        tx.execute_raw(sql(
            "UPDATE buyer_messages SET content_encrypted = ?, recipient_count = ? WHERE id = ?",
            [
                encrypted.into(),
                (content.recipients.len() as i64).into(),
                id.into(),
            ],
        ))
        .await?;
        tx.commit().await?;
        return Err(MessageError::Changed);
    }
    if confirmed_snapshot != hex::encode(Sha256::digest(encrypted.as_bytes())) {
        return Err(MessageError::Changed);
    }
    if current.is_empty() {
        return Err(MessageError::Empty);
    }
    for email in current {
        let encrypted = cipher.encrypt("buyer_message_email", &email)?;
        tx.execute_raw(sql("INSERT INTO buyer_message_recipients (id, message_id, email_encrypted, state, updated_at) VALUES (?, ?, ?, 'Pending', ?)", [Uuid::new_v4().to_string().into(), id.into(), encrypted.into(), now.into()])).await?;
    }
    tx.execute_raw(sql(
        "UPDATE buyer_messages SET state = 'Queued' WHERE id = ?",
        [id.into()],
    ))
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn messages(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: &str,
) -> Result<Vec<Message>, MessageError> {
    let rows = db.query_all_raw(sql("SELECT m.id, m.state, m.content_encrypted, m.recipient_count, COALESCE(SUM(r.state = 'Pending'), 0) AS pending, COALESCE(SUM(r.state = 'Sending'), 0) AS sending, COALESCE(SUM(r.state = 'Accepted'), 0) AS accepted, COALESCE(SUM(r.state = 'Failed'), 0) AS failed, COALESCE(SUM(r.state = 'Uncertain'), 0) AS uncertain, COALESCE(SUM(r.state = 'Skipped'), 0) AS skipped FROM buyer_messages m LEFT JOIN buyer_message_recipients r ON r.message_id = m.id WHERE m.event_id = ? GROUP BY m.id ORDER BY m.created_at DESC, m.rowid DESC", [event_id.into()])).await?;
    let mut messages = Vec::new();
    for row in rows {
        let content: Content = serde_json::from_str(&cipher.decrypt(
            "buyer_message",
            &row.try_get::<String>("", "content_encrypted")?,
        )?)?;
        messages.push(Message {
            id: row.try_get("", "id")?,
            snapshot: hex::encode(Sha256::digest(
                row.try_get::<String>("", "content_encrypted")?.as_bytes(),
            )),
            state: row.try_get("", "state")?,
            subject: content.subject,
            body: content.body,
            recipient_count: row.try_get("", "recipient_count")?,
            pending: row.try_get("", "pending")?,
            sending: row.try_get("", "sending")?,
            accepted: row.try_get("", "accepted")?,
            failed: row.try_get("", "failed")?,
            uncertain: row.try_get("", "uncertain")?,
            skipped: row.try_get("", "skipped")?,
        });
    }
    Ok(messages)
}

pub(crate) async fn anonymize_recipients<C: ConnectionTrait>(
    db: &C,
    cipher: &OrganizerCipher,
    order_id: &str,
) -> Result<(), MessageError> {
    let event_id: String = db
        .query_one_raw(sql(
            "SELECT event_id FROM orders WHERE id = ?",
            [order_id.into()],
        ))
        .await?
        .ok_or(MessageError::Missing)?
        .try_get("", "event_id")?;
    let mut retained = BTreeSet::new();
    for row in db
        .query_all_raw(sql(
            "SELECT buyer_email_encrypted FROM orders WHERE event_id = ? AND id != ?",
            [event_id.clone().into(), order_id.into()],
        ))
        .await?
    {
        let email = cipher.decrypt(
            "buyer_email",
            &row.try_get::<String>("", "buyer_email_encrypted")?,
        )?;
        let email = email.trim().to_lowercase();
        if !email.is_empty() {
            retained.insert(email);
        }
    }
    for row in db
        .query_all_raw(sql(
            "SELECT id, content_encrypted FROM buyer_messages WHERE event_id = ?",
            [event_id.clone().into()],
        ))
        .await?
    {
        let id: String = row.try_get("", "id")?;
        let mut content: Content = serde_json::from_str(&cipher.decrypt(
            "buyer_message",
            &row.try_get::<String>("", "content_encrypted")?,
        )?)?;
        let count = content.recipients.len();
        content.recipients.retain(|email| retained.contains(email));
        if count != content.recipients.len() {
            let encrypted = cipher.encrypt("buyer_message", &serde_json::to_string(&content)?)?;
            db.execute_raw(sql("UPDATE buyer_messages SET content_encrypted = ?, recipient_count = CASE WHEN state = 'Preview' THEN ? ELSE recipient_count END WHERE id = ?", [encrypted.into(), (content.recipients.len() as i64).into(), id.into()])).await?;
        }
    }
    let empty = cipher.encrypt("buyer_message_email", "")?;
    for row in db.query_all_raw(sql("SELECT r.id, r.email_encrypted FROM buyer_message_recipients r JOIN buyer_messages m ON m.id = r.message_id WHERE m.event_id = ?", [event_id.into()])).await? {
        let email = cipher.decrypt("buyer_message_email", &row.try_get::<String>("", "email_encrypted")?)?;
        if !email.is_empty() && !retained.contains(&email) {
            db.execute_raw(sql("UPDATE buyer_message_recipients SET email_encrypted = ?, state = CASE WHEN state IN ('Pending', 'Failed', 'Uncertain') THEN 'Skipped' ELSE state END WHERE id = ?", [empty.clone().into(), row.try_get::<String>("", "id")?.into()])).await?;
        }
    }
    Ok(())
}

pub async fn recover(db: &DatabaseConnection, now: i64) -> Result<(), MessageError> {
    db.execute_raw(sql("UPDATE buyer_message_recipients SET state = 'Uncertain', claim_token = NULL, updated_at = ? WHERE state = 'Sending' AND claimed_at <= ?", [now.into(), (now - 300).into()])).await?;
    Ok(())
}

pub async fn retry(
    db: &DatabaseConnection,
    event_id: &str,
    id: &str,
    include_uncertain: bool,
    now: i64,
) -> Result<(), MessageError> {
    if db
        .query_one_raw(sql(
            "SELECT id FROM buyer_messages WHERE id = ? AND event_id = ? AND state = 'Queued'",
            [id.into(), event_id.into()],
        ))
        .await?
        .is_none()
    {
        return Err(MessageError::Missing);
    }
    db.execute_raw(sql("UPDATE buyer_message_recipients SET state = 'Pending', claim_token = NULL, claimed_at = NULL, updated_at = ? WHERE message_id = ? AND (state = 'Failed' OR (? AND state = 'Uncertain'))", [now.into(), id.into(), include_uncertain.into()])).await?;
    Ok(())
}

pub async fn deliver_pending(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    organizer: &Organizer,
    db: &DatabaseConnection,
) -> Result<(), MessageError> {
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
) -> Result<(), MessageError> {
    let now = chrono::Utc::now().timestamp();
    recover(db, now).await?;
    let cipher = registry
        .cipher(organizer)
        .map_err(|_| MessageError::Unavailable)?;
    let contact = settings::contact(db)
        .await
        .map_err(|_| MessageError::Unavailable)?;
    let rows = db.query_all_raw(sql("SELECT id FROM buyer_message_recipients WHERE state = 'Pending' ORDER BY updated_at, id LIMIT 200", [])).await?;
    for row in rows {
        let id: String = row.try_get("", "id")?;
        match turn.admit(&id) {
            Admission::Granted => {}
            Admission::AlreadyAttempted => continue,
            Admission::OutOfBudget => break,
        }
        let now = chrono::Utc::now().timestamp();
        let token = Uuid::new_v4().to_string();
        let tx = db.begin().await?;
        let claimed = tx.execute_raw(sql("UPDATE buyer_message_recipients SET state = 'Sending', claim_token = ?, claimed_at = ?, updated_at = ?, attempts = attempts + 1 WHERE id = ? AND state = 'Pending'", [token.clone().into(), now.into(), now.into(), id.clone().into()])).await?.rows_affected();
        if claimed != 1 {
            tx.commit().await?;
            continue;
        }
        let row = tx.query_one_raw(sql("SELECT m.event_id, m.content_encrypted, r.email_encrypted FROM buyer_message_recipients r JOIN buyer_messages m ON m.id = r.message_id WHERE r.id = ?", [id.clone().into()])).await?.ok_or(MessageError::Missing)?;
        let email = cipher.decrypt(
            "buyer_message_email",
            &row.try_get::<String>("", "email_encrypted")?,
        )?;
        let current = recipients(&tx, &cipher, &row.try_get::<String>("", "event_id")?).await?;
        if !current.contains(&email) {
            tx.execute_raw(sql("UPDATE buyer_message_recipients SET state = 'Skipped', claim_token = NULL WHERE id = ? AND claim_token = ?", [id.into(), token.into()])).await?;
            tx.commit().await?;
            continue;
        }
        let content: Content = serde_json::from_str(&cipher.decrypt(
            "buyer_message",
            &row.try_get::<String>("", "content_encrypted")?,
        )?)?;
        let mail = buyer_message::prepare(
            &email,
            &content.subject,
            &content.body,
            (!contact.reply_to.is_empty()).then_some(contact.reply_to.as_str()),
        );
        tx.commit().await?;
        let outcome = buyer_message::send(ctx, &mail).await;
        let state = match outcome {
            Outcome::Accepted => "Accepted",
            Outcome::Failed => "Failed",
            Outcome::Uncertain => "Uncertain",
        };
        db.execute_raw(sql("UPDATE buyer_message_recipients SET state = ?, claim_token = NULL, updated_at = ? WHERE id = ? AND state = 'Sending' AND claim_token = ?", [state.into(), chrono::Utc::now().timestamp().into(), id.into(), token.into()])).await?;
    }
    Ok(())
}

pub async fn run(ctx: &AppContext, registry: &OrganizerRegistry) -> loco_rs::Result<()> {
    run_bounded(ctx, registry, Budget::MAIL).await
}

pub async fn run_bounded(
    ctx: &AppContext,
    registry: &OrganizerRegistry,
    budget: Budget,
) -> loco_rs::Result<()> {
    let mut pass = Pass::new(budget);
    while let Some(organizers) = pass
        .next_round(registry)
        .await
        .map_err(loco_rs::Error::wrap)?
    {
        for organizer in organizers {
            let Some((scope, mut turn)) = pass.turn(registry, &organizer).await else {
                continue;
            };
            if deliver_turn(ctx, registry, &scope.organizer, &scope.db, &mut turn)
                .await
                .is_err()
            {
                tracing::error!(organizer_id = %organizer.id, "Buyer message job failed");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use loco_rs::testing::prelude::*;
    use sea_orm::{ConnAcquireErr, DbErr};

    use super::{run_bounded, Budget};
    use crate::{app::App, initializers::organizers::OrganizerRuntime, organizers::OrganizerError};

    #[tokio::test]
    #[serial_test::serial]
    async fn organizer_lookup_preserves_the_original_closed_pool_error() {
        request::<App, _, _>(|_, ctx| async move {
            let runtime = ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
            let registry = runtime.registry();
            ctx.db.clone().close().await.unwrap();

            let error = run_bounded(&ctx, &registry, Budget::MAIL)
                .await
                .expect_err("a closed control pool must fail the Organizer lookup");
            let loco_rs::Error::Any(cause) = error else {
                panic!("the Organizer lookup must retain its original cause: {error:?}");
            };
            assert!(matches!(
                cause.downcast_ref::<OrganizerError>(),
                Some(OrganizerError::Database(DbErr::ConnectionAcquire(
                    ConnAcquireErr::ConnectionClosed
                )))
            ));
            assert_eq!(
                cause.to_string(),
                "Failed to acquire connection from pool: Connection closed"
            );
        })
        .await;
    }
}
