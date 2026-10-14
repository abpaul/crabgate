use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use crabgate::{
    admin_orders,
    app::App,
    crypto::MasterKey,
    events::{self, EventInput, TicketTypeInput},
    members::{self, Role},
    settings,
};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde_json::json;
use serial_test::serial;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Provider {
    history: Vec<serde_json::Value>,
    read_fail: bool,
    history_reads: usize,
    stall: bool,
    cache: bool,
    cached: std::collections::HashMap<String, serde_json::Value>,
    pending: bool,
    fail: bool,
    keys: Vec<String>,
    refunds: std::collections::HashSet<String>,
    delay: u64,
}

async fn refund(
    State(provider): State<Arc<Mutex<Provider>>>,
    headers: HeaderMap,
) -> (StatusCode, Json<serde_json::Value>) {
    let (stall, delay) = {
        let state = provider.lock().unwrap();
        (state.stall, state.delay)
    };
    tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
    if stall {
        {
            let mut state = provider.lock().unwrap();
            let key = headers["idempotency-key"].to_str().unwrap().to_owned();
            state.keys.push(key.clone());
            state.refunds.insert(key);
            state.history.insert(0, json!({"id":"re_stalled", "status":"succeeded", "amount":2000, "payment_intent":"pi_cg005"}));
        }
        std::future::pending::<()>().await;
    }
    let mut state = provider.lock().unwrap();
    state
        .keys
        .push(headers["idempotency-key"].to_str().unwrap().to_owned());
    if state.fail {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"synthetic failure"})),
        )
    } else {
        let key = state.keys.last().unwrap().clone();
        if state.cache {
            if let Some(body) = state.cached.get(&key) {
                return (StatusCode::OK, Json(body.clone()));
            }
        }
        if !state.pending {
            state.refunds.insert(key.clone());
        }
        let body = json!({"id": if state.cache { format!("re_cg005_{}", state.keys.len()) } else { "re_cg005".into() }, "status": if state.pending { "pending" } else { "succeeded" }});
        state.history.insert(0, json!({"id":body["id"], "status":body["status"], "amount":2000, "payment_intent":"pi_cg005"}));
        if state.cache {
            state.cached.insert(key, body.clone());
        }
        (StatusCode::OK, Json(body))
    }
}

async fn refund_history(
    State(provider): State<Arc<Mutex<Provider>>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut state = provider.lock().unwrap();
    state.history_reads += 1;
    if state.read_fail {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"synthetic read uncertainty"})),
        );
    }
    (
        StatusCode::OK,
        Json(json!({"data": state.history, "has_more":false})),
    )
}

async fn retrieve_refund(
    State(provider): State<Arc<Mutex<Provider>>>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    Json(
        provider
            .lock()
            .unwrap()
            .history
            .iter()
            .find(|r| r["id"] == id)
            .unwrap()
            .clone(),
    )
}

#[tokio::test]
#[serial]
async fn cancellation_continues_after_stripe_failure_and_can_retry() {
    let provider = Arc::new(Mutex::new(Provider {
        fail: true,
        ..Default::default()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/refunds", post(refund).get(refund_history))
        .route("/v1/refunds/{id}", get(retrieve_refund))
        .with_state(provider.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var("STRIPE_API_BASE", base);
    request::<App, _, _>(|request, ctx| async move {
        let Fixture {
            registry,
            db,
            cipher,
            cookie,
            event,
            paid,
            free,
            ..
        } = fixture(&ctx).await;
        settings::set(&db, &cipher, "stripe_secret_key", "sk_test_cg005_synthetic")
            .await
            .unwrap();
        assert_eq!(
            request
                .post(&format!("/office/orders/{paid}/refund"))
                .add_header("cookie", &cookie)
                .await
                .status_code(),
            303
        );
        assert_eq!(
            admin_orders::work_progress(&db, &paid)
                .await
                .unwrap()
                .refund,
            "Failed"
        );
        assert_eq!(
            events::get(&db, &event.id).await.unwrap().unwrap().status,
            "Published"
        );
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 0);
        let response = request
            .post(&format!("/office/events/{}/cancel", event.id))
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(response.status_code(), 303);
        assert_eq!(
            events::get(&db, &event.id).await.unwrap().unwrap().status,
            "Cancelled"
        );
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 0);
        assert_eq!(provider.lock().unwrap().keys.len(), 1);
        let queued = admin_orders::work_progress(&db, &free).await.unwrap();
        assert_eq!(queued.refund, "Pending");
        assert_eq!(queued.cancellation_notice, "Pending");
        assert_eq!(
            admin_orders::order(&db, &cipher, &free)
                .await
                .unwrap()
                .status,
            "Paid"
        );
        crabgate::controllers::admin_orders::run_refund_work(&ctx, &registry)
            .await
            .unwrap();
        assert_eq!(
            admin_orders::order(&db, &cipher, &free)
                .await
                .unwrap()
                .status,
            "Refunded",
            "a Stripe failure must not abandon later free orders"
        );
        let page = request
            .get(&format!("/office/orders/{paid}"))
            .add_header("cookie", &cookie)
            .await;
        assert!(page.text().contains("Refund: Failed"));
        assert!(page.text().contains("Cancellation notice: Completed"));
        std::fs::write(super::evidence_path("cg005-order.html"), page.text()).unwrap();
        let event_page = request
            .get(&format!("/office/events/{}", event.id))
            .add_header("cookie", &cookie)
            .await;
        assert!(event_page
            .text()
            .contains("Retry pending or failed cancellation work"));
        std::fs::write(super::evidence_path("cg005-event.html"), event_page.text()).unwrap();
        assert!(crabgate::inventory::reserve(
            &db,
            &event.id,
            &[crabgate::inventory::Selection {
                ticket_type_id: admin_orders::order(&db, &cipher, &free)
                    .await
                    .unwrap()
                    .tickets[0]
                    .ticket_type_id
                    .clone(),
                quantity: 1
            }],
            Utc::now().timestamp()
        )
        .await
        .is_err());
        let messages = ctx.mailer.as_ref().unwrap().deliveries();
        assert_eq!(messages.count, 2);
        assert!(messages
            .messages
            .iter()
            .any(|m| m.contains("refund is not complete")));
        assert!(!messages
            .messages
            .iter()
            .any(|m| m.contains("was refunded in full")));
        provider.lock().unwrap().fail = false;
        let route = format!("/office/events/{}/cancel", event.id);
        let (first, second) = tokio::join!(
            async { request.post(&route).add_header("cookie", &cookie).await },
            async { request.post(&route).add_header("cookie", &cookie).await }
        );
        assert_eq!(first.status_code(), 303);
        assert_eq!(second.status_code(), 303);
        assert_eq!(provider.lock().unwrap().keys.len(), 2);
        assert_eq!(
            admin_orders::work_progress(&db, &paid)
                .await
                .unwrap()
                .refund,
            "Failed"
        );
        let (first, second) = tokio::join!(
            crabgate::controllers::admin_orders::run_refund_work(&ctx, &registry),
            crabgate::controllers::admin_orders::run_refund_work(&ctx, &registry)
        );
        first.unwrap();
        second.unwrap();
        crabgate::controllers::admin_orders::run_refund_work(&ctx, &registry)
            .await
            .unwrap();
        assert_eq!(
            admin_orders::order(&db, &cipher, &paid)
                .await
                .unwrap()
                .status,
            "Refunded"
        );
        assert_eq!(
            admin_orders::order(&db, &cipher, &paid)
                .await
                .unwrap()
                .tickets[0]
                .status,
            "Void"
        );
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 3);
        assert_eq!(
            request
                .post(&format!("/office/events/{}/cancel", event.id))
                .add_header("cookie", &cookie)
                .await
                .status_code(),
            303
        );
        crabgate::controllers::admin_orders::run_refund_work(&ctx, &registry)
            .await
            .unwrap();
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 3);
        assert_eq!(
            provider.lock().unwrap().keys,
            vec![
                format!("order-refund-{paid}"),
                format!("order-refund-{paid}"),
                format!("order-refund-{paid}")
            ]
        );
        assert_eq!(
            events::get(&db, &event.id).await.unwrap().unwrap().status,
            "Cancelled"
        );
        assert_eq!(provider.lock().unwrap().refunds.len(), 1);
    })
    .await;
    server.abort();
    if let Some(value) = prior {
        std::env::set_var("STRIPE_API_BASE", value);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }
}

#[tokio::test]
#[serial]
async fn cancellation_notice_does_not_claim_refund_completion() {
    request::<App, _, _>(|_, ctx| async move {
        crabgate::mailers::refund::RefundMailer::send(
            &ctx,
            "synthetic@example.com",
            "Test",
            "CG-005",
            "Cancelled Test",
            true,
            None,
        )
        .await
        .unwrap();
        let messages = ctx.mailer.as_ref().unwrap().deliveries();
        assert!(!messages.messages[0].contains("was refunded in full"));
    })
    .await;
}

struct Fixture {
    registry: crabgate::organizers::OrganizerRegistry,
    organizer: crabgate::organizers::Organizer,
    db: sea_orm::DatabaseConnection,
    cipher: crabgate::crypto::OrganizerCipher,
    cookie: String,
    event: crabgate::events::Event,
    paid: String,
    free: String,
}

async fn fixture(ctx: &loco_rs::app::AppContext) -> Fixture {
    let row = ctx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT id, db_path, data_key FROM organizers WHERE subdomain = 'local'",
        ))
        .await
        .unwrap()
        .unwrap();
    let organizer: String = row.try_get("", "id").unwrap();
    let path: String = row.try_get("", "db_path").unwrap();
    let wrapped: String = row.try_get("", "data_key").unwrap();
    let db = sea_orm::Database::connect(format!("sqlite://{path}?mode=rwc"))
        .await
        .unwrap();
    let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .unwrap()
        .open_organizer_key(&organizer, &wrapped)
        .unwrap();
    let now = Utc::now().timestamp();
    members::add_member(&ctx.db, &organizer, "cg005@example.com", Role::Admin)
        .await
        .unwrap();
    let link = members::request_magic_link(&ctx.db, &organizer, "cg005@example.com", now)
        .await
        .unwrap()
        .unwrap();
    let session = members::claim_magic_link(&ctx.db, &organizer, &link, now)
        .await
        .unwrap()
        .unwrap();
    let cookie = format!("cg_session={session}");
    let event = events::create(
        &db,
        &EventInput {
            title: "Cancellation Test".into(),
            date: "2027-06-15".into(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Hall".into(),
            address: "Main Street".into(),
            description: String::new(),
            event_capacity: Some(10),
            max_per_order: 10,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let kind = events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "Standard".into(),
            price: "20".into(),
            capacity: 10,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(&db, &event.id, now).await.unwrap();
    let paid = admin_orders::complimentary(
        &db,
        &cipher,
        &event.id,
        &kind.id,
        "paid@example.com",
        "Paid",
        now,
    )
    .await
    .unwrap();
    let free = admin_orders::complimentary(
        &db,
        &cipher,
        &event.id,
        &kind.id,
        "free@example.com",
        "Free",
        now + 1,
    )
    .await
    .unwrap();
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "UPDATE orders SET total_cents = 2000, is_complimentary = 0, stripe_payment_intent_id = 'pi_cg005' WHERE id = ?", [paid.clone().into()])).await.unwrap();

    let registry = crabgate::organizers::OrganizerRegistry::new(
        ctx.db.clone(),
        ctx.config.database.clone(),
        crabgate::organizers::Edition::SelfHosted,
        "crabgate.com".into(),
        std::env::var("CRABGATE_ORGANIZER_DATABASE_DIR")
            .unwrap()
            .into(),
        MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap(),
    )
    .unwrap();
    let scope = registry.active_dbs().await.unwrap().remove(0);
    Fixture {
        registry,
        organizer: scope.organizer,
        db,
        cipher,
        cookie,
        event,
        paid,
        free,
    }
}

