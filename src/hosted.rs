use chrono::Utc;
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, Statement, TransactionTrait,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    members::{self, Role},
    organizers::{reserved_subdomain, Edition, Organizer, OrganizerError, OrganizerRegistry},
    settings,
};

const DAY: i64 = 86_400;

#[derive(Debug, thiserror::Error)]
pub enum HostedError {
    #[error("Enter an organizer name, subdomain, and valid email")]
    InvalidSignup,
    #[error("Too many signup attempts. Wait one hour and try again, or recover your existing Shop using the link below.")]
    TooManySignups,
    #[error("This Shop address is taken. Choose another one.")]
    ShopAddressTaken,
    #[error("Enter your Shop address and a valid signup email")]
    InvalidRecovery,
    #[error("Verification link is invalid or expired")]
    InvalidVerification,
    #[error("You'll need a subscription before you can publish.")]
    SubscriptionRequired,
    #[error("Verify your email before you publish.")]
    VerificationRequired,
    #[error("Finish the Stripe key and webhook checks in Setup before you publish.")]
    SetupRequired,
    #[error("Organizer not found")]
    NotFound,
    #[error(transparent)]
    Database(#[from] DbErr),
    #[error(transparent)]
    Organizer(#[from] OrganizerError),
    #[error(transparent)]
    Member(#[from] members::MemberError),
    #[error(transparent)]
    Settings(#[from] settings::SettingsError),
}

pub struct Signup {
    pub organizer: Organizer,
    pub verification_token: String,
}

fn hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

async fn shop_address_exists(control: &DatabaseConnection, subdomain: &str) -> Result<bool, DbErr> {
    Ok(control
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT 1 FROM organizers WHERE subdomain = ?",
            [subdomain.into()],
        ))
        .await?
        .is_some())
}

async fn reserve_signup_attempt(
    control: &DatabaseConnection,
    key: &str,
    limit: i64,
    now: i64,
) -> Result<(), HostedError> {
    let cutoff = now - 3600;
    let reserved = control
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO signup_rate_limits (key, started_at, count) VALUES (?, ?, 1)
         ON CONFLICT(key) DO UPDATE SET
         started_at = CASE WHEN started_at <= ? THEN excluded.started_at ELSE started_at END,
         count = CASE WHEN started_at <= ? THEN 1 ELSE count + 1 END
         WHERE started_at <= ? OR count < ?",
            [
                key.into(),
                now.into(),
                cutoff.into(),
                cutoff.into(),
                cutoff.into(),
                limit.into(),
            ],
        ))
        .await?;
    if reserved.rows_affected() == 0 {
        return Err(HostedError::TooManySignups);
    }
    Ok(())
}

pub async fn public_signup(
    registry: &OrganizerRegistry,
    control: &DatabaseConnection,
    name: &str,
    subdomain: &str,
    email: &str,
    now: i64,
) -> Result<Signup, HostedError> {
    reserve_signup_attempt(control, "all", 20, now).await?;
    control
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "DELETE FROM signup_rate_limits WHERE key != 'all' AND started_at <= ?",
            [(now - 3600).into()],
        ))
        .await?;
    let recipient = email.trim().to_ascii_lowercase();
    if recipient.len() <= 254
        && recipient.contains('@')
        && !recipient.starts_with('@')
        && !recipient.ends_with('@')
    {
        reserve_signup_attempt(
            control,
            &hash(&format!("signup-recipient:{recipient}")),
            3,
            now,
        )
        .await?;
    }
    signup(registry, control, name, subdomain, email, now).await
}

