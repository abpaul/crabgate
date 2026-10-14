use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, QueryResult, Statement,
    TransactionTrait,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    crypto::{CryptoError, OrganizerCipher},
    inventory,
    orders::{self, Order},
    questions::{self, ExportAnswers, Question, QuestionError},
    stripe::{StripeClient, StripeError},
};

#[derive(Debug, thiserror::Error)]
pub enum AdminOrderError {
    #[error("Order not found")]
    NotFound,
    #[error("Refund claim is no longer owned by this worker")]
    ClaimLost,
    #[error("Order cannot be refunded")]
    CannotRefund,
    #[error("Ticket type is full or unavailable")]
    NoCapacity,
    #[error("That email address doesn't look right. Please check it.")]
    InvalidEmail,
    #[error("Choose an available seat for this event")]
    SeatRequired,
    #[error(transparent)]
    Database(#[from] DbErr),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Stripe(#[from] StripeError),
    #[error(transparent)]
    Question(#[from] QuestionError),
    #[error(transparent)]
    Message(#[from] crate::buyer_messages::MessageError),
}

pub struct OrderRow {
    pub id: String,
    pub number: String,
    pub event_id: String,
    pub event_title: String,
    pub buyer_email: String,
    pub total_cents: i64,
    pub subtotal_cents: i64,
    pub discount_cents: i64,
    pub discount_code: Option<String>,
    pub status: String,
    pub complimentary: bool,
    pub created_at: i64,
    pub ticket_count: i64,
}

pub async fn list(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: Option<&str>,
    query: &str,
) -> Result<Vec<OrderRow>, AdminOrderError> {
    let rows = db.query_all_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT o.id, o.number, o.event_id, e.title AS event_title, o.buyer_email_encrypted, o.total_cents, hd.subtotal_cents, hd.discount_cents, hd.code_snapshot, o.status, o.is_complimentary, o.created_at, COUNT(t.id) AS ticket_count FROM orders o JOIN events e ON e.id = o.event_id LEFT JOIN hold_discounts hd ON hd.hold_id = o.hold_id LEFT JOIN tickets t ON t.order_id = o.id GROUP BY o.id ORDER BY o.created_at DESC, o.id DESC".to_owned()
    )).await?;
    matching_orders(db, cipher, event_id, query, rows).await
}

pub const ORDERS_PAGE_SIZE: usize = 50;

pub struct OrderPage {
    pub orders: Vec<OrderRow>,
    pub has_next: bool,
}

pub async fn list_page(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: Option<&str>,
    query: &str,
    page: u32,
) -> Result<OrderPage, AdminOrderError> {
    let offset = u64::from(page.max(1) - 1) * ORDERS_PAGE_SIZE as u64;
    let mut orders = if query.trim().is_empty() {
        let mut values = Vec::<sea_orm::Value>::new();
        let filter = if let Some(id) = event_id {
            values.push(id.into());
            "WHERE event_id = ?"
        } else {
            ""
        };
        values.extend([(ORDERS_PAGE_SIZE as i64 + 1).into(), (offset as i64).into()]);
        let sql = format!("WITH selected AS MATERIALIZED (SELECT id FROM orders {filter} ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?) SELECT o.id, o.number, o.event_id, e.title AS event_title, o.buyer_email_encrypted, o.total_cents, hd.subtotal_cents, hd.discount_cents, hd.code_snapshot, o.status, o.is_complimentary, o.created_at, (SELECT COUNT(*) FROM tickets t WHERE t.order_id = o.id) AS ticket_count FROM selected page JOIN orders o ON o.id = page.id JOIN events e ON e.id = o.event_id LEFT JOIN hold_discounts hd ON hd.hold_id = o.hold_id ORDER BY o.created_at DESC, o.id DESC");
        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                sql,
                values,
            ))
            .await?;
        matching_orders(db, cipher, event_id, query, rows).await?
    } else {
        list(db, cipher, event_id, query)
            .await?
            .into_iter()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
            .take(ORDERS_PAGE_SIZE + 1)
            .collect()
    };
    let has_next = orders.len() > ORDERS_PAGE_SIZE;
    orders.truncate(ORDERS_PAGE_SIZE);
    Ok(OrderPage { orders, has_next })
}

async fn matching_orders(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: Option<&str>,
    query: &str,
    rows: Vec<QueryResult>,
) -> Result<Vec<OrderRow>, AdminOrderError> {
    let needle = query.trim().to_lowercase();
    let mut matching_names = HashSet::new();
    if let Some(event_id) = event_id.filter(|_| needle.len() >= 2) {
        let tickets = db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT order_id, attendee_name_encrypted FROM tickets WHERE event_id = ? AND attendee_name_encrypted IS NOT NULL", [event_id.into()]
        )).await?;
        for ticket in tickets {
            let encrypted: String = ticket.try_get("", "attendee_name_encrypted")?;
            if cipher
                .decrypt("attendee_name", &encrypted)?
                .to_lowercase()
                .contains(&needle)
            {
                matching_names.insert(ticket.try_get::<String>("", "order_id")?);
            }
        }
    }
    let mut result = Vec::new();
    for row in rows {
        let event: String = row.try_get("", "event_id")?;
        if event_id.is_some_and(|selected| selected != event) {
            continue;
        }
        let encrypted: String = row.try_get("", "buyer_email_encrypted")?;
        let buyer_email = cipher.decrypt("buyer_email", &encrypted)?;
        let number: String = row.try_get("", "number")?;
        let id: String = row.try_get("", "id")?;
        if !needle.is_empty()
            && !buyer_email.to_lowercase().contains(&needle)
            && !number.to_lowercase().contains(&needle)
            && !matching_names.contains(&id)
        {
            continue;
        }
        let total_cents: i64 = row.try_get("", "total_cents")?;
        result.push(OrderRow {
            id,
            number,
            event_id: event,
            event_title: row.try_get("", "event_title")?,
            buyer_email,
            total_cents,
            subtotal_cents: row
                .try_get::<Option<i64>>("", "subtotal_cents")?
                .unwrap_or(total_cents),
            discount_cents: row
                .try_get::<Option<i64>>("", "discount_cents")?
                .unwrap_or(0),
            discount_code: row.try_get("", "code_snapshot")?,
            status: row.try_get("", "status")?,
            complimentary: row.try_get::<i64>("", "is_complimentary")? != 0,
            created_at: row.try_get("", "created_at")?,
            ticket_count: row.try_get("", "ticket_count")?,
        });
    }
    Ok(result)
}