#[tokio::test]
#[serial]
async fn pending_refund_webhook_completes_work_once_after_cancellation() {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        settings::set(&f.db, &f.cipher, "webhook_secret", "whsec_cg005").await.unwrap();
        admin_orders::cancel_event(&f.db, &f.event.id, Utc::now().timestamp()).await.unwrap();
        let provider = Arc::new(Mutex::new(Provider { pending: true, ..Default::default() }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let prior = std::env::var("STRIPE_API_BASE").ok();
        std::env::set_var("STRIPE_API_BASE", format!("http://{}", listener.local_addr().unwrap()));
        let client = crabgate::stripe::StripeClient::from_secret("sk_test_synthetic").unwrap();
        let fake = provider.clone();
        let server = tokio::spawn(async move { axum::serve(listener, Router::new().route("/v1/refunds", post(refund).get(refund_history))
        .route("/v1/refunds/{id}", get(retrieve_refund)).with_state(fake)).await.unwrap() });
        admin_orders::refund(&f.db, &f.paid, Some(&client), Utc::now().timestamp()).await.unwrap();
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund, "Pending");
        assert_eq!(admin_orders::order(&f.db, &f.cipher, &f.paid).await.unwrap().tickets[0].status, "Valid");
        crabgate::controllers::admin_orders::process_order_work(&ctx, &f.registry, &f.organizer, &f.db, &f.paid, false).await.unwrap();
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().cancellation_notice, "Completed");
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 1);
        settings::set(&f.db, &f.cipher, "stripe_secret_key", "sk_test_synthetic").await.unwrap();
        for status in ["pending", "failed", "succeeded", "succeeded"] {
            provider.lock().unwrap().history[0]["status"] = json!(status);
            let body = json!({"type":"refund.updated", "data":{"object":{"id":"re_cg005", "payment_intent":"pi_cg005", "amount":2000, "status":status}}}).to_string();
            let now = Utc::now().timestamp();
            let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec_cg005").unwrap();
            mac.update(format!("{now}.{body}").as_bytes());
            let signature = format!("t={now},v1={}", hex::encode(mac.finalize().into_bytes()));
            assert_eq!(request.post("/webhooks/stripe").text(&body).add_header("Stripe-Signature", &signature).await.status_code(), 200);
            if status != "succeeded" {
                assert_eq!(admin_orders::order(&f.db, &f.cipher, &f.paid).await.unwrap().status, "RefundPending");
                assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund, if status == "failed" { "Failed" } else { "Pending" });
                assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 1);
            }
        }
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund, "Completed");
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund_notice, "Completed");
        assert_eq!(admin_orders::order(&f.db, &f.cipher, &f.paid).await.unwrap().tickets[0].status, "Void");
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 2);
        server.abort();
        if let Some(value) = prior { std::env::set_var("STRIPE_API_BASE", value); } else { std::env::remove_var("STRIPE_API_BASE"); }
    }).await;
}

#[tokio::test]
#[serial]
async fn smtp_refusal_does_not_abandon_orders_and_http_retry_recovers() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = accepted.clone();
    let server = tokio::spawn(async move {
        let mut first = true;
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            if first {
                first = false;
                let mut socket = socket;
                socket
                    .write_all(b"421 synthetic refusal\r\n")
                    .await
                    .unwrap();
                continue;
            }
            let captured = seen.clone();
            tokio::spawn(async move {
                let (read, mut write) = socket.into_split();
                let mut reader = BufReader::new(read);
                write.write_all(b"220 synthetic SMTP\r\n").await.unwrap();
                let mut data = false;
                let mut message = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap() == 0 {
                        break;
                    }
                    if data {
                        if line == ".\r\n" {
                            captured.lock().unwrap().push(message.clone());
                            data = false;
                            write.write_all(b"250 accepted\r\n").await.unwrap();
                        } else {
                            message.push_str(&line);
                        }
                    } else if line.starts_with("DATA") {
                        data = true;
                        write.write_all(b"354 continue\r\n").await.unwrap();
                    } else if line.starts_with("QUIT") {
                        write.write_all(b"221 bye\r\n").await.unwrap();
                        break;
                    } else {
                        write.write_all(b"250 OK\r\n").await.unwrap();
                    }
                }
            });
        }
    });
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        f.db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE orders SET total_cents = 0 WHERE id = ?",
            [f.paid.clone().into()],
        ))
        .await
        .unwrap();
        admin_orders::cancel_event(&f.db, &f.event.id, Utc::now().timestamp())
            .await
            .unwrap();
        let mut smtp_ctx = ctx.clone();
        let mut config = smtp_ctx
            .config
            .mailer
            .as_ref()
            .unwrap()
            .smtp
            .as_ref()
            .unwrap()
            .clone();
        config.host = "127.0.0.1".into();
        config.port = port;
        smtp_ctx.mailer = Some(loco_rs::mailer::EmailSender::smtp(&config).unwrap());
        crabgate::controllers::admin_orders::run_refund_work(&smtp_ctx, &f.registry)
            .await
            .unwrap();
        assert_eq!(
            admin_orders::work_progress(&f.db, &f.paid)
                .await
                .unwrap()
                .cancellation_notice,
            "Failed"
        );
        assert_eq!(
            admin_orders::work_progress(&f.db, &f.free)
                .await
                .unwrap()
                .cancellation_notice,
            "Completed"
        );
        assert_eq!(
            admin_orders::order(&f.db, &f.cipher, &f.paid)
                .await
                .unwrap()
                .status,
            "Refunded"
        );
        assert_eq!(accepted.lock().unwrap().len(), 1);
        assert!(!admin_orders::notice_sent(&f.db, &f.paid, true)
            .await
            .unwrap());
        assert_eq!(
            request
                .post(&format!("/office/orders/{}/refund", f.paid))
                .add_header("cookie", &f.cookie)
                .await
                .status_code(),
            303
        );
        assert_eq!(
            admin_orders::work_progress(&f.db, &f.paid)
                .await
                .unwrap()
                .cancellation_notice,
            "Completed"
        );
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 1);
        crabgate::controllers::admin_orders::run_refund_work(&smtp_ctx, &f.registry)
            .await
            .unwrap();
        assert_eq!(accepted.lock().unwrap().len(), 1);
        assert_eq!(
            events::get(&f.db, &f.event.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "Cancelled"
        );
    })
    .await;
    server.abort();
}

#[tokio::test]
#[serial]
async fn provider_success_before_status_write_recovers_with_same_key() {
    let provider = Arc::new(Mutex::new(Provider::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let client = crabgate::stripe::StripeClient::from_secret("sk_test_synthetic").unwrap();
    let fake = provider.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/refunds", post(refund).get(refund_history))
                .route("/v1/refunds/{id}", get(retrieve_refund))
                .with_state(fake),
        )
        .await
        .unwrap()
    });
    request::<App, _, _>(|_, ctx| async move {
        let f = fixture(&ctx).await;
        let now = Utc::now().timestamp();
        f.db.execute_unprepared("CREATE TRIGGER interrupt_refund BEFORE UPDATE OF status ON orders WHEN NEW.status = 'Refunded' BEGIN SELECT RAISE(ABORT, 'synthetic interruption'); END;").await.unwrap();
        assert!(admin_orders::refund(&f.db, &f.paid, Some(&client), now).await.is_err());
        assert_eq!(provider.lock().unwrap().refunds.len(), 1);
        let saved=f.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"SELECT stripe_refund_id FROM orders WHERE id = ?",[f.paid.clone().into()])).await.unwrap().unwrap();
        assert_eq!(saved.try_get::<String>("","stripe_refund_id").unwrap(),"re_cg005");
        provider.lock().unwrap().cached.clear();
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund, "Pending");
        assert_eq!(admin_orders::order(&f.db, &f.cipher, &f.paid).await.unwrap().status, "RefundPending");
        f.db.execute_unprepared("DROP TRIGGER interrupt_refund").await.unwrap();
        admin_orders::refund(&f.db, &f.paid, Some(&client), now+1).await.unwrap();
        assert_eq!(provider.lock().unwrap().keys.len(), 1);
        admin_orders::refund(&f.db, &f.paid, Some(&client), now+300).await.unwrap();
        assert_eq!(provider.lock().unwrap().keys, vec![format!("order-refund-{}", f.paid)]);
        assert_eq!(provider.lock().unwrap().refunds.len(), 1);
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund, "Completed");
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund_notice, "Pending");
        assert_eq!(admin_orders::order(&f.db, &f.cipher, &f.paid).await.unwrap().tickets[0].status, "Void");
        crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry).await.unwrap();
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund_notice, "Completed");
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 1);
        assert_eq!(events::get(&f.db, &f.event.id).await.unwrap().unwrap().status, "Published");
    }).await;
    server.abort();
    if let Some(value) = prior {
        std::env::set_var("STRIPE_API_BASE", value);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }
}

