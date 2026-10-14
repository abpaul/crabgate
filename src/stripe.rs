use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct StripeClient {
    client: reqwest::Client,
    base_url: String,
    secret_key: String,
}

// Requests name their version so a Stripe account's default cannot change their behaviour.
const API_VERSION: &str = "2026-08-26.dahlia";

// Three sequential requests of one refund fit inside its 40-second limit.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub const WEBHOOK_EVENTS: [&str; 8] = [
    "checkout.session.completed",
    "checkout.session.async_payment_succeeded",
    "checkout.session.async_payment_failed",
    "checkout.session.expired",
    "charge.refunded",
    "refund.created",
    "refund.updated",
    "refund.failed",
];

#[derive(Debug, thiserror::Error)]
pub enum StripeError {
    #[error("Stripe is not configured")]
    NotConfigured,
    #[error("Stripe rejected the request: {0}")]
    Rejected(String),
    #[error("Stripe does not allow this key to manage webhook endpoints: {0}")]
    PermissionDenied(String),
    #[error("Stripe returned an invalid response")]
    InvalidResponse,
    #[error("The checkout deadline is too close or outside Stripe's expiry window")]
    InvalidExpiry,
    #[error("Invalid Stripe webhook signature")]
    InvalidSignature,
    #[error(
        "Stripe did not answer within {} seconds; the request may have been accepted",
        REQUEST_TIMEOUT.as_secs()
    )]
    Timeout,
    #[error(transparent)]
    Http(reqwest::Error),
}

impl From<reqwest::Error> for StripeError {
    fn from(error: reqwest::Error) -> Self {
        // A connect timeout ends before the request is sent, so Stripe cannot have accepted it.
        if error.is_timeout() && !error.is_connect() {
            Self::Timeout
        } else {
            Self::Http(error)
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct WebhookEndpoint {
    pub id: String,
    pub secret: String,
}

#[derive(Debug, Deserialize)]
pub struct CheckoutSession {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub expires_at: Option<i64>,
}

pub struct SubscriptionCheckout<'a> {
    pub organizer_id: &'a str,
    pub attempt_key: &'a str,
    pub expires_at: i64,
    pub email: &'a str,
    pub price_id: &'a str,
    pub success_url: &'a str,
    pub cancel_url: &'a str,
}

pub struct StripeLineItem {
    pub name: String,
    pub description: String,
    pub price_cents: i64,
    pub quantity: i64,
}

#[derive(Debug, Deserialize)]
pub struct StripeRefund {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub amount: i64,
    #[serde(default)]
    pub payment_intent: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefundStatus {
    Succeeded,
    Pending,
}

impl StripeClient {
    pub fn from_environment() -> Result<Self, StripeError> {
        let secret_key =
            std::env::var("STRIPE_TEST_SECRET_KEY").map_err(|_| StripeError::NotConfigured)?;
        Self::from_secret(&secret_key)
    }

    pub fn from_secret(secret_key: &str) -> Result<Self, StripeError> {
        if !["sk_test_", "sk_live_", "rk_test_", "rk_live_"]
            .iter()
            .any(|prefix| secret_key.starts_with(prefix))
        {
            return Err(StripeError::NotConfigured);
        }
        let base_url = std::env::var("STRIPE_API_BASE")
            .unwrap_or_else(|_| "https://api.stripe.com".to_owned());
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(1))
                .timeout(REQUEST_TIMEOUT)
                .build()?,
            base_url,
            secret_key: secret_key.to_owned(),
        })
    }

