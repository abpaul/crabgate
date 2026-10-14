use crate::{
    crypto::{CryptoError, OrganizerCipher},
    organizers::Edition,
    stripe::{StripeClient, StripeError},
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, Statement, TransactionTrait,
};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("Enter a valid HTTPS Imprint or Privacy URL")]
    InvalidLegalUrl,
    #[error(
        "Enter a valid HTTPS booking-terms URL; a link is needed when acknowledgement is required"
    )]
    InvalidBookingTerms,
    #[error("Enter up to 16 exact HTTPS website origins without paths or credentials")]
    InvalidEmbedOrigins,
    #[error("Enter one valid reply-to email address")]
    InvalidReplyTo,
    #[error("Choose EUR, GBP, or USD")]
    InvalidCurrency,
    #[error("The currency can't change after your first checkout.")]
    CurrencyLocked,
    #[error(transparent)]
    Database(#[from] DbErr),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Stripe(#[from] StripeError),
}

#[derive(Clone, Serialize)]
pub struct Contact {
    pub imprint_url: String,
    pub privacy_url: String,
    pub reply_to: String,
}

pub async fn contact(db: &DatabaseConnection) -> Result<Contact, SettingsError> {
    let mut contact = Contact {
        imprint_url: String::new(),
        privacy_url: String::new(),
        reply_to: String::new(),
    };
    let rows = db.query_all_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT key, value FROM organizer_settings WHERE key IN ('imprint_url', 'privacy_url', 'reply_to')".to_owned()
    )).await?;
    for row in rows {
        let key: String = row.try_get("", "key")?;
        let value: String = row.try_get("", "value")?;
        match key.as_str() {
            "imprint_url" => contact.imprint_url = value,
            "privacy_url" => contact.privacy_url = value,
            "reply_to" => contact.reply_to = value,
            _ => {}
        }
    }
    Ok(contact)
}

pub async fn set_contact(
    db: &DatabaseConnection,
    imprint_url: &str,
    privacy_url: &str,
    reply_to: &str,
) -> Result<(), SettingsError> {
    let imprint_url = imprint_url.trim();
    let privacy_url = privacy_url.trim();
    let reply_to = reply_to.trim();
    for url in [imprint_url, privacy_url] {
        if !url.is_empty() {
            let parsed = reqwest::Url::parse(url).map_err(|_| SettingsError::InvalidLegalUrl)?;
            if url.len() > 2048
                || parsed.scheme() != "https"
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
            {
                return Err(SettingsError::InvalidLegalUrl);
            }
        }
    }
    if !reply_to.is_empty()
        && (reply_to.len() > 254
            || reply_to.chars().any(|ch| {
                ch.is_whitespace() || ch.is_control() || matches!(ch, '<' | '>' | ',' | ';')
            })
            || reply_to.split('@').count() != 2
            || !reply_to.split('@').nth(1).is_some_and(|domain| {
                domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
            }))
    {
        return Err(SettingsError::InvalidReplyTo);
    }
    let transaction = db.begin().await?;
    for (key, value) in [
        ("imprint_url", imprint_url),
        ("privacy_url", privacy_url),
        ("reply_to", reply_to),
    ] {
        transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO organizer_settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key.into(), value.into()]
        )).await?;
    }
    transaction.commit().await?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, Serialize)]
pub struct BookingTerms {
    pub url: String,
    pub required: bool,
    pub wording: String,
}

pub async fn booking_terms<C: ConnectionTrait>(db: &C) -> Result<BookingTerms, DbErr> {
    let rows = db.query_all_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT key, value FROM organizer_settings WHERE key IN ('booking_terms_url', 'booking_terms_required')"
    )).await?;
    let mut terms = BookingTerms {
        url: String::new(),
        required: false,
        wording: "I acknowledge the organizer's booking terms.".to_owned(),
    };
    for row in rows {
        match row.try_get::<String>("", "key")?.as_str() {
            "booking_terms_url" => terms.url = row.try_get("", "value")?,
            "booking_terms_required" => {
                terms.required = row.try_get::<String>("", "value")? == "yes"
            }
            _ => {}
        }
    }
    Ok(terms)
}