#[tokio::test]
#[serial]
async fn confirmed_failed_stripe_refund_uses_one_new_key_and_ignores_old_failure() {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let provider = Arc::new(Mutex::new(Provider {
        cache: true,
        pending: true,
        ..Default::default()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let fake = provider.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/refunds", post(refund).get(refund_history))
                .route("/v1/refunds/{id}", get(retrieve_refund))
                .with_state(fake),
        )
        .await
        .unwrap()
    });
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        settings::set(&f.db, &f.cipher, "stripe_secret_key", "sk_test_synthetic").await.unwrap();
        settings::set(&f.db, &f.cipher, "webhook_secret", "whsec_cg005").await.unwrap();
        assert_eq!(request.post(&format!("/office/events/{}/cancel", f.event.id)).add_header("cookie", &f.cookie).await.status_code(), 303);
        assert_eq!(provider.lock().unwrap().keys.len(), 0);
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund, "Pending");
        crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry).await.unwrap();
        assert_eq!(provider.lock().unwrap().keys.len(), 1);
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund, "Pending");
        provider.lock().unwrap().history[0]["status"] = json!("failed");
        let body = json!({"type":"refund.updated", "data":{"object":{"id":"re_cg005_1", "payment_intent":"pi_cg005", "amount":2000, "status":"failed"}}}).to_string();
        let now = Utc::now().timestamp();
        let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec_cg005").unwrap();
        mac.update(format!("{now}.{body}").as_bytes());
        let signature = format!("t={now},v1={}", hex::encode(mac.finalize().into_bytes()));
        assert_eq!(request.post("/webhooks/stripe").text(&body).add_header("Stripe-Signature", &signature).await.status_code(), 200);
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund, "Failed");
        provider.lock().unwrap().pending = false;
        assert_eq!(request.post(&format!("/office/orders/{}/refund", f.paid)).add_header("cookie", &f.cookie).await.status_code(), 303);
        assert_eq!(provider.lock().unwrap().keys, vec![format!("order-refund-{}", f.paid), format!("order-refund-{}-after-re_cg005_1", f.paid)]);
        assert_eq!(provider.lock().unwrap().refunds.len(), 1);
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund, "Completed");
        assert_eq!(request.post("/webhooks/stripe").text(&body).add_header("Stripe-Signature", &signature).await.status_code(), 200);
        assert_eq!(request.post(&format!("/office/events/{}/cancel", f.event.id)).add_header("cookie", &f.cookie).await.status_code(), 303);
        crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry).await.unwrap();
        assert_eq!(provider.lock().unwrap().keys.len(), 2);
        assert_eq!(provider.lock().unwrap().refunds.len(), 1);
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 3);
        assert_eq!(admin_orders::order(&f.db, &f.cipher, &f.paid).await.unwrap().tickets[0].status, "Void");
        assert_eq!(events::get(&f.db, &f.event.id).await.unwrap().unwrap().status, "Cancelled");
    }).await;
    server.abort();
    if let Some(value) = prior {
        std::env::set_var("STRIPE_API_BASE", value);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }
}

#[tokio::test]
#[serial]
async fn smtp_acceptance_before_status_write_stays_uncertain_without_duplicate_send() {
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        f.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE orders SET total_cents = 0 WHERE id = ?", [f.paid.clone().into()])).await.unwrap();
        f.db.execute_unprepared("CREATE TRIGGER interrupt_notice BEFORE UPDATE OF cancel_notice_sent_at ON orders WHEN NEW.cancel_notice_sent_at IS NOT NULL BEGIN SELECT RAISE(ABORT, 'synthetic interruption'); END;").await.unwrap();
        let route = format!("/office/events/{}/cancel", f.event.id);
        assert_eq!(request.post(&route).add_header("cookie", &f.cookie).await.status_code(), 303);
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 0);
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().cancellation_notice, "Pending");
        crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry).await.unwrap();
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 2);
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().cancellation_notice, "Sending");
        assert_eq!(admin_orders::work_progress(&f.db, &f.free).await.unwrap().cancellation_notice, "Sending");
        assert!(!admin_orders::notice_sent(&f.db, &f.paid, true).await.unwrap());
        f.db.execute_unprepared("DROP TRIGGER interrupt_notice").await.unwrap();
        assert_eq!(request.post(&route).add_header("cookie", &f.cookie).await.status_code(), 303);
        crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry).await.unwrap();
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 2);
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().cancellation_notice, "Sending");
        assert_eq!(events::get(&f.db, &f.event.id).await.unwrap().unwrap().status, "Cancelled");
        assert_eq!(admin_orders::order(&f.db, &f.cipher, &f.paid).await.unwrap().tickets[0].status, "Void");
        f.db.execute_unprepared("UPDATE orders SET cancel_notice_state_claimed_at = unixepoch() - 301").await.unwrap();
        let recovered=request.post(&format!("/office/orders/{}/notice-reconcile",f.paid)).add_header("cookie",&f.cookie).form(&json!({"notice":"cancellation","outcome":"accepted","confirmed":"yes"})).await;
        assert_eq!(recovered.status_code(),303);
        assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().cancellation_notice,"Completed");
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count,2);
    }).await;
}

#[tokio::test]
#[serial]
async fn correction_notice_crash_recovery_has_confirmed_http_actions() {
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        admin_orders::cancel_event(&f.db, &f.event.id, Utc::now().timestamp())
            .await
            .unwrap();
        assert!(admin_orders::claim_notice(&f.db, &f.paid, true)
            .await
            .unwrap());
        let page = request
            .get(&format!("/office/orders/{}", f.paid))
            .add_header("cookie", &f.cookie)
            .await;
        assert!(page.text().contains("Record confirmed acceptance"));
        std::fs::write(super::evidence_path("cg005-correction-uncertain.html"),page.text()).unwrap();
        let response = request
            .post(&format!("/office/orders/{}/notice-reconcile", f.paid))
            .add_header("cookie", &f.cookie)
            .form(&json!({"notice":"cancellation", "outcome":"retry", "confirmed":"yes"}))
            .await;
        assert_eq!(
            response.status_code(),
            409,
            "an in-flight send cannot be reset"
        );
        let route = format!("/office/orders/{}/notice-reconcile", f.paid);
        assert_eq!(request.post(&route).add_header("cookie", &f.cookie).form(&json!({"notice":"cancellation", "outcome":"accepted"})).await.status_code(), 400);
        f.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE orders SET cancel_notice_state_claimed_at = unixepoch() - 301 WHERE id = ?", [f.paid.clone().into()])).await.unwrap();
        assert_eq!(request.post(&route).add_header("cookie", &f.cookie).form(&json!({"notice":"cancellation", "outcome":"retry", "confirmed":"yes"})).await.status_code(), 303);
        assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().cancellation_notice,"Completed");
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count,1);
        assert_eq!(request.post(&route).add_header("cookie", &f.cookie).form(&json!({"notice":"cancellation", "outcome":"retry", "confirmed":"yes"})).await.status_code(),409);
        f.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE orders SET cancel_notice_state = 'Uncertain', cancel_notice_sent_at = 1 WHERE id = ?", [f.free.clone().into()])).await.unwrap();
        assert_eq!(request.post(&format!("/office/orders/{}/notice-reconcile",f.free)).add_header("cookie", &f.cookie).form(&json!({"notice":"cancellation", "outcome":"accepted", "confirmed":"yes"})).await.status_code(),303);
        assert_eq!(admin_orders::work_progress(&f.db,&f.free).await.unwrap().cancellation_notice,"Completed");
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count,1);
        let page=request.get(&format!("/office/orders/{}", f.paid)).add_header("cookie", &f.cookie).await;
        std::fs::write(super::evidence_path("cg005-correction-order.html"),page.text()).unwrap();
    })
    .await;
}

#[tokio::test]
#[serial]
async fn correction_uncertain_refund_uses_history_after_cache_expiry() {
    let provider = Arc::new(Mutex::new(Provider {
        history: vec![
            json!({"id":"re_already_paid", "status":"succeeded", "amount":2000, "payment_intent":"pi_cg005"}),
        ],
        fail: true,
        ..Default::default()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let client = crabgate::stripe::StripeClient::from_secret("sk_test_synthetic").unwrap();
    let fake = provider.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/refunds", post(refund).get(refund_history))
                .route("/v1/refunds/{id}", get(retrieve_refund))
                .with_state(fake),
        )
        .await
        .unwrap()
    });
    request::<App, _, _>(|_, ctx| async move {
        let f = fixture(&ctx).await;
        assert!(
            admin_orders::refund(&f.db, &f.paid, Some(&client), Utc::now().timestamp())
                .await
                .is_ok()
        );
        assert_eq!(
            admin_orders::work_progress(&f.db, &f.paid)
                .await
                .unwrap()
                .refund,
            "Completed"
        );
        assert_eq!(provider.lock().unwrap().keys.len(), 0);
    })
    .await;
    server.abort();
    std::env::remove_var("STRIPE_API_BASE");
}

#[tokio::test]
#[serial]
async fn correction_old_pending_payload_cannot_change_saved_attempt() {
    request::<App, _, _>(|_, ctx| async move {
        let f = fixture(&ctx).await;
        for id in ["re_one", "re_two"] {
            admin_orders::record_refund_provider_status(&f.db, &f.paid, id, false)
                .await
                .unwrap();
            admin_orders::record_refund_provider_status(&f.db, &f.paid, id, true)
                .await
                .unwrap();
        }
        admin_orders::record_refund_provider_status(&f.db, &f.paid, "re_one", false)
            .await
            .unwrap();
        assert_eq!(
            admin_orders::work_progress(&f.db, &f.paid)
                .await
                .unwrap()
                .refund,
            "Failed"
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn correction_stalled_provider_is_bounded_and_next_order_runs() {
    let provider = Arc::new(Mutex::new(Provider {
        stall: true,
        ..Default::default()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let fake = provider.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/refunds", post(refund).get(refund_history))
                .with_state(fake),
        )
        .await
        .unwrap()
    });
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        settings::set(&f.db, &f.cipher, "stripe_secret_key", "sk_test_synthetic")
            .await
            .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(7),
            request
                .post(&format!("/office/events/{}/cancel", f.event.id))
                .add_header("cookie", &f.cookie),
        )
        .await;
        assert!(result.is_ok(), "cancellation must finish within 7 seconds");
        assert_eq!(result.unwrap().status_code(), 303);
        assert_eq!(provider.lock().unwrap().keys.len(), 0);
        assert_eq!(
            admin_orders::work_progress(&f.db, &f.paid)
                .await
                .unwrap()
                .refund,
            "Pending"
        );
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(13),
            crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry),
        )
        .await;
        let elapsed = started.elapsed();
        assert!(
            result.is_ok(),
            "provider timeout must let the refund job finish within 13 seconds"
        );
        assert!(elapsed >= std::time::Duration::from_secs(10), "{elapsed:?}");
        println!("accepted-without-response refund job elapsed: {elapsed:?}");
        result.unwrap().unwrap();
        let progress = admin_orders::work_progress(&f.db, &f.paid).await.unwrap();
        assert_eq!(progress.refund, "Failed");
        assert!(!progress.refund_claimed);
        assert_eq!(
            admin_orders::order(&f.db, &f.cipher, &f.free)
                .await
                .unwrap()
                .status,
            "Refunded"
        );
        assert_eq!(provider.lock().unwrap().keys.len(), 1);
        provider.lock().unwrap().stall = false;
        crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry)
            .await
            .unwrap();
        assert_eq!(
            admin_orders::work_progress(&f.db, &f.paid)
                .await
                .unwrap()
                .refund,
            "Completed"
        );
        assert_eq!(provider.lock().unwrap().keys.len(), 1);
        assert_eq!(provider.lock().unwrap().refunds.len(), 1);
    })
    .await;
    server.abort();
    std::env::remove_var("STRIPE_API_BASE");
}