    pub async fn test_key(&self) -> Result<(), StripeError> {
        let response = self
            .client
            .get(format!("{}/v1/account", self.base_url))
            .basic_auth(&self.secret_key, Some(""))
            .header("Stripe-Version", API_VERSION)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(StripeError::Rejected(response.text().await?));
        }
        let body: serde_json::Value = response.json().await?;
        if body.get("object").and_then(|value| value.as_str()) != Some("account") {
            return Err(StripeError::InvalidResponse);
        }
        Ok(())
    }

    pub async fn create_webhook_endpoint(&self, url: &str) -> Result<WebhookEndpoint, StripeError> {
        let mut form = vec![("url", url)];
        form.extend(
            WEBHOOK_EVENTS
                .iter()
                .map(|event| ("enabled_events[]", *event)),
        );
        let response = self
            .client
            .post(format!("{}/v1/webhook_endpoints", self.base_url))
            .basic_auth(&self.secret_key, Some(""))
            .header("Stripe-Version", API_VERSION)
            .form(&form)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Self::webhook_refusal(response).await);
        }
        let endpoint: WebhookEndpoint = response.json().await?;
        if endpoint.id.is_empty() || !endpoint.secret.starts_with("whsec_") {
            return Err(StripeError::InvalidResponse);
        }
        Ok(endpoint)
    }

    pub async fn other_webhook_endpoint_ids(
        &self,
        url: &str,
        keep_id: &str,
    ) -> Result<Vec<String>, StripeError> {
        let response = self
            .client
            .get(format!("{}/v1/webhook_endpoints", self.base_url))
            .query(&[("limit", "100")])
            .basic_auth(&self.secret_key, Some(""))
            .header("Stripe-Version", API_VERSION)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Self::webhook_refusal(response).await);
        }
        let body: serde_json::Value = response.json().await?;
        let stale: Vec<String> = body["data"]
            .as_array()
            .ok_or(StripeError::InvalidResponse)?
            .iter()
            .filter(|endpoint| endpoint["url"].as_str() == Some(url))
            .filter_map(|endpoint| endpoint["id"].as_str())
            .filter(|id| *id != keep_id)
            .map(str::to_owned)
            .collect();
        if body["has_more"].as_bool() != Some(false) {
            return Err(StripeError::InvalidResponse);
        }
        Ok(stale)
    }

    pub async fn delete_webhook_endpoints(&self, ids: &[String]) -> Result<usize, StripeError> {
        for id in ids {
            let response = self
                .client
                .delete(format!("{}/v1/webhook_endpoints/{id}", self.base_url))
                .basic_auth(&self.secret_key, Some(""))
                .header("Stripe-Version", API_VERSION)
                .send()
                .await?;
            if !response.status().is_success() {
                return Err(Self::webhook_refusal(response).await);
            }
        }
        Ok(ids.len())
    }

    async fn webhook_refusal(response: reqwest::Response) -> StripeError {
        let status = response.status();
        match response.text().await {
            Ok(text) if matches!(status.as_u16(), 401 | 403) => StripeError::PermissionDenied(text),
            Ok(text) => StripeError::Rejected(text),
            Err(error) => error.into(),
        }
    }

    pub async fn create_session(
        &self,
        hold_id: &str,
        currency: &str,
        expires_at: i64,
        success_url: &str,
        cancel_url: &str,
        items: &[StripeLineItem],
    ) -> Result<CheckoutSession, StripeError> {
        let now = chrono::Utc::now().timestamp();
        // Stripe measures its 30-minute minimum at creation; allow the bounded request time.
        let earliest = now + 30 * 60 + REQUEST_TIMEOUT.as_secs() as i64;
        if !(earliest..=now + 24 * 60 * 60).contains(&expires_at) {
            return Err(StripeError::InvalidExpiry);
        }
        let mut form = vec![
            ("mode".to_owned(), "payment".to_owned()),
            ("managed_payments[enabled]".to_owned(), "false".to_owned()),
            ("success_url".to_owned(), success_url.to_owned()),
            ("cancel_url".to_owned(), cancel_url.to_owned()),
            ("expires_at".to_owned(), expires_at.to_string()),
            ("metadata[hold_id]".to_owned(), hold_id.to_owned()),
        ];
        for (index, item) in items.iter().enumerate() {
            form.push((
                format!("line_items[{index}][price_data][currency]"),
                currency.to_ascii_lowercase(),
            ));
            form.push((
                format!("line_items[{index}][price_data][unit_amount]"),
                item.price_cents.to_string(),
            ));
            form.push((
                format!("line_items[{index}][price_data][product_data][name]"),
                item.name.clone(),
            ));
            form.push((
                format!("line_items[{index}][price_data][product_data][description]"),
                item.description.clone(),
            ));
            form.push((
                format!("line_items[{index}][quantity]"),
                item.quantity.to_string(),
            ));
        }
        let response = self
            .client
            .post(format!("{}/v1/checkout/sessions", self.base_url))
            .basic_auth(&self.secret_key, Some(""))
            .header("Stripe-Version", API_VERSION)
            .header("Idempotency-Key", hold_id)
            .form(&form)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(StripeError::Rejected(response.text().await?));
        }
        response.json().await.map_err(StripeError::from)
    }

    pub async fn create_portal_session(
        &self,
        customer_id: &str,
        return_url: &str,
    ) -> Result<String, StripeError> {
        let response = self
            .client
            .post(format!("{}/v1/billing_portal/sessions", self.base_url))
            .basic_auth(&self.secret_key, Some(""))
            .header("Stripe-Version", API_VERSION)
            .form(&[("customer", customer_id), ("return_url", return_url)])
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(StripeError::Rejected(response.text().await?));
        }
        let body: serde_json::Value = response.json().await?;
        body["url"]
            .as_str()
            .map(str::to_owned)
            .ok_or(StripeError::InvalidResponse)
    }

    pub async fn create_subscription_session(
        &self,
        checkout: SubscriptionCheckout<'_>,
    ) -> Result<CheckoutSession, StripeError> {
        let SubscriptionCheckout {
            organizer_id,
            attempt_key,
            expires_at,
            email,
            price_id,
            success_url,
            cancel_url,
        } = checkout;
        let expires_at = expires_at.to_string();
        let response = self
            .client
            .post(format!("{}/v1/checkout/sessions", self.base_url))
            .basic_auth(&self.secret_key, Some(""))
            .header("Stripe-Version", API_VERSION)
            .header("Idempotency-Key", attempt_key)
            .form(&[
                ("mode", "subscription"),
                ("expires_at", expires_at.as_str()),
                ("line_items[0][price]", price_id),
                ("line_items[0][quantity]", "1"),
                ("client_reference_id", organizer_id),
                ("customer_email", email),
                ("success_url", success_url),
                ("cancel_url", cancel_url),
            ])
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(StripeError::Rejected(response.text().await?));
        }
        response.json().await.map_err(StripeError::from)
    }

    pub async fn expire_session(&self, session_id: &str) -> Result<(), StripeError> {
        let response = self
            .client
            .post(format!(
                "{}/v1/checkout/sessions/{session_id}/expire",
                self.base_url
            ))
            .basic_auth(&self.secret_key, Some(""))
            .header("Stripe-Version", API_VERSION)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(StripeError::Rejected(response.text().await?));
        }
        let body: serde_json::Value = response.json().await?;
        if body["status"].as_str() != Some("expired") {
            return Err(StripeError::InvalidResponse);
        }
        Ok(())
    }

    pub async fn refund(
        &self,
        payment_intent: &str,
        idempotency_key: &str,
    ) -> Result<RefundStatus, StripeError> {
        let refund = self.create_refund(payment_intent, idempotency_key).await?;
        match refund.status.as_str() {
            "succeeded" => Ok(RefundStatus::Succeeded),
            "pending" | "requires_action" => Ok(RefundStatus::Pending),
            _ => Err(StripeError::InvalidResponse),
        }
    }

    pub async fn retrieve_refund(&self, id: &str) -> Result<StripeRefund, StripeError> {
        let response = self
            .client
            .get(format!("{}/v1/refunds/{id}", self.base_url))
            .basic_auth(&self.secret_key, Some(""))
            .header("Stripe-Version", API_VERSION)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(StripeError::Rejected(response.text().await?));
        }
        Ok(response.json().await?)
    }

    pub async fn refund_history(&self, intent: &str) -> Result<Vec<StripeRefund>, StripeError> {
        #[derive(Deserialize)]
        struct Page {
            data: Vec<StripeRefund>,
            has_more: bool,
        }
        let mut refunds = Vec::new();
        let mut cursor = String::new();
        for _ in 0..5 {
            let mut query = vec![("payment_intent", intent), ("limit", "100")];
            if !cursor.is_empty() {
                query.push(("starting_after", &cursor));
            }
            let response = self
                .client
                .get(format!("{}/v1/refunds", self.base_url))
                .basic_auth(&self.secret_key, Some(""))
                .header("Stripe-Version", API_VERSION)
                .query(&query)
                .send()
                .await?;
            if !response.status().is_success() {
                return Err(StripeError::Rejected(response.text().await?));
            }
            let page: Page = response.json().await?;
            if page.data.iter().any(|r| {
                r.id.is_empty()
                    || r.amount <= 0
                    || r.payment_intent != intent
                    || !matches!(
                        r.status.as_str(),
                        "succeeded" | "pending" | "requires_action" | "failed" | "canceled"
                    )
            }) {
                return Err(StripeError::InvalidResponse);
            }
            let next = page.data.last().map(|r| r.id.clone());
            refunds.extend(page.data);
            if !page.has_more {
                return Ok(refunds);
            }
            let next = next.ok_or(StripeError::InvalidResponse)?;
            if next == cursor {
                return Err(StripeError::InvalidResponse);
            }
            cursor = next;
        }
        Err(StripeError::InvalidResponse)
    }

    pub async fn create_refund(
        &self,
        payment_intent: &str,
        idempotency_key: &str,
    ) -> Result<StripeRefund, StripeError> {
        let response = self
            .client
            .post(format!("{}/v1/refunds", self.base_url))
            .basic_auth(&self.secret_key, Some(""))
            .header("Stripe-Version", API_VERSION)
            .header("Idempotency-Key", idempotency_key)
            .form(&[("payment_intent", payment_intent)])
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(StripeError::Rejected(response.text().await?));
        }
        let refund: StripeRefund = response.json().await?;
        if refund.id.is_empty()
            || !matches!(
                refund.status.as_str(),
                "succeeded" | "pending" | "requires_action" | "failed" | "canceled"
            )
        {
            return Err(StripeError::InvalidResponse);
        }
        Ok(refund)
    }
}