pub async fn signup(
    registry: &OrganizerRegistry,
    control: &DatabaseConnection,
    name: &str,
    subdomain: &str,
    email: &str,
    now: i64,
) -> Result<Signup, HostedError> {
    let name = name.trim();
    let email = email.trim().to_ascii_lowercase();
    if name.is_empty()
        || name.len() > 120
        || email.len() > 254
        || !email.contains('@')
        || email.starts_with('@')
        || email.ends_with('@')
    {
        return Err(HostedError::InvalidSignup);
    }
    let subdomain = subdomain.trim();
    if reserved_subdomain(subdomain) || shop_address_exists(control, subdomain).await? {
        return Err(HostedError::ShopAddressTaken);
    }
    let organizer = match registry.create(name, subdomain).await {
        Ok(organizer) => organizer,
        Err(OrganizerError::InvalidSubdomain) => return Err(HostedError::InvalidSignup),
        Err(_) if shop_address_exists(control, subdomain).await? => {
            return Err(HostedError::ShopAddressTaken)
        }
        Err(error) => return Err(error.into()),
    };
    members::add_member(control, &organizer.id, &email, Role::Admin).await?;
    control.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO subscriptions (id, organizer_id, status, updated_at) VALUES (?, ?, 'Pending', ?)",
        [Uuid::new_v4().to_string().into(), organizer.id.clone().into(), now.into()]
    )).await?;
    let verification_token = request_verification(control, &organizer.id, &email, now).await?;
    Ok(Signup {
        organizer,
        verification_token,
    })
}

pub async fn request_verification(
    control: &DatabaseConnection,
    organizer_id: &str,
    email: &str,
    now: i64,
) -> Result<String, HostedError> {
    let token = Uuid::new_v4().simple().to_string();
    control.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO organizer_verification_links (token_hash, organizer_id, email, expires_at, created_at) VALUES (?, ?, ?, ?, ?)",
        [hash(&token).into(), organizer_id.into(), email.into(), (now + DAY).into(), now.into()]
    )).await?;
    Ok(token)
}

pub struct VerificationRecovery {
    pub organizer_id: String,
    pub email: String,
    pub token: String,
}

pub async fn recover_verification(
    control: &DatabaseConnection,
    subdomain: &str,
    email: &str,
    now: i64,
) -> Result<Option<VerificationRecovery>, HostedError> {
    let subdomain = subdomain.trim().to_ascii_lowercase();
    let email = email.trim().to_ascii_lowercase();
    if subdomain.is_empty()
        || subdomain.len() > 63
        || subdomain.starts_with('-')
        || subdomain.ends_with('-')
        || !subdomain
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || email.len() > 254
        || email.chars().any(char::is_whitespace)
        || email.chars().any(char::is_control)
        || !email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
        })
    {
        return Err(HostedError::InvalidRecovery);
    }
    let transaction = control.begin().await?;
    let email_hash = hash(&format!("signup-verification:{email}"));
    let reserved = transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO magic_link_rate_limits (organizer_id, email_hash, hour_bucket, count)
         SELECT o.id, ?, ?, 1 FROM organizers o
         WHERE o.subdomain = ? AND o.verified_at IS NULL AND o.suspension_reason IS NULL AND o.data_key IS NOT NULL
         AND ? = (SELECT email FROM organizer_verification_links WHERE organizer_id = o.id ORDER BY rowid LIMIT 1)
         AND EXISTS (SELECT 1 FROM memberships ms JOIN members m ON m.id = ms.member_id WHERE ms.organizer_id = o.id AND ms.role = 'Admin' AND m.email = ?)
         ON CONFLICT(organizer_id, email_hash, hour_bucket) DO UPDATE SET count = count + 1 WHERE count < 3",
        [email_hash.into(), (now / 3600).into(), subdomain.clone().into(), email.clone().into(), email.into()]
    )).await?;
    if reserved.rows_affected() == 0 {
        transaction.commit().await?;
        return Ok(None);
    }
    let row = transaction.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT links.organizer_id, links.email FROM organizer_verification_links links JOIN organizers o ON o.id = links.organizer_id WHERE o.subdomain = ? ORDER BY links.rowid LIMIT 1",
        [subdomain.into()]
    )).await?.ok_or(HostedError::InvalidVerification)?;
    let organizer_id: String = row.try_get("", "organizer_id")?;
    let email: String = row.try_get("", "email")?;
    let token = Uuid::new_v4().simple().to_string();
    transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO organizer_verification_links (token_hash, organizer_id, email, expires_at, created_at) VALUES (?, ?, ?, ?, ?)",
        [hash(&token).into(), organizer_id.clone().into(), email.clone().into(), (now + DAY).into(), now.into()]
    )).await?;
    transaction.commit().await?;
    Ok(Some(VerificationRecovery {
        organizer_id,
        email,
        token,
    }))
}

