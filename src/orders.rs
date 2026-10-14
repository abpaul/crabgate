use std::collections::HashMap;

use chrono::Utc;
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, SqliteTransactionMode, Statement,
    TransactionOptions, TransactionTrait,
};
use uuid::Uuid;

use crate::{
    crypto::{CryptoError, OrganizerCipher},
    inventory,
};

#[derive(Debug, thiserror::Error)]
pub enum OrderError {
    #[error("That checkout has ended. Please choose your tickets again.")]
    HoldNotFound,
    #[error("Your tickets were held for 30 minutes, and that time has run out. Please choose them again.")]
    HoldExpired,
    #[error("That email address doesn't look right. Please check it.")]
    InvalidEmail,
    #[error("Please add a name for every ticket.")]
    MissingName,
    #[error(transparent)]
    BookingTerms(#[from] crate::booking_terms::BookingTermsError),
    #[error(transparent)]
    Database(#[from] DbErr),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

pub struct HoldDetails {
    pub id: String,
    pub event_id: String,
    pub expires_at: i64,
    pub created_at: i64,
    pub status: String,
    pub items: Vec<HoldItem>,
    pub subtotal_cents: i64,
    pub discount_cents: i64,
    pub discount_code: Option<String>,
    pub total_cents: i64,
}

pub struct HoldItem {
    pub ticket_type_id: String,
    pub name: String,
    pub quantity: i64,
    pub price_cents: i64,
}

pub struct Order {
    pub id: String,
    pub number: String,
    pub event_id: String,
    pub buyer_email: String,
    pub total_cents: i64,
    pub status: String,
    pub tickets: Vec<Ticket>,
}

pub struct Ticket {
    pub id: String,
    pub code: String,
    pub ticket_type_id: String,
    pub attendee_name: Option<String>,
    pub status: String,
    pub seat: Option<String>,
}

pub enum PaymentOutcome {
    Paid(String),
    AlreadyProcessed(String),
    RefundNeeded,
    AlreadyRefunded,
}

pub async fn hold_details<C: ConnectionTrait>(db: &C, id: &str) -> Result<HoldDetails, OrderError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT h.id, h.event_id, h.expires_at, h.created_at, h.status, COALESCE(d.discount_cents, 0) AS discount_cents, d.code_snapshot FROM holds h LEFT JOIN hold_discounts d ON d.hold_id = h.id WHERE h.id = ?",
            [id.into()],
        ))
        .await?
        .ok_or(OrderError::HoldNotFound)?;
    let items = db.query_all_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT hi.ticket_type_id, tt.name, hi.quantity, hi.price_cents FROM hold_items hi JOIN ticket_types tt ON tt.id = hi.ticket_type_id WHERE hi.hold_id = ? ORDER BY tt.sort_order, tt.id",
        [id.into()],
    )).await?.into_iter().map(|row| Ok(HoldItem {
        ticket_type_id: row.try_get("", "ticket_type_id")?,
        name: row.try_get("", "name")?,
        quantity: row.try_get("", "quantity")?,
        price_cents: row.try_get("", "price_cents")?,
    })).collect::<Result<Vec<_>, DbErr>>()?;
    let subtotal_cents: i64 = items
        .iter()
        .map(|item| item.price_cents * item.quantity)
        .sum();
    let discount_cents: i64 = row.try_get("", "discount_cents")?;
    Ok(HoldDetails {
        id: row.try_get("", "id")?,
        event_id: row.try_get("", "event_id")?,
        expires_at: row.try_get("", "expires_at")?,
        created_at: row.try_get("", "created_at")?,
        status: row.try_get("", "status")?,
        items,
        subtotal_cents,
        discount_cents,
        discount_code: row.try_get("", "code_snapshot")?,
        total_cents: subtotal_cents - discount_cents,
    })
}