pub struct AttendeeRow {
    pub ticket_id: String,
    pub code: String,
    pub name: Option<String>,
    pub ticket_type: String,
    pub seat: Option<String>,
    pub ticket_status: String,
    pub order_id: String,
    pub order_number: String,
    pub order_status: String,
    pub event_title: String,
    pub buyer_email: String,
    pub admission_valid: bool,
    pub checked_at: Option<i64>,
    pub undone_at: Option<i64>,
}

pub async fn attendees(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: Option<&str>,
    query: &str,
) -> Result<Vec<AttendeeRow>, AdminOrderError> {
    let rows = db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT t.id, t.code, t.attendee_name_encrypted, t.status AS ticket_status, tt.name AS ticket_type, o.id AS order_id, o.number, o.status AS order_status, o.buyer_email_encrypted, e.title AS event_title, e.status AS event_status, ci.checked_at, ci.undone_at, s.section, s.row_label, s.seat_label FROM tickets t JOIN orders o ON o.id = t.order_id JOIN events e ON e.id = t.event_id JOIN ticket_types tt ON tt.id = t.ticket_type_id LEFT JOIN seats s ON s.id = t.seat_id LEFT JOIN checkins ci ON ci.id = (SELECT latest.id FROM checkins latest WHERE latest.ticket_id = t.id ORDER BY latest.checked_at DESC, latest.rowid DESC LIMIT 1) WHERE (? IS NULL OR t.event_id = ?) ORDER BY o.created_at DESC, o.id, t.rowid",
        [event_id.map(str::to_owned).into(), event_id.map(str::to_owned).into()]
    )).await?;
    let needle = query.trim().to_lowercase();
    let mut result = Vec::new();
    for row in rows {
        let name = row
            .try_get::<Option<String>>("", "attendee_name_encrypted")?
            .map(|value| cipher.decrypt("attendee_name", &value))
            .transpose()?;
        let buyer_email = cipher.decrypt(
            "buyer_email",
            &row.try_get::<String>("", "buyer_email_encrypted")?,
        )?;
        let code: String = row.try_get("", "code")?;
        let order_number: String = row.try_get("", "number")?;
        if !needle.is_empty()
            && !name
                .as_deref()
                .unwrap_or_default()
                .to_lowercase()
                .contains(&needle)
            && !buyer_email.to_lowercase().contains(&needle)
            && !code.to_lowercase().contains(&needle)
            && !order_number.to_lowercase().contains(&needle)
        {
            continue;
        }
        let ticket_status: String = row.try_get("", "ticket_status")?;
        let order_status: String = row.try_get("", "order_status")?;
        let event_status: String = row.try_get("", "event_status")?;
        result.push(AttendeeRow {
            ticket_id: row.try_get("", "id")?,
            code,
            name,
            ticket_type: row.try_get("", "ticket_type")?,
            seat: match (
                row.try_get::<Option<String>>("", "section")?,
                row.try_get::<Option<String>>("", "row_label")?,
                row.try_get::<Option<String>>("", "seat_label")?,
            ) {
                (Some(section), Some(row), Some(label)) => {
                    Some(crate::seats::display(&section, &row, &label))
                }
                _ => None,
            },
            admission_valid: ticket_status == "Valid"
                && order_status != "Refunded"
                && event_status != "Cancelled",
            ticket_status,
            order_status,
            order_id: row.try_get("", "order_id")?,
            order_number,
            event_title: row.try_get("", "event_title")?,
            buyer_email,
            checked_at: row.try_get("", "checked_at")?,
            undone_at: row.try_get("", "undone_at")?,
        });
    }
    Ok(result)
}

pub struct TicketActivity {
    pub ticket_id: String,
    pub attendee_number: i64,
    pub checked_at: Option<i64>,
    pub undone_at: Option<i64>,
}

pub async fn ticket_activity(
    db: &DatabaseConnection,
    order_id: &str,
) -> Result<Vec<TicketActivity>, AdminOrderError> {
    let rows = db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT t.id, (SELECT COUNT(*) + 1 FROM tickets earlier WHERE earlier.order_id = t.order_id AND earlier.rowid < t.rowid) AS attendee_number, ci.checked_at, ci.undone_at FROM tickets t LEFT JOIN checkins ci ON ci.ticket_id = t.id WHERE t.order_id = ? ORDER BY t.rowid, ci.checked_at, ci.rowid", [order_id.into()]
    )).await?;
    rows.into_iter()
        .map(|row| {
            Ok(TicketActivity {
                ticket_id: row.try_get("", "id")?,
                attendee_number: row.try_get("", "attendee_number")?,
                checked_at: row.try_get("", "checked_at")?,
                undone_at: row.try_get("", "undone_at")?,
            })
        })
        .collect()
}

pub async fn payment_intent(
    db: &DatabaseConnection,
    id: &str,
) -> Result<(String, Option<String>, i64, String), AdminOrderError> {
    let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT status, stripe_payment_intent_id, total_cents, event_id FROM orders WHERE id = ?", [id.into()]
    )).await?.ok_or(AdminOrderError::NotFound)?;
    Ok((
        row.try_get("", "status")?,
        row.try_get("", "stripe_payment_intent_id")?,
        row.try_get("", "total_cents")?,
        row.try_get("", "event_id")?,
    ))
}

pub async fn order_id_by_payment_intent(
    db: &DatabaseConnection,
    payment_intent: &str,
) -> Result<Option<String>, AdminOrderError> {
    db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT id FROM orders WHERE stripe_payment_intent_id = ?",
        [payment_intent.into()],
    ))
    .await?
    .map(|row| row.try_get("", "id"))
    .transpose()
    .map_err(AdminOrderError::from)
}

pub async fn refund(
    db: &DatabaseConnection,
    id: &str,
    stripe: Option<&StripeClient>,
    now: i64,
) -> Result<(), AdminOrderError> {
    perform_refund_work(db, id, stripe, now, true).await
}