pub async fn verify_email(
    control: &DatabaseConnection,
    token: &str,
    now: i64,
) -> Result<String, HostedError> {
    let transaction = control.begin().await?;
    let updated = transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE organizer_verification_links SET used_at = ? WHERE token_hash = ? AND used_at IS NULL AND expires_at > ?
         AND EXISTS (SELECT 1 FROM organizers WHERE id = organizer_verification_links.organizer_id AND verified_at IS NULL AND suspension_reason IS NULL AND data_key IS NOT NULL)",
        [now.into(), hash(token).into(), now.into()]
    )).await?;
    if updated.rows_affected() != 1 {
        return Err(HostedError::InvalidVerification);
    }
    let row = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT organizer_id FROM organizer_verification_links WHERE token_hash = ?",
            [hash(token).into()],
        ))
        .await?
        .ok_or(HostedError::InvalidVerification)?;
    let id: String = row.try_get("", "organizer_id")?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE organizers SET verified_at = COALESCE(verified_at, ?) WHERE id = ?",
            [now.into(), id.clone().into()],
        ))
        .await?;
    transaction.commit().await?;
    Ok(id)
}

pub async fn pending_verification(
    control: &DatabaseConnection,
    token: &str,
    now: i64,
) -> Result<(String, String, String), HostedError> {
    let row = control
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT links.organizer_id, links.email, organizers.subdomain FROM organizer_verification_links links JOIN organizers ON organizers.id = links.organizer_id WHERE links.token_hash = ? AND links.used_at IS NULL AND links.expires_at > ? AND organizers.verified_at IS NULL AND organizers.suspension_reason IS NULL AND organizers.data_key IS NOT NULL",
            [hash(token).into(), now.into()],
        ))
        .await?
        .ok_or(HostedError::InvalidVerification)?;
    Ok((
        row.try_get("", "organizer_id")?,
        row.try_get("", "email")?,
        row.try_get("", "subdomain")?,
    ))
}

pub struct AccessState {
    pub verified: bool,
    pub subscription_status: String,
    pub grace_until: Option<i64>,
}

pub async fn access_state(
    control: &DatabaseConnection,
    organizer_id: &str,
) -> Result<AccessState, HostedError> {
    let row = control.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT o.verified_at, s.status, s.grace_until FROM organizers o LEFT JOIN subscriptions s ON s.organizer_id = o.id WHERE o.id = ?", [organizer_id.into()]
    )).await?.ok_or(HostedError::NotFound)?;
    Ok(AccessState {
        verified: row.try_get::<Option<i64>>("", "verified_at")?.is_some(),
        subscription_status: row
            .try_get::<Option<String>>("", "status")?
            .unwrap_or_else(|| "Pending".to_owned()),
        grace_until: row.try_get("", "grace_until")?,
    })
}

pub fn subscription_allows_sales(state: &AccessState, now: i64) -> bool {
    state.subscription_status == "Active"
        || (state.subscription_status == "Grace"
            && state.grace_until.is_some_and(|until| now < until))
}

pub async fn setup_status(
    db: &DatabaseConnection,
    organizer_id: &str,
) -> Result<Option<String>, DbErr> {
    db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT status FROM setup_requests WHERE organizer_id = ?",
        [organizer_id.into()],
    ))
    .await?
    .map(|row| row.try_get("", "status"))
    .transpose()
}

pub async fn request_setup(
    db: &DatabaseConnection,
    organizer_id: &str,
    now: i64,
) -> Result<bool, DbErr> {
    Ok(db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO setup_requests (organizer_id, status, requested_at, updated_at) VALUES (?, 'Requested', ?, ?)
         ON CONFLICT(organizer_id) DO UPDATE SET status = 'Requested', requested_at = excluded.requested_at, updated_at = excluded.updated_at
         WHERE setup_requests.status = 'Cancelled'",
        [organizer_id.into(), now.into(), now.into()]
    )).await?.rows_affected() > 0)
}

pub async fn update_setup_status(
    db: &DatabaseConnection,
    organizer_id: &str,
    status: &str,
    now: i64,
) -> Result<bool, DbErr> {
    if !matches!(status, "Contacted" | "Complete" | "Cancelled") {
        return Ok(false);
    }
    Ok(db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE setup_requests SET status = ?, updated_at = ? WHERE organizer_id = ? AND status IN ('Requested', 'Contacted')",
        [status.into(), now.into(), organizer_id.into()]
    )).await?.rows_affected() > 0)
}