#[tokio::test]
#[serial]
async fn cg065_refund_answer_after_six_seconds_completes_in_one_attempt() {
    let provider = Arc::new(Mutex::new(Provider {
        delay: 6,
        ..Default::default()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let fake = provider.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/refunds", post(refund).get(refund_history))
                .with_state(fake),
        )
        .await
        .unwrap()
    });
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        settings::set(&f.db, &f.cipher, "stripe_secret_key", "sk_test_synthetic")
            .await
            .unwrap();
        let cancelled = request
            .post(&format!("/office/events/{}/cancel", f.event.id))
            .add_header("cookie", &f.cookie)
            .await;
        assert_eq!(cancelled.status_code(), 303);
        let started = std::time::Instant::now();
        crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry)
            .await
            .unwrap();
        println!(
            "refund answer after 6 s: job elapsed {:?}",
            started.elapsed()
        );
        let progress = admin_orders::work_progress(&f.db, &f.paid).await.unwrap();
        assert_eq!(progress.refund, "Completed");
        assert!(!progress.refund_claimed);
        assert_eq!(
            admin_orders::order(&f.db, &f.cipher, &f.paid)
                .await
                .unwrap()
                .status,
            "Refunded"
        );
        assert_eq!(
            provider.lock().unwrap().keys,
            vec![format!("order-refund-{}", f.paid)]
        );
        assert_eq!(provider.lock().unwrap().refunds.len(), 1);
    })
    .await;
    server.abort();
    std::env::remove_var("STRIPE_API_BASE");
}

#[tokio::test]
#[serial]
async fn correction_failed_refund_dispatch_retains_invalid_admission() {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let provider = Arc::new(Mutex::new(Provider {
        history: vec![
            json!({"id":"re_returned", "status":"failed", "amount":2000, "payment_intent":"pi_cg005"}),
        ],
        ..Default::default()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/refunds", post(refund).get(refund_history))
                .route("/v1/refunds/{id}", get(retrieve_refund))
                .with_state(provider),
        )
        .await
        .unwrap()
    });
    request::<App,_,_>(|request,ctx|async move {
        let f=fixture(&ctx).await;
        settings::set(&f.db,&f.cipher,"stripe_secret_key","sk_test_synthetic").await.unwrap();
        settings::set(&f.db,&f.cipher,"webhook_secret","whsec_cg005").await.unwrap();
        admin_orders::mark_refunded(&f.db,&f.paid,Utc::now().timestamp()).await.unwrap();
        let body=json!({"type":"refund.failed","data":{"object":{"id":"re_returned","payment_intent":"pi_cg005","amount":2000,"status":"failed"}}}).to_string();
        let now=Utc::now().timestamp();let mut mac=Hmac::<Sha256>::new_from_slice(b"whsec_cg005").unwrap();mac.update(format!("{now}.{body}").as_bytes());
        assert_eq!(request.post("/webhooks/stripe").text(&body).add_header("Stripe-Signature",&format!("t={now},v1={}",hex::encode(mac.finalize().into_bytes()))).await.status_code(),200);
        assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().refund,"Failed");
        assert_eq!(admin_orders::order(&f.db,&f.cipher,&f.paid).await.unwrap().status,"Refunded");
        assert_eq!(admin_orders::order(&f.db,&f.cipher,&f.paid).await.unwrap().tickets[0].status,"Void");
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count,0);
        admin_orders::cancel_event(&f.db,&f.event.id,Utc::now().timestamp()).await.unwrap();
        assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().refund,"Failed");
        assert_eq!(request.post(&format!("/office/orders/{}/refund",f.paid)).add_header("cookie",&f.cookie).await.status_code(),303);
        assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().refund,"Completed");
        assert_eq!(admin_orders::order(&f.db,&f.cipher,&f.paid).await.unwrap().status,"Refunded");
        assert_eq!(admin_orders::order(&f.db,&f.cipher,&f.paid).await.unwrap().tickets[0].status,"Void");

    }).await;
    server.abort();
    std::env::remove_var("STRIPE_API_BASE");
}

#[tokio::test]
#[serial]
async fn correction_broken_tenant_does_not_abandon_healthy_tenant() {
    request::<App, _, _>(|_, ctx| async move {
        let f = fixture(&ctx).await;
        let registry = crabgate::organizers::OrganizerRegistry::new(
            ctx.db.clone(),
            ctx.config.database.clone(),
            crabgate::organizers::Edition::Hosted,
            "crabgate.com".into(),
            std::env::var("CRABGATE_ORGANIZER_DATABASE_DIR")
                .unwrap()
                .into(),
            MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap(),
        )
        .unwrap();
        let broken = registry.create("Broken", "broken").await.unwrap();
        let broken_db = registry.resolve("broken.crabgate.com").await.unwrap().db;
        broken_db
            .execute_unprepared("ALTER TABLE orders RENAME TO unavailable_orders")
            .await
            .unwrap();
        ctx.db
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE organizers SET id = '000-broken' WHERE id = ?",
                [broken.id.into()],
            ))
            .await
            .unwrap();
        admin_orders::cancel_event(&f.db, &f.event.id, Utc::now().timestamp())
            .await
            .unwrap();
        assert!(
            crabgate::controllers::admin_orders::run_refund_work(&ctx, &registry)
                .await
                .is_ok()
        );
        assert_eq!(
            admin_orders::order(&f.db, &f.cipher, &f.free)
                .await
                .unwrap()
                .status,
            "Refunded"
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn correction_signed_replays_across_three_attempts_use_provider_truth() {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let provider = Arc::new(Mutex::new(Provider {
        cache: true,
        pending: true,
        ..Default::default()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let fake = provider.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/refunds", post(refund).get(refund_history))
                .route("/v1/refunds/{id}", get(retrieve_refund))
                .with_state(fake),
        )
        .await
        .unwrap()
    });
    request::<App,_,_>(|request,ctx|async move {
        let f=fixture(&ctx).await;
        settings::set(&f.db,&f.cipher,"stripe_secret_key","sk_test_synthetic").await.unwrap();
        settings::set(&f.db,&f.cipher,"webhook_secret","whsec_cg005").await.unwrap();
        let route=format!("/office/orders/{}/refund",f.paid);
        for attempt in 1..=3 {
            assert_eq!(request.post(&route).add_header("cookie",&f.cookie).await.status_code(),303);
            assert_eq!(provider.lock().unwrap().keys.len(),attempt);
            assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().refund,"Pending");
            for (payload_id,payload_status,truth) in [(format!("re_cg005_{attempt}"),"failed","failed"),("re_cg005_1".into(),"pending","failed"),("re_cg005_1".into(),"succeeded","failed")] {
                provider.lock().unwrap().history[0]["status"]=json!(truth);
                let body=json!({"type":"refund.updated","data":{"object":{"id":payload_id,"payment_intent":"pi_cg005","amount":2000,"status":payload_status}}}).to_string();
                let now=Utc::now().timestamp(); let mut mac=Hmac::<Sha256>::new_from_slice(b"whsec_cg005").unwrap();mac.update(format!("{now}.{body}").as_bytes());
                assert_eq!(request.post("/webhooks/stripe").text(&body).add_header("Stripe-Signature",&format!("t={now},v1={}",hex::encode(mac.finalize().into_bytes()))).await.status_code(),200);
                assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().refund,"Failed");
                let row=f.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"SELECT stripe_refund_id,refund_retry_after FROM orders WHERE id = ?",[f.paid.clone().into()])).await.unwrap().unwrap();
                assert_eq!(row.try_get::<String>("","stripe_refund_id").unwrap(),format!("re_cg005_{attempt}"));
                assert_eq!(row.try_get::<String>("","refund_retry_after").unwrap(),format!("re_cg005_{attempt}"));
            }
        }
        provider.lock().unwrap().pending=false;
        assert_eq!(request.post(&route).add_header("cookie",&f.cookie).await.status_code(),303);
        assert_eq!(provider.lock().unwrap().keys,vec![format!("order-refund-{}",f.paid),format!("order-refund-{}-after-re_cg005_1",f.paid),format!("order-refund-{}-after-re_cg005_2",f.paid),format!("order-refund-{}-after-re_cg005_3",f.paid)]);
        assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().refund,"Completed");
        let body=json!({"type":"refund.failed","data":{"object":{"id":"re_cg005_1","payment_intent":"pi_cg005","amount":2000,"status":"failed"}}}).to_string();
        let now=Utc::now().timestamp();let mut mac=Hmac::<Sha256>::new_from_slice(b"whsec_cg005").unwrap();mac.update(format!("{now}.{body}").as_bytes());
        assert_eq!(request.post("/webhooks/stripe").text(&body).add_header("Stripe-Signature",&format!("t={now},v1={}",hex::encode(mac.finalize().into_bytes()))).await.status_code(),200);
        assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().refund,"Completed");
        assert_eq!(provider.lock().unwrap().keys.len(),4);
        assert_eq!(provider.lock().unwrap().refunds.len(),1);
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count,1);
        assert_eq!(admin_orders::order(&f.db,&f.cipher,&f.paid).await.unwrap().tickets[0].status,"Void");
    }).await;
    server.abort();
    std::env::remove_var("STRIPE_API_BASE");
}

#[tokio::test]
#[serial]
async fn correction_provider_acceptance_before_identity_write_recovers_without_post() {
    let provider = Arc::new(Mutex::new(Provider::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let client = crabgate::stripe::StripeClient::from_secret("sk_test_synthetic").unwrap();
    let fake = provider.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/refunds", post(refund).get(refund_history))
                .with_state(fake),
        )
        .await
        .unwrap()
    });
    request::<App,_,_>(|_,ctx|async move {
        let f=fixture(&ctx).await;
        f.db.execute_unprepared("CREATE TRIGGER interrupt_identity BEFORE UPDATE OF stripe_refund_id ON orders BEGIN SELECT RAISE(ABORT,'synthetic identity write failure'); END").await.unwrap();
        assert!(admin_orders::refund(&f.db,&f.paid,Some(&client),Utc::now().timestamp()).await.is_err());
        let row=f.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"SELECT stripe_refund_id FROM orders WHERE id = ?",[f.paid.clone().into()])).await.unwrap().unwrap();
        assert_eq!(row.try_get::<Option<String>>("","stripe_refund_id").unwrap(),None);
        assert_eq!(provider.lock().unwrap().keys.len(),1);
        f.db.execute_unprepared("DROP TRIGGER interrupt_identity").await.unwrap();
        provider.lock().unwrap().fail=true;
        admin_orders::refund(&f.db,&f.paid,Some(&client),Utc::now().timestamp()+86401).await.unwrap();
        assert_eq!(provider.lock().unwrap().keys.len(),1);
        assert_eq!(provider.lock().unwrap().refunds.len(),1);
        assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().refund,"Completed");
        assert_eq!(admin_orders::order(&f.db,&f.cipher,&f.paid).await.unwrap().tickets[0].status,"Void");
    }).await;
    server.abort();
    std::env::remove_var("STRIPE_API_BASE");
}