pub async fn reconcile_refund(
    db: &DatabaseConnection,
    id: &str,
    stripe: &StripeClient,
    now: i64,
) -> Result<(), AdminOrderError> {
    perform_refund_work(db, id, Some(stripe), now, false).await
}

async fn perform_refund_work(
    db: &DatabaseConnection,
    id: &str,
    stripe: Option<&StripeClient>,
    now: i64,
    request_create: bool,
) -> Result<(), AdminOrderError> {
    let (status, intent, total, _) = payment_intent(db, id).await?;
    let progress = work_progress(db, id).await?;
    if request_create
        && status == "Refunded"
        && progress.refund == "Completed"
        && !progress.refund_claimed
    {
        return Ok(());
    }
    if !matches!(status.as_str(), "Paid" | "RefundPending" | "Refunded") {
        return Err(AdminOrderError::CannotRefund);
    }
    if total > 0 && (stripe.is_none() || intent.as_deref().is_none_or(str::is_empty)) {
        record_refund_failure(db, id).await?;
        return Err(AdminOrderError::Stripe(StripeError::NotConfigured));
    }
    let create = request_create
        && !matches!(
            progress.refund.as_str(),
            "Completed" | "Reconciliation required"
        );
    let token = Uuid::new_v4().to_string();
    let claimed = db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE orders SET status = CASE WHEN status = 'Refunded' THEN status ELSE 'RefundPending' END, refund_work_state = CASE WHEN refund_work_state = 'Not requested' THEN 'Pending' ELSE refund_work_state END, refund_claim_until = ?, refund_claim_token = ? WHERE id = ? AND status IN ('Paid', 'RefundPending', 'Refunded') AND (refund_claim_until IS NULL OR refund_claim_until <= ?) AND (? = 0 OR status != 'Refunded' OR refund_work_state != 'Completed' OR refund_claim_token IS NOT NULL OR refund_claim_until IS NOT NULL)",
        [(now + 300).into(), token.clone().into(), id.into(), now.into(), request_create.into()])).await?.rows_affected();
    if claimed == 0 {
        if create {
            return Ok(());
        }
        if request_create
            && db.query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT id FROM orders WHERE id = ? AND status = 'Refunded' AND refund_work_state = 'Completed' AND refund_claim_token IS NULL AND refund_claim_until IS NULL",
                [id.into()],
            )).await?.is_some()
        {
            return Ok(());
        }
        return Err(AdminOrderError::Stripe(StripeError::Rejected(
            "refund reconciliation is already in flight".into(),
        )));
    }
    let result = tokio::time::timeout(std::time::Duration::from_secs(40), async {
        if total == 0 {
            return Ok(true);
        }
        let client = stripe.ok_or(StripeError::NotConfigured)?;
        let intent = intent.as_deref().ok_or(AdminOrderError::CannotRefund)?;
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT stripe_refund_id, refund_retry_after FROM orders WHERE id = ?",
                [id.into()],
            ))
            .await?
            .ok_or(AdminOrderError::NotFound)?;
        let saved: Option<String> = row.try_get("", "stripe_refund_id")?;
        let retry_after: Option<String> = row.try_get("", "refund_retry_after")?;
        let saved_refund = if let Some(saved) = saved.as_deref() {
            Some(client.retrieve_refund(saved).await?)
        } else {
            None
        };
        let mut history = client.refund_history(intent).await?;
        if let Some(saved) = saved_refund {
            if saved.payment_intent != intent || saved.amount <= 0 {
                return Err(AdminOrderError::Stripe(StripeError::InvalidResponse));
            }
            if !history.iter().any(|r| r.id == saved.id) {
                history.push(saved);
            }
        }
        let active: Vec<_> = history
            .iter()
            .filter(|r| !matches!(r.status.as_str(), "failed" | "canceled"))
            .collect();
        let succeeded: i64 = active
            .iter()
            .filter(|r| r.status == "succeeded")
            .map(|r| r.amount)
            .sum();
        if succeeded == total {
            save_refund_identity(
                db,
                id,
                &token,
                &active
                    .iter()
                    .find(|r| r.status == "succeeded")
                    .ok_or(StripeError::InvalidResponse)?
                    .id,
            )
            .await?;
            return Ok(true);
        }
        if history.iter().any(|refund| refund.amount != total)
            || succeeded > total
            || (history.is_empty() && matches!(progress.refund.as_str(), "Completed" | "Reconciliation required"))
        {
            owned_refund_update(db, id, &token,
                "UPDATE orders SET refund_work_state = 'Reconciliation required', refund_notice_state = CASE WHEN refund_notice_state IN ('Pending', 'Failed') THEN 'Not requested' ELSE refund_notice_state END WHERE id = ? AND refund_claim_token = ?",
                vec![]).await?;
            return Ok(false);
        }
        if !active.is_empty() {
            if active.len() != 1 {
                return Err(AdminOrderError::Stripe(StripeError::InvalidResponse));
            }
            save_refund_identity(db, id, &token, &active[0].id).await?;
            record_owned_refund_provider_status(db, id, &token, &active[0].id, false).await?;
            return Ok(false);
        }
        let failed_id = history.first().map(|r| r.id.as_str());
        if let Some(failed_id) = failed_id {
            save_refund_identity(db, id, &token, failed_id).await?;
            record_owned_refund_provider_status(db, id, &token, failed_id, true).await?;
        }
        if !create {
            return failed_id
                .map(|_| false)
                .ok_or(AdminOrderError::Stripe(StripeError::InvalidResponse));
        }
        let after = failed_id.or(retry_after.as_deref());
        let key = after.map_or_else(
            || format!("order-refund-{id}"),
            |failed| format!("order-refund-{id}-after-{failed}"),
        );
        owned_refund_update(db, id, &token,
            "UPDATE orders SET refund_claim_token = refund_claim_token WHERE id = ? AND refund_claim_token = ?",
            vec![]).await?;
        let refund = client.create_refund(intent, &key).await?;
        save_refund_identity(db, id, &token, &refund.id).await?;
        match refund.status.as_str() {
            "succeeded" => Ok(true),
            "failed" | "canceled" => {
                record_owned_refund_provider_status(db, id, &token, &refund.id, true).await?;
                Err(AdminOrderError::Stripe(StripeError::InvalidResponse))
            }
            _ => {
                record_owned_refund_provider_status(db, id, &token, &refund.id, false).await?;
                Ok(false)
            }
        }
    })
    .await;
    let result = match result {
        Ok(result) => result,
        Err(_) => Err(AdminOrderError::Stripe(StripeError::Rejected(
            "refund reconciliation timed out; provider acceptance is uncertain".into(),
        ))),
    };
    match result {
        Err(error) => {
            owned_refund_update(db, id, &token,
                "UPDATE orders SET refund_work_state = CASE WHEN refund_work_state IN ('Completed', 'Reconciliation required') THEN refund_work_state ELSE 'Failed' END, refund_claim_until = CASE WHEN refund_work_state IN ('Completed', 'Reconciliation required') THEN ? ELSE NULL END, refund_claim_token = CASE WHEN refund_work_state IN ('Completed', 'Reconciliation required') THEN refund_claim_token ELSE NULL END WHERE id = ? AND refund_claim_token = ?",
                vec![now.into()]).await?;
            Err(error)
        }
        Ok(false) => {
            owned_refund_update(db, id, &token,
                "UPDATE orders SET refund_claim_until = NULL, refund_claim_token = NULL WHERE id = ? AND refund_claim_token = ?",
                vec![]).await
        }
        Ok(true) => finalize_refund(db, id, &token, now).await,
    }
}