pub struct BillingAttempt {
    pub key: String,
    pub expires_at: i64,
    pub checkout_url: Option<String>,
}

impl BillingAttempt {
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }

    pub fn can_request_checkout(&self, now: i64) -> bool {
        self.expires_at - now >= 30 * 60
    }
}

pub async fn billing_attempt(
    control: &DatabaseConnection,
    organizer_id: &str,
    now: i64,
) -> Result<Option<BillingAttempt>, HostedError> {
    let cutoff = now - 70 * 60;
    let fresh_key = Uuid::new_v4().to_string();
    let updated = control.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE subscriptions SET
            billing_attempt_key = CASE WHEN billing_attempt_started_at IS NULL OR billing_attempt_started_at <= ? THEN ? ELSE billing_attempt_key END,
            billing_checkout_url = CASE WHEN billing_attempt_started_at IS NULL OR billing_attempt_started_at <= ? THEN NULL ELSE billing_checkout_url END,
            stripe_checkout_session_id = CASE WHEN billing_attempt_started_at IS NULL OR billing_attempt_started_at <= ? THEN NULL ELSE stripe_checkout_session_id END,
            billing_attempt_started_at = CASE WHEN billing_attempt_started_at IS NULL OR billing_attempt_started_at <= ? THEN ? ELSE billing_attempt_started_at END
         WHERE organizer_id = ? AND status IN ('Pending', 'Lapsed')",
        [cutoff.into(), fresh_key.into(), cutoff.into(), cutoff.into(), cutoff.into(), now.into(), organizer_id.into()]
    )).await?;
    if updated.rows_affected() == 0 {
        return Ok(None);
    }
    let row = control.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT billing_attempt_key, billing_attempt_started_at, billing_checkout_url FROM subscriptions WHERE organizer_id = ?",
        [organizer_id.into()]
    )).await?.ok_or(HostedError::NotFound)?;
    Ok(Some(BillingAttempt {
        key: row.try_get("", "billing_attempt_key")?,
        expires_at: row.try_get::<i64>("", "billing_attempt_started_at")? + 66 * 60,
        checkout_url: row.try_get("", "billing_checkout_url")?,
    }))
}

pub async fn save_billing_checkout(
    control: &DatabaseConnection,
    organizer_id: &str,
    attempt_key: &str,
    session_id: &str,
    url: &str,
) -> Result<bool, HostedError> {
    let result = control
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE subscriptions SET stripe_checkout_session_id = ?, billing_checkout_url = ?
         WHERE organizer_id = ? AND billing_attempt_key = ? AND status IN ('Pending', 'Lapsed')",
            [
                session_id.into(),
                url.into(),
                organizer_id.into(),
                attempt_key.into(),
            ],
        ))
        .await?;
    Ok(result.rows_affected() == 1)
}

pub fn publish_allowed(
    state: &AccessState,
    stripe_tested: bool,
    webhook_tested: bool,
    now: i64,
) -> Result<(), HostedError> {
    if !state.verified {
        return Err(HostedError::VerificationRequired);
    }
    if !subscription_allows_sales(state, now) {
        return Err(HostedError::SubscriptionRequired);
    }
    if !stripe_tested || !webhook_tested {
        return Err(HostedError::SetupRequired);
    }
    Ok(())
}

pub async fn publish_check(
    control: &DatabaseConnection,
    registry: &OrganizerRegistry,
    organizer: &Organizer,
    db: &DatabaseConnection,
    now: i64,
) -> Result<(), HostedError> {
    if registry.edition() != Edition::Hosted {
        return Ok(());
    }
    let state = access_state(control, &organizer.id).await?;
    let cipher = registry.cipher(organizer)?;
    let stripe_tested = settings::get(db, &cipher, "stripe_key_tested_at")
        .await?
        .is_some_and(|value| !value.is_empty());
    let webhook_tested = settings::get(db, &cipher, "webhook_tested_at")
        .await?
        .is_some_and(|value| !value.is_empty());
    publish_allowed(&state, stripe_tested, webhook_tested, now)
}

