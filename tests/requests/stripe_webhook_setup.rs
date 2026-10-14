use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post},
    Form, Json, Router,
};
use base64::Engine;
use chrono::Utc;
use crabgate::{
    app::App,
    members::{self, Role},
};
use hmac::{Hmac, Mac};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde_json::{json, Value};
use serial_test::serial;
use sha2::Sha256;

const WEBHOOK_URL: &str = "http://localhost:5150/webhooks/stripe";

#[derive(Default)]
struct Account {
    endpoints: Vec<Value>,
    created: usize,
    refuse_creation: bool,
    refuse_cleanup: bool,
    delete_refused: bool,
    deleted: Vec<String>,
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

type Fake = Arc<Mutex<Account>>;
type Refusal = (StatusCode, Json<Value>);

fn refusal(headers: &HeaderMap) -> Option<Refusal> {
    let credentials = base64::engine::general_purpose::STANDARD
        .decode(
            headers["authorization"]
                .to_str()
                .unwrap()
                .strip_prefix("Basic ")
                .unwrap(),
        )
        .unwrap();
    match String::from_utf8(credentials).unwrap().as_str() {
        "rk_test_nowebhooks:" => Some((
            StatusCode::FORBIDDEN,
            Json(
                json!({"error": {"message": "The provided key does not have the required permissions"}}),
            ),
        )),
        "rk_test_badaddress:" => Some((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": "Invalid URL", "param": "url"}})),
        )),
        _ => None,
    }
}

fn credentials(headers: &HeaderMap) -> String {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(
            headers["authorization"]
                .to_str()
                .unwrap()
                .strip_prefix("Basic ")
                .unwrap(),
        )
        .unwrap();
    String::from_utf8(bytes).unwrap()
}

async fn account(State(fake): State<Fake>, headers: HeaderMap) -> Result<Json<Value>, Refusal> {
    if credentials(&headers) == "sk_test_slowaccount:" {
        let (started, release) = {
            let state = fake.lock().unwrap();
            (state.started.clone(), state.release.clone())
        };
        started.notify_one();
        release.notified().await;
    }
    if credentials(&headers) == "sk_test_rejected:" {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "rejected"}))));
    }
    Ok(Json(json!({"id": "acct_test_1", "object": "account"})))
}

async fn create_endpoint(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Json<Value>, Refusal> {
    if let Some(refused) = refusal(&headers) {
        return Err(refused);
    }
    if credentials(&headers) == "sk_test_slow:" {
        let (started, release) = {
            let state = fake.lock().unwrap();
            (state.started.clone(), state.release.clone())
        };
        started.notify_one();
        release.notified().await;
    }
    if fake.lock().unwrap().refuse_creation {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "unavailable"})),
        ));
    }
    match credentials(&headers).as_str() {
        "sk_test_malformed:" => return Ok(Json(json!({"id": "we_invalid"}))),
        "sk_test_empty:" => return Ok(Json(json!({"id": "", "secret": "whsec_invalid"}))),
        "sk_test_wrongsecret:" => {
            return Ok(Json(json!({"id": "we_invalid", "secret": "invalid"})))
        }
        _ => {}
    }
    let mut account = fake.lock().unwrap();
    account.created += 1;
    let endpoint = json!({
        "id": format!("we_test_{}", account.created),
        "object": "webhook_endpoint",
        "secret": format!("whsec_created_{}", account.created),
        "url": form.iter().find(|(key, _)| key == "url").unwrap().1,
        "enabled_events": form
            .iter()
            .filter(|(key, _)| key == "enabled_events[]")
            .map(|(_, value)| value.clone())
            .collect::<Vec<_>>(),
    });
    account.endpoints.push(endpoint.clone());
    Ok(Json(endpoint))
}

async fn list_endpoints(State(fake): State<Fake>) -> Result<Json<Value>, Refusal> {
    if fake.lock().unwrap().refuse_cleanup {
        return Err((StatusCode::FORBIDDEN, Json(json!({"error": "list denied"}))));
    }
    let listed: Vec<Value> = fake
        .lock()
        .unwrap()
        .endpoints
        .iter()
        .map(|endpoint| {
            let mut endpoint = endpoint.clone();
            endpoint.as_object_mut().unwrap().remove("secret");
            endpoint
        })
        .collect();
    Ok(Json(
        json!({"object": "list", "has_more": false, "data": listed}),
    ))
}

async fn delete_endpoint(
    State(fake): State<Fake>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, Refusal> {
    if credentials(&headers) == "sk_test_slowcleanup:" {
        let (started, release) = {
            let state = fake.lock().unwrap();
            (state.started.clone(), state.release.clone())
        };
        started.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(10), release.notified())
            .await
            .unwrap();
    }
    let mut state = fake.lock().unwrap();
    if state.delete_refused {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error":"delete denied"})),
        ));
    }
    state.deleted.push(id.clone());
    state
        .endpoints
        .retain(|endpoint| endpoint["id"] != id.as_str());
    Ok(Json(json!({"id":id,"deleted":true})))
}

struct StripeBase(Option<String>, tokio::task::JoinHandle<()>);

impl Drop for StripeBase {
    fn drop(&mut self) {
        self.1.abort();
        if let Some(value) = &self.0 {
            std::env::set_var("STRIPE_API_BASE", value);
        } else {
            std::env::remove_var("STRIPE_API_BASE");
        }
    }
}

async fn start_fake() -> (Fake, StripeBase) {
    let fake: Fake = Arc::default();
    let app = Router::new()
        .route("/v1/account", get(account))
        .route(
            "/v1/webhook_endpoints",
            post(create_endpoint).get(list_endpoints),
        )
        .route("/v1/webhook_endpoints/{id}", delete(delete_endpoint))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var("STRIPE_API_BASE", base);
    (fake, StripeBase(prior, task))
}