struct HistoryGate {
    entered: tokio::sync::mpsc::UnboundedSender<usize>,
    permits: Vec<Arc<tokio::sync::Semaphore>>,
    replies: Vec<(StatusCode, serde_json::Value)>,
    calls: std::sync::atomic::AtomicUsize,
}

async fn gated_history(
    State(gate): State<Arc<HistoryGate>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let call = gate.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    gate.entered.send(call).unwrap();
    gate.permits[call].acquire().await.unwrap().forget();
    let (status, body) = &gate.replies[call];
    (*status, Json(body.clone()))
}

async fn independent_connection(f: &Fixture) -> sea_orm::DatabaseConnection {
    let row =
        f.db.query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA database_list",
        ))
        .await
        .unwrap()
        .unwrap();
    let path: String = row.try_get("", "file").unwrap();
    let mut options = sea_orm::ConnectOptions::new(format!("sqlite://{path}?mode=rwc"));
    options.max_connections(1);
    let second = sea_orm::Database::connect(options).await.unwrap();
    f.db.execute_unprepared("CREATE TEMP TABLE cg005_owner_probe (id INTEGER)")
        .await
        .unwrap();
    assert!(second
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT * FROM cg005_owner_probe"
        ))
        .await
        .is_err());
    f.db.execute_unprepared("DROP TABLE cg005_owner_probe")
        .await
        .unwrap();
    second
}

#[tokio::test]
#[serial]
async fn correction_r2_standalone_interruption_reopens_and_job_recovers() {
    let (tx, mut entered) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new(HistoryGate {
        entered: tx,
        permits: vec![Arc::new(tokio::sync::Semaphore::new(0))],
        replies: vec![(StatusCode::OK, json!({"data":[],"has_more":false}))],
        calls: 0.into(),
    });
    let provider = Arc::new(Mutex::new(Provider::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/refunds", get(gated_history))
                .with_state(gate),
        )
        .await
        .unwrap()
    });
    request::<App, _, _>(|_, ctx| async move {
        let f = fixture(&ctx).await;
        settings::set(&f.db, &f.cipher, "stripe_secret_key", "sk_test_synthetic")
            .await
            .unwrap();
        let client = crabgate::stripe::StripeClient::from_secret("sk_test_synthetic").unwrap();
        let db = f.db.clone();
        let id = f.paid.clone();
        let worker = tokio::spawn(async move {
            admin_orders::refund(&db, &id, Some(&client), Utc::now().timestamp()).await
        });
        assert_eq!(entered.recv().await, Some(0));
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        let reopened = independent_connection(&f).await;
        assert_eq!(
            admin_orders::work_progress(&reopened, &f.paid)
                .await
                .unwrap()
                .refund,
            "Pending"
        );
        assert_eq!(
            admin_orders::order(&reopened, &f.cipher, &f.paid)
                .await
                .unwrap()
                .tickets[0]
                .status,
            "Valid"
        );
        let row = reopened
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT stripe_refund_id FROM orders WHERE id=?",
                [f.paid.clone().into()],
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.try_get::<Option<String>>("", "stripe_refund_id")
                .unwrap(),
            None
        );
        reopened
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE orders SET refund_claim_until=0 WHERE id=?",
                [f.paid.clone().into()],
            ))
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::env::set_var(
            "STRIPE_API_BASE",
            format!("http://{}", listener.local_addr().unwrap()),
        );
        let fake = provider.clone();
        let recovery = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/refunds", post(refund).get(refund_history))
                    .with_state(fake),
            )
            .await
            .unwrap()
        });
        crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry)
            .await
            .unwrap();
        assert_eq!(
            admin_orders::work_progress(&reopened, &f.paid)
                .await
                .unwrap()
                .refund,
            "Completed"
        );
        assert_eq!(
            admin_orders::order(&reopened, &f.cipher, &f.paid)
                .await
                .unwrap()
                .tickets[0]
                .status,
            "Void"
        );
        assert_eq!(provider.lock().unwrap().keys.len(), 1);
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 1);
        recovery.abort();
    })
    .await;
    server.abort();
    std::env::remove_var("STRIPE_API_BASE");
}

#[tokio::test]
#[serial]
async fn correction_r2_stale_workers_and_configuration_cannot_clear_new_owner() {
    for stale_success in [false, true] {
        let (tx, mut entered) = tokio::sync::mpsc::unbounded_channel();
        let full = json!({"data":[{"id":"re_owner","amount":2000,"payment_intent":"pi_cg005","status":"succeeded"}],"has_more":false});
        let gate = Arc::new(HistoryGate {
            entered: tx,
            permits: vec![
                Arc::new(tokio::sync::Semaphore::new(0)),
                Arc::new(tokio::sync::Semaphore::new(0)),
            ],
            replies: vec![
                if stale_success {
                    (StatusCode::OK, full.clone())
                } else {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        json!({"error":"old worker failure"}),
                    )
                },
                (StatusCode::OK, full),
            ],
            calls: 0.into(),
        });
        let fake = gate.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::env::set_var(
            "STRIPE_API_BASE",
            format!("http://{}", listener.local_addr().unwrap()),
        );
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/refunds", get(gated_history))
                    .with_state(fake),
            )
            .await
            .unwrap()
        });
        request::<App, _, _>(|_, ctx| async move {
            let f = fixture(&ctx).await;
            let second = independent_connection(&f).await;
            let client = crabgate::stripe::StripeClient::from_secret("sk_test_synthetic").unwrap();
            let now = Utc::now().timestamp();
            let db = f.db.clone();
            let id = f.paid.clone();
            let first_client = client.clone();
            let first = tokio::spawn(async move {
                admin_orders::refund(&db, &id, Some(&first_client), now).await
            });
            assert_eq!(entered.recv().await, Some(0));
            let before =
                f.db.query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT refund_claim_until FROM orders WHERE id=?",
                    [f.paid.clone().into()],
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get::<Option<i64>>("", "refund_claim_until")
                .unwrap();
            assert!(admin_orders::refund(&second, &f.paid, None, now)
                .await
                .is_err());
            let after =
                f.db.query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT refund_claim_until FROM orders WHERE id=?",
                    [f.paid.clone().into()],
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get::<Option<i64>>("", "refund_claim_until")
                .unwrap();
            assert_eq!(
                after, before,
                "configuration failure clobbered active claim"
            );
            let original =
                f.db.query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT refund_claim_token FROM orders WHERE id=?",
                    [f.paid.clone().into()],
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get::<String>("", "refund_claim_token")
                .unwrap();
            second
                .execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "UPDATE orders SET refund_claim_until=0 WHERE id=?",
                    [f.paid.clone().into()],
                ))
                .await
                .unwrap();
            let db = second.clone();
            let id = f.paid.clone();
            let second_client = client.clone();
            let next = tokio::spawn(async move {
                admin_orders::refund(&db, &id, Some(&second_client), now).await
            });
            assert_eq!(entered.recv().await, Some(1));
            let replacement =
                f.db.query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT refund_claim_token, refund_claim_until FROM orders WHERE id=?",
                    [f.paid.clone().into()],
                ))
                .await
                .unwrap()
                .unwrap();
            assert_ne!(
                replacement
                    .try_get::<String>("", "refund_claim_token")
                    .unwrap(),
                original
            );
            assert_eq!(
                replacement
                    .try_get::<Option<i64>>("", "refund_claim_until")
                    .unwrap(),
                before
            );
            assert!(matches!(
                admin_orders::mark_refunded(&f.db, &f.paid, now).await,
                Err(admin_orders::AdminOrderError::ClaimLost)
            ));
            admin_orders::record_refund_provider_status(&f.db, &f.paid, "re_bypass", true)
                .await
                .unwrap();
            admin_orders::record_refund_failure(&f.db, &f.paid)
                .await
                .unwrap();
            gate.permits[0].add_permits(1);
            assert!(first.await.unwrap().is_err());
            assert_eq!(
                admin_orders::work_progress(&second, &f.paid)
                    .await
                    .unwrap()
                    .refund,
                "Pending"
            );
            assert_eq!(
                admin_orders::order(&second, &f.cipher, &f.paid)
                    .await
                    .unwrap()
                    .tickets[0]
                    .status,
                "Valid"
            );
            admin_orders::refund(&f.db, &f.paid, Some(&client), now)
                .await
                .unwrap();
            assert_eq!(
                gate.calls.load(std::sync::atomic::Ordering::SeqCst),
                2,
                "third worker acquired live claim"
            );
            gate.permits[1].add_permits(1);
            next.await.unwrap().unwrap();
            assert_eq!(
                admin_orders::work_progress(&f.db, &f.paid)
                    .await
                    .unwrap()
                    .refund,
                "Completed"
            );
            assert_eq!(
                admin_orders::order(&f.db, &f.cipher, &f.paid)
                    .await
                    .unwrap()
                    .tickets[0]
                    .status,
                "Void"
            );
        })
        .await;
        server.abort();
        std::env::remove_var("STRIPE_API_BASE");
    }
}

