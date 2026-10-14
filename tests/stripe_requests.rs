use std::sync::{Arc, Mutex};

use axum::{body::Bytes, extract::State, http::HeaderMap, routing::post, Json, Router};
use crabgate::stripe::{StripeClient, StripeError, StripeLineItem, SubscriptionCheckout};
use serial_test::serial;

type Seen = Arc<Mutex<Option<(HeaderMap, String)>>>;

async fn checkout(
    State(seen): State<Seen>,
    headers: HeaderMap,
    body: Bytes,
) -> Json<serde_json::Value> {
    let body = String::from_utf8(body.to_vec()).unwrap();
    let expires_at: i64 = body
        .split('&')
        .find_map(|pair| pair.strip_prefix("expires_at="))
        .unwrap()
        .parse()
        .unwrap();
    *seen.lock().unwrap() = Some((headers, body));
    Json(
        serde_json::json!({"id": "cs_test_1", "url": "https://checkout.stripe.com/c/pay/cs_test_1", "expires_at":expires_at}),
    )
}

#[tokio::test]
#[serial]
async fn ticket_checkout_sends_what_stripe_accepts() {
    let seen: Seen = Arc::default();
    let app = Router::new()
        .route("/v1/checkout/sessions", post(checkout))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var("STRIPE_API_BASE", &base);
    let client = StripeClient::from_secret("sk_test_example").unwrap();
    if let Some(value) = prior {
        std::env::set_var("STRIPE_API_BASE", value);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }

    let expires_at = chrono::Utc::now().timestamp() + 1860;
    let session = client
        .create_session(
            "hold-1",
            "EUR",
            expires_at,
            "https://shop.example/ok",
            "https://shop.example/back",
            &[StripeLineItem {
                name: "Songs in the Round, Standard".to_owned(),
                description: "Sat 17 October 2026, 19:30, The Lantern Room".to_owned(),
                price_cents: 1600,
                quantity: 2,
            }],
        )
        .await
        .unwrap();
    assert_eq!(session.id, "cs_test_1");
    assert_eq!(session.expires_at, Some(expires_at));

    let (headers, body) = seen.lock().unwrap().take().unwrap();
    let form: Vec<(String, String)> = body
        .split('&')
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap();
            let decode = |text: &str| {
                percent_encoding::percent_decode_str(&text.replace('+', " "))
                    .decode_utf8()
                    .unwrap()
                    .into_owned()
            };
            (decode(key), decode(value))
        })
        .collect();
    let value = |key: &str| {
        form.iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(headers["stripe-version"], "2026-08-26.dahlia");
    assert_eq!(headers["idempotency-key"], "hold-1");
    assert_eq!(value("mode"), Some("payment"));
    assert_eq!(value("managed_payments[enabled]"), Some("false"));
    assert!(form
        .iter()
        .all(|(name, _)| !name.starts_with("payment_method_types")));
    assert_eq!(value("line_items[0][price_data][currency]"), Some("eur"));
    assert_eq!(
        value("line_items[0][price_data][unit_amount]"),
        Some("1600")
    );
    assert_eq!(value("line_items[0][quantity]"), Some("2"));
    assert_eq!(
        value("line_items[0][price_data][product_data][name]"),
        Some("Songs in the Round, Standard")
    );
    assert_eq!(
        value("line_items[0][price_data][product_data][description]"),
        Some("Sat 17 October 2026, 19:30, The Lantern Room")
    );
    assert_eq!(value("metadata[hold_id]"), Some("hold-1"));
    assert_eq!(value("expires_at"), Some(expires_at.to_string().as_str()));
}