async fn admin_cookie(ctx: &loco_rs::app::AppContext) -> String {
    let organizer_id: String = ctx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT id FROM organizers WHERE subdomain = 'local'",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "id")
        .unwrap();
    let email = "webhook-admin@example.com";
    members::add_member(&ctx.db, &organizer_id, email, Role::Admin)
        .await
        .unwrap();
    let now = Utc::now().timestamp();
    let link = members::request_magic_link(&ctx.db, &organizer_id, email, now)
        .await
        .unwrap()
        .unwrap();
    let session = members::claim_magic_link(&ctx.db, &organizer_id, &link, now + 1)
        .await
        .unwrap()
        .unwrap();
    format!("cg_session={session}")
}

fn signature(body: &str, secret: &str) -> String {
    let now = Utc::now().timestamp();
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{now}.{body}").as_bytes());
    format!("t={now},v1={}", hex::encode(mac.finalize().into_bytes()))
}

macro_rules! webhook_status {
    ($request:expr, $secret:expr) => {{
        let body = json!({
            "type": "checkout.session.expired",
            "data": {"object": {"id": "cs_other_tool", "payment_status": "unpaid", "metadata": {}}}
        })
        .to_string();
        $request
            .post("/webhooks/stripe")
            .text(&body)
            .add_header("Stripe-Signature", signature(&body, $secret))
            .await
            .status_code()
            .as_u16()
    }};
}

macro_rules! save_key {
    ($request:expr, $cookie:expr, $key:expr) => {{
        let response = $request
            .post("/office/setup/stripe")
            .add_header("cookie", $cookie)
            .form(&[("secret_key", $key)])
            .await;
        assert_eq!(response.status_code(), 303);
        response.header("location").to_str().unwrap().to_owned()
    }};
}

macro_rules! page {
    ($request:expr, $cookie:expr, $path:expr) => {
        $request
            .get($path)
            .add_header("cookie", $cookie)
            .await
            .text()
    };
}

#[tokio::test]
#[serial]
async fn saving_a_key_creates_the_webhook_and_finishes_the_step() {
    let (fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        let before = page!(request, &cookie, "/office/setup?step=webhook");
        assert!(!before.contains("CrabGate created the webhook"));
        assert!(before.contains("<b>0 of 6</b> done"));
        assert_eq!(webhook_status!(request, "whsec_created_1"), 503);

        assert_eq!(
            save_key!(request, &cookie, "sk_test_example"),
            "/office/setup"
        );

        {
            let account = fake.lock().unwrap();
            assert_eq!(account.endpoints.len(), 1);
            assert_eq!(account.endpoints[0]["url"], WEBHOOK_URL);
            assert_eq!(
                account.endpoints[0]["enabled_events"],
                json!([
                    "checkout.session.completed",
                    "checkout.session.async_payment_succeeded",
                    "checkout.session.async_payment_failed",
                    "checkout.session.expired",
                    "charge.refunded",
                    "refund.created",
                    "refund.updated",
                    "refund.failed"
                ])
            );
        }

        assert_eq!(webhook_status!(request, "whsec_created_1"), 200);
        assert_eq!(webhook_status!(request, "whsec_wrong"), 400);

        let after = page!(request, &cookie, "/office/setup?step=webhook");
        assert!(after.contains("CrabGate created the webhook in your Stripe account"));
        assert!(after.contains("<b>…ed_1</b>"));
        assert!(after.contains("<span class=\"stamp stamp-ink\">Done</span>"));
        assert!(after.contains("<b>2 of 6</b> done"));
        for event in [
            "checkout.session.completed",
            "refund.updated",
            "refund.failed",
        ] {
            assert!(after.contains(&format!("<li>{event}</li>")));
        }
    })
    .await;
}