pub async fn save_details(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    hold: &HoldDetails,
    email: &str,
    names: &[String],
    name_required: bool,
    now: i64,
) -> Result<(), OrderError> {
    let transaction = db.begin().await?;
    save_checkout_details_in(
        &transaction,
        cipher,
        hold,
        email,
        names,
        name_required,
        now,
        None,
    )
    .await?;
    transaction.commit().await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn save_checkout_details_in<C: ConnectionTrait>(
    transaction: &C,
    cipher: &OrganizerCipher,
    hold: &HoldDetails,
    email: &str,
    names: &[String],
    name_required: bool,
    now: i64,
    terms: Option<&crate::booking_terms::Submission<'_>>,
) -> Result<(), OrderError> {
    if hold.status != "Open" || hold.expires_at <= now {
        return Err(OrderError::HoldExpired);
    }
    let email = email.trim();
    if email.len() > 254 || !email.contains('@') || email.starts_with('@') || email.ends_with('@') {
        return Err(OrderError::InvalidEmail);
    }
    let quantity: usize = hold.items.iter().map(|item| item.quantity as usize).sum();
    if names.len() != quantity || (name_required && names.iter().any(|name| name.trim().is_empty()))
    {
        return Err(OrderError::MissingName);
    }
    let encrypted_email = cipher.encrypt("buyer_email", email)?;
    let affected = transaction.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE holds SET buyer_email_encrypted = ? WHERE id = ? AND status = 'Open' AND expires_at > ?",
        [encrypted_email.into(), hold.id.clone().into(), now.into()],
    )).await?.rows_affected();
    if affected == 0 {
        return Err(OrderError::HoldExpired);
    }
    crate::booking_terms::confirm(transaction, &hold.id, terms, now).await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "DELETE FROM hold_attendees WHERE hold_id = ?",
            [hold.id.clone().into()],
        ))
        .await?;
    for (ordinal, name) in names.iter().enumerate() {
        let encrypted = if name.trim().is_empty() {
            None
        } else {
            Some(cipher.encrypt("attendee_name", name.trim())?)
        };
        transaction.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO hold_attendees (hold_id, ordinal, attendee_name_encrypted) VALUES (?, ?, ?)",
            [hold.id.clone().into(), (ordinal as i64).into(), encrypted.into()],
        )).await?;
    }
    Ok(())
}

pub async fn attach_session(
    db: &DatabaseConnection,
    hold_id: &str,
    session_id: &str,
    _now: i64,
) -> Result<(), OrderError> {
    attach_session_with_url(db, hold_id, session_id, None).await
}

pub async fn attach_session_with_url(
    db: &DatabaseConnection,
    hold_id: &str,
    session_id: &str,
    session_url: Option<&str>,
) -> Result<(), OrderError> {
    let affected = db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE holds SET stripe_session_id = ?, stripe_session_url = COALESCE(?, stripe_session_url) WHERE id = ? AND (stripe_session_id = ? OR (stripe_session_id IS NULL AND status IN ('Open', 'Expired')))",
        [session_id.into(), session_url.into(), hold_id.into(), session_id.into()],
    )).await?.rows_affected();
    if affected == 0 {
        return Err(OrderError::HoldExpired);
    }
    Ok(())
}

pub async fn session_url(
    db: &DatabaseConnection,
    hold_id: &str,
) -> Result<Option<String>, OrderError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT stripe_session_url FROM holds WHERE id = ?",
            [hold_id.into()],
        ))
        .await?
        .ok_or(OrderError::HoldNotFound)?;
    row.try_get("", "stripe_session_url")
        .map_err(OrderError::from)
}

pub async fn hold_buyer_email(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    hold_id: &str,
) -> Result<String, OrderError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT buyer_email_encrypted FROM holds WHERE id = ?",
            [hold_id.into()],
        ))
        .await?
        .ok_or(OrderError::HoldNotFound)?;
    let encrypted: String = row.try_get("", "buyer_email_encrypted")?;
    cipher
        .decrypt("buyer_email", &encrypted)
        .map_err(OrderError::from)
}

pub async fn order_id_for_hold<C: ConnectionTrait>(
    db: &C,
    hold_id: &str,
) -> Result<Option<String>, OrderError> {
    db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT id FROM orders WHERE hold_id = ?",
        [hold_id.into()],
    ))
    .await?
    .map(|row| row.try_get("", "id"))
    .transpose()
    .map_err(OrderError::from)
}

pub async fn email_sent(db: &DatabaseConnection, order_id: &str) -> Result<bool, OrderError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT email_sent_at IS NOT NULL AS sent FROM orders WHERE id = ?",
            [order_id.into()],
        ))
        .await?
        .ok_or(OrderError::HoldNotFound)?;
    Ok(row.try_get::<i64>("", "sent")? != 0)
}

/// Stores the receipt of an accepted automatic attempt. Acceptance is a fact also when the claim
/// of `attempt` has ended; the claim is cleared only while that attempt still owns it.
pub async fn mark_email_sent(
    db: &DatabaseConnection,
    order_id: &str,
    attempt: i64,
    now: i64,
) -> Result<(), OrderError> {
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE orders SET email_sent_at = COALESCE(email_sent_at, ?),
         email_claim_until = CASE WHEN email_attempts = ? THEN NULL ELSE email_claim_until END
         WHERE id = ?",
        [now.into(), attempt.into(), order_id.into()],
    ))
    .await?;
    Ok(())
}