pub async fn subscription_active(
    control: &DatabaseConnection,
    organizer_id: &str,
    customer_id: &str,
    subscription_id: &str,
    now: i64,
    event_created: i64,
) -> Result<(), HostedError> {
    control.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE subscriptions SET status = 'Active', grace_until = NULL, lapsed_at = NULL, stripe_customer_id = COALESCE(NULLIF(?, ''), stripe_customer_id), stripe_subscription_id = COALESCE(NULLIF(?, ''), stripe_subscription_id), updated_at = ?, last_billing_event_created = ? WHERE organizer_id = ? AND status != 'Deleted' AND last_billing_event_created < ?",
        [customer_id.into(), subscription_id.into(), now.into(), event_created.into(), organizer_id.into(), event_created.into()]
    )).await?;
    Ok(())
}

pub async fn subscription_checkout_complete(
    control: &DatabaseConnection,
    organizer_id: &str,
    checkout_session_id: &str,
    customer_id: &str,
    subscription_id: &str,
    now: i64,
    event_created: i64,
) -> Result<bool, HostedError> {
    let updated = control
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE subscriptions SET status = 'Active', grace_until = NULL, lapsed_at = NULL,
         stripe_customer_id = ?, stripe_subscription_id = ?, updated_at = ?, last_billing_event_created = ?,
         billing_attempt_key = NULL, billing_attempt_started_at = NULL, billing_checkout_url = NULL
         WHERE organizer_id = ? AND stripe_checkout_session_id = ? AND status IN ('Pending', 'Active', 'Lapsed')
         AND last_billing_event_created <= ?
         AND (status != 'Lapsed' OR last_billing_event_created < ?)",
            [
                customer_id.into(),
                subscription_id.into(),
                now.into(),
                event_created.into(),
                organizer_id.into(),
                checkout_session_id.into(),
                event_created.into(),
                event_created.into(),
            ],
        ))
        .await?;
    Ok(updated.rows_affected() == 1)
}

pub async fn subscription_failed(
    control: &DatabaseConnection,
    subscription_id: &str,
    now: i64,
    event_created: i64,
) -> Result<(), HostedError> {
    control.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE subscriptions SET status = 'Grace', grace_until = COALESCE(grace_until, ?), updated_at = ?, last_billing_event_created = ? WHERE stripe_subscription_id = ? AND status IN ('Active', 'Grace') AND last_billing_event_created <= ?",
        [(now + 14 * DAY).into(), now.into(), event_created.into(), subscription_id.into(), event_created.into()]
    )).await?;
    Ok(())
}

pub async fn subscription_lapsed(
    control: &DatabaseConnection,
    organizer_id: &str,
    now: i64,
) -> Result<(), HostedError> {
    control.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE subscriptions SET status = 'Lapsed', lapsed_at = COALESCE(lapsed_at, ?), grace_until = NULL, updated_at = ? WHERE organizer_id = ? AND status != 'Deleted'",
        [now.into(), now.into(), organizer_id.into()]
    )).await?;
    Ok(())
}

pub async fn subscription_cancelled(
    control: &DatabaseConnection,
    subscription_id: &str,
    now: i64,
    event_created: i64,
) -> Result<(), HostedError> {
    control.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE subscriptions SET status = 'Lapsed', lapsed_at = COALESCE(lapsed_at, ?), grace_until = NULL, stripe_subscription_id = NULL, updated_at = ?, last_billing_event_created = ? WHERE stripe_subscription_id = ? AND status != 'Deleted' AND last_billing_event_created <= ?",
        [now.into(), now.into(), event_created.into(), subscription_id.into(), event_created.into()]
    )).await?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum LifecycleAction {
    Warn {
        organizer_id: String,
        days_left: i64,
        lapse_generation: i64,
    },
    Delete {
        organizer_id: String,
    },
}