async fn owned_refund_update(
    db: &DatabaseConnection,
    id: &str,
    token: &str,
    sql: &str,
    mut values: Vec<sea_orm::Value>,
) -> Result<(), AdminOrderError> {
    values.extend([id.into(), token.into()]);
    if db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            sql,
            values,
        ))
        .await?
        .rows_affected()
        != 1
    {
        return Err(AdminOrderError::ClaimLost);
    }
    Ok(())
}

async fn save_refund_identity(
    db: &DatabaseConnection,
    id: &str,
    token: &str,
    refund_id: &str,
) -> Result<(), AdminOrderError> {
    owned_refund_update(
        db,
        id,
        token,
        "UPDATE orders SET stripe_refund_id = ? WHERE id = ? AND refund_claim_token = ?",
        vec![refund_id.into()],
    )
    .await
}

pub async fn mark_refunded(
    db: &DatabaseConnection,
    id: &str,
    now: i64,
) -> Result<(), AdminOrderError> {
    let token = Uuid::new_v4().to_string();
    let claimed = db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE orders SET refund_claim_token = ?, refund_claim_until = ?, refund_work_state = CASE WHEN refund_work_state = 'Not requested' THEN 'Pending' ELSE refund_work_state END WHERE id = ? AND status IN ('Paid', 'RefundPending', 'Refunded') AND (refund_claim_until IS NULL OR refund_claim_until <= ?)",
        [token.clone().into(), (now + 300).into(), id.into(), now.into()])).await?.rows_affected();
    if claimed != 1 {
        return Err(AdminOrderError::ClaimLost);
    }
    finalize_refund(db, id, &token, now).await
}

async fn finalize_refund(
    db: &DatabaseConnection,
    id: &str,
    token: &str,
    now: i64,
) -> Result<(), AdminOrderError> {
    let transaction = db.begin().await?;
    let changed = transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE orders SET status = 'Refunded', refunded_at = COALESCE(refunded_at, ?), refund_work_state = 'Completed', refund_claim_until = NULL, refund_claim_token = NULL, refund_notice_state = CASE WHEN refund_notice_state = 'Not requested' AND (total_cents > 0 OR event_id NOT IN (SELECT id FROM events WHERE status = 'Cancelled')) THEN 'Pending' ELSE refund_notice_state END WHERE id = ? AND refund_claim_token = ? AND status IN ('Paid', 'RefundPending', 'Refunded')", [now.into(), id.into(), token.into()]
    )).await?.rows_affected();
    if changed != 1 {
        return Err(AdminOrderError::ClaimLost);
    }
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE tickets SET status = 'Void' WHERE order_id = ? AND status = 'Valid'",
            [id.into()],
        ))
        .await?;
    transaction.commit().await?;
    Ok(())
}

pub async fn cancel_event(
    db: &DatabaseConnection,
    event_id: &str,
    now: i64,
) -> Result<Vec<String>, AdminOrderError> {
    let transaction = db.begin().await?;
    let changed = transaction.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE events SET updated_at = CASE WHEN status = 'Cancelled' THEN updated_at ELSE ? END, status = 'Cancelled' WHERE id = ?",
        [now.into(), event_id.into()],
    )).await?.rows_affected();
    if changed == 0 {
        return Err(AdminOrderError::NotFound);
    }
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE holds SET status = 'Expired' WHERE event_id = ? AND status = 'Open'",
            [event_id.into()],
        ))
        .await?;
    transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE orders SET refund_work_state = CASE WHEN refund_work_state = 'Not requested' AND status = 'Refunded' THEN 'Completed' WHEN refund_work_state = 'Not requested' THEN 'Pending' ELSE refund_work_state END, cancel_notice_state = CASE WHEN cancel_notice_state = 'Not requested' THEN CASE WHEN cancel_notice_sent_at IS NOT NULL THEN 'Uncertain' ELSE 'Pending' END ELSE cancel_notice_state END WHERE event_id = ? AND status IN ('Paid', 'RefundPending', 'Refunded')", [event_id.into()]
    )).await?;
    let rows = transaction
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id FROM orders WHERE event_id = ? AND status IN ('Paid', 'RefundPending', 'Refunded') ORDER BY created_at, id",
            [event_id.into()],
        ))
        .await?;
    let ids = rows
        .into_iter()
        .map(|row| row.try_get("", "id"))
        .collect::<Result<Vec<String>, DbErr>>()?;
    transaction.commit().await?;
    Ok(ids)
}