/// Automatic attempts of the first Ticket email for one Order. Explicit resends do not count.
pub const EMAIL_ATTEMPT_LIMIT: i64 = 8;

/// The last automatic attempt was claimed and its claim ended without a stored result.
fn email_attempts_abandoned() -> String {
    format!(
        "email_state = 'Pending' AND email_attempts >= {EMAIL_ATTEMPT_LIMIT}
         AND (email_claim_until IS NULL OR email_claim_until <= ?)"
    )
}

/// Why an automatic Ticket email attempt ended without mail-server acceptance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmailFailure {
    NoRecipient,
    Refused,
    Unconfirmed,
}

pub struct EmailProgress {
    pub accepted: bool,
    pub state: String,
    pub attempts: i64,
}

/// The state reads `Stopped` as soon as the claim of the last attempt has ended without a
/// result, also before the retry job stores that state.
pub async fn email_progress(
    db: &DatabaseConnection,
    order_id: &str,
    now: i64,
) -> Result<EmailProgress, OrderError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            format!(
                "SELECT email_sent_at IS NOT NULL AS accepted, email_attempts,
                 CASE WHEN {abandoned} THEN 'Stopped' ELSE email_state END AS email_state
                 FROM orders WHERE id = ?",
                abandoned = email_attempts_abandoned()
            ),
            [now.into(), order_id.into()],
        ))
        .await?
        .ok_or(OrderError::HoldNotFound)?;
    Ok(EmailProgress {
        accepted: row.try_get::<i64>("", "accepted")? != 0,
        state: row.try_get("", "email_state")?,
        attempts: row.try_get("", "email_attempts")?,
    })
}

/// Takes the one automatic attempt that may run now and returns its number, which identifies
/// the owner of the claim. The claim counts the attempt before SMTP starts, so a crash keeps
/// the count and the retry time, and no attempt starts after the limit. The wait before the
/// next job attempt grows fourfold from 5 minutes to at most one day.
pub async fn claim_email(
    db: &DatabaseConnection,
    order_id: &str,
    now: i64,
) -> Result<Option<i64>, OrderError> {
    db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE orders SET email_attempts = email_attempts + 1, email_claim_until = ? + 60,
         email_retry_after = ? + MIN(86400, 300 << MIN(2 * email_attempts, 10))
         WHERE id = ? AND email_sent_at IS NULL AND email_state = 'Pending' AND email_attempts < ?
         AND (email_claim_until IS NULL OR email_claim_until <= ?)
         RETURNING email_attempts",
        [
            now.into(),
            now.into(),
            order_id.into(),
            EMAIL_ATTEMPT_LIMIT.into(),
            now.into(),
        ],
    ))
    .await?
    .map(|row| row.try_get("", "email_attempts"))
    .transpose()
    .map_err(Into::into)
}

/// Whether `attempt` still owns its claim with more time left than the 10-second send bound.
/// The database clock is read when the statement runs, so a wait for the database counts.
pub async fn email_claim_current(
    db: &DatabaseConnection,
    order_id: &str,
    attempt: i64,
) -> Result<bool, OrderError> {
    Ok(db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id FROM orders WHERE id = ? AND email_attempts = ? AND email_sent_at IS NULL
             AND email_state = 'Pending' AND email_claim_until > unixepoch() + 10",
            [order_id.into(), attempt.into()],
        ))
        .await?
        .is_some())
}

/// Ends the claim of `attempt` unless a later attempt owns the Order. An Order without a
/// recipient stops at once; any other failure stops automatic attempts at the limit and keeps
/// the receipt unset. Returns true when this call stopped the automatic attempts.
pub async fn record_email_failure(
    db: &DatabaseConnection,
    order_id: &str,
    attempt: i64,
    failure: EmailFailure,
) -> Result<bool, OrderError> {
    let (limit, state) = match failure {
        EmailFailure::NoRecipient => (0, "No recipient"),
        EmailFailure::Refused => (EMAIL_ATTEMPT_LIMIT, "Refused"),
        EmailFailure::Unconfirmed => (EMAIL_ATTEMPT_LIMIT, "Stopped"),
    };
    let stored: Option<String> = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE orders SET email_claim_until = NULL,
             email_state = CASE WHEN email_attempts >= ? THEN ? ELSE 'Pending' END
             WHERE id = ? AND email_attempts = ? AND email_sent_at IS NULL AND email_state = 'Pending'
             RETURNING email_state",
            [limit.into(), state.into(), order_id.into(), attempt.into()],
        ))
        .await?
        .map(|row| row.try_get("", "email_state"))
        .transpose()?;
    Ok(matches!(stored.as_deref(), Some("Refused" | "Stopped")))
}