#[tokio::test]
#[serial]
async fn checkout_rejects_old_and_near_expiry_holds_without_provider_requests() {
    let seen: Seen = Arc::default();
    let app = Router::new()
        .route("/v1/checkout/sessions", post(checkout))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var("STRIPE_API_BASE", &base);
    let client = StripeClient::from_secret("sk_test_example").unwrap();
    if let Some(value) = prior {
        std::env::set_var("STRIPE_API_BASE", value);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }
    let now = chrono::Utc::now().timestamp();
    for deadline in [now - 1, now + 1799, now + 1800, now + 1809, now + 86401] {
        assert!(matches!(
            client
                .create_session(
                    "old-hold",
                    "EUR",
                    deadline,
                    "https://shop.example/ok",
                    "https://shop.example/back",
                    &[]
                )
                .await,
            Err(StripeError::InvalidExpiry)
        ));
        assert!(seen.lock().unwrap().is_none());
    }
    let session = client
        .create_session(
            "boundary-hold",
            "EUR",
            now + 1811,
            "https://shop.example/ok",
            "https://shop.example/back",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(session.expires_at, Some(now + 1811));
    assert!(seen.lock().unwrap().is_some());
    server.abort();
}

async fn portal(
    State(seen): State<Seen>,
    headers: HeaderMap,
    body: Bytes,
) -> Json<serde_json::Value> {
    *seen.lock().unwrap() = Some((headers, String::from_utf8(body.to_vec()).unwrap()));
    Json(serde_json::json!({"id": "bps_1", "url": "https://billing.stripe.com/p/session/test_1"}))
}

#[tokio::test]
#[serial]
async fn subscription_portal_opens_for_the_organizers_customer() {
    let seen: Seen = Arc::default();
    let app = Router::new()
        .route("/v1/billing_portal/sessions", post(portal))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var("STRIPE_API_BASE", &base);
    let client = StripeClient::from_secret("sk_test_example").unwrap();
    if let Some(value) = prior {
        std::env::set_var("STRIPE_API_BASE", value);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }

    let url = client
        .create_portal_session(
            "cus_123",
            "https://lantern.crabgate.com/office/setup?step=subscription",
        )
        .await
        .unwrap();
    assert_eq!(url, "https://billing.stripe.com/p/session/test_1");
    let (headers, body) = seen.lock().unwrap().take().unwrap();
    assert_eq!(headers["stripe-version"], "2026-08-26.dahlia");
    assert_eq!(
        body,
        "customer=cus_123&return_url=https%3A%2F%2Flantern.crabgate.com%2Foffice%2Fsetup%3Fstep%3Dsubscription"
    );
}

#[tokio::test]
#[serial]
async fn subscription_checkout_uses_the_configured_stripe_price_without_an_amount_override() {
    let seen: Seen = Arc::default();
    let app = Router::new()
        .route("/v1/checkout/sessions", post(checkout))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var("STRIPE_API_BASE", &base);
    let client = StripeClient::from_secret("sk_test_example").unwrap();
    if let Some(value) = prior {
        std::env::set_var("STRIPE_API_BASE", value);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }
    let session = client
        .create_subscription_session(SubscriptionCheckout {
            organizer_id: "organizer-1",
            attempt_key: "annual-attempt-1",
            expires_at: 1_800_003_960,
            email: "organizer@example.com",
            price_id: "price_annual_usd_99",
            success_url: "https://shop.example/ok",
            cancel_url: "https://shop.example/back",
        })
        .await
        .unwrap();
    assert_eq!(session.id, "cs_test_1");
    assert_eq!(session.url, "https://checkout.stripe.com/c/pay/cs_test_1");
    assert_eq!(session.expires_at, Some(1_800_003_960));
    let (headers, body) = seen.lock().unwrap().clone().unwrap();
    assert_eq!(headers["idempotency-key"], "annual-attempt-1");
    assert!(body.contains("client_reference_id=organizer-1"));
    assert!(body.contains("customer_email=organizer%40example.com"));
    assert!(body.contains("success_url=https%3A%2F%2Fshop.example%2Fok"));
    assert!(body.contains("cancel_url=https%3A%2F%2Fshop.example%2Fback"));
    assert!(body.contains("mode=subscription"));
    assert!(body.contains("expires_at=1800003960"));
    assert!(body.contains("line_items%5B0%5D%5Bprice%5D=price_annual_usd_99"));
    assert!(body.contains("line_items%5B0%5D%5Bquantity%5D=1"));
    assert!(!body.contains("price_data"));
    assert!(!body.contains("unit_amount"));
}

type Keys = Arc<Mutex<Vec<String>>>;

async fn slow(
    State(keys): State<Keys>,
    headers: HeaderMap,
    body: Bytes,
) -> Json<serde_json::Value> {
    let key = headers["idempotency-key"].to_str().unwrap().to_owned();
    keys.lock().unwrap().push(key.clone());
    if key.starts_with("silent") {
        std::future::pending::<()>().await;
    }
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    let body = String::from_utf8(body.to_vec()).unwrap();
    match body
        .split('&')
        .find_map(|pair| pair.strip_prefix("expires_at="))
    {
        Some(expires_at) => Json(
            serde_json::json!({"id": format!("cs_{key}"), "url": "https://checkout.stripe.com/c/pay/slow", "expires_at": expires_at.parse::<i64>().unwrap()}),
        ),
        None => Json(
            serde_json::json!({"id": format!("re_{key}"), "status": "succeeded", "amount": 3200, "payment_intent": "pi_slow"}),
        ),
    }
}

async fn slow_provider() -> (StripeClient, Keys) {
    let keys = Keys::default();
    let app = Router::new()
        .route("/v1/checkout/sessions", post(slow))
        .route("/v1/refunds", post(slow))
        .with_state(keys.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var("STRIPE_API_BASE", &base);
    let client = StripeClient::from_secret("sk_test_example").unwrap();
    if let Some(value) = prior {
        std::env::set_var("STRIPE_API_BASE", value);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }
    (client, keys)
}

fn subscription(attempt_key: &str) -> SubscriptionCheckout<'_> {
    SubscriptionCheckout {
        organizer_id: "organizer-1",
        attempt_key,
        expires_at: 1_800_003_960,
        email: "organizer@example.com",
        price_id: "price_annual_usd_99",
        success_url: "https://shop.example/ok",
        cancel_url: "https://shop.example/back",
    }
}

#[tokio::test]
#[serial]
async fn answers_that_take_six_seconds_are_accepted_once_with_their_idempotency_keys() {
    let (client, keys) = slow_provider().await;
    let expires_at = chrono::Utc::now().timestamp() + 1860;
    let (session, billing, refund) = tokio::join!(
        client.create_session(
            "slow-hold",
            "EUR",
            expires_at,
            "https://shop.example/ok",
            "https://shop.example/back",
            &[]
        ),
        client.create_subscription_session(subscription("slow-attempt")),
        client.create_refund("pi_slow", "slow-refund"),
    );
    let session = session.unwrap();
    assert_eq!(session.id, "cs_slow-hold");
    assert_eq!(session.expires_at, Some(expires_at));
    assert_eq!(billing.unwrap().id, "cs_slow-attempt");
    let refund = refund.unwrap();
    assert_eq!(refund.id, "re_slow-refund");
    assert_eq!(refund.status, "succeeded");
    let mut keys = keys.lock().unwrap().clone();
    keys.sort();
    assert_eq!(keys, ["slow-attempt", "slow-hold", "slow-refund"]);
}

#[tokio::test]
#[serial]
async fn a_provider_that_does_not_answer_ends_as_a_timeout_after_ten_seconds_without_a_retry() {
    let (client, keys) = slow_provider().await;
    let started = std::time::Instant::now();
    let (session, billing, refund) = tokio::join!(
        client.create_session(
            "silent-hold",
            "EUR",
            chrono::Utc::now().timestamp() + 1860,
            "https://shop.example/ok",
            "https://shop.example/back",
            &[]
        ),
        client.create_subscription_session(subscription("silent-attempt")),
        client.create_refund("pi_slow", "silent-refund"),
    );
    let elapsed = started.elapsed();
    assert!(matches!(session, Err(StripeError::Timeout)), "{session:?}");
    assert!(matches!(billing, Err(StripeError::Timeout)), "{billing:?}");
    assert!(matches!(refund, Err(StripeError::Timeout)), "{refund:?}");
    assert_eq!(
        StripeError::Timeout.to_string(),
        "Stripe did not answer within 10 seconds; the request may have been accepted"
    );
    assert!(
        elapsed >= std::time::Duration::from_secs(10)
            && elapsed < std::time::Duration::from_secs(13),
        "{elapsed:?}"
    );
    let mut keys = keys.lock().unwrap().clone();
    keys.sort();
    assert_eq!(keys, ["silent-attempt", "silent-hold", "silent-refund"]);
}