#[tokio::test]
#[serial]
async fn correction_r2_signed_partial_history_late_failure_suppresses_confirmation() {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let provider = Arc::new(Mutex::new(Provider {
        history: vec![
            json!({"id":"re_b","status":"succeeded","amount":1000,"payment_intent":"pi_cg005"}),
            json!({"id":"re_a","status":"succeeded","amount":1000,"payment_intent":"pi_cg005"}),
        ],
        ..Default::default()
    }));
    let fake = provider.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/refunds", post(refund).get(refund_history))
                .route("/v1/refunds/{id}", get(retrieve_refund))
                .with_state(fake),
        )
        .await
        .unwrap()
    });
    request::<App,_,_>(|request,ctx|async move {
        let f=fixture(&ctx).await;settings::set(&f.db,&f.cipher,"stripe_secret_key","sk_test_synthetic").await.unwrap();settings::set(&f.db,&f.cipher,"webhook_secret","whsec_cg005").await.unwrap();
        let client=crabgate::stripe::StripeClient::from_secret("sk_test_synthetic").unwrap();
        let ticket=admin_orders::order(&f.db,&f.cipher,&f.paid).await.unwrap().tickets.remove(0);
        let checked_at=Utc::now().timestamp();
        assert!(matches!(crabgate::door::check_in(&f.db,&f.cipher,&f.event.id,&ticket.code,"Member","synthetic",checked_at).await.unwrap(),crabgate::door::CheckinResult::CheckedIn(_)));
        admin_orders::reconcile_refund(&f.db,&f.paid,&client,checked_at).await.unwrap();
        assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().refund,"Completed");
        assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().refund_notice,"Pending");
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count,0);
        provider.lock().unwrap().history[0]["status"]=json!("failed");
        let body=json!({"type":"refund.failed","data":{"object":{"id":"re_b","amount":1000,"payment_intent":"pi_cg005","status":"failed"}}}).to_string();
        let now=Utc::now().timestamp();let mut mac=Hmac::<Sha256>::new_from_slice(b"whsec_cg005").unwrap();mac.update(format!("{now}.{body}").as_bytes());
        let signature=format!("t={now},v1={}",hex::encode(mac.finalize().into_bytes()));
        provider.lock().unwrap().read_fail=true;
        assert_eq!(request.post("/webhooks/stripe").text(&body).add_header("Stripe-Signature",&signature).await.status_code(),500);
        assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().refund,"Completed");
        provider.lock().unwrap().read_fail=false;
        assert_eq!(request.post("/webhooks/stripe").text(&body).add_header("Stripe-Signature",&signature).await.status_code(),200);
        assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().refund,"Reconciliation required");
        assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().refund_notice,"Not requested");
        let expires = Utc::now().timestamp() + 3600;
        let url = format!("/orders/{}?expires_at={expires}&sig={}", f.paid,
            crabgate::links::sign_order(&f.organizer.id, &f.paid, expires).unwrap());
        let buyer = request.get(&url).await;
        assert_eq!(buyer.status_code(), 200);
        assert!(buyer.text().contains("The organizer needs to check the amount returned for this order. A full refund is not confirmed."));
        assert!(!buyer.text().contains("refunded in full"));
        assert!(!buyer.text().contains("Reconciliation required"));
        assert!(!buyer.text().contains("stub-qr\"><img"));
        assert!(!buyer.text().contains("Resend email"));
        assert!(!buyer.text().contains("Save names"));
        std::fs::write(super::evidence_path("cg008-correction-reconciliation.html"), buyer.text()).unwrap();
        assert!(buyer
            .text()
            .contains("<h2 class=\"h-s\">Tickets not valid</h2>"));
        assert!(!buyer.text().contains("Refunded"));
        assert!(buyer.text().contains("Order total: €20"));
        assert!(buyer.text().contains("<strong>Void</strong>"));

        assert_eq!(admin_orders::order(&f.db,&f.cipher,&f.paid).await.unwrap().status,"Refunded");
        assert_eq!(admin_orders::order(&f.db,&f.cipher,&f.paid).await.unwrap().tickets[0].status,"Void");
        let checkins=f.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"SELECT checked_at, undone_at FROM checkins WHERE ticket_id=?",[ticket.id.clone().into()])).await.unwrap().unwrap();
        assert_eq!(checkins.try_get::<i64>("","checked_at").unwrap(),checked_at);
        assert_eq!(checkins.try_get::<Option<i64>>("","undone_at").unwrap(),None);
        assert!(matches!(crabgate::door::check_in(&f.db,&f.cipher,&f.event.id,&ticket.code,"Member","synthetic",checked_at+1).await.unwrap(),crabgate::door::CheckinResult::VoidTicket(_)));
        assert!(!admin_orders::claim_notice(&f.db,&f.paid,false).await.unwrap());
        assert_eq!(request.post(&format!("/office/orders/{}/refund",f.paid)).add_header("cookie",&f.cookie).await.status_code(),303);
        crabgate::controllers::admin_orders::run_refund_work(&ctx,&f.registry).await.unwrap();
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count,0);
        assert_eq!(provider.lock().unwrap().keys.len(),0);
        let page=request.get(&format!("/office/orders/{}",f.paid)).add_header("cookie",&f.cookie).await.text();
        assert!(page.contains("Resolve the partial refunds in Stripe"));
        std::fs::write(super::evidence_path("cg005-correction-r2-order.html"),page).unwrap();
        provider.lock().unwrap().history.insert(0,json!({"id":"re_c","status":"succeeded","amount":1000,"payment_intent":"pi_cg005"}));
        let body=json!({"type":"refund.updated","data":{"object":{"id":"re_c","amount":1000,"payment_intent":"pi_cg005","status":"succeeded"}}}).to_string();
        let now=Utc::now().timestamp();let mut mac=Hmac::<Sha256>::new_from_slice(b"whsec_cg005").unwrap();mac.update(format!("{now}.{body}").as_bytes());
        assert_eq!(request.post("/webhooks/stripe").text(&body).add_header("Stripe-Signature",&format!("t={now},v1={}",hex::encode(mac.finalize().into_bytes()))).await.status_code(),200);
        crabgate::controllers::admin_orders::run_refund_work(&ctx,&f.registry).await.unwrap();
        assert_eq!(admin_orders::work_progress(&f.db,&f.paid).await.unwrap().refund,"Completed");
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count,1);
        assert_eq!(provider.lock().unwrap().keys.len(),0);
    }).await;
    server.abort();
    std::env::remove_var("STRIPE_API_BASE");
}

#[tokio::test]
#[serial]
async fn correction_r2_free_standalone_write_error_retains_durable_claim() {
    request::<App, _, _>(|_, ctx| async move {
        let f = fixture(&ctx).await;
        let now = Utc::now().timestamp();
        f.db.execute_unprepared("CREATE TRIGGER cg005_interrupt_free BEFORE UPDATE OF status ON orders WHEN NEW.status='Refunded' BEGIN SELECT RAISE(ABORT,'synthetic standalone finalization failure'); END").await.unwrap();
        assert!(admin_orders::refund(&f.db, &f.free, None, now).await.is_err());
        let reopened = independent_connection(&f).await;
        assert_eq!(admin_orders::work_progress(&reopened, &f.free).await.unwrap().refund, "Pending");
        assert_eq!(admin_orders::order(&reopened, &f.cipher, &f.free).await.unwrap().tickets[0].status, "Valid");
        reopened.execute_unprepared("DROP TRIGGER cg005_interrupt_free").await.unwrap();
        reopened.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE orders SET refund_claim_until=0 WHERE id=?", [f.free.clone().into()])).await.unwrap();
        crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry).await.unwrap();
        assert_eq!(admin_orders::work_progress(&reopened, &f.free).await.unwrap().refund, "Completed");
        assert_eq!(admin_orders::order(&reopened, &f.cipher, &f.free).await.unwrap().tickets[0].status, "Void");
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 1);
    }).await;
}

#[tokio::test]
#[serial]
async fn correction_r3_completed_claim_crash_recovers_without_new_money() {
    expired_reconciliation_claim_recovery(false).await;
}

#[tokio::test]
#[serial]
async fn correction_r3_partial_claim_crash_recovers_without_speculative_money() {
    expired_reconciliation_claim_recovery(true).await;
}