/// Stops the Orders whose last automatic attempt ended without a stored result, for example
/// after a stop of the process or a dropped request. Returns the Orders stopped by this call.
pub async fn stop_abandoned_emails(
    db: &DatabaseConnection,
    now: i64,
) -> Result<Vec<String>, OrderError> {
    db.query_all_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        format!(
            "UPDATE orders SET email_state = 'Stopped', email_claim_until = NULL
             WHERE email_sent_at IS NULL AND {} RETURNING id",
            email_attempts_abandoned()
        ),
        [now.into()],
    ))
    .await?
    .iter()
    .map(|row| row.try_get("", "id").map_err(Into::into))
    .collect()
}

/// Stores the receipt when the mail server accepted an explicit resend of an Order whose
/// automatic attempts have stopped. An Order that still has automatic attempts keeps its state.
pub async fn record_resend_receipt(
    db: &DatabaseConnection,
    order_id: &str,
    now: i64,
) -> Result<(), OrderError> {
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        format!(
            "UPDATE orders SET email_sent_at = ?, email_state = 'Pending', email_claim_until = NULL
             WHERE id = ? AND email_sent_at IS NULL
             AND (email_state IN ('Refused', 'Stopped') OR ({}))",
            email_attempts_abandoned()
        ),
        [now.into(), order_id.into(), now.into()],
    ))
    .await?;
    Ok(())
}

pub async fn claim_resend(
    db: &DatabaseConnection,
    order_id: &str,
    now: i64,
) -> Result<bool, OrderError> {
    let updated = db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO order_resend_attempts (order_id, hour_bucket, count) VALUES (?, ?, 1)
         ON CONFLICT(order_id, hour_bucket) DO UPDATE SET count = count + 1 WHERE count < 3",
            [order_id.into(), (now / 3600).into()],
        ))
        .await?;
    Ok(updated.rows_affected() == 1)
}

pub async fn update_attendee_names(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    order: &Order,
    names: &[(String, String)],
    starts_at: i64,
    required: bool,
    now: i64,
) -> Result<(), OrderError> {
    if now >= starts_at {
        return Err(OrderError::HoldExpired);
    }
    if names.len() != order.tickets.len()
        || names
            .iter()
            .any(|(_, name)| required && name.trim().is_empty())
    {
        return Err(OrderError::MissingName);
    }
    let transaction = db.begin().await?;
    for (ticket_id, name) in names {
        if !order.tickets.iter().any(|ticket| ticket.id == *ticket_id) {
            return Err(OrderError::HoldNotFound);
        }
        let encrypted = if name.trim().is_empty() {
            None
        } else {
            Some(cipher.encrypt("attendee_name", name.trim())?)
        };
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE tickets SET attendee_name_encrypted = ? WHERE id = ? AND order_id = ?",
                [
                    encrypted.into(),
                    ticket_id.clone().into(),
                    order.id.clone().into(),
                ],
            ))
            .await?;
    }
    transaction.commit().await?;
    Ok(())
}

pub async fn complete_payment(
    db: &DatabaseConnection,
    session_id: &str,
    payment_intent: &str,
    amount_cents: i64,
    now: i64,
) -> Result<PaymentOutcome, OrderError> {
    complete_payment_in_currency(db, session_id, payment_intent, amount_cents, "EUR", now).await
}