#[tokio::test]
#[serial]
async fn saving_a_key_again_replaces_the_endpoint_instead_of_adding_one() {
    let (fake, _base) = start_fake().await;
    {
        let mut account = fake.lock().unwrap();
        account
            .endpoints
            .push(json!({"id": "we_by_hand", "url": WEBHOOK_URL, "enabled_events": []}));
        account.endpoints.push(
            json!({"id": "we_other_app", "url": "https://other.example/hook", "enabled_events": []}),
        );
    }
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        assert_eq!(
            save_key!(request, &cookie, "sk_test_example"),
            "/office/setup"
        );
        assert_eq!(
            save_key!(request, &cookie, "sk_test_example"),
            "/office/setup"
        );

        let ids: Vec<String> = fake
            .lock()
            .unwrap()
            .endpoints
            .iter()
            .map(|endpoint| endpoint["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids, ["we_other_app", "we_test_2"]);
        assert_eq!(webhook_status!(request, "whsec_created_2"), 200);
        assert_eq!(webhook_status!(request, "whsec_created_1"), 400);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn a_refused_endpoint_keeps_the_key_and_offers_the_manual_step() {
    let (fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;

        assert_eq!(
            save_key!(request, &cookie, "rk_test_nowebhooks"),
            "/office/setup?step=webhook&error=webhook_permission"
        );
        let denied = page!(
            request,
            &cookie,
            "/office/setup?step=webhook&error=webhook_permission"
        );
        assert!(denied.contains("Your key is saved, but the webhook is not ready."));
        assert!(denied.contains("Stripe won't let this key create webhook endpoints."));
        assert!(denied.contains("action=\"/office/setup/webhook\""));
        assert!(!denied.contains("CrabGate created the webhook"));
        let stripe = page!(request, &cookie, "/office/setup?step=stripe");
        assert!(stripe.contains("<b>Connected.</b> Your key ends in <b>…ooks</b>"));

        assert_eq!(
            save_key!(request, &cookie, "rk_test_badaddress"),
            "/office/setup?step=webhook&error=webhook_refused"
        );
        let refused = page!(
            request,
            &cookie,
            "/office/setup?step=webhook&error=webhook_refused"
        );
        assert!(refused.contains("Stripe did not create the webhook."));
        assert!(refused.contains("such as <code>localhost</code>"));
        assert!(fake.lock().unwrap().endpoints.is_empty());

        let pasted = request
            .post("/office/setup/webhook")
            .add_header("cookie", &cookie)
            .form(&[
                ("secret", "whsec_by_hand"),
                (
                    "attempt",
                    saved(&ctx, "stripe_setup_attempt")
                        .await
                        .unwrap_or_default()
                        .as_str(),
                ),
            ])
            .await;
        assert_eq!(pasted.status_code(), 303);
        assert_eq!(webhook_status!(request, "whsec_by_hand"), 200);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn a_pasted_secret_keeps_working_when_endpoint_creation_is_refused() {
    let (_fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        assert_eq!(
            save_key!(request, &cookie, "rk_test_nowebhooks"),
            "/office/setup?step=webhook&error=webhook_permission"
        );
        request
            .post("/office/setup/webhook")
            .add_header("cookie", &cookie)
            .form(&[
                ("secret", "whsec_pasted"),
                (
                    "attempt",
                    saved(&ctx, "stripe_setup_attempt")
                        .await
                        .unwrap_or_default()
                        .as_str(),
                ),
            ])
            .await;
        assert_eq!(webhook_status!(request, "whsec_pasted"), 200);

        assert_eq!(
            save_key!(request, &cookie, "rk_test_nowebhooks"),
            "/office/setup?step=webhook&error=webhook_permission"
        );

        assert_eq!(webhook_status!(request, "whsec_pasted"), 200);
        let step = page!(request, &cookie, "/office/setup?step=webhook");
        assert!(step.contains("<b>Working.</b> Stripe has reached your Shop."));
        assert!(!step.contains("CrabGate created the webhook"));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn replacing_ready_credentials_with_refused_webhook_clears_readiness() {
    let (_fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup");
        let before = page!(request, &cookie, "/office/setup?step=webhook");
        assert!(before.contains("<b>2 of 6</b> done"));
        assert_eq!(
            save_key!(request, &cookie, "rk_test_nowebhooks"),
            "/office/setup?step=webhook&error=webhook_permission"
        );
        let after = page!(request, &cookie, "/office/setup?step=webhook");
        assert!(after.contains("<b>1 of 6</b> done"));
        assert!(!after.contains("CrabGate created the webhook"));
        assert_eq!(webhook_status!(request, "whsec_created_1"), 503);
    })
    .await;
}

async fn saved(ctx: &loco_rs::app::AppContext, key: &str) -> Option<String> {
    let row = ctx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT id, db_path, data_key FROM organizers WHERE subdomain = 'local'",
        ))
        .await
        .unwrap()
        .unwrap();
    let id: String = row.try_get("", "id").unwrap();
    let path: String = row.try_get("", "db_path").unwrap();
    let wrapped: String = row.try_get("", "data_key").unwrap();
    let cipher =
        crabgate::crypto::MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
            .unwrap()
            .open_organizer_key(&id, &wrapped)
            .unwrap();
    let db = sea_orm::Database::connect(format!("sqlite://{path}?mode=rw"))
        .await
        .unwrap();
    crabgate::settings::get(&db, &cipher, key).await.unwrap()
}

#[tokio::test]
#[serial]
async fn invalid_and_rejected_keys_preserve_the_working_configuration() {
    let (fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup");
        for (key, error) in [
            ("invalid", "key_format"),
            ("sk_test_rejected", "key_rejected"),
        ] {
            assert_eq!(
                save_key!(request, &cookie, key),
                format!("/office/setup?step=stripe&error={error}")
            );
            assert_eq!(
                saved(&ctx, "stripe_secret_key").await.as_deref(),
                Some("sk_test_old")
            );
            assert_eq!(
                saved(&ctx, "webhook_endpoint_id").await.as_deref(),
                Some("we_test_1")
            );
            assert_eq!(webhook_status!(request, "whsec_created_1"), 200);
        }
        assert_eq!(fake.lock().unwrap().created, 1);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn same_key_failure_keeps_current_readiness_and_retry_succeeds() {
    let (fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup");
        fake.lock().unwrap().refuse_creation = true;
        assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup?step=webhook&error=webhook_refused");
        assert_eq!(webhook_status!(request, "whsec_created_1"), 200);
        let page = page!(request, &cookie, "/office/setup?step=webhook&error=webhook_refused");
        assert!(page.contains("Your current webhook still works. The replacement was not created."));
        assert!(page.contains("<b>2 of 6</b> done"));
        fake.lock().unwrap().refuse_creation = false;
        assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup");
        assert_eq!(webhook_status!(request, "whsec_created_2"), 200);
        assert_eq!(webhook_status!(request, "whsec_created_1"), 400);
    }).await;
}

#[tokio::test]
#[serial]
async fn malformed_endpoint_responses_invalidate_old_credentials() {
    let (_fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        for key in ["sk_test_malformed", "sk_test_empty", "sk_test_wrongsecret"] {
            assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup");
            assert_eq!(
                save_key!(request, &cookie, key),
                "/office/setup?step=webhook&error=webhook_refused"
            );
            assert_eq!(saved(&ctx, "stripe_secret_key").await.as_deref(), Some(key));
            for setting in ["webhook_secret", "webhook_endpoint_id", "webhook_tested_at"] {
                assert_eq!(saved(&ctx, setting).await.as_deref(), Some(""));
            }
            let page = page!(request, &cookie, "/office/setup?step=webhook");
            assert!(page.contains("<b>1 of 6</b> done"));
            assert!(!page.contains("<b>Working.</b>"));
        }
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cleanup_failure_keeps_the_new_endpoint_ready_with_a_truthful_warning() {
    let (fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup");
        fake.lock().unwrap().refuse_cleanup = true;
        assert_eq!(
            save_key!(request, &cookie, "sk_test_new"),
            "/office/setup?step=webhook&error=webhook_duplicate"
        );
        assert_eq!(
            saved(&ctx, "webhook_endpoint_id").await.as_deref(),
            Some("we_test_2")
        );
        assert_eq!(webhook_status!(request, "whsec_created_2"), 200);
        let page = page!(
            request,
            &cookie,
            "/office/setup?step=webhook&error=webhook_duplicate"
        );
        assert!(page.contains("Your webhook is ready. Check older endpoints."));
        assert!(!page.contains("Your key is saved, but the webhook is not ready."));
        assert_eq!(fake.lock().unwrap().endpoints.len(), 2);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn late_automatic_result_cannot_replace_a_newer_key_or_delete_its_endpoint() {
    let (fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup");
        let (started, release) = {
            let state = fake.lock().unwrap();
            (state.started.clone(), state.release.clone())
        };
        let slow = async { save_key!(request, &cookie, "sk_test_slow") };
        let newer = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), started.notified())
                .await
                .unwrap();
            assert_eq!(save_key!(request, &cookie, "sk_test_new"), "/office/setup");
            release.notify_one();
        };
        let (late, ()) = tokio::join!(slow, newer);
        assert_eq!(late, "/office/setup?step=webhook&error=webhook_superseded");
        assert_eq!(
            saved(&ctx, "stripe_secret_key").await.as_deref(),
            Some("sk_test_new")
        );
        assert_eq!(
            saved(&ctx, "webhook_endpoint_id").await.as_deref(),
            Some("we_test_2")
        );
        assert_eq!(webhook_status!(request, "whsec_created_2"), 200);
        let ids: Vec<String> = fake
            .lock()
            .unwrap()
            .endpoints
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids, ["we_test_2", "we_test_3"]);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn manual_save_supersedes_an_in_flight_automatic_result() {
    let (fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        let (started, release) = {
            let state = fake.lock().unwrap();
            (state.started.clone(), state.release.clone())
        };
        let slow = async { save_key!(request, &cookie, "sk_test_slow") };
        let manual = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), started.notified())
                .await
                .unwrap();
            assert_eq!(
                request
                    .post("/office/setup/webhook")
                    .add_header("cookie", &cookie)
                    .form(&[
                        ("secret", "whsec_manual"),
                        (
                            "attempt",
                            saved(&ctx, "stripe_setup_attempt")
                                .await
                                .unwrap_or_default()
                                .as_str()
                        )
                    ])
                    .await
                    .status_code(),
                303
            );
            let page = page!(request, &cookie, "/office/setup?step=webhook");
            assert!(page.contains("Secret saved. Now send a test event from Stripe."));
            assert_eq!(saved(&ctx, "webhook_tested_at").await.as_deref(), Some(""));
            release.notify_one();
        };
        let (late, ()) = tokio::join!(slow, manual);
        assert_eq!(late, "/office/setup?step=webhook&error=webhook_superseded");
        assert_eq!(
            saved(&ctx, "webhook_secret").await.as_deref(),
            Some("whsec_manual")
        );
        assert_eq!(webhook_status!(request, "whsec_manual"), 200);
        assert_eq!(webhook_status!(request, "whsec_created_1"), 400);
    })
    .await;
}

struct EnvironmentVariable(&'static str, Option<String>);

impl Drop for EnvironmentVariable {
    fn drop(&mut self) {
        if let Some(value) = &self.1 {
            std::env::set_var(self.0, value);
        } else {
            std::env::remove_var(self.0);
        }
    }
}

#[tokio::test]
#[serial]
async fn hosted_paid_publish_is_blocked_until_current_webhook_is_ready() {
    use crabgate::{
        events::{self, EventInput, TicketTypeInput},
        hosted,
    };
    let (_fake, _base) = start_fake().await;
    let _edition = EnvironmentVariable("CRABGATE_EDITION", std::env::var("CRABGATE_EDITION").ok());
    std::env::set_var("CRABGATE_EDITION", "hosted");
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let signup = hosted::signup(&registry, &ctx.db, "Gate proof", "gate-proof", "gate@example.com", hosted::now()).await.unwrap();
        let complete = request.get(&format!("/office/complete-signup/{}", signup.verification_token))
            .add_header("host", "gate-proof.crabgate.com").await;
        assert_eq!(complete.status_code(), 303);
        let cookie = complete.header("set-cookie").to_str().unwrap().split(';').next().unwrap().to_owned();
        ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "UPDATE subscriptions SET status = 'Active' WHERE organizer_id = ?", [signup.organizer.id.clone().into()],
        )).await.unwrap();
        let db = sea_orm::Database::connect(format!("sqlite://{}?mode=rw", signup.organizer.db_path)).await.unwrap();
        let mut ids = Vec::new();
        for title in ["Old ready", "New pending"] {
            let event = events::create(&db, &EventInput {
                title: title.to_owned(), date: "2027-06-15".to_owned(), start_time: "19:00".to_owned(),
                end_time: "21:00".to_owned(), timezone: "Europe/Dublin".to_owned(), venue: "Hall".to_owned(),
                address: "Road".to_owned(), description: String::new(), event_capacity: None,
                max_per_order: 10, attendee_name_required: false,
            }).await.unwrap();
            events::add_ticket_type(&db, &event, &TicketTypeInput {
                name: "Paid".to_owned(), price: "10".to_owned(), capacity: 10, sales_open: None, sales_close: None,
            }).await.unwrap();
            ids.push(event.id);
        }
        let old = request.post("/office/setup/stripe").add_header("host", "gate-proof.crabgate.com")
            .add_header("cookie", &cookie).form(&[("secret_key", "sk_test_old")]).await;
        assert_eq!(old.status_code(), 303);
        assert_eq!(old.header("location").to_str().unwrap(), "/office/setup");
        assert_eq!(request.post(&format!("/office/events/{}/publish", ids[0]))
            .add_header("host", "gate-proof.crabgate.com").add_header("cookie", &cookie).await.status_code(), 303);
        let cipher = registry.cipher(&signup.organizer).unwrap();
        let captured = tokio::sync::Notify::new();
        let release = tokio::sync::Notify::new();
        let receipt = async {
            let authenticated = crabgate::settings::webhook_configuration(&db, &cipher).await.unwrap().unwrap();
            let body = json!({"type":"checkout.session.expired", "data":{"object":{"id":"cs_foreign", "metadata":{}}}}).to_string();
            crabgate::stripe::verify_webhook(body.as_bytes(), &signature(&body, authenticated.secret()), authenticated.secret(), Utc::now().timestamp()).unwrap();
            captured.notify_one();
            tokio::time::timeout(std::time::Duration::from_secs(10), release.notified()).await.unwrap();
            assert!(!crabgate::settings::mark_webhook_received(&db, &authenticated, "1234567890").await.unwrap());
            authenticated
        };
        let replacement = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), captured.notified()).await.unwrap();
            let new = request.post("/office/setup/stripe").add_header("host", "gate-proof.crabgate.com")
                .add_header("cookie", &cookie).form(&[("secret_key", "rk_test_nowebhooks")]).await;
            assert_eq!(new.header("location").to_str().unwrap(), "/office/setup?step=webhook&error=webhook_permission");
            release.notify_one();
        };
        let (authenticated, ()) = tokio::join!(receipt, replacement);
        assert_eq!(crabgate::settings::get(&db, &cipher, "webhook_tested_at").await.unwrap().as_deref(), Some(""));
        let blocked = request.post(&format!("/office/events/{}/publish", ids[1]))
            .add_header("host", "gate-proof.crabgate.com").add_header("cookie", &cookie).await;
        assert_eq!(blocked.status_code(), 403);
        assert_eq!(blocked.text(), "Finish the Stripe key and webhook checks in Setup before you publish.");
        let editor = request.get(&format!("/office/events/{}", ids[1]))
            .add_header("host", "gate-proof.crabgate.com").add_header("cookie", &cookie).await;
        assert!(editor.text().contains("Pass the webhook check"));
        assert!(editor.text().contains("disabled aria-describedby=\"publish-note\""));
        let manual = request.post("/office/setup/webhook").add_header("host", "gate-proof.crabgate.com")
            .add_header("cookie", &cookie).form(&[("secret", "whsec_created_1"), ("attempt", crabgate::settings::get(&db, &registry.cipher(&signup.organizer).unwrap(), "stripe_setup_attempt").await.unwrap().unwrap_or_default().as_str())]).await;
        assert_eq!(manual.status_code(), 303);
        crabgate::settings::mark_webhook_received(&db, &authenticated, "1234567890").await.unwrap();
        assert_eq!(crabgate::settings::get(&db, &cipher, "webhook_tested_at").await.unwrap().as_deref(), Some(""));
        let pending = request.get("/office/setup?step=webhook").add_header("host", "gate-proof.crabgate.com").add_header("cookie", &cookie).await.text();
        assert!(pending.contains("Secret saved. Now send a test event from Stripe."));
        assert!(!pending.contains("<b>Working.</b>"));
        assert_eq!(request.post(&format!("/office/events/{}/publish", ids[1]))
            .add_header("host", "gate-proof.crabgate.com").add_header("cookie", &cookie).await.status_code(), 403);
        let body = json!({"type":"checkout.session.expired", "data":{"object":{"id":"cs_foreign", "metadata":{}}}}).to_string();
        assert_eq!(request.post("/webhooks/stripe").add_header("host", "gate-proof.crabgate.com")
            .add_header("Stripe-Signature", signature(&body, "whsec_created_1")).text(&body).await.status_code(), 200);
        assert!(!crabgate::settings::get(&db, &cipher, "webhook_tested_at").await.unwrap().unwrap().is_empty());
        assert_eq!(request.post(&format!("/office/events/{}/publish", ids[1]))
            .add_header("host", "gate-proof.crabgate.com").add_header("cookie", &cookie).await.status_code(), 303);
    }).await;
}