pub async fn set_booking_terms(
    db: &DatabaseConnection,
    url: &str,
    required: bool,
) -> Result<(), SettingsError> {
    let url = url.trim();
    if required && url.is_empty() {
        return Err(SettingsError::InvalidBookingTerms);
    }
    if !url.is_empty() {
        let parsed = reqwest::Url::parse(url).map_err(|_| SettingsError::InvalidBookingTerms)?;
        if url.len() > 2048
            || url.chars().any(|ch| ch.is_control())
            || parsed.scheme() != "https"
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(SettingsError::InvalidBookingTerms);
        }
    }
    let transaction = db.begin().await?;
    for (key, value) in [
        ("booking_terms_url", url),
        (
            "booking_terms_required",
            if required { "yes" } else { "no" },
        ),
    ] {
        transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO organizer_settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key.into(), value.into()]
        )).await?;
    }
    transaction.commit().await?;
    Ok(())
}

pub async fn currency(db: &DatabaseConnection) -> Result<String, SettingsError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT value FROM organizer_settings WHERE key = 'currency'",
            [],
        ))
        .await?;
    Ok(row
        .map(|row| row.try_get("", "value"))
        .transpose()?
        .unwrap_or_else(|| "EUR".to_owned()))
}

pub async fn set_currency(db: &DatabaseConnection, code: &str) -> Result<(), SettingsError> {
    if !matches!(code, "EUR" | "GBP" | "USD") {
        return Err(SettingsError::InvalidCurrency);
    }
    let transaction = db.begin().await?;
    let current = transaction
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT value FROM organizer_settings WHERE key = 'currency'",
            [],
        ))
        .await?
        .map(|row| row.try_get::<String>("", "value"))
        .transpose()?
        .unwrap_or_else(|| "EUR".to_owned());
    if code != current
        && transaction
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT 1 FROM holds LIMIT 1",
                [],
            ))
            .await?
            .is_some()
    {
        return Err(SettingsError::CurrencyLocked);
    }
    transaction.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO organizer_settings (key, value) VALUES ('currency', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [code.into()]
    )).await?;
    transaction.commit().await?;
    Ok(())
}

pub async fn stripe_client(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
) -> Result<StripeClient, SettingsError> {
    if let Some(key) = get(db, cipher, "stripe_secret_key").await? {
        return Ok(StripeClient::from_secret(&key)?);
    }
    if Edition::from_environment() == Edition::SelfHosted {
        return Ok(StripeClient::from_environment()?);
    }
    Err(StripeError::NotConfigured.into())
}

pub struct WebhookConfiguration {
    secret: String,
    stored_secret: Option<String>,
    stored_key: Option<String>,
    attempt: Option<String>,
}

impl WebhookConfiguration {
    pub fn secret(&self) -> &str {
        &self.secret
    }
}

pub async fn webhook_configuration(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
) -> Result<Option<WebhookConfiguration>, SettingsError> {
    let rows = db.query_all_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT key, value FROM organizer_settings WHERE key IN ('webhook_secret', 'stripe_secret_key', 'stripe_setup_attempt')"
    )).await?;
    let mut stored_secret = None;
    let mut stored_key = None;
    let mut attempt = None;
    for row in rows {
        let key: String = row.try_get("", "key")?;
        let value: String = row.try_get("", "value")?;
        match key.as_str() {
            "webhook_secret" => stored_secret = Some(value),
            "stripe_secret_key" => stored_key = Some(value),
            "stripe_setup_attempt" => attempt = Some(value),
            _ => {}
        }
    }
    let secret = match &stored_secret {
        Some(value) => Some(cipher.decrypt("webhook_secret", value)?),
        None if Edition::from_environment() == Edition::SelfHosted => {
            std::env::var("STRIPE_TEST_WEBHOOK_SECRET").ok()
        }
        None => None,
    };
    Ok(secret
        .filter(|secret| !secret.is_empty())
        .map(|secret| WebhookConfiguration {
            secret,
            stored_secret,
            stored_key,
            attempt,
        }))
}