pub async fn complete_payment_in_currency(
    db: &DatabaseConnection,
    session_id: &str,
    payment_intent: &str,
    amount_cents: i64,
    paid_currency: &str,
    now: i64,
) -> Result<PaymentOutcome, OrderError> {
    let transaction = db
        .begin_with_options(TransactionOptions {
            sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
            ..Default::default()
        })
        .await?;
    let hold = transaction.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT id, event_id, status, expires_at, buyer_email_encrypted FROM holds WHERE stripe_session_id = ?",
        [session_id.into()],
    )).await?.ok_or(OrderError::HoldNotFound)?;
    let hold_id: String = hold.try_get("", "id")?;
    let event_id: String = hold.try_get("", "event_id")?;
    let existing = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id FROM orders WHERE stripe_session_id = ?",
            [session_id.into()],
        ))
        .await?;
    if let Some(existing) = existing {
        return Ok(PaymentOutcome::AlreadyProcessed(
            existing.try_get("", "id")?,
        ));
    }
    let buyer_email_encrypted: Option<String> = hold.try_get("", "buyer_email_encrypted")?;
    let items = transaction.query_all_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT hi.ticket_type_id, hi.quantity, hi.price_cents FROM hold_items hi JOIN ticket_types tt ON tt.id = hi.ticket_type_id WHERE hi.hold_id = ? ORDER BY tt.sort_order, tt.id",
        [hold_id.clone().into()],
    )).await?;
    let subtotal: i64 = items
        .iter()
        .map(|row| -> Result<i64, DbErr> {
            let quantity: i64 = row.try_get("", "quantity")?;
            let price: i64 = row.try_get("", "price_cents")?;
            Ok(quantity * price)
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .sum();
    let discount_row = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT discount_cents FROM hold_discounts WHERE hold_id = ?",
            [hold_id.clone().into()],
        ))
        .await?;
    let discount_cents: i64 = discount_row
        .map(|row| row.try_get("", "discount_cents"))
        .transpose()?
        .unwrap_or(0);
    let expected = subtotal - discount_cents;
    if !payment_intent.is_empty() {
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE holds SET stripe_payment_intent_id = ? WHERE id = ?",
                [payment_intent.into(), hold_id.clone().into()],
            ))
            .await?;
        let external = transaction
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT payment_intent_id FROM external_refunds WHERE payment_intent_id = ?",
                [payment_intent.into()],
            ))
            .await?;
        if external.is_some() {
            transaction
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "UPDATE holds SET status = 'Refunded' WHERE id = ?",
                    [hold_id.into()],
                ))
                .await?;
            transaction.commit().await?;
            return Ok(PaymentOutcome::AlreadyRefunded);
        }
    }
    if amount_cents != expected {
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE holds SET status = 'RefundPending' WHERE id = ?",
                [hold_id.into()],
            ))
            .await?;
        transaction.commit().await?;
        return Ok(PaymentOutcome::RefundNeeded);
    }
    let configured_currency = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT value FROM organizer_settings WHERE key = 'currency'",
            [],
        ))
        .await?
        .map(|row| row.try_get::<String>("", "value"))
        .transpose()?
        .unwrap_or_else(|| "EUR".to_owned());
    if !configured_currency.eq_ignore_ascii_case(paid_currency) {
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE holds SET status = 'RefundPending' WHERE id = ?",
                [hold_id.into()],
            ))
            .await?;
        transaction.commit().await?;
        return Ok(PaymentOutcome::RefundNeeded);
    }
    let status: String = hold.try_get("", "status")?;
    let expires_at: i64 = hold.try_get("", "expires_at")?;
    let event_state = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT status FROM events WHERE id = ?",
            [event_id.clone().into()],
        ))
        .await?
        .ok_or(OrderError::HoldNotFound)?;
    let event_status: String = event_state.try_get("", "status")?;
    if status == "RefundPending" {
        return Ok(PaymentOutcome::RefundNeeded);
    }
    if status == "Refunded" {
        return Ok(PaymentOutcome::AlreadyRefunded);
    }
    if event_status == "Cancelled" {
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE holds SET status = 'RefundPending' WHERE id = ?",
                [hold_id.into()],
            ))
            .await?;
        transaction.commit().await?;
        return Ok(PaymentOutcome::RefundNeeded);
    }
    let required = transaction.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT q.scope, (SELECT COUNT(*) FROM hold_answers a WHERE a.hold_id = q.hold_id AND a.question_id = q.question_id) AS answers FROM hold_questions q WHERE q.hold_id = ? AND q.required = 1",
        [hold_id.clone().into()]
    )).await?;
    let ticket_count: i64 = items
        .iter()
        .map(|item| item.try_get::<i64>("", "quantity"))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .sum();
    for question in required {
        let scope: String = question.try_get("", "scope")?;
        let answers: i64 = question.try_get("", "answers")?;
        let needed = if scope == "Order" { 1 } else { ticket_count };
        if answers < needed {
            transaction
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "UPDATE holds SET status = 'RefundPending' WHERE id = ?",
                    [hold_id.into()],
                ))
                .await?;
            transaction.commit().await?;
            return Ok(PaymentOutcome::RefundNeeded);
        }
    }
    if !crate::booking_terms::payment_ready(&transaction, &hold_id).await? {
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE holds SET status = 'RefundPending' WHERE id = ?",
                [hold_id.into()],
            ))
            .await?;
        transaction.commit().await?;
        return Ok(PaymentOutcome::RefundNeeded);
    }
    let Some(buyer_email_encrypted) = buyer_email_encrypted else {
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE holds SET status = 'RefundPending' WHERE id = ?",
                [hold_id.into()],
            ))
            .await?;
        transaction.commit().await?;
        return Ok(PaymentOutcome::RefundNeeded);
    };
    let seat_rows = transaction.query_all_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT s.id, s.ticket_type_id, s.active,
            EXISTS(SELECT 1 FROM tickets t WHERE t.seat_id = s.id AND t.status = 'Valid') AS sold,
            EXISTS(SELECT 1 FROM hold_seats other JOIN holds h ON h.id = other.hold_id WHERE other.seat_id = s.id AND h.id != ? AND h.status = 'Open' AND h.expires_at > ?) AS held
         FROM hold_seats hs JOIN seats s ON s.id = hs.seat_id WHERE hs.hold_id = ? ORDER BY s.ticket_type_id, s.section, s.row_label, s.seat_label",
        [hold_id.clone().into(), now.into(), hold_id.clone().into()],
    )).await?;
    let assigned = crate::seats::assigned(&transaction, &event_id).await?;
    let mut seats_by_type = HashMap::<String, Vec<String>>::new();
    let mut seats_free = (!assigned && seat_rows.is_empty())
        || (assigned && seat_rows.len() == ticket_count as usize);
    for seat in seat_rows {
        let type_id: String = seat.try_get("", "ticket_type_id")?;
        if seat.try_get::<i64>("", "active")? != 1
            || seat.try_get::<i64>("", "sold")? != 0
            || seat.try_get::<i64>("", "held")? != 0
        {
            seats_free = false;
        }
        seats_by_type
            .entry(type_id)
            .or_default()
            .push(seat.try_get("", "id")?);
    }
    for item in &items {
        let type_id: String = item.try_get("", "ticket_type_id")?;
        let quantity: i64 = item.try_get("", "quantity")?;
        if assigned && seats_by_type.get(&type_id).map_or(0, Vec::len) != quantity as usize {
            seats_free = false;
        }
    }
    if !seats_free {
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE holds SET status = 'RefundPending' WHERE id = ?",
                [hold_id.into()],
            ))
            .await?;
        transaction.commit().await?;
        return Ok(PaymentOutcome::RefundNeeded);
    }
    if status != "Open" || expires_at <= now {
        let mut capacity_free = true;
        let event = transaction
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT event_capacity FROM events WHERE id = ?",
                [event_id.clone().into()],
            ))
            .await?
            .ok_or(OrderError::HoldNotFound)?;
        let event_capacity: Option<i64> = event.try_get("", "event_capacity")?;
        let quantity: i64 = items
            .iter()
            .map(|row| row.try_get::<i64>("", "quantity"))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .sum();
        let (sold_event, held_event) = inventory::open_counts(&transaction, &event_id, now).await?;
        if event_capacity.is_some_and(|cap| sold_event + held_event + quantity > cap) {
            capacity_free = false;
        }
        for item in &items {
            let type_id: String = item.try_get("", "ticket_type_id")?;
            let quantity: i64 = item.try_get("", "quantity")?;
            let cap_row = transaction
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT capacity FROM ticket_types WHERE id = ?",
                    [type_id.clone().into()],
                ))
                .await?
                .ok_or(OrderError::HoldNotFound)?;
            let cap: i64 = cap_row.try_get("", "capacity")?;
            let (sold, held) = inventory::type_counts(&transaction, &type_id, now).await?;
            if sold + held + quantity > cap {
                capacity_free = false;
            }
        }
        if !capacity_free {
            transaction
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "UPDATE holds SET status = 'RefundPending' WHERE id = ?",
                    [hold_id.into()],
                ))
                .await?;
            transaction.commit().await?;
            return Ok(PaymentOutcome::RefundNeeded);
        }
    }
    let id = Uuid::new_v4().to_string();
    let number = format!("CG-{}", id[..8].to_uppercase());
    transaction.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "INSERT INTO orders (id, number, event_id, hold_id, stripe_session_id, stripe_payment_intent_id, buyer_email_encrypted, total_cents, status, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'Paid', ?)",
        [id.clone().into(), number.into(), event_id.clone().into(), hold_id.clone().into(), session_id.into(), payment_intent.into(), buyer_email_encrypted.into(), amount_cents.into(), now.into()],
    )).await?;
    transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE orders SET booking_terms_snapshot = (SELECT booking_terms_snapshot FROM holds WHERE id = ?), booking_terms_accepted_at = (SELECT booking_terms_accepted_at FROM holds WHERE id = ?) WHERE id = ?",
        [hold_id.clone().into(), hold_id.clone().into(), id.clone().into()]
    )).await?;
    let names = transaction
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT attendee_name_encrypted FROM hold_attendees WHERE hold_id = ? ORDER BY ordinal",
            [hold_id.clone().into()],
        ))
        .await?;
    let mut ordinal = 0;
    for item in &items {
        let type_id: String = item.try_get("", "ticket_type_id")?;
        let quantity: i64 = item.try_get("", "quantity")?;
        for _ in 0..quantity {
            let attendee_name_encrypted: Option<String> = names
                .get(ordinal)
                .map(|row| row.try_get("", "attendee_name_encrypted"))
                .transpose()?
                .flatten();
            ordinal += 1;
            transaction.execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "INSERT INTO tickets (id, order_id, event_id, ticket_type_id, code, attendee_name_encrypted, status, created_at, seat_id) VALUES (?, ?, ?, ?, ?, ?, 'Valid', ?, ?)",
                [Uuid::new_v4().to_string().into(), id.clone().into(), event_id.clone().into(), type_id.clone().into(), ticket_code().into(), attendee_name_encrypted.into(), now.into(), seats_by_type.get_mut(&type_id).and_then(|seats| if seats.is_empty() { None } else { Some(seats.remove(0)) }).into()],
            )).await?;
        }
    }
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE holds SET status = 'Paid' WHERE id = ?",
            [hold_id.into()],
        ))
        .await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET first_sale_at = COALESCE(first_sale_at, ?) WHERE id = ?",
            [now.into(), event_id.into()],
        ))
        .await?;
    transaction.commit().await?;
    Ok(PaymentOutcome::Paid(id))
}

