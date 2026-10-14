use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, Statement};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum DiscountError {
    #[error("Use 2 to 32 letters, numbers, or hyphens for the code")]
    InvalidCode,
    #[error("Enter a valid percent or fixed amount")]
    InvalidValue,
    #[error("Enter a valid discount date range")]
    InvalidDates,
    #[error("That code isn't valid, has expired, or has been used up.")]
    Unavailable,
    #[error("This discount code already exists for the event")]
    AlreadyExists,
    #[error(transparent)]
    Database(#[from] DbErr),
}

pub struct DiscountCode {
    pub id: String,
    pub code: String,
    pub kind: String,
    pub value: i64,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub max_redemptions: Option<i64>,
    pub used: i64,
    pub active: bool,
}

pub struct AppliedDiscount {
    pub code_id: String,
    pub code: String,
    pub discount_cents: i64,
}

pub struct NewDiscount<'a> {
    pub event_id: &'a str,
    pub code: &'a str,
    pub kind: &'a str,
    pub value: i64,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub max_redemptions: Option<i64>,
}

pub fn normalize_code(code: &str) -> Result<String, DiscountError> {
    let normalized = code.trim().to_ascii_uppercase();
    if !(2..=32).contains(&normalized.len())
        || !normalized
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(DiscountError::InvalidCode);
    }
    Ok(normalized)
}

pub async fn create(
    db: &DatabaseConnection,
    input: NewDiscount<'_>,
    now: i64,
) -> Result<String, DiscountError> {
    let NewDiscount {
        event_id,
        code,
        kind,
        value,
        starts_at,
        ends_at,
        max_redemptions,
    } = input;
    let code = normalize_code(code)?;
    if !matches!(kind, "Percent" | "Fixed") || value <= 0 || (kind == "Percent" && value > 100) {
        return Err(DiscountError::InvalidValue);
    }
    if starts_at
        .zip(ends_at)
        .is_some_and(|(start, end)| start >= end)
        || max_redemptions.is_some_and(|limit| limit <= 0)
    {
        return Err(DiscountError::InvalidDates);
    }
    if db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT 1 FROM discount_codes WHERE event_id = ? AND code = ?",
            [event_id.into(), code.clone().into()],
        ))
        .await?
        .is_some()
    {
        return Err(DiscountError::AlreadyExists);
    }
    let id = Uuid::new_v4().to_string();
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO discount_codes (id, event_id, code, kind, value, starts_at, ends_at, max_redemptions, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        [id.clone().into(), event_id.into(), code.into(), kind.into(), value.into(), starts_at.into(), ends_at.into(), max_redemptions.into(), now.into()]
    )).await?;
    Ok(id)
}

pub async fn list(
    db: &DatabaseConnection,
    event_id: &str,
) -> Result<Vec<DiscountCode>, DiscountError> {
    let rows = db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT d.id, d.code, d.kind, d.value, d.starts_at, d.ends_at, d.max_redemptions, d.active,
            (SELECT COUNT(*) FROM hold_discounts hd JOIN orders o ON o.hold_id = hd.hold_id WHERE hd.code_id = d.id) AS used
         FROM discount_codes d WHERE d.event_id = ? ORDER BY d.created_at DESC",
        [event_id.into()]
    )).await?;
    rows.into_iter()
        .map(|row| {
            Ok(DiscountCode {
                id: row.try_get("", "id")?,
                code: row.try_get("", "code")?,
                kind: row.try_get("", "kind")?,
                value: row.try_get("", "value")?,
                starts_at: row.try_get("", "starts_at")?,
                ends_at: row.try_get("", "ends_at")?,
                max_redemptions: row.try_get("", "max_redemptions")?,
                used: row.try_get("", "used")?,
                active: row.try_get::<i64>("", "active")? == 1,
            })
        })
        .collect::<Result<Vec<_>, DbErr>>()
        .map_err(DiscountError::from)
}

pub async fn deactivate(
    db: &DatabaseConnection,
    event_id: &str,
    id: &str,
) -> Result<(), DiscountError> {
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE discount_codes SET active = 0 WHERE id = ? AND event_id = ?",
        [id.into(), event_id.into()],
    ))
    .await?;
    Ok(())
}

pub async fn apply<C: ConnectionTrait>(
    db: &C,
    event_id: &str,
    code: &str,
    subtotal_cents: i64,
    now: i64,
) -> Result<AppliedDiscount, DiscountError> {
    let code = normalize_code(code)?;
    let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT id, kind, value, starts_at, ends_at, max_redemptions FROM discount_codes WHERE event_id = ? AND code = ? AND active = 1",
        [event_id.into(), code.clone().into()]
    )).await?.ok_or(DiscountError::Unavailable)?;
    let starts_at: Option<i64> = row.try_get("", "starts_at")?;
    let ends_at: Option<i64> = row.try_get("", "ends_at")?;
    if starts_at.is_some_and(|start| now < start) || ends_at.is_some_and(|end| now >= end) {
        return Err(DiscountError::Unavailable);
    }
    let id: String = row.try_get("", "id")?;
    let limit: Option<i64> = row.try_get("", "max_redemptions")?;
    if let Some(limit) = limit {
        let count = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT (SELECT COUNT(*) FROM hold_discounts hd JOIN orders o ON o.hold_id = hd.hold_id WHERE hd.code_id = ?) +
                    (SELECT COUNT(*) FROM hold_discounts hd JOIN holds h ON h.id = hd.hold_id WHERE hd.code_id = ? AND h.status = 'Open' AND h.expires_at > ?) AS count",
            [id.clone().into(), id.clone().into(), now.into()]
        )).await?.ok_or(DiscountError::Unavailable)?;
        if count.try_get::<i64>("", "count")? >= limit {
            return Err(DiscountError::Unavailable);
        }
    }
    let kind: String = row.try_get("", "kind")?;
    let value: i64 = row.try_get("", "value")?;
    let discount_cents = if kind == "Percent" {
        subtotal_cents
            .checked_mul(value)
            .ok_or(DiscountError::InvalidValue)?
            / 100
    } else {
        value.min(subtotal_cents)
    };
    Ok(AppliedDiscount {
        code_id: id,
        code,
        discount_cents,
    })
}

pub async fn order_summary(
    db: &DatabaseConnection,
    order_id: &str,
) -> Result<Option<(String, i64, i64)>, DiscountError> {
    let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT d.code_snapshot, d.subtotal_cents, d.discount_cents FROM hold_discounts d JOIN orders o ON o.hold_id = d.hold_id WHERE o.id = ?",
        [order_id.into()]
    )).await?;
    match row {
        Some(row) => Ok(Some((
            row.try_get("", "code_snapshot")?,
            row.try_get("", "subtotal_cents")?,
            row.try_get("", "discount_cents")?,
        ))),
        None => Ok(None),
    }
}