pub async fn webhook_secret(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
) -> Result<Option<String>, SettingsError> {
    Ok(webhook_configuration(db, cipher)
        .await?
        .map(|configuration| configuration.secret))
}

pub async fn mark_webhook_received(
    db: &DatabaseConnection,
    configuration: &WebhookConfiguration,
    tested_at: &str,
) -> Result<bool, SettingsError> {
    let Some(stored_secret) = &configuration.stored_secret else {
        return Ok(false);
    };
    // The attempt distinguishes setup changes even when the signing secret is reused.
    let result = db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO organizer_settings (key, value)
         SELECT 'webhook_tested_at', ?
         WHERE (SELECT value FROM organizer_settings WHERE key = 'webhook_secret') IS ?
           AND (SELECT value FROM organizer_settings WHERE key = 'stripe_secret_key') IS ?
           AND (SELECT value FROM organizer_settings WHERE key = 'stripe_setup_attempt') IS ?
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [
                tested_at.into(),
                stored_secret.clone().into(),
                configuration.stored_key.clone().into(),
                configuration.attempt.clone().into(),
            ],
        ))
        .await?;
    Ok(result.rows_affected() == 1)
}

fn secret(key: &str) -> bool {
    matches!(
        key,
        "stripe_secret_key" | "webhook_secret" | "smtp_password"
    )
}

pub async fn get(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    key: &str,
) -> Result<Option<String>, SettingsError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT value FROM organizer_settings WHERE key = ?",
            [key.into()],
        ))
        .await?;
    row.map(|row| -> Result<String, SettingsError> {
        let value: String = row.try_get("", "value")?;
        Ok(if secret(key) {
            cipher.decrypt(key, &value)?
        } else {
            value
        })
    })
    .transpose()
}

pub async fn set(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    key: &str,
    value: &str,
) -> Result<(), SettingsError> {
    set_on(db, cipher, key, value).await
}

async fn set_on(
    db: &impl ConnectionTrait,
    cipher: &OrganizerCipher,
    key: &str,
    value: &str,
) -> Result<(), SettingsError> {
    let stored = if secret(key) {
        cipher.encrypt(key, value)?
    } else {
        value.to_owned()
    };
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO organizer_settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value", [key.into(), stored.into()]
    )).await?;
    Ok(())
}

pub async fn begin_stripe_setup(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    key: &str,
    tested_at: &str,
    expected_attempt: &str,
) -> Result<Option<String>, SettingsError> {
    let attempt = uuid::Uuid::new_v4().to_string();
    let transaction = db.begin().await?;
    if !replace_setup_attempt(&transaction, expected_attempt, &attempt).await? {
        transaction.rollback().await?;
        return Ok(None);
    }
    let previous = transaction
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT value FROM organizer_settings WHERE key = 'stripe_secret_key'",
        ))
        .await?
        .map(|row| row.try_get::<String>("", "value"))
        .transpose()?;
    let previous = previous
        .map(|value| cipher.decrypt("stripe_secret_key", &value))
        .transpose()?;
    if previous.as_deref() != Some(key) {
        for name in ["webhook_secret", "webhook_endpoint_id", "webhook_tested_at"] {
            set_on(&transaction, cipher, name, "").await?;
        }
    }
    set_on(&transaction, cipher, "stripe_secret_key", key).await?;
    set_on(&transaction, cipher, "stripe_key_tested_at", tested_at).await?;
    transaction.commit().await?;
    Ok(Some(attempt))
}

pub async fn finish_stripe_setup(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    attempt: &str,
    endpoint: &crate::stripe::WebhookEndpoint,
    tested_at: &str,
) -> Result<bool, SettingsError> {
    let transaction = db.begin().await?;
    let current = transaction.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE organizer_settings SET value = value WHERE key = 'stripe_setup_attempt' AND value = ?",
        [attempt.into()],
    )).await?;
    if current.rows_affected() == 0 {
        transaction.rollback().await?;
        return Ok(false);
    }
    for (key, value) in [
        ("webhook_secret", endpoint.secret.as_str()),
        ("webhook_endpoint_id", endpoint.id.as_str()),
        ("webhook_tested_at", tested_at),
    ] {
        set_on(&transaction, cipher, key, value).await?;
    }
    transaction.commit().await?;
    Ok(true)
}