pub async fn mark_refunded(db: &DatabaseConnection, session_id: &str) -> Result<(), OrderError> {
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE holds SET status = 'Refunded' WHERE stripe_session_id = ? AND status = 'RefundPending'",
        [session_id.into()],
    )).await?;
    Ok(())
}

pub async fn record_external_refund(
    db: &DatabaseConnection,
    payment_intent: &str,
    now: i64,
) -> Result<Option<String>, OrderError> {
    let transaction = db.begin().await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT OR IGNORE INTO external_refunds (payment_intent_id, created_at) VALUES (?, ?)",
            [payment_intent.into(), now.into()],
        ))
        .await?;
    let row = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id FROM holds WHERE stripe_payment_intent_id = ?",
            [payment_intent.into()],
        ))
        .await?;
    let hold_id: Option<String> = row.map(|row| row.try_get("", "id")).transpose()?;
    if let Some(id) = &hold_id {
        transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "UPDATE holds SET status = 'Refunded' WHERE id = ? AND status IN ('RefundPending', 'Expired', 'Open')", [id.clone().into()]
        )).await?;
    }
    transaction.commit().await?;
    Ok(hold_id)
}

pub async fn hold_refund_email_sent(
    db: &DatabaseConnection,
    hold_id: &str,
) -> Result<bool, OrderError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT refund_email_sent_at FROM holds WHERE id = ?",
            [hold_id.into()],
        ))
        .await?
        .ok_or(OrderError::HoldNotFound)?;
    Ok(row
        .try_get::<Option<i64>>("", "refund_email_sent_at")?
        .is_some())
}