async fn expired_reconciliation_claim_recovery(partial: bool) {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    for mode in ["job", "organiser", "direct"] {
        let provider = Arc::new(Mutex::new(Provider::default()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::env::set_var("STRIPE_API_BASE", &base);
        let fake = provider.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/refunds", post(refund).get(refund_history))
                    .route("/v1/refunds/{id}", get(retrieve_refund))
                    .with_state(fake),
            )
            .await
            .unwrap()
        });
        request::<App, _, _>(|request, ctx| async move {
            let f = fixture(&ctx).await;
            settings::set(&f.db, &f.cipher, "stripe_secret_key", "sk_test_synthetic").await.unwrap();
            let client = crabgate::stripe::StripeClient::from_secret("sk_test_synthetic").unwrap();
            let now = Utc::now().timestamp();
            let ticket = admin_orders::order(&f.db, &f.cipher, &f.paid).await.unwrap().tickets.remove(0);
            assert!(matches!(crabgate::door::check_in(&f.db, &f.cipher, &f.event.id, &ticket.code, "Member", "synthetic", now).await.unwrap(), crabgate::door::CheckinResult::CheckedIn(_)));
            admin_orders::refund(&f.db, &f.paid, Some(&client), now).await.unwrap();
            assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund_notice, "Pending");
            assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 0);
            if partial {
                provider.lock().unwrap().history[0]["amount"] = json!(1000);
                admin_orders::reconcile_refund(&f.db, &f.paid, &client, now).await.unwrap();
            }
            let expected = if partial { "Reconciliation required" } else { "Completed" };
            let (tx, mut entered) = tokio::sync::mpsc::unbounded_channel();
            let gate = Arc::new(HistoryGate {
                entered: tx, permits: vec![Arc::new(tokio::sync::Semaphore::new(0))],
                replies: vec![(StatusCode::OK, json!({"data":[],"has_more":false}))], calls: 0.into(),
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            std::env::set_var("STRIPE_API_BASE", format!("http://{}", listener.local_addr().unwrap()));
            let fake = provider.clone();
            let gated = tokio::spawn(async move {
                axum::serve(listener, Router::new().route("/v1/refunds", get(gated_history)).with_state(gate)
                    .merge(Router::new().route("/v1/refunds/{id}", get(retrieve_refund)).with_state(fake))).await.unwrap()
            });
            settings::set(&f.db, &f.cipher, "webhook_secret", "whsec_cg005").await.unwrap();
            let body = json!({"type":"refund.updated","data":{"object":{"id":"re_cg005","payment_intent":"pi_cg005","status":"succeeded"}}}).to_string();
            let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec_cg005").unwrap();
            mac.update(format!("{now}.{body}").as_bytes());
            let signature = format!("t={now},v1={}", hex::encode(mac.finalize().into_bytes()));
            let webhook = request.post("/webhooks/stripe").text(&body).add_header("Stripe-Signature", &signature);
            let mut worker = Box::pin(async move { webhook.await });
            tokio::select! {
                response = &mut worker => panic!("webhook completed before gated read: {}", response.status_code()),
                call = tokio::time::timeout(std::time::Duration::from_secs(3), entered.recv()) => assert_eq!(call.unwrap(), Some(0)),
            }
            let reopened = independent_connection(&f).await;
            let claim = reopened.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT refund_claim_token FROM orders WHERE id=?", [f.paid.clone().into()])).await.unwrap().unwrap().try_get::<String>("", "refund_claim_token").unwrap();
            std::env::set_var("STRIPE_API_BASE", &base);
            let active_reads = provider.lock().unwrap().history_reads;
            assert!(admin_orders::refund(&reopened, &f.paid, Some(&client), now).await.is_err());
            assert_eq!(request.post(&format!("/office/orders/{}/refund", f.paid)).add_header("cookie", &f.cookie).await.status_code(), 303);
            crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry).await.unwrap();
            assert!(!admin_orders::claim_notice(&reopened, &f.paid, false).await.unwrap());
            assert_eq!(reopened.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT refund_claim_token FROM orders WHERE id=?", [f.paid.clone().into()])).await.unwrap().unwrap().try_get::<String>("", "refund_claim_token").unwrap(), claim);
            assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 0);
            assert_eq!(provider.lock().unwrap().history_reads, active_reads);
            assert_eq!(admin_orders::work_progress(&reopened, &f.paid).await.unwrap().refund, expected);
            drop(worker); gated.abort();
            let expire = || Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "UPDATE orders SET refund_claim_until=0 WHERE id=?", [f.paid.clone().into()]);
            reopened.execute_raw(expire()).await.unwrap();
            provider.lock().unwrap().read_fail = true;
            let reads = provider.lock().unwrap().history_reads;
            match mode {
                "job" => crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry).await.unwrap(),
                "organiser" => assert_eq!(request.post(&format!("/office/orders/{}/refund", f.paid)).add_header("cookie", &f.cookie).await.status_code(), 303),
                _ => assert!(admin_orders::refund(&reopened, &f.paid, Some(&client), now).await.is_err()),
            }
            assert_eq!(provider.lock().unwrap().history_reads, reads + 1, "{mode}: recovery must read provider history");
            assert_eq!(admin_orders::work_progress(&reopened, &f.paid).await.unwrap().refund, expected);
            assert!(!admin_orders::claim_notice(&reopened, &f.paid, false).await.unwrap(), "unresolved recovery must block confirmation");
            assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 0);
            let recovery_claim = reopened.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT refund_claim_token, stripe_refund_id FROM orders WHERE id=?", [f.paid.clone().into()])).await.unwrap().unwrap();
            assert_ne!(recovery_claim.try_get::<String>("", "refund_claim_token").unwrap(), claim);
            assert_eq!(recovery_claim.try_get::<String>("", "stripe_refund_id").unwrap(), "re_cg005");
            crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry).await.unwrap();
            assert_eq!(request.post(&format!("/office/orders/{}/refund", f.paid)).add_header("cookie", &f.cookie).await.status_code(), 303);
            assert_eq!(provider.lock().unwrap().history_reads, reads + 3);
            assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 0);
            reopened.execute_raw(expire()).await.unwrap();
            provider.lock().unwrap().read_fail = false;
            match mode {
                "job" => crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry).await.unwrap(),
                "organiser" => assert_eq!(request.post(&format!("/office/orders/{}/refund", f.paid)).add_header("cookie", &f.cookie).await.status_code(), 303),
                _ => { admin_orders::refund(&reopened, &f.paid, Some(&client), now).await.unwrap(); crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry).await.unwrap(); }
            }
            assert_eq!(provider.lock().unwrap().history_reads, reads + 4);
            assert_eq!(admin_orders::work_progress(&reopened, &f.paid).await.unwrap().refund, expected);
            assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, if partial { 0 } else { 1 });
            if !partial {
                assert!(ctx.mailer.as_ref().unwrap().deliveries().messages[0].contains("was refunded in full"));
            }
            let row = reopened.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT refund_claim_token, refund_claim_until FROM orders WHERE id=?", [f.paid.clone().into()])).await.unwrap().unwrap();
            assert_eq!(row.try_get::<Option<String>>("", "refund_claim_token").unwrap(), None);
            assert_eq!(row.try_get::<Option<i64>>("", "refund_claim_until").unwrap(), None);
            assert_eq!(admin_orders::order(&reopened, &f.cipher, &f.paid).await.unwrap().status, "Refunded");
            assert_eq!(admin_orders::order(&reopened, &f.cipher, &f.paid).await.unwrap().tickets[0].status, "Void");
            let history = reopened.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT checked_at, undone_at FROM checkins WHERE ticket_id=?", [ticket.id.clone().into()])).await.unwrap().unwrap();
            assert_eq!(history.try_get::<i64>("", "checked_at").unwrap(), now);
            assert_eq!(history.try_get::<Option<i64>>("", "undone_at").unwrap(), None);
            assert!(matches!(crabgate::door::check_in(&reopened, &f.cipher, &f.event.id, &ticket.code, "Member", "synthetic", now+1).await.unwrap(), crabgate::door::CheckinResult::VoidTicket(_)));
            if partial {
                provider.lock().unwrap().history[0]["status"] = json!("failed");
            }
            assert_eq!(request.post(&format!("/office/orders/{}/refund", f.paid)).add_header("cookie", &f.cookie).await.status_code(), 303);
            crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry).await.unwrap();
            admin_orders::refund(&reopened, &f.paid, Some(&client), now).await.unwrap();
            assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, if partial { 0 } else { 1 });
            assert_eq!(provider.lock().unwrap().keys.len(), 1);
            assert_eq!(provider.lock().unwrap().refunds.len(), 1);
            println!("r3 {mode} partial={partial}: active owner protected; expired recovery reads completed; state/history retained; confirmations={}; POSTs=1; money=1", if partial {0} else {1});
        }).await;
        server.abort();
        std::env::remove_var("STRIPE_API_BASE");
    }
}