#[tokio::test]
#[serial]
async fn setup_remains_admin_only_and_denies_granted_support() {
    use crabgate::operator;
    let (fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let admin = admin_cookie(&ctx).await;
        let row = ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT memberships.organizer_id, members.id FROM memberships JOIN members ON members.id = memberships.member_id WHERE members.email = 'webhook-admin@example.com'",
            ))
            .await
            .unwrap()
            .unwrap();
        let id: String = row.try_get("", "organizer_id").unwrap();
        let member: String = row.try_get("", "id").unwrap();
        members::add_member(&ctx.db, &id, "door@example.com", Role::DoorStaff)
            .await
            .unwrap();
        let now = crabgate::orders::now();
        let link = members::request_magic_link(&ctx.db, &id, "door@example.com", now)
            .await.unwrap().unwrap();
        let session = members::claim_magic_link(&ctx.db, &id, &link, now + 1)
            .await.unwrap().unwrap();
        ctx.db
            .execute_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "INSERT INTO operators (id, email) VALUES ('setup-support', 'support@example.com')",
            ))
            .await
            .unwrap();
        let token = operator::request_link(&ctx.db, "support@example.com", crabgate::orders::now())
            .await
            .unwrap()
            .unwrap();
        let support = operator::claim_link(&ctx.db, &token, crabgate::orders::now())
            .await
            .unwrap()
            .unwrap();
        operator::grant(&ctx.db, &id, &member, 1, crabgate::orders::now())
            .await
            .unwrap();
        for cookie in [
            format!("cg_session={session}"),
            format!("cg_operator_session={support}"),
        ] {
            assert_eq!(
                request
                    .get("/office/setup")
                    .add_header("cookie", &cookie)
                    .await
                    .status_code(),
                403
            );
            assert_eq!(
                request
                    .post("/office/setup/stripe")
                    .add_header("cookie", &cookie)
                    .form(&[("secret_key", "sk_test_forbidden")])
                    .await
                    .status_code(),
                403
            );
            assert_eq!(
                request
                    .post("/office/setup/webhook")
                    .add_header("cookie", &cookie)
                    .form(&[
                        ("secret", "whsec_forbidden"),
                        (
                            "attempt",
                            saved(&ctx, "stripe_setup_attempt")
                                .await
                                .unwrap_or_default()
                                .as_str()
                        )
                    ])
                    .await
                    .status_code(),
                403
            );
        }
        assert_eq!(
            request
                .get("/office/setup")
                .add_header("cookie", &admin)
                .await
                .status_code(),
            200
        );
        assert_eq!(saved(&ctx, "stripe_secret_key").await, None);
        assert_eq!(fake.lock().unwrap().created, 0);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn a_key_test_finishing_late_cannot_overwrite_a_newer_save() {
    let (fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        let (started, release) = {
            let state = fake.lock().unwrap();
            (state.started.clone(), state.release.clone())
        };
        let slow = async { save_key!(request, &cookie, "sk_test_slowaccount") };
        let newer = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), started.notified())
                .await
                .unwrap();
            assert_eq!(save_key!(request, &cookie, "sk_test_new"), "/office/setup");
            release.notify_one();
        };
        let (late, ()) = tokio::join!(slow, newer);
        assert_eq!(late, "/office/setup?step=webhook&error=webhook_superseded");
        assert_eq!(
            saved(&ctx, "stripe_secret_key").await.as_deref(),
            Some("sk_test_new")
        );
        assert_eq!(
            saved(&ctx, "webhook_endpoint_id").await.as_deref(),
            Some("we_test_1")
        );
        assert_eq!(fake.lock().unwrap().created, 1);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn a_manual_form_for_previous_credentials_is_rejected() {
    let (_fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup");
        let previous = saved(&ctx, "stripe_setup_attempt").await.unwrap();
        let page = page!(request, &cookie, "/office/setup?step=webhook");
        assert!(page.contains(&format!("name=\"attempt\" value=\"{previous}\"")));
        assert_eq!(save_key!(request, &cookie, "sk_test_new"), "/office/setup");
        let response = request
            .post("/office/setup/webhook")
            .add_header("cookie", &cookie)
            .form(&[
                ("secret", "whsec_oldaccount"),
                ("attempt", previous.as_str()),
            ])
            .await;
        assert_eq!(response.status_code(), 303);
        assert_eq!(
            response.header("location").to_str().unwrap(),
            "/office/setup?step=webhook&error=webhook_superseded"
        );
        assert_eq!(
            saved(&ctx, "webhook_secret").await.as_deref(),
            Some("whsec_created_2")
        );
        assert_eq!(webhook_status!(request, "whsec_created_2"), 200);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn key_invalidation_rolls_back_as_one_unit_on_database_failure() {
    let (fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup");
        let attempt = saved(&ctx, "stripe_setup_attempt").await;
        let path: String = ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT db_path FROM organizers WHERE subdomain = 'local'",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "db_path")
            .unwrap();
        let db = sea_orm::Database::connect(format!("sqlite://{path}?mode=rw"))
            .await
            .unwrap();
        db.execute_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "CREATE TRIGGER refuse_invalidation BEFORE UPDATE ON organizer_settings
             WHEN OLD.key = 'webhook_tested_at' AND NEW.value = ''
             BEGIN SELECT RAISE(ABORT, 'synthetic settings failure'); END",
        ))
        .await
        .unwrap();
        let failed = request
            .post("/office/setup/stripe")
            .add_header("cookie", &cookie)
            .form(&[("secret_key", "sk_test_new")])
            .await;
        assert_eq!(failed.status_code(), 500);
        assert_eq!(saved(&ctx, "stripe_setup_attempt").await, attempt);
        assert_eq!(
            saved(&ctx, "stripe_secret_key").await.as_deref(),
            Some("sk_test_old")
        );
        assert_eq!(
            saved(&ctx, "webhook_secret").await.as_deref(),
            Some("whsec_created_1")
        );
        assert_eq!(
            saved(&ctx, "webhook_endpoint_id").await.as_deref(),
            Some("we_test_1")
        );
        assert_eq!(fake.lock().unwrap().created, 1);
        assert_eq!(webhook_status!(request, "whsec_created_1"), 200);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn replaced_credentials_do_not_fall_back_to_an_environment_webhook_secret() {
    let (_fake, _base) = start_fake().await;
    let _env = EnvironmentVariable(
        "STRIPE_TEST_WEBHOOK_SECRET",
        std::env::var("STRIPE_TEST_WEBHOOK_SECRET").ok(),
    );
    std::env::set_var("STRIPE_TEST_WEBHOOK_SECRET", "whsec_environment_synthetic");
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup");
        assert_eq!(
            save_key!(request, &cookie, "rk_test_nowebhooks"),
            "/office/setup?step=webhook&error=webhook_permission"
        );
        assert_eq!(webhook_status!(request, "whsec_environment_synthetic"), 503);
        assert_eq!(saved(&ctx, "webhook_tested_at").await.as_deref(), Some(""));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn stale_receipts_cannot_confirm_reused_manual_secrets_or_new_attempts() {
    use crabgate::settings;
    let (fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup");
        let registry = super::organizer_registry(&ctx);
        let scope = registry.active_dbs().await.unwrap().remove(0);
        let cipher = registry.cipher(&scope.organizer).unwrap();
        let authenticated = settings::webhook_configuration(&scope.db, &cipher)
            .await
            .unwrap()
            .unwrap();
        fake.lock().unwrap().refuse_creation = true;
        assert_eq!(
            save_key!(request, &cookie, "sk_test_old"),
            "/office/setup?step=webhook&error=webhook_refused"
        );
        let ready = saved(&ctx, "webhook_tested_at").await;
        assert!(
            !settings::mark_webhook_received(&scope.db, &authenticated, "1234567890")
                .await
                .unwrap()
        );
        assert_eq!(saved(&ctx, "webhook_tested_at").await, ready);
        for _ in 0..2 {
            let authenticated = settings::webhook_configuration(&scope.db, &cipher)
                .await
                .unwrap()
                .unwrap();
            let attempt = saved(&ctx, "stripe_setup_attempt").await.unwrap();
            assert_eq!(
                request
                    .post("/office/setup/webhook")
                    .add_header("cookie", &cookie)
                    .form(&[("secret", "whsec_created_1"), ("attempt", attempt.as_str())])
                    .await
                    .status_code(),
                303
            );
            assert!(
                !settings::mark_webhook_received(&scope.db, &authenticated, "1234567890")
                    .await
                    .unwrap()
            );
            assert_eq!(saved(&ctx, "webhook_tested_at").await.as_deref(), Some(""));
        }
        let current = settings::webhook_configuration(&scope.db, &cipher)
            .await
            .unwrap()
            .unwrap();
        assert!(
            settings::mark_webhook_received(&scope.db, &current, "1234567890")
                .await
                .unwrap()
        );
        assert_eq!(
            saved(&ctx, "webhook_tested_at").await.as_deref(),
            Some("1234567890")
        );
        settings::set(
            &scope.db,
            &cipher,
            "stripe_setup_attempt",
            "synthetic-next-attempt",
        )
        .await
        .unwrap();
        assert!(
            !settings::mark_webhook_received(&scope.db, &current, "1234567891")
                .await
                .unwrap()
        );
        assert_eq!(
            saved(&ctx, "webhook_tested_at").await.as_deref(),
            Some("1234567890")
        );
        let current = settings::webhook_configuration(&scope.db, &cipher)
            .await
            .unwrap()
            .unwrap();
        settings::set(&scope.db, &cipher, "stripe_secret_key", "sk_test_new")
            .await
            .unwrap();
        assert!(
            !settings::mark_webhook_received(&scope.db, &current, "1234567891")
                .await
                .unwrap()
        );
        assert_eq!(
            saved(&ctx, "webhook_tested_at").await.as_deref(),
            Some("1234567890")
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn environment_receipts_cannot_confirm_a_later_database_secret() {
    use crabgate::settings;
    let _env = EnvironmentVariable(
        "STRIPE_TEST_WEBHOOK_SECRET",
        std::env::var("STRIPE_TEST_WEBHOOK_SECRET").ok(),
    );
    std::env::set_var("STRIPE_TEST_WEBHOOK_SECRET", "whsec_environment_synthetic");
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        let registry = super::organizer_registry(&ctx);
        let scope = registry.active_dbs().await.unwrap().remove(0);
        let cipher = registry.cipher(&scope.organizer).unwrap();
        let authenticated = settings::webhook_configuration(&scope.db, &cipher)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(authenticated.secret(), "whsec_environment_synthetic");
        assert_eq!(webhook_status!(request, "whsec_environment_synthetic"), 200);
        assert_eq!(saved(&ctx, "webhook_tested_at").await, None);
        assert_eq!(
            request
                .post("/office/setup/webhook")
                .add_header("cookie", &cookie)
                .form(&[("secret", "whsec_environment_synthetic"), ("attempt", "")])
                .await
                .status_code(),
            303
        );
        assert!(
            !settings::mark_webhook_received(&scope.db, &authenticated, "1234567890")
                .await
                .unwrap()
        );
        assert_eq!(saved(&ctx, "webhook_tested_at").await.as_deref(), Some(""));
        assert_eq!(webhook_status!(request, "whsec_environment_synthetic"), 200);
        assert!(!saved(&ctx, "webhook_tested_at").await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
#[serial]
async fn receipt_database_failure_preserves_pending_state_and_retry_confirms() {
    let (_fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup");
        let attempt = saved(&ctx, "stripe_setup_attempt").await.unwrap();
        assert_eq!(
            request
                .post("/office/setup/webhook")
                .add_header("cookie", &cookie)
                .form(&[("secret", "whsec_pending"), ("attempt", attempt.as_str())])
                .await
                .status_code(),
            303
        );
        let registry = super::organizer_registry(&ctx);
        let scope = registry.active_dbs().await.unwrap().remove(0);
        scope
            .db
            .execute_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "CREATE TRIGGER refuse_receipt BEFORE UPDATE ON organizer_settings
             WHEN OLD.key = 'webhook_tested_at' AND NEW.value != ''
             BEGIN SELECT RAISE(ABORT, 'synthetic receipt failure'); END",
            ))
            .await
            .unwrap();
        assert_eq!(webhook_status!(request, "whsec_wrong"), 400);
        assert_eq!(
            request
                .post("/webhooks/stripe")
                .text("invalid json")
                .add_header(
                    "Stripe-Signature",
                    signature("invalid json", "whsec_pending")
                )
                .await
                .status_code(),
            400
        );
        assert_eq!(saved(&ctx, "webhook_tested_at").await.as_deref(), Some(""));
        assert_eq!(webhook_status!(request, "whsec_pending"), 500);
        assert_eq!(saved(&ctx, "webhook_tested_at").await.as_deref(), Some(""));
        assert_eq!(
            saved(&ctx, "webhook_secret").await.as_deref(),
            Some("whsec_pending")
        );
        let pending = page!(request, &cookie, "/office/setup?step=webhook");
        assert!(pending.contains("Secret saved. Now send a test event from Stripe."));
        scope
            .db
            .execute_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "DROP TRIGGER refuse_receipt",
            ))
            .await
            .unwrap();
        assert_eq!(webhook_status!(request, "whsec_pending"), 200);
        assert!(!saved(&ctx, "webhook_tested_at").await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cleanup_delete_failure_leaves_new_endpoint_ready() {
    let (fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup");
        fake.lock().unwrap().delete_refused = true;
        assert_eq!(
            save_key!(request, &cookie, "sk_test_new"),
            "/office/setup?step=webhook&error=webhook_duplicate"
        );
        assert_eq!(
            fake.lock()
                .unwrap()
                .endpoints
                .iter()
                .map(|e| e["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["we_test_1", "we_test_2"]
        );
        assert_eq!(fake.lock().unwrap().deleted.clone(), Vec::<String>::new());
        assert_eq!(
            saved(&ctx, "webhook_endpoint_id").await.as_deref(),
            Some("we_test_2")
        );
        assert_eq!(webhook_status!(request, "whsec_created_2"), 200);
        let page = page!(
            request,
            &cookie,
            "/office/setup?step=webhook&error=webhook_duplicate"
        );
        assert!(page.contains("Your webhook is ready. Check older endpoints."));
        assert!(page.contains("The webhook works,"));
        assert!(!page.contains("Your current webhook is pending verification."));
        assert!(page.contains("role=\"alert\""));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn delayed_cleanup_cannot_delete_later_selected_endpoint() {
    let (fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup");
        let (started, release) = {
            let state = fake.lock().unwrap();
            (state.started.clone(), state.release.clone())
        };
        let slow = async { save_key!(request, &cookie, "sk_test_slowcleanup") };
        let newer = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), started.notified())
                .await
                .unwrap();
            assert_eq!(
                saved(&ctx, "webhook_endpoint_id").await.as_deref(),
                Some("we_test_2")
            );
            assert_eq!(save_key!(request, &cookie, "sk_test_new"), "/office/setup");
            release.notify_one();
        };
        let (late, ()) = tokio::join!(slow, newer);
        assert_eq!(late, "/office/setup");
        assert_eq!(
            saved(&ctx, "stripe_secret_key").await.as_deref(),
            Some("sk_test_new")
        );
        assert_eq!(
            saved(&ctx, "webhook_endpoint_id").await.as_deref(),
            Some("we_test_3")
        );
        assert_eq!(
            fake.lock()
                .unwrap()
                .endpoints
                .iter()
                .map(|e| e["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["we_test_3"]
        );
        assert_eq!(
            fake.lock().unwrap().deleted.clone(),
            ["we_test_1", "we_test_2", "we_test_1"]
        );
        assert_eq!(webhook_status!(request, "whsec_created_3"), 200);
        assert_eq!(webhook_status!(request, "whsec_created_2"), 400);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn delayed_cleanup_failure_does_not_claim_new_manual_state_is_ready() {
    let (fake, _base) = start_fake().await;
    request::<App, _, _>(|request, ctx| async move {
        let cookie = admin_cookie(&ctx).await;
        assert_eq!(save_key!(request, &cookie, "sk_test_old"), "/office/setup");
        let (started, release) = {
            let state = fake.lock().unwrap();
            (state.started.clone(), state.release.clone())
        };
        let slow = async { save_key!(request, &cookie, "sk_test_slowcleanup") };
        let manual = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), started.notified())
                .await
                .unwrap();
            let attempt = saved(&ctx, "stripe_setup_attempt").await.unwrap();
            assert_eq!(
                request
                    .post("/office/setup/webhook")
                    .add_header("cookie", &cookie)
                    .form(&[
                        ("secret", "whsec_pending_new"),
                        ("attempt", attempt.as_str())
                    ])
                    .await
                    .status_code(),
                303
            );
            assert_eq!(saved(&ctx, "webhook_tested_at").await.as_deref(), Some(""));
            fake.lock().unwrap().delete_refused = true;
            release.notify_one();
        };
        let (late, ()) = tokio::join!(slow, manual);
        assert_eq!(late, "/office/setup?step=webhook&error=webhook_duplicate");
        let response = request
            .get(late.as_str())
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(response.status_code(), 200);
        let page = response.text();
        if let Ok(directory) = std::env::var("CG017_HTTP_EVIDENCE_DIR") {
            std::fs::write(
                std::path::Path::new(&directory).join("delayed-cleanup-manual.html"),
                &page,
            )
            .unwrap();
        }
        assert_eq!(
            saved(&ctx, "webhook_secret").await.as_deref(),
            Some("whsec_pending_new")
        );
        assert_eq!(saved(&ctx, "webhook_tested_at").await.as_deref(), Some(""));
        assert_eq!(
            saved(&ctx, "webhook_endpoint_id").await.as_deref(),
            Some("")
        );
        assert_eq!(
            saved(&ctx, "stripe_secret_key").await.as_deref(),
            Some("sk_test_slowcleanup")
        );
        assert!(page.contains("Secret saved. Now send a test event from Stripe."));
        assert!(!page.contains("Your webhook is ready."));
        assert!(!page.contains("The webhook works,"));
        assert!(
            page.contains("Your current webhook is pending verification. Check older endpoints.")
        );
        assert!(
            page.contains("Send a test event from Stripe to verify your current signing secret.")
        );
        assert!(page.contains("Open the <b>Webhooks</b> page in Stripe and delete the older one"));
        assert!(page.contains("role=\"alert\""));
        assert!(!page.contains("<b>Working.</b>"));
    })
    .await;
}