pub enum HoldRefundEmailClaim {
    Sent,
    Busy,
    Owned(String),
}

pub async fn claim_hold_refund_email(
    db: &DatabaseConnection,
    hold_id: &str,
) -> Result<HoldRefundEmailClaim, OrderError> {
    let token = Uuid::new_v4().to_string();
    let claimed = db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE holds SET refund_email_claim_token = ?, refund_email_claim_until = unixepoch() + 60
         WHERE id = ? AND status = 'Refunded' AND refund_email_sent_at IS NULL
         AND (refund_email_claim_token IS NULL OR refund_email_claim_until <= unixepoch())",
        [token.clone().into(), hold_id.into()],
    )).await?.rows_affected();
    if claimed == 1 {
        return Ok(HoldRefundEmailClaim::Owned(token));
    }
    if hold_refund_email_sent(db, hold_id).await? {
        Ok(HoldRefundEmailClaim::Sent)
    } else {
        Ok(HoldRefundEmailClaim::Busy)
    }
}

pub async fn hold_refund_email_claim_current(
    db: &DatabaseConnection,
    hold_id: &str,
    token: &str,
) -> Result<bool, OrderError> {
    Ok(db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT id FROM holds WHERE id = ? AND status = 'Refunded' AND refund_email_sent_at IS NULL
         AND refund_email_claim_token = ? AND refund_email_claim_until > unixepoch() + 10",
        [hold_id.into(), token.into()],
    )).await?.is_some())
}