pub async fn set_manual_webhook(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    secret: &str,
    expected_attempt: &str,
) -> Result<bool, SettingsError> {
    let transaction = db.begin().await?;
    if !replace_setup_attempt(
        &transaction,
        expected_attempt,
        &uuid::Uuid::new_v4().to_string(),
    )
    .await?
    {
        transaction.rollback().await?;
        return Ok(false);
    }
    for (key, value) in [
        ("webhook_secret", secret),
        ("webhook_endpoint_id", ""),
        ("webhook_tested_at", ""),
    ] {
        set_on(&transaction, cipher, key, value).await?;
    }
    transaction.commit().await?;
    Ok(true)
}

async fn replace_setup_attempt(
    db: &impl ConnectionTrait,
    expected: &str,
    replacement: &str,
) -> Result<bool, SettingsError> {
    let result = db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "INSERT INTO organizer_settings (key, value)
         SELECT 'stripe_setup_attempt', ?
         WHERE COALESCE((SELECT value FROM organizer_settings WHERE key = 'stripe_setup_attempt'), '') = ?
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [replacement.into(), expected.into()],
    )).await?;
    Ok(result.rows_affected() == 1)
}

#[derive(Clone, Debug, Default)]
pub struct EmbedOrigins(Vec<String>);

impl EmbedOrigins {
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    pub fn parse(origins: &[String]) -> Result<Self, SettingsError> {
        if origins.len() > 16 {
            return Err(SettingsError::InvalidEmbedOrigins);
        }
        let mut normalized = Vec::new();
        for origin in origins {
            if origin.len() > 256
                || !origin.is_ascii()
                || origin
                    .chars()
                    .any(|ch| ch.is_whitespace() || ch.is_control())
                || origin.contains(['*', '\\', '%', '@'])
            {
                return Err(SettingsError::InvalidEmbedOrigins);
            }
            let url =
                reqwest::Url::parse(origin).map_err(|_| SettingsError::InvalidEmbedOrigins)?;
            if url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.path() != "/"
                || url.query().is_some()
                || url.fragment().is_some()
                || !origin
                    .strip_prefix("https://")
                    .is_some_and(|authority| !authority.contains('/'))
            {
                return Err(SettingsError::InvalidEmbedOrigins);
            }
            let origin = url.origin().ascii_serialization();
            if !normalized.contains(&origin) {
                normalized.push(origin);
            }
        }
        Ok(Self(normalized))
    }

    pub(crate) fn ancestors(&self) -> Option<String> {
        (!self.0.is_empty()).then(|| self.0.join(" "))
    }
}

pub async fn embed_origins(db: &DatabaseConnection) -> Result<EmbedOrigins, SettingsError> {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT value FROM organizer_settings WHERE key = 'embed_origins'",
        ))
        .await?;
    let Some(row) = row else {
        return Ok(EmbedOrigins::default());
    };
    let value: String = row.try_get("", "value")?;
    if value.len() > 8192 {
        return Ok(EmbedOrigins::default());
    }
    let origins: Vec<String> = match serde_json::from_str(&value) {
        Ok(origins) => origins,
        Err(_) => return Ok(EmbedOrigins::default()),
    };
    Ok(EmbedOrigins::parse(&origins).unwrap_or_default())
}

pub async fn set_embed_origins(
    db: &DatabaseConnection,
    origins: &[String],
) -> Result<(), SettingsError> {
    let origins = EmbedOrigins::parse(origins)?;
    let value =
        serde_json::to_string(&origins.0).map_err(|_| SettingsError::InvalidEmbedOrigins)?;
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO organizer_settings (key, value) VALUES ('embed_origins', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [value.into()]
    )).await?;
    Ok(())
}