pub async fn notice_sent(
    db: &DatabaseConnection,
    id: &str,
    cancellation: bool,
) -> Result<bool, AdminOrderError> {
    let column = if cancellation {
        "cancel_notice_sent_at"
    } else {
        "refund_notice_sent_at"
    };
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            format!("SELECT {column} AS sent_at FROM orders WHERE id = ?"),
            [id.into()],
        ))
        .await?
        .ok_or(AdminOrderError::NotFound)?;
    Ok(row.try_get::<Option<i64>>("", "sent_at")?.is_some())
}

pub async fn mark_notice_sent(
    db: &DatabaseConnection,
    id: &str,
    cancellation: bool,
    now: i64,
) -> Result<(), AdminOrderError> {
    let column = if cancellation {
        "cancel_notice_sent_at"
    } else {
        "refund_notice_sent_at"
    };
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        format!(
            "UPDATE orders SET {column} = COALESCE({column}, ?), {} = 'Completed' WHERE id = ?",
            if cancellation {
                "cancel_notice_state"
            } else {
                "refund_notice_state"
            }
        ),
        [now.into(), id.into()],
    ))
    .await?;
    Ok(())
}

pub async fn anonymize(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    id: &str,
) -> Result<(), AdminOrderError> {
    let empty = cipher.encrypt("buyer_email", "")?;
    let transaction = db.begin().await?;
    let affected = transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE orders SET buyer_email_encrypted = ?, email_state = CASE WHEN email_sent_at IS NULL THEN 'No recipient' ELSE email_state END WHERE id = ?",
            [empty.into(), id.into()],
        ))
        .await?
        .rows_affected();
    if affected == 0 {
        return Err(AdminOrderError::NotFound);
    }
    crate::buyer_messages::anonymize_recipients(&transaction, cipher, id).await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE tickets SET attendee_name_encrypted = NULL WHERE order_id = ?",
            [id.into()],
        ))
        .await?;
    transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE holds SET buyer_email_encrypted = NULL WHERE id = (SELECT hold_id FROM orders WHERE id = ?)", [id.into()]
    )).await?;
    transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE hold_attendees SET attendee_name_encrypted = NULL WHERE hold_id = (SELECT hold_id FROM orders WHERE id = ?)", [id.into()]
    )).await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "DELETE FROM hold_answers WHERE hold_id = (SELECT hold_id FROM orders WHERE id = ?)",
            [id.into()],
        ))
        .await?;
    transaction.commit().await?;
    Ok(())
}

pub async fn complimentary(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: &str,
    ticket_type_id: &str,
    email: &str,
    name: &str,
    now: i64,
) -> Result<String, AdminOrderError> {
    complimentary_with_seat(
        db,
        cipher,
        event_id,
        (ticket_type_id, None),
        email,
        name,
        now,
    )
    .await
}

pub async fn complimentary_with_seat(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: &str,
    choice: (&str, Option<&str>),
    email: &str,
    name: &str,
    now: i64,
) -> Result<String, AdminOrderError> {
    let (ticket_type_id, seat_id) = choice;
    let email = email.trim();
    if email.len() > 254 || !email.contains('@') || email.starts_with('@') || email.ends_with('@') {
        return Err(AdminOrderError::InvalidEmail);
    }
    let transaction = db.begin().await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET updated_at = updated_at WHERE id = ?",
            [event_id.into()],
        ))
        .await?;
    let event = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT event_capacity, status FROM events WHERE id = ?",
            [event_id.into()],
        ))
        .await?
        .ok_or(AdminOrderError::NoCapacity)?;
    let status: String = event.try_get("", "status")?;
    if status == "Cancelled" {
        return Err(AdminOrderError::NoCapacity);
    }
    let assigned = crate::seats::assigned(&transaction, event_id).await?;
    if assigned != seat_id.is_some() {
        return Err(AdminOrderError::SeatRequired);
    }
    if let Some(seat_id) = seat_id {
        let seat = transaction.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT s.active,
                EXISTS(SELECT 1 FROM tickets t WHERE t.seat_id = s.id AND t.status = 'Valid') AS sold,
                EXISTS(SELECT 1 FROM hold_seats hs JOIN holds h ON h.id = hs.hold_id WHERE hs.seat_id = s.id AND h.status = 'Open' AND h.expires_at > ?) AS held
             FROM seats s WHERE s.id = ? AND s.event_id = ? AND s.ticket_type_id = ?",
            [now.into(), seat_id.into(), event_id.into(), ticket_type_id.into()]
        )).await?.ok_or(AdminOrderError::SeatRequired)?;
        if seat.try_get::<i64>("", "active")? != 1
            || seat.try_get::<i64>("", "sold")? != 0
            || seat.try_get::<i64>("", "held")? != 0
        {
            return Err(AdminOrderError::SeatRequired);
        }
    }
    let capacity: Option<i64> = event.try_get("", "event_capacity")?;
    let kind = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT capacity FROM ticket_types WHERE id = ? AND event_id = ?",
            [ticket_type_id.into(), event_id.into()],
        ))
        .await?
        .ok_or(AdminOrderError::NoCapacity)?;
    let type_capacity: i64 = kind.try_get("", "capacity")?;
    let (sold, held) = inventory::open_counts(&transaction, event_id, now).await?;
    let (type_sold, type_held) = inventory::type_counts(&transaction, ticket_type_id, now).await?;
    if capacity.is_some_and(|cap| sold + held + 1 > cap)
        || type_sold + type_held + 1 > type_capacity
    {
        return Err(AdminOrderError::NoCapacity);
    }
    let id = Uuid::new_v4().to_string();
    let hold_id = Uuid::new_v4().to_string();
    let session_id = format!("complimentary_{hold_id}");
    let number = format!("CG-{}", id[..8].to_uppercase());
    let buyer = cipher.encrypt("buyer_email", email)?;
    let attendee = if name.trim().is_empty() {
        None
    } else {
        Some(cipher.encrypt("attendee_name", name.trim())?)
    };
    transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO holds (id, event_id, status, expires_at, stripe_session_id, buyer_email_encrypted, created_at) VALUES (?, ?, 'Paid', ?, ?, ?, ?)",
        [hold_id.clone().into(), event_id.into(), (now + 1800).into(), session_id.clone().into(), buyer.clone().into(), now.into()]
    )).await?;
    transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO hold_items (hold_id, ticket_type_id, quantity, price_cents) VALUES (?, ?, 1, 0)", [hold_id.clone().into(), ticket_type_id.into()]
    )).await?;
    if let Some(seat_id) = seat_id {
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "INSERT INTO hold_seats (hold_id, seat_id) VALUES (?, ?)",
                [hold_id.clone().into(), seat_id.into()],
            ))
            .await?;
    }
    transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO orders (id, number, event_id, hold_id, stripe_session_id, buyer_email_encrypted, total_cents, status, is_complimentary, created_at) VALUES (?, ?, ?, ?, ?, ?, 0, 'Paid', 1, ?)",
        [id.clone().into(), number.into(), event_id.into(), hold_id.into(), session_id.into(), buyer.into(), now.into()]
    )).await?;
    transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO tickets (id, order_id, event_id, ticket_type_id, code, attendee_name_encrypted, status, created_at, seat_id) VALUES (?, ?, ?, ?, ?, ?, 'Valid', ?, ?)",
        [Uuid::new_v4().to_string().into(), id.clone().into(), event_id.into(), ticket_type_id.into(), orders::ticket_code().into(), attendee.into(), now.into(), seat_id.into()]
    )).await?;
    transaction.commit().await?;
    Ok(id)
}