#[tokio::test]
#[serial]
async fn cg008_buyer_refund_progress_uses_real_cancellation_retry_and_signed_history() {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let provider = Arc::new(Mutex::new(Provider::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let fake = provider.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/refunds", post(refund).get(refund_history))
                .route("/v1/refunds/{id}", get(retrieve_refund))
                .with_state(fake),
        )
        .await
        .unwrap()
    });
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        let expires = Utc::now().timestamp() + 3600;
        let buyer_url = |id: &str| format!("/orders/{id}?expires_at={expires}&sig={}",
            crabgate::links::sign_order(&f.organizer.id, id, expires).unwrap());
        let route = format!("/office/events/{}/cancel", f.event.id);
        assert_eq!(request.post(&route).add_header("cookie", &f.cookie).await.status_code(), 303);
        let page = request.get(&buyer_url(&f.paid)).await;
        assert_eq!(page.status_code(), 200);
        std::fs::write(super::evidence_path("cg008-correction-unconfigured.html"), page.text()).unwrap();
        assert!(page.text().contains("Your refund has not been confirmed"));
        assert!(page.text().contains("The organizer handles refund retries"));
        assert!(!page.text().contains("Paid €20"));
        assert!(page.text().contains("Order total: €20"));
        assert!(page.text().contains("Ticket 1 of 1, Standard"));
        assert!(page.text().contains("Tue 15 June 2027"));
        assert!(page.text().contains("<dt>Total</dt><dd>€20</dd>"));
        assert_eq!(admin_orders::order(&f.db, &f.cipher, &f.paid).await.unwrap().status, "Paid");
        let free = request.get(&buyer_url(&f.free)).await;
        assert!(free.text().contains("No money was paid for this order. No money will be returned."));
        assert!(!free.text().contains("refunded in full"));
        settings::set(&f.db, &f.cipher, "stripe_secret_key", "sk_test_cg008_synthetic").await.unwrap();
        settings::set(&f.db, &f.cipher, "webhook_secret", "whsec_cg008").await.unwrap();
        for (fail, pending, expected) in [(true, false, "Failed"), (false, true, "Pending")] {
            provider.lock().unwrap().fail = fail;
            provider.lock().unwrap().pending = pending;
            assert_eq!(request.post(&format!("/office/orders/{}/refund", f.paid)).add_header("cookie", &f.cookie).await.status_code(), 303);
            assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund, expected);
            let page = request.get(&buyer_url(&f.paid)).await;
            assert!(page.text().contains("Your refund has not been confirmed"));
            assert!(page.text().contains("Contact the organizer"));
            assert!(!page.text().contains("refunded in full"));
            std::fs::write(super::evidence_path(&format!("cg008-correction-{expected}.html")), page.text()).unwrap();
        }
        for (status, expected, label) in [("succeeded", "Completed", "This order was refunded in full."),
            ("failed", "Failed", "Your refund has not been confirmed")] {
            provider.lock().unwrap().history[0]["status"] = json!(status);
            let body = json!({"type":"refund.updated", "data":{"object":{"id":"re_cg005", "payment_intent":"pi_cg005", "amount":2000, "status":status}}}).to_string();
            let now = Utc::now().timestamp();
            let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec_cg008").unwrap();
            mac.update(format!("{now}.{body}").as_bytes());
            let signature = format!("t={now},v1={}", hex::encode(mac.finalize().into_bytes()));
            assert_eq!(request.post("/webhooks/stripe").text(&body).add_header("Stripe-Signature", &signature).await.status_code(), 200);
            assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund, expected);
            let order = admin_orders::order(&f.db, &f.cipher, &f.paid).await.unwrap();
            assert_eq!(order.status, "Refunded");
            assert_eq!(order.total_cents, 2000);
            assert_eq!(order.tickets.len(), 1);
            assert_eq!(order.tickets[0].status, "Void");
            let page = request.get(&buyer_url(&f.paid)).await;
            assert!(page.text().contains(label));
            assert!(page.text().contains("This event is cancelled"));
            assert!(!page.text().contains("stub-qr\"><img"));
            assert!(!page.text().contains("Resend email"));
            assert!(!page.text().contains("Save names"));
            assert!(!page.text().contains("At the door"));
            assert!(!page.text().contains("pi_cg005"));
            assert!(!page.text().contains("re_cg005"));
            if status == "failed" { assert!(!page.text().contains("refunded in full")); }
            std::fs::write(super::evidence_path(&format!("cg008-correction-{expected}-signed.html")), page.text()).unwrap();
        }
        let counts = f.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT (SELECT COUNT(*) FROM orders) AS orders, (SELECT COUNT(*) FROM tickets) AS tickets")).await.unwrap().unwrap();
        assert_eq!(counts.try_get::<i64>("", "orders").unwrap(), 2);
        assert_eq!(counts.try_get::<i64>("", "tickets").unwrap(), 2);
    }).await;
    server.abort();
    if let Some(value) = prior {
        std::env::set_var("STRIPE_API_BASE", value);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }
}
#[tokio::test]
#[serial]
async fn immediate_cancellation_refund_confirmation_is_durable_and_sent_once() {
    use hmac::{Hmac, Mac};
    let provider = Arc::new(Mutex::new(Provider::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let fake = provider.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/refunds", post(refund).get(refund_history))
                .route("/v1/refunds/{id}", get(retrieve_refund))
                .with_state(fake),
        )
        .await
        .unwrap()
    });
    request::<App, _, _>(|request, ctx| async move {
        let f = fixture(&ctx).await;
        settings::set(&f.db, &f.cipher, "stripe_secret_key", "sk_test_cg057_synthetic").await.unwrap();
        let now = Utc::now().timestamp();
        members::add_member(&ctx.db, &f.organizer.id, "door-cg057@example.com", Role::DoorStaff).await.unwrap();
        let link = members::request_magic_link(&ctx.db, &f.organizer.id, "door-cg057@example.com", now).await.unwrap().unwrap();
        let session = members::claim_magic_link(&ctx.db, &f.organizer.id, &link, now).await.unwrap().unwrap();
        let staff_cookie = format!("cg_session={session}");
        for path in [format!("/office/events/{}/cancel", f.event.id), format!("/office/orders/{}/refund", f.paid)] {
            assert_eq!(request.post(&path).add_header("cookie", &staff_cookie).await.status_code(), 403);
        }
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 0);
        assert_eq!(provider.lock().unwrap().keys.len(), 0);
        assert_eq!(admin_orders::work_progress(&f.db, &f.paid).await.unwrap().refund_notice, "Not requested");
        let before = admin_orders::order(&f.db, &f.cipher, &f.paid).await.unwrap();
        let checked_at = Utc::now().timestamp();
        assert!(matches!(crabgate::door::check_in(&f.db, &f.cipher, &f.event.id, &before.tickets[0].code, "Member", "cg057", checked_at).await.unwrap(), crabgate::door::CheckinResult::CheckedIn(_)));
        let route = format!("/office/events/{}/cancel", f.event.id);
        assert_eq!(request.post(&route).add_header("cookie", &f.cookie).await.status_code(), 303);
        for _ in 0..2 {
            crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry).await.unwrap();
        }
        let paid = admin_orders::order(&f.db, &f.cipher, &f.paid).await.unwrap();
        let progress = admin_orders::work_progress(&f.db, &f.paid).await.unwrap();
        let messages = ctx.mailer.as_ref().unwrap().deliveries();
        {
        let provider_state = provider.lock().unwrap();
        println!("CG057 immediate: order={} ticket={} refund={} cancellation={} confirmation={} messages={} refund confirmations={} provider posts={} refunds={}",
            paid.status, paid.tickets[0].status, progress.refund, progress.cancellation_notice, progress.refund_notice, messages.count,
            messages.messages.iter().filter(|m| m.contains("was refunded in full")).count(), provider_state.keys.len(), provider_state.refunds.len());
        }
        assert_eq!(paid.status, "Refunded");
        assert_eq!(paid.tickets[0].status, "Void");
        assert_eq!(progress.refund, "Completed");
        assert_eq!(progress.cancellation_notice, "Completed");
        assert_eq!(progress.refund_notice, "Completed");
        assert_eq!(messages.count, 3);
        let confirmations: Vec<_> = messages.messages.iter().filter(|m| m.contains("was refunded in full")).collect();
        assert_eq!(confirmations.len(), 1);
        assert!(confirmations[0].contains(&format!("Your order {} for Cancellation Test was refunded in full.", paid.number)));
        assert!(confirmations[0].contains("Stripe will return the payment to the original method."));
        let free = admin_orders::order(&f.db, &f.cipher, &f.free).await.unwrap();
        assert_eq!(free.total_cents, 0);
        assert_eq!(free.status, "Refunded");
        assert_eq!(free.tickets[0].status, "Void");
        assert!(!messages.messages.iter().any(|m| m.contains(&format!("Your order {} for Cancellation Test was refunded in full.", free.number))));
        let page = request.get(&format!("/office/orders/{}", f.paid)).add_header("cookie", &f.cookie).await;
        assert_eq!(page.status_code(), 200);
        assert!(page.text().contains("Refund notice: Completed"));
        assert!(page.text().contains("Admission invalid"));
        let history = admin_orders::ticket_activity(&f.db, &f.paid).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].checked_at, Some(checked_at));
        assert!(matches!(crabgate::door::check_in(&f.db, &f.cipher, &f.event.id, &paid.tickets[0].code, "Member", "cg057", checked_at + 1).await.unwrap(), crabgate::door::CheckinResult::VoidTicket(_)));
        assert!(crabgate::inventory::reserve(&f.db, &f.event.id, &[crabgate::inventory::Selection { ticket_type_id: paid.tickets[0].ticket_type_id.clone(), quantity: 1 }], checked_at + 1).await.is_err());
        assert_eq!(events::get(&f.db, &f.event.id).await.unwrap().unwrap().status, "Cancelled");
        assert_eq!(request.post(&route).add_header("cookie", &f.cookie).await.status_code(), 303);
        settings::set(&f.db, &f.cipher, "webhook_secret", "whsec_cg057_synthetic").await.unwrap();
        let body = json!({"type":"refund.updated", "data":{"object":{"id":"re_cg005", "payment_intent":"pi_cg005", "amount":2000, "status":"succeeded"}}}).to_string();
        for _ in 0..2 {
            let now = Utc::now().timestamp();
            let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"whsec_cg057_synthetic").unwrap();
            mac.update(format!("{now}.{body}").as_bytes());
            let signature = format!("t={now},v1={}", hex::encode(mac.finalize().into_bytes()));
            assert_eq!(request.post("/webhooks/stripe").text(&body).add_header("Stripe-Signature", &signature).await.status_code(), 200);
        }
        for _ in 0..2 { crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry).await.unwrap(); }
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 3);
        assert_eq!(provider.lock().unwrap().keys, vec![format!("order-refund-{}", f.paid)]);
        assert_eq!(provider.lock().unwrap().refunds.len(), 1);
    }).await;
    server.abort();
    let _ = server.await;
    if let Some(value) = prior {
        std::env::set_var("STRIPE_API_BASE", value);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }
}
#[tokio::test]
#[serial]
async fn cancellation_refund_confirmation_smtp_refusal_recovers_in_jobs() {
    use loco_rs::{app::Hooks, boot::StartMode, config::WorkerMode, environment::Environment};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let refused = Arc::new(AtomicBool::new(true));
    let accepted = Arc::new(Mutex::new(Vec::<String>::new()));
    let refusal_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = accepted.clone();
    let reject = refused.clone();
    let rejections = refusal_count.clone();
    let smtp = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                connection = listener.accept() => {
                    let (stream, _) = connection.unwrap();
                    let captured = captured.clone();
                    let reject = reject.clone();
                    let rejections = rejections.clone();
                    connections.spawn(async move {
                        let (read, mut write) = stream.into_split();
                        let mut reader = BufReader::new(read);
                        write.write_all(b"220 synthetic SMTP\r\n").await.unwrap();
                        let mut data = false;
                        let mut body = String::new();
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).await.unwrap() == 0 { break; }
                            let response = if data {
                                if line != ".\r\n" { body.push_str(&line); continue; }
                                data = false;
                                if reject.load(Ordering::SeqCst) && body.replace("=\r\n", "").contains("was refunded in full") {
                                    rejections.fetch_add(1, Ordering::SeqCst);
                                    body.clear();
                                    b"451 synthetic refund notice refusal\r\n".as_slice()
                                } else {
                                    captured.lock().unwrap().push(std::mem::take(&mut body));
                                    b"250 accepted\r\n".as_slice()
                                }
                            } else if line.starts_with("DATA") {
                                data = true;
                                b"354 continue\r\n".as_slice()
                            } else if line.starts_with("QUIT") {
                                let _ = write.write_all(b"221 bye\r\n").await; break;
                            } else { b"250 OK\r\n".as_slice() };
                            if write.write_all(response).await.is_err() { break; }
                        }
                    });
                }
                _ = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    });
    let provider = Arc::new(Mutex::new(Provider::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    let fake = provider.clone();
    let stripe = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/refunds", post(refund).get(refund_history))
                .route("/v1/refunds/{id}", get(retrieve_refund))
                .with_state(fake),
        )
        .await
        .unwrap()
    });
    let mut config = App::load_config(&Environment::Test).await.unwrap();
    config.workers.mode = WorkerMode::BackgroundAsync;
    let mailer = config.mailer.as_mut().unwrap();
    mailer.stub = false;
    let transport = mailer.smtp.as_mut().unwrap();
    transport.host = "127.0.0.1".into();
    transport.port = port;
    transport.secure = false;
    transport.auth = None;
    let boot = App::boot(StartMode::ServerOnly, &Environment::Test, config)
        .await
        .unwrap();
    let ctx = boot.app_context;
    let f = fixture(&ctx).await;
    settings::set(
        &f.db,
        &f.cipher,
        "stripe_secret_key",
        "sk_test_cg057_synthetic",
    )
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let http =
        tokio::spawn(async move { axum::serve(listener, boot.router.unwrap()).await.unwrap() });
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let route = format!("{origin}/office/events/{}/cancel", f.event.id);
    assert_eq!(
        client
            .post(&route)
            .header("cookie", &f.cookie)
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    let progress = admin_orders::work_progress(&f.db, &f.paid).await.unwrap();
    assert_eq!(progress.refund, "Pending");
    assert_eq!(progress.cancellation_notice, "Pending");
    assert_eq!(progress.refund_notice, "Not requested");
    assert_eq!(accepted.lock().unwrap().len(), 0);
    assert_eq!(provider.lock().unwrap().keys.len(), 0);
    crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry)
        .await
        .unwrap();
    let progress = admin_orders::work_progress(&f.db, &f.paid).await.unwrap();
    assert_eq!(progress.refund, "Completed");
    assert_eq!(progress.cancellation_notice, "Completed");
    assert_eq!(progress.refund_notice, "Failed");
    assert!(!admin_orders::notice_sent(&f.db, &f.paid, false)
        .await
        .unwrap());
    assert_eq!(refusal_count.load(Ordering::SeqCst), 1);
    assert_eq!(accepted.lock().unwrap().len(), 2);
    assert_eq!(provider.lock().unwrap().keys.len(), 1);
    crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry)
        .await
        .unwrap();
    assert_eq!(refusal_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        admin_orders::work_progress(&f.db, &f.paid)
            .await
            .unwrap()
            .refund_notice,
        "Failed"
    );
    refused.store(false, Ordering::SeqCst);
    crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry)
        .await
        .unwrap();
    let progress = admin_orders::work_progress(&f.db, &f.paid).await.unwrap();
    assert_eq!(progress.refund_notice, "Completed");
    assert!(admin_orders::notice_sent(&f.db, &f.paid, false)
        .await
        .unwrap());
    for _ in 0..2 {
        crabgate::controllers::admin_orders::run_refund_work(&ctx, &f.registry)
            .await
            .unwrap();
    }
    assert_eq!(
        client
            .post(&route)
            .header("cookie", &f.cookie)
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    let messages = accepted.lock().unwrap().clone();
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.replace("=\r\n", "").contains("was refunded in full"))
            .count(),
        1
    );
    let order = admin_orders::order(&f.db, &f.cipher, &f.paid)
        .await
        .unwrap();
    assert_eq!(order.status, "Refunded");
    assert_eq!(order.tickets[0].status, "Void");
    assert_eq!(
        provider.lock().unwrap().keys,
        vec![format!("order-refund-{}", f.paid)]
    );
    assert_eq!(provider.lock().unwrap().refunds.len(), 1);
    if let Some(root) = std::env::var_os("CG032_EVIDENCE_DIR") {
        std::fs::write(
            std::path::PathBuf::from(root).join("cg057-smtp.json"),
            serde_json::to_vec_pretty(&messages).unwrap(),
        )
        .unwrap();
    }
    println!("CG057 SMTP: 2 refusals; refund notice Failed -> Completed; 3 accepted messages; 1 confirmation; 1 provider post/refund");
    for task in [http, stripe, smtp] {
        task.abort();
        let _ = task.await;
    }
    if let Some(value) = prior {
        std::env::set_var("STRIPE_API_BASE", value);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }
}