pub async fn due_lifecycle(
    control: &DatabaseConnection,
    now: i64,
) -> Result<Vec<LifecycleAction>, HostedError> {
    control.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE subscriptions SET status = 'Lapsed', lapsed_at = grace_until, grace_until = NULL, updated_at = ? WHERE status = 'Grace' AND grace_until <= ?",
        [now.into(), now.into()]
    )).await?;
    let rows = control.query_all_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT organizer_id, lapsed_at, lapse_generation FROM subscriptions WHERE status = 'Lapsed' AND lapsed_at IS NOT NULL".to_owned()
    )).await?;
    let mut actions = Vec::new();
    for row in rows {
        let organizer_id: String = row.try_get("", "organizer_id")?;
        let lapsed_at: i64 = row.try_get("", "lapsed_at")?;
        let lapse_generation: i64 = row.try_get("", "lapse_generation")?;
        let delete_at = lapsed_at + 90 * DAY;
        if now >= delete_at {
            actions.push(LifecycleAction::Delete { organizer_id });
            continue;
        }
        let days_left = if now >= delete_at - DAY {
            Some(1)
        } else if now >= delete_at - 7 * DAY {
            Some(7)
        } else if now >= delete_at - 30 * DAY {
            Some(30)
        } else {
            None
        };
        if let Some(days_left) = days_left {
            let missing = control.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT 1 FROM memberships ms JOIN members m ON m.id = ms.member_id
                 WHERE ms.organizer_id = ? AND ms.role = 'Admin' AND NOT EXISTS (
                     SELECT 1 FROM hosted_lifecycle_recipients r WHERE r.organizer_id = ms.organizer_id
                     AND r.lapse_generation = ? AND r.days_left = ? AND r.email = lower(trim(m.email))
                     AND r.sent_at IS NOT NULL)",
                [organizer_id.clone().into(), lapse_generation.into(), days_left.into()]
            )).await?.is_some();
            let admins = control
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT 1 FROM memberships WHERE organizer_id = ? AND role = 'Admin'",
                    [organizer_id.clone().into()],
                ))
                .await?
                .is_some();
            if missing || !admins {
                actions.push(LifecycleAction::Warn {
                    organizer_id: organizer_id.clone(),
                    days_left,
                    lapse_generation,
                });
            }
        }
    }
    let pending = control
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT organizer_id FROM hosted_cleanup WHERE completed_at IS NULL",
        ))
        .await?;
    for row in pending {
        actions.push(LifecycleAction::Delete {
            organizer_id: row.try_get("", "organizer_id")?,
        });
    }
    Ok(actions)
}

pub async fn mark_deleted(
    control: &DatabaseConnection,
    organizer_id: &str,
    now: i64,
) -> Result<bool, HostedError> {
    let transaction = control.begin().await?;
    let claimed = transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE subscriptions SET status = 'Deleted', updated_at = ? WHERE organizer_id = ? AND status = 'Lapsed' AND lapsed_at + ? <= ?", [now.into(), organizer_id.into(), (90 * DAY).into(), now.into()]
    )).await?;
    if claimed.rows_affected() != 1 {
        transaction.rollback().await?;
        return Ok(false);
    }
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE organizers SET data_key = NULL WHERE id = ?",
            [organizer_id.into()],
        ))
        .await?;
    transaction
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO hosted_cleanup (organizer_id, db_path, requested_at)
         SELECT id, db_path, ? FROM organizers WHERE id = ?",
            [now.into(), organizer_id.into()],
        ))
        .await?;
    transaction.commit().await?;
    Ok(true)
}

pub fn now() -> i64 {
    Utc::now().timestamp()
}

pub fn origin(subdomain: Option<&str>) -> String {
    let base = std::env::var("SHOP_BASE_DOMAIN").unwrap_or_else(|_| "crabgate.com".to_owned());
    let host = subdomain.map_or(base.clone(), |subdomain| format!("{subdomain}.{base}"));
    let scheme = std::env::var("SHOP_SCHEME").unwrap_or_else(|_| "https".to_owned());
    let port = std::env::var("SHOP_PORT")
        .ok()
        .map(|value| format!(":{value}"))
        .unwrap_or_default();
    format!("{scheme}://{host}{port}")
}

pub async fn redirect_www(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let base = std::env::var("SHOP_BASE_DOMAIN").unwrap_or_else(|_| "crabgate.com".to_owned());
    let www = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|host| host.split(':').next())
        .is_some_and(|host| host.eq_ignore_ascii_case(&format!("www.{base}")));
    if www && crate::organizers::Edition::from_environment() == crate::organizers::Edition::Hosted {
        let path = request
            .uri()
            .path_and_query()
            .map_or("/", |value| value.as_str());
        return axum::response::IntoResponse::into_response(axum::response::Redirect::permanent(
            &format!("{}{path}", origin(None)),
        ));
    }
    next.run(request).await
}