pub async fn order(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    id: &str,
) -> Result<Order, AdminOrderError> {
    orders::get_order(db, cipher, id)
        .await
        .map_err(|error| match error {
            orders::OrderError::HoldNotFound => AdminOrderError::NotFound,
            orders::OrderError::Database(error) => AdminOrderError::Database(error),
            orders::OrderError::Crypto(error) => AdminOrderError::Crypto(error),
            _ => AdminOrderError::NotFound,
        })
}

pub enum Report {
    Orders,
    Tickets,
    Checkins,
}

fn csv_field(value: &str) -> String {
    let safe = if value.trim_start().starts_with(['=', '+', '-', '@']) {
        format!("'{value}")
    } else {
        value.to_owned()
    };
    if safe.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", safe.replace('"', "\"\""))
    } else {
        safe
    }
}

// Unchecked checkboxes have no answer row; only the matching asked snapshot establishes `No`.
fn answer_cells(
    questions: &[Question],
    order: &ExportAnswers,
    attendee_number: Option<i64>,
    anonymized: bool,
) -> Vec<String> {
    questions
        .iter()
        .map(|question| {
            let answer = order
                .answers
                .iter()
                .find(|saved| {
                    saved.question == *question && saved.attendee_number == attendee_number
                })
                .map(|saved| saved.answer.as_str());
            match (question.kind.as_str(), answer) {
                _ if anonymized => String::new(),
                ("Checkbox", Some(_)) => "Yes".to_owned(),
                ("Checkbox", None) if order.asked.contains(question) => "No".to_owned(),
                (_, answer) => answer.unwrap_or_default().to_owned(),
            }
        })
        .collect()
}

fn scoped_questions(questions: &[Question], scope: &str) -> Vec<Question> {
    questions
        .iter()
        .filter(|question| question.scope == scope)
        .cloned()
        .collect()
}

fn question_headers(
    questions: &[Question],
    fixed: &[&str],
) -> Result<Vec<String>, AdminOrderError> {
    let mut label_counts = HashMap::new();
    let mut used = fixed
        .iter()
        .map(|label| csv_field(label))
        .collect::<HashSet<_>>();
    for question in questions {
        *label_counts.entry(csv_field(&question.label)).or_insert(0) += 1;
    }
    let fixed_headers = used.clone();
    used.extend(label_counts.keys().cloned());
    questions
        .iter()
        .map(|question| {
            let label = csv_field(&question.label);
            if label_counts[&label] == 1 && !fixed_headers.contains(&label) {
                return Ok(question.label.clone());
            }
            let snapshot = serde_json::to_vec(question).map_err(QuestionError::from)?;
            let base = format!(
                "{} [{}; {}]",
                question.label,
                question.id,
                hex::encode(Sha256::digest(snapshot))
            );
            let mut header = base.clone();
            let mut suffix = 2;
            while !used.insert(csv_field(&header)) {
                header = format!("{base} [{suffix}]");
                suffix += 1;
            }
            Ok(header)
        })
        .collect()
}

fn csv_line<S: AsRef<str>>(fields: &[S]) -> String {
    let mut line = fields
        .iter()
        .map(|value| csv_field(value.as_ref()))
        .collect::<Vec<_>>()
        .join(",");
    line.push_str("\r\n");
    line
}

