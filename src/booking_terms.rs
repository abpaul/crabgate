use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, Statement, TransactionTrait,
};
use serde::Serialize;
use uuid::Uuid;

use crate::settings::{self, BookingTerms};

#[derive(Debug, thiserror::Error)]
pub enum BookingTermsError {
    #[error("Booking terms changed. Review the link below and confirm again.")]
    ReviewRequired,
    #[error("Please acknowledge the organizer's booking terms.")]
    AcknowledgementRequired,
    #[error(transparent)]
    Database(#[from] DbErr),
}

#[derive(Serialize)]
pub struct Display {
    pub terms: BookingTerms,
    pub token: String,
}

pub struct Submission<'a> {
    pub token: &'a str,
    pub accepted: bool,
}

fn decode(value: &str) -> Result<BookingTerms, DbErr> {
    serde_json::from_str(value).map_err(|error| DbErr::Custom(error.to_string()))
}

pub async fn display(db: &DatabaseConnection, hold_id: &str, now: i64) -> Result<Display, DbErr> {
    let transaction = db.begin().await?;
    transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE holds SET booking_terms_token = booking_terms_token WHERE id = ? AND status = 'Open' AND expires_at > ?",
        [hold_id.into(), now.into()]
    )).await?;
    let row = transaction.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT booking_terms_snapshot, booking_terms_token, booking_terms_submitted_at FROM holds WHERE id = ? AND status = 'Open' AND expires_at > ?",
        [hold_id.into(), now.into()]
    )).await?.ok_or_else(|| DbErr::Custom("Checkout has closed".into()))?;
    let snapshot: Option<String> = row.try_get("", "booking_terms_snapshot")?;
    let token: Option<String> = row.try_get("", "booking_terms_token")?;
    let submitted: Option<i64> = row.try_get("", "booking_terms_submitted_at")?;
    let current = settings::booking_terms(&transaction).await?;
    let previous = snapshot.as_deref().map(decode).transpose()?;
    if submitted.is_some() {
        transaction.commit().await?;
        return Ok(Display {
            terms: previous.unwrap_or(BookingTerms {
                url: String::new(),
                required: false,
                wording: current.wording,
            }),
            token: token.unwrap_or_default(),
        });
    }
    if let (Some(terms), Some(token)) = (&previous, token) {
        if terms == &current {
            let display = Display {
                terms: terms.clone(),
                token,
            };
            transaction.commit().await?;
            return Ok(display);
        }
    }
    let token = Uuid::new_v4().to_string();
    let snapshot =
        serde_json::to_string(&current).map_err(|error| DbErr::Custom(error.to_string()))?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE holds SET booking_terms_snapshot = ?, booking_terms_token = ? WHERE id = ?",
            [snapshot.into(), token.clone().into(), hold_id.into()],
        ))
        .await?;
    transaction.commit().await?;
    Ok(Display {
        terms: current,
        token,
    })
}

pub(crate) async fn confirm<C: ConnectionTrait>(
    db: &C,
    hold_id: &str,
    submission: Option<&Submission<'_>>,
    now: i64,
) -> Result<(), BookingTermsError> {
    let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT booking_terms_snapshot, booking_terms_token, booking_terms_submitted_at FROM holds WHERE id = ?",
        [hold_id.into()]
    )).await?.ok_or(BookingTermsError::ReviewRequired)?;
    let snapshot: Option<String> = row.try_get("", "booking_terms_snapshot")?;
    let terms = snapshot.as_deref().map(decode).transpose()?;
    let submitted: Option<i64> = row.try_get("", "booking_terms_submitted_at")?;
    let Some(submission) = submission else {
        if submitted.is_some() {
            return Ok(());
        }
        if terms.is_some_and(|terms| terms.required) || settings::booking_terms(db).await?.required
        {
            return Err(BookingTermsError::AcknowledgementRequired);
        }
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE holds SET booking_terms_submitted_at = ? WHERE id = ?",
            [now.into(), hold_id.into()],
        ))
        .await?;
        return Ok(());
    };
    let token: Option<String> = row.try_get("", "booking_terms_token")?;
    if submitted.is_some() && token.is_none() && submission.token.is_empty() {
        return Ok(());
    }
    let terms = terms.ok_or(BookingTermsError::ReviewRequired)?;
    if token.as_deref() != Some(submission.token) {
        let current = settings::booking_terms(db).await?;
        if !terms.url.is_empty() || terms.required || terms != current {
            return Err(BookingTermsError::ReviewRequired);
        }
    }
    if submitted.is_some() {
        return Ok(());
    }
    if terms != settings::booking_terms(db).await? {
        return Err(BookingTermsError::ReviewRequired);
    }
    if terms.required && !submission.accepted {
        return Err(BookingTermsError::AcknowledgementRequired);
    }
    let accepted_at = (!terms.url.is_empty() && submission.accepted).then_some(now);
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE holds SET booking_terms_submitted_at = ?, booking_terms_accepted_at = ? WHERE id = ?",
        [now.into(), accepted_at.into(), hold_id.into()]
    )).await?;
    Ok(())
}

pub(crate) async fn payment_ready<C: ConnectionTrait>(
    db: &C,
    hold_id: &str,
) -> Result<bool, DbErr> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT booking_terms_snapshot, booking_terms_accepted_at FROM holds WHERE id = ?",
            [hold_id.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::Custom("Checkout missing".into()))?;
    let snapshot: Option<String> = row.try_get("", "booking_terms_snapshot")?;
    let accepted: Option<i64> = row.try_get("", "booking_terms_accepted_at")?;
    Ok(!snapshot
        .as_deref()
        .map(decode)
        .transpose()?
        .is_some_and(|terms| terms.required)
        || accepted.is_some())
}

#[derive(Serialize)]
pub struct Evidence {
    pub terms: BookingTerms,
    pub accepted_at: i64,
}

pub async fn order_evidence<C: ConnectionTrait>(
    db: &C,
    order_id: &str,
) -> Result<Option<Evidence>, DbErr> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT booking_terms_snapshot, booking_terms_accepted_at FROM orders WHERE id = ?",
            [order_id.into()],
        ))
        .await?;
    let Some(row) = row else { return Ok(None) };
    match (
        row.try_get::<Option<String>>("", "booking_terms_snapshot")?,
        row.try_get::<Option<i64>>("", "booking_terms_accepted_at")?,
    ) {
        (Some(snapshot), Some(accepted_at)) => Ok(Some(Evidence {
            terms: decode(&snapshot)?,
            accepted_at,
        })),
        _ => Ok(None),
    }
}