pub fn verify_webhook(
    body: &[u8],
    header: &str,
    secret: &str,
    now: i64,
) -> Result<(), StripeError> {
    let timestamp = header
        .split(',')
        .find_map(|item| item.strip_prefix("t="))
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or(StripeError::InvalidSignature)?;
    if timestamp.abs_diff(now) > 300 {
        return Err(StripeError::InvalidSignature);
    }
    let signatures = header
        .split(',')
        .filter_map(|item| item.strip_prefix("v1="));
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| StripeError::InvalidSignature)?;
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    if signatures.into_iter().any(|signature| {
        hex::decode(signature).is_ok_and(|bytes| mac.clone().verify_slice(&bytes).is_ok())
    }) {
        Ok(())
    } else {
        Err(StripeError::InvalidSignature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_verification_uses_raw_body_and_checks_timestamp() {
        let body = br#"{"id":"evt_test_1","type":"checkout.session.completed"}"#;
        let mut mac = HmacSha256::new_from_slice(b"whsec_test").unwrap();
        mac.update(b"1000.");
        mac.update(body);
        let header = format!("t=1000,v1={}", hex::encode(mac.finalize().into_bytes()));
        assert!(verify_webhook(body, &header, "whsec_test", 1001).is_ok());
        assert!(verify_webhook(b"{}", &header, "whsec_test", 1001).is_err());
        assert!(verify_webhook(body, &header, "whsec_other", 1001).is_err());
        assert!(verify_webhook(body, &header, "whsec_test", 1301).is_err());
        assert!(verify_webhook(body, "t=-9223372036854775808,v1=00", "whsec_test", 1000).is_err());
    }
}