pub async fn report(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: &str,
    kind: Report,
) -> Result<String, AdminOrderError> {
    let mut output = String::new();
    let event_questions = match kind {
        Report::Orders | Report::Tickets => questions::list_for_export(db, event_id).await?,
        Report::Checkins => questions::ExportQuestions::default(),
    };
    match kind {
        Report::Orders => {
            let questions = scoped_questions(&event_questions.columns, "Order");
            let mut header = vec![
                "Order",
                "Buyer email",
                "Original cents",
                "Discount code",
                "Discount cents",
                "Paid cents",
                "Status",
                "Complimentary",
                "Created at",
            ];
            let labels = question_headers(&questions, &header)?;
            header.extend(labels.iter().map(String::as_str));
            output.push_str(&csv_line(&header));
            let orders = list(db, cipher, Some(event_id), "").await?;
            for chunk in orders.chunks(questions::EXPORT_BATCH_SIZE) {
                let saved_answers = if questions.is_empty() {
                    HashMap::new()
                } else {
                    let ids = chunk
                        .iter()
                        .map(|order| order.id.as_str())
                        .collect::<Vec<_>>();
                    questions::export_answers(
                        db,
                        cipher,
                        questions::ExportSelection::Orders(&ids),
                        &questions,
                        &[],
                    )
                    .await?
                };
                for order in chunk {
                    let mut fields = vec![
                        order.number.clone(),
                        order.buyer_email.clone(),
                        order.subtotal_cents.to_string(),
                        order.discount_code.clone().unwrap_or_default(),
                        order.discount_cents.to_string(),
                        order.total_cents.to_string(),
                        order.status.clone(),
                        if order.complimentary { "Yes" } else { "No" }.to_owned(),
                        order.created_at.to_string(),
                    ];
                    if !questions.is_empty() {
                        fields.extend(answer_cells(
                            &questions,
                            &saved_answers[&order.id],
                            None,
                            order.buyer_email.is_empty(),
                        ));
                    }
                    output.push_str(&csv_line(&fields));
                }
            }
        }
        Report::Tickets => {
            let questions = scoped_questions(&event_questions.columns, "Attendee");
            let mut header = vec![
                "Order",
                "Ticket code",
                "Ticket Type",
                "Seat",
                "Attendee",
                "Buyer email",
                "Status",
                "Checked in at",
            ];
            let labels = question_headers(&questions, &header)?;
            header.extend(labels.iter().map(String::as_str));
            output.push_str(&csv_line(&header));
            // Attendee answers are keyed by checkout position, which matches ticket insertion order within an order.
            let rows = db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT o.id AS order_id, o.number, o.buyer_email_encrypted, (SELECT COUNT(*) FROM tickets earlier WHERE earlier.order_id = t.order_id AND earlier.rowid < t.rowid) AS ordinal, t.code, t.attendee_name_encrypted, t.status, tt.name AS ticket_type, ci.checked_at, s.section, s.row_label, s.seat_label FROM tickets t JOIN orders o ON o.id=t.order_id JOIN ticket_types tt ON tt.id=t.ticket_type_id LEFT JOIN checkins ci ON ci.ticket_id=t.id AND ci.undone_at IS NULL LEFT JOIN seats s ON s.id=t.seat_id WHERE t.event_id=? ORDER BY t.created_at, t.id", [event_id.into()]
            )).await?;
            let answer_ids = event_questions.answer_ids(&questions);
            for chunk in rows.chunks(questions::export_ticket_batch_size(&answer_ids)) {
                let saved_answers = if questions.is_empty() {
                    HashMap::new()
                } else {
                    let tickets = chunk
                        .iter()
                        .map(|row| {
                            Ok((
                                row.try_get::<String>("", "order_id")?,
                                row.try_get::<i64>("", "ordinal")?,
                            ))
                        })
                        .collect::<Result<Vec<_>, DbErr>>()?;
                    let tickets = tickets
                        .iter()
                        .map(|(id, ordinal)| (id.as_str(), *ordinal))
                        .collect::<Vec<_>>();
                    questions::export_answers(
                        db,
                        cipher,
                        questions::ExportSelection::Tickets(&tickets),
                        &questions,
                        &answer_ids,
                    )
                    .await?
                };
                for row in chunk {
                    let email: String = row.try_get("", "buyer_email_encrypted")?;
                    let name: Option<String> = row.try_get("", "attendee_name_encrypted")?;
                    let checked: Option<i64> = row.try_get("", "checked_at")?;
                    let seat = match (
                        row.try_get::<Option<String>>("", "section")?,
                        row.try_get::<Option<String>>("", "row_label")?,
                        row.try_get::<Option<String>>("", "seat_label")?,
                    ) {
                        (Some(section), Some(row), Some(label)) => {
                            crate::seats::display(&section, &row, &label)
                        }
                        _ => String::new(),
                    };
                    let buyer_email = cipher.decrypt("buyer_email", &email)?;
                    let mut fields = vec![
                        row.try_get::<String>("", "number")?,
                        row.try_get::<String>("", "code")?,
                        row.try_get::<String>("", "ticket_type")?,
                        seat,
                        name.map(|value| cipher.decrypt("attendee_name", &value))
                            .transpose()?
                            .unwrap_or_default(),
                        buyer_email.clone(),
                        row.try_get::<String>("", "status")?,
                        checked.map(|value| value.to_string()).unwrap_or_default(),
                    ];
                    if !questions.is_empty() {
                        let order_id: String = row.try_get("", "order_id")?;
                        let ordinal: i64 = row.try_get("", "ordinal")?;
                        fields.extend(answer_cells(
                            &questions,
                            &saved_answers[&order_id],
                            Some(ordinal + 1),
                            buyer_email.is_empty(),
                        ));
                    }
                    output.push_str(&csv_line(&fields));
                }
            }
        }
        Report::Checkins => {
            output.push_str(&csv_line(&[
                "Ticket code",
                "Seat",
                "Attendee",
                "Checked in at",
                "Actor Type",
                "Actor ID",
                "Undone at",
                "Undone by",
            ]));
            let rows = db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT t.code, t.attendee_name_encrypted, ci.checked_at, ci.actor_type, ci.actor_id, ci.undone_at, ci.undo_member_id AS undone_by, s.section, s.row_label, s.seat_label FROM checkins ci JOIN tickets t ON t.id=ci.ticket_id LEFT JOIN seats s ON s.id=t.seat_id WHERE ci.event_id=? ORDER BY ci.checked_at, ci.id", [event_id.into()]
            )).await?;
            for row in rows {
                let name: Option<String> = row.try_get("", "attendee_name_encrypted")?;
                let undone: Option<i64> = row.try_get("", "undone_at")?;
                let undone_by: Option<String> = row.try_get("", "undone_by")?;
                let seat = match (
                    row.try_get::<Option<String>>("", "section")?,
                    row.try_get::<Option<String>>("", "row_label")?,
                    row.try_get::<Option<String>>("", "seat_label")?,
                ) {
                    (Some(section), Some(row), Some(label)) => {
                        crate::seats::display(&section, &row, &label)
                    }
                    _ => String::new(),
                };
                output.push_str(&csv_line(&[
                    &row.try_get::<String>("", "code")?,
                    &seat,
                    &name
                        .map(|value| cipher.decrypt("attendee_name", &value))
                        .transpose()?
                        .unwrap_or_default(),
                    &row.try_get::<i64>("", "checked_at")?.to_string(),
                    &row.try_get::<String>("", "actor_type")?,
                    &row.try_get::<String>("", "actor_id")?,
                    &undone.map(|value| value.to_string()).unwrap_or_default(),
                    &undone_by.unwrap_or_default(),
                ]));
            }
        }
    }
    Ok(output)
}
use std::collections::{HashMap, HashSet};