pub async fn release_hold_refund_email_claim(
    db: &DatabaseConnection,
    hold_id: &str,
    token: &str,
) -> Result<(), OrderError> {
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE holds SET refund_email_claim_token = NULL, refund_email_claim_until = NULL
         WHERE id = ? AND refund_email_claim_token = ?",
        [hold_id.into(), token.into()],
    ))
    .await?;
    Ok(())
}

pub async fn mark_hold_refund_email_sent(
    db: &DatabaseConnection,
    hold_id: &str,
    token: &str,
    now: i64,
) -> Result<(), OrderError> {
    let recorded = db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE holds SET refund_email_sent_at = ?, refund_email_claim_token = NULL, refund_email_claim_until = NULL
         WHERE id = ? AND refund_email_sent_at IS NULL AND refund_email_claim_token = ?
         AND refund_email_claim_until > unixepoch()",
        [now.into(), hold_id.into(), token.into()],
    )).await?.rows_affected();
    if recorded != 1 {
        return Err(DbErr::Custom(
            "Late-payment mail claim is no longer owned; SMTP acceptance may be uncertain".into(),
        )
        .into());
    }
    Ok(())
}

pub async fn expire_hold(db: &DatabaseConnection, hold_id: &str) -> Result<(), OrderError> {
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE holds SET status = 'Expired' WHERE id = ? AND status = 'Open'",
        [hold_id.into()],
    ))
    .await?;
    Ok(())
}

pub async fn get_order(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    id: &str,
) -> Result<Order, OrderError> {
    let row = db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT id, number, event_id, buyer_email_encrypted, total_cents, status FROM orders WHERE id = ?",
        [id.into()],
    )).await?.ok_or(OrderError::HoldNotFound)?;
    let tickets = db.query_all_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT t.id, t.code, t.ticket_type_id, t.attendee_name_encrypted, t.status, s.section, s.row_label, s.seat_label FROM tickets t LEFT JOIN seats s ON s.id = t.seat_id WHERE t.order_id = ? ORDER BY t.created_at, t.id",
        [id.into()],
    )).await?.into_iter().map(|row| {
        let encrypted: Option<String> = row.try_get("", "attendee_name_encrypted")?;
        Ok(Ticket {
            id: row.try_get("", "id")?, code: row.try_get("", "code")?,
            ticket_type_id: row.try_get("", "ticket_type_id")?,
            attendee_name: encrypted.map(|value| cipher.decrypt("attendee_name", &value)).transpose()?,
            status: row.try_get("", "status")?,
            seat: match (row.try_get::<Option<String>>("", "section")?, row.try_get::<Option<String>>("", "row_label")?, row.try_get::<Option<String>>("", "seat_label")?) {
                (Some(section), Some(row), Some(label)) => Some(crate::seats::display(&section, &row, &label)),
                _ => None,
            },
        })
    }).collect::<Result<Vec<_>, OrderError>>()?;
    let encrypted: String = row.try_get("", "buyer_email_encrypted")?;
    Ok(Order {
        id: row.try_get("", "id")?,
        number: row.try_get("", "number")?,
        event_id: row.try_get("", "event_id")?,
        buyer_email: cipher.decrypt("buyer_email", &encrypted)?,
        total_cents: row.try_get("", "total_cents")?,
        status: row.try_get("", "status")?,
        tickets,
    })
}

pub fn now() -> i64 {
    Utc::now().timestamp()
}

pub(crate) fn ticket_code() -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let random = Uuid::new_v4();
    let mut result = String::with_capacity(19);
    let mut bits = 0_u32;
    let mut available = 0;
    for byte in &random.as_bytes()[..10] {
        bits = (bits << 8) | u32::from(*byte);
        available += 8;
        while available >= 5 {
            available -= 5;
            let index = ((bits >> available) & 31) as usize;
            if result.len() == 4 || result.len() == 9 || result.len() == 14 {
                result.push('-');
            }
            result.push(ALPHABET[index] as char);
        }
    }
    result
}