#[derive(serde::Serialize)]
pub struct WorkProgress {
    pub refund: String,
    pub cancellation_notice: String,
    pub refund_notice: String,
    #[serde(skip)]
    pub refund_claimed: bool,
}

pub async fn work_progress(
    db: &DatabaseConnection,
    id: &str,
) -> Result<WorkProgress, AdminOrderError> {
    let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT refund_work_state, cancel_notice_state, refund_notice_state, (refund_claim_token IS NOT NULL OR refund_claim_until IS NOT NULL) AS refund_claimed FROM orders WHERE id = ?", [id.into()]
    )).await?.ok_or(AdminOrderError::NotFound)?;
    Ok(WorkProgress {
        refund: row.try_get("", "refund_work_state")?,
        cancellation_notice: row.try_get("", "cancel_notice_state")?,
        refund_notice: row.try_get("", "refund_notice_state")?,
        refund_claimed: row.try_get("", "refund_claimed")?,
    })
}

pub async fn record_refund_failure(
    db: &DatabaseConnection,
    id: &str,
) -> Result<(), AdminOrderError> {
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE orders SET refund_work_state = 'Failed', refund_claim_until = NULL WHERE id = ? AND status IN ('Paid', 'RefundPending', 'Refunded') AND refund_work_state NOT IN ('Completed', 'Reconciliation required') AND refund_claim_until IS NULL AND refund_claim_token IS NULL", [id.into()]
    )).await?;
    Ok(())
}

pub async fn claim_notice(
    db: &DatabaseConnection,
    id: &str,
    cancellation: bool,
) -> Result<bool, AdminOrderError> {
    let (state, sent) = if cancellation {
        ("cancel_notice_state", "cancel_notice_sent_at")
    } else {
        ("refund_notice_state", "refund_notice_sent_at")
    };
    let eligibility = if cancellation {
        "event_id IN (SELECT id FROM events WHERE status = 'Cancelled')"
    } else {
        "refund_work_state = 'Completed' AND refund_claim_token IS NULL"
    };
    Ok(db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        format!("UPDATE orders SET {state} = 'Sending', {state}_claimed_at = unixepoch() WHERE id = ? AND {sent} IS NULL AND {state} IN ('Not requested', 'Pending', 'Failed') AND {eligibility}"), [id.into()]
    )).await?.rows_affected() == 1)
}

pub async fn record_notice_error(
    db: &DatabaseConnection,
    id: &str,
    cancellation: bool,
    safe_to_retry: bool,
) -> Result<(), AdminOrderError> {
    let state = if cancellation {
        "cancel_notice_state"
    } else {
        "refund_notice_state"
    };
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        format!("UPDATE orders SET {state} = ? WHERE id = ? AND {state} = 'Sending'"),
        [
            if safe_to_retry { "Failed" } else { "Uncertain" }.into(),
            id.into(),
        ],
    ))
    .await?;
    Ok(())
}

pub async fn skip_notice(
    db: &DatabaseConnection,
    id: &str,
    cancellation: bool,
) -> Result<(), AdminOrderError> {
    let state = if cancellation {
        "cancel_notice_state"
    } else {
        "refund_notice_state"
    };
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        format!("UPDATE orders SET {state} = 'No recipient' WHERE id = ? AND {state} IN ('Not requested', 'Pending', 'Failed')"), [id.into()]
    )).await?;
    Ok(())
}

pub async fn record_refund_provider_status(
    db: &DatabaseConnection,
    id: &str,
    refund_id: &str,
    failed: bool,
) -> Result<(), AdminOrderError> {
    let state = if failed { "Failed" } else { "Pending" };
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE orders SET status = CASE WHEN status = 'Refunded' THEN status ELSE 'RefundPending' END, refund_work_state = ?, refund_retry_after = CASE WHEN ? THEN ? ELSE refund_retry_after END, stripe_refund_id = ?, refund_notice_state = CASE WHEN refund_notice_state IN ('Pending', 'Failed') THEN 'Not requested' ELSE refund_notice_state END WHERE id = ? AND refund_claim_until IS NULL AND refund_claim_token IS NULL AND status IN ('Paid', 'RefundPending', 'Refunded') AND (stripe_refund_id = ? OR (stripe_refund_id IS NULL AND refund_work_state != 'Completed')) AND (COALESCE(refund_retry_after, '') != ? OR ?)",
        [state.into(), failed.into(), refund_id.into(), refund_id.into(), id.into(), refund_id.into(), refund_id.into(), failed.into()])).await?;
    Ok(())
}

async fn record_owned_refund_provider_status(
    db: &DatabaseConnection,
    id: &str,
    token: &str,
    refund_id: &str,
    failed: bool,
) -> Result<(), AdminOrderError> {
    owned_refund_update(db, id, token,
        "UPDATE orders SET refund_work_state = ?, refund_retry_after = CASE WHEN ? THEN ? ELSE refund_retry_after END, refund_notice_state = CASE WHEN refund_notice_state IN ('Pending', 'Failed') THEN 'Not requested' ELSE refund_notice_state END WHERE id = ? AND refund_claim_token = ?",
        vec![if failed { "Failed" } else { "Pending" }.into(), failed.into(), refund_id.into()]).await
}

pub async fn reconcile_notice(
    db: &DatabaseConnection,
    id: &str,
    cancellation: bool,
    accepted: bool,
    now: i64,
) -> Result<bool, AdminOrderError> {
    let (state, receipt) = if cancellation {
        ("cancel_notice_state", "cancel_notice_sent_at")
    } else {
        ("refund_notice_state", "refund_notice_sent_at")
    };
    let eligibility = if cancellation {
        "event_id IN (SELECT id FROM events WHERE status = 'Cancelled')"
    } else {
        "refund_work_state = 'Completed'"
    };
    Ok(db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        format!("UPDATE orders SET {state} = ?, {receipt} = ?, {state}_claimed_at = NULL WHERE id = ? AND {state} IN ('Sending', 'Uncertain') AND ({state}_claimed_at IS NULL OR {state}_claimed_at <= ?) AND (? OR {eligibility})"),
        [if accepted { "Completed" } else { "Pending" }.into(), if accepted { Some(now) } else { None }.into(), id.into(), (now - 300).into(), accepted.into()])).await?.rows_affected() == 1)
}
