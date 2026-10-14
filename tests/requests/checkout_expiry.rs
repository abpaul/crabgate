use axum::{
    extract::{Form, State},
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use crabgate::{
    app::App,
    crypto::MasterKey,
    events::{self, EventInput, TicketTypeInput},
    inventory, orders,
    organizers::{Edition, OrganizerRegistry},
    settings,
};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Provider {
    deadlines: Vec<i64>,
    holds: Vec<String>,
    offset: i64,
    clock_offset: i64,
    omit_expiry: bool,
    fail: bool,
    expire_fail: bool,
    expire_invalid: bool,
    expirations: usize,
    refunds: usize,
    delay: u64,
    keys: Vec<String>,
}

async fn create(
    State(state): State<Arc<Mutex<Provider>>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let deadline: i64 = form["expires_at"].parse().unwrap();
    let delay = {
        let mut state = state.lock().unwrap();
        state.deadlines.push(deadline);
        state.holds.push(form["metadata[hold_id]"].clone());
        state
            .keys
            .push(headers["idempotency-key"].to_str().unwrap().to_owned());
        state.delay
    };
    tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
    let state = state.lock().unwrap();
    let now = orders::now() + state.clock_offset;
    if state.fail || !(now + 1800..=now + 86400).contains(&deadline) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"invalid expiry"})),
        );
    }
    let id = format!("cs_cg007_{}", state.deadlines.len());
    let mut body = serde_json::json!({"id":id, "url":format!("https://checkout.stripe.com/{id}"), "expires_at":deadline + state.offset});
    if state.omit_expiry {
        body.as_object_mut().unwrap().remove("expires_at");
    }
    (StatusCode::OK, Json(body))
}

async fn expire(
    State(state): State<Arc<Mutex<Provider>>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut state = state.lock().unwrap();
    state.expirations += 1;
    if state.expire_fail {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"already expired"})),
        )
    } else {
        (
            StatusCode::OK,
            Json(
                serde_json::json!({"status": if state.expire_invalid { "open" } else { "expired" }}),
            ),
        )
    }
}

async fn refund(State(state): State<Arc<Mutex<Provider>>>) -> Json<serde_json::Value> {
    state.lock().unwrap().refunds += 1;
    Json(serde_json::json!({"id":"re_cg007", "status":"succeeded"}))
}

pub(super) struct Mock {
    state: Arc<Mutex<Provider>>,
    task: tokio::task::JoinHandle<()>,
    prior: Option<String>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(prior) = &self.prior {
            std::env::set_var("STRIPE_API_BASE", prior);
        } else {
            std::env::remove_var("STRIPE_API_BASE");
        }
    }
}
pub(super) async fn mock() -> Mock {
    let state = Arc::new(Mutex::new(Provider::default()));
    let router = Router::new()
        .route("/v1/checkout/sessions", post(create))
        .route("/v1/checkout/sessions/{id}/expire", post(expire))
        .route("/v1/refunds", post(refund))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var(
        "STRIPE_API_BASE",
        format!("http://{}", listener.local_addr().unwrap()),
    );
    Mock {
        state,
        prior,
        task: tokio::spawn(async move { axum::serve(listener, router).await.unwrap() }),
    }
}

async fn fixture(
    ctx: &loco_rs::app::AppContext,
    price: &str,
) -> (tenant_sqlite_loco::TenantHandle, String, String) {
    let registry = OrganizerRegistry::new(
        ctx.db.clone(),
        ctx.config.database.clone(),
        Edition::SelfHosted,
        "crabgate.com".into(),
        std::env::var("CRABGATE_ORGANIZER_DATABASE_DIR")
            .unwrap()
            .into(),
        MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap(),
    )
    .unwrap();
    let scope = registry.active_dbs().await.unwrap().remove(0);
    let cipher = registry.cipher(&scope.organizer).unwrap();
    settings::set(&scope.db, &cipher, "stripe_secret_key", "sk_test_synthetic")
        .await
        .unwrap();
    settings::set(&scope.db, &cipher, "webhook_secret", "whsec_cg007")
        .await
        .unwrap();
    let event = events::create(
        &scope.db,
        &EventInput {
            title: "Expiry test".into(),
            date: "2027-06-15".into(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Hall".into(),
            address: "Street".into(),
            description: String::new(),
            event_capacity: Some(1),
            max_per_order: 1,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let kind = events::add_ticket_type(
        &scope.db,
        &event,
        &TicketTypeInput {
            name: "Standard".into(),
            price: price.into(),
            capacity: 1,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(&scope.db, &event.id, orders::now())
        .await
        .unwrap();
    (scope.db, event.id, kind.id)
}

#[tokio::test]
#[serial]
async fn cg007_paid_deadline_is_persisted_sent_and_rendered_without_reset_on_retry() {
    let mock = mock().await;
    request::<App, _, _>(|request, ctx| async move {
        let (db, event, kind) = fixture(&ctx, "10").await;
        let response = request
            .post("/checkout")
            .form(&[
                ("event_id".into(), event.clone()),
                (format!("quantity_{kind}"), "1".into()),
            ])
            .await;
        assert_eq!(response.status_code(), 303, "{}", response.text());
        let path = response.header("location").to_str().unwrap().to_owned();
        let id = path.strip_prefix("/checkout/").unwrap();
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT created_at, expires_at FROM holds WHERE id = ?",
                [id.into()],
            ))
            .await
            .unwrap()
            .unwrap();
        let created: i64 = row.try_get("", "created_at").unwrap();
        let deadline: i64 = row.try_get("", "expires_at").unwrap();
        println!(
            "persisted duration={} Stripe deadline={} Hold deadline={deadline}",
            deadline - created,
            mock.state.lock().unwrap().deadlines[0]
        );
        assert_eq!(deadline - created, 1860);
        assert_eq!(mock.state.lock().unwrap().deadlines, vec![deadline]);
        let page = request.get(&path).await;
        assert_eq!(page.status_code(), 200);
        assert!(page
            .text()
            .contains(&format!("data-expires-at=\"{deadline}\"")));
        assert!(!page.text().contains("for 30 minutes"));
        std::fs::write(super::evidence_path("cg007-checkout.html"), page.text()).unwrap();
        for _ in 0..2 {
            let submit = request
                .post(&path)
                .form(&[("email", "buyer@example.com"), ("name_0", "")])
                .await;
            assert_eq!(submit.status_code(), 303);
            assert_eq!(
                submit.header("location"),
                "https://checkout.stripe.com/cs_cg007_1"
            );
        }
        assert_eq!(mock.state.lock().unwrap().deadlines.len(), 1);
        assert_eq!(
            orders::hold_details(&db, id).await.unwrap().expires_at,
            deadline
        );
        let cancelled = request.post(&format!("{path}/cancel")).await;
        assert_eq!(cancelled.status_code(), 303);
        assert_eq!(mock.state.lock().unwrap().expirations, 1);
        assert_eq!(
            inventory::open_counts(&db, &event, orders::now())
                .await
                .unwrap(),
            (0, 0)
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg007_provider_failure_or_different_expiry_releases_capacity() {
    let mock = mock().await;
    request::<App, _, _>(|request, ctx| async move {
        let (db, event, kind) = fixture(&ctx, "10").await;
        for (fail, offset, clock_offset, omit_expiry) in [
            (true, 0, 0, false),
            (false, 60, 0, false),
            (false, -60, 0, false),
            (false, 0, 0, true),
            (false, 0, 65, false),
        ] {
            {
                let mut state = mock.state.lock().unwrap();
                state.fail = fail;
                state.offset = offset;
                state.clock_offset = clock_offset;
                state.omit_expiry = omit_expiry;
            }
            let response = request
                .post("/checkout")
                .form(&[
                    ("event_id".into(), event.clone()),
                    (format!("quantity_{kind}"), "1".into()),
                ])
                .await;
            assert_eq!(response.status_code(), 502, "{}", response.text());
            assert_eq!(
                inventory::open_counts(&db, &event, orders::now())
                    .await
                    .unwrap(),
                (0, 0)
            );
            let id = mock.state.lock().unwrap().holds.last().unwrap().clone();
            assert_eq!(
                orders::hold_details(&db, &id).await.unwrap().status,
                "Expired"
            );
        }
        assert_eq!(mock.state.lock().unwrap().expirations, 3);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg007_free_expiry_boundary_and_paid_cancel_provider_error_release_capacity() {
    let mock = mock().await;
    request::<App, _, _>(|request, ctx| async move {
        let (db, event, kind) = fixture(&ctx, "0").await;
        let selection = [inventory::Selection {
            ticket_type_id: kind.clone(),
            quantity: 1,
        }];
        let now = orders::now();
        let hold = inventory::reserve(&db, &event, &selection, now)
            .await
            .unwrap();
        assert_eq!(hold.expires_at, now + 1860);
        assert_eq!(
            inventory::open_counts(&db, &event, hold.expires_at - 1)
                .await
                .unwrap(),
            (0, 1)
        );
        assert_eq!(
            inventory::open_counts(&db, &event, hold.expires_at)
                .await
                .unwrap(),
            (0, 0)
        );
        orders::expire_hold(&db, &hold.id).await.unwrap();
        let old = inventory::reserve(&db, &event, &selection, now - 1900)
            .await
            .unwrap();
        let expired = request.get(&format!("/checkout/{}", old.id)).await;
        assert_eq!(expired.status_code(), 410);
        assert!(expired.text().contains("Please choose them again"));
        std::fs::write(super::evidence_path("cg007-expired.txt"), expired.text()).unwrap();
        let retry = request
            .post(&format!("/checkout/{}", old.id))
            .form(&[("email", "buyer@example.com")])
            .await;
        assert_eq!(retry.status_code(), 410);
        assert_eq!(mock.state.lock().unwrap().deadlines.len(), 0);
        let (paid_db, paid_event, paid_kind) = fixture(&ctx, "10").await;
        let response = request
            .post("/checkout")
            .form(&[
                ("event_id".into(), paid_event.clone()),
                (format!("quantity_{paid_kind}"), "1".into()),
            ])
            .await;
        assert_eq!(response.status_code(), 303);
        let path = response.header("location").to_str().unwrap().to_owned();
        mock.state.lock().unwrap().expire_fail = true;
        let cancel = request.post(&format!("{path}/cancel")).await;
        assert_eq!(cancel.status_code(), 502);
        assert!(cancel.text().contains("released"));
        assert_eq!(
            inventory::open_counts(&paid_db, &paid_event, orders::now())
                .await
                .unwrap(),
            (0, 0)
        );
        std::fs::write(
            super::evidence_path("cg007-cancel-error.txt"),
            cancel.text(),
        )
        .unwrap();
        {
            let mut state = mock.state.lock().unwrap();
            state.expire_fail = false;
            state.expire_invalid = true;
        }
        let response = request
            .post("/checkout")
            .form(&[
                ("event_id".into(), paid_event.clone()),
                (format!("quantity_{paid_kind}"), "1".into()),
            ])
            .await;
        assert_eq!(response.status_code(), 303);
        let path = response.header("location").to_str().unwrap().to_owned();
        assert_eq!(
            request.post(&format!("{path}/cancel")).await.status_code(),
            502
        );
        assert_eq!(
            inventory::open_counts(&paid_db, &paid_event, orders::now())
                .await
                .unwrap(),
            (0, 0)
        );
    })
    .await;
}

fn callback(kind: &str, hold: &str, session: &str) -> (String, String) {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let body = serde_json::json!({"type":kind,"data":{"object":{"id":session,"payment_status":"paid","payment_intent":format!("pi_{session}"),"amount_total":1000,"currency":"eur","metadata":{"hold_id":hold}}}}).to_string();
    let timestamp = orders::now();
    let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec_cg007").unwrap();
    mac.update(format!("{timestamp}.{body}").as_bytes());
    (
        body,
        format!(
            "t={timestamp},v1={}",
            hex::encode(mac.finalize().into_bytes())
        ),
    )
}

#[tokio::test]
#[serial]
async fn cg007_delayed_buyer_entry_uses_existing_session_and_late_callbacks_stay_safe() {
    let mock = mock().await;
    request::<App, _, _>(|request,ctx| async move {
        for replace in [false,true] {
            let (db,event,kind)=fixture(&ctx,"10").await;
            let now=orders::now();
            let selection=[inventory::Selection{ticket_type_id:kind.clone(),quantity:1}];
            let incomplete=inventory::reserve(&db,&event,&selection,now-1740).await.unwrap();
            let retry=request.post(&format!("/checkout/{}",incomplete.id)).form(&[("email","buyer@example.com")]).await;
            assert_eq!(retry.status_code(),410);
            assert!(retry.text().contains("no payment session"));
            assert_eq!(inventory::open_counts(&db,&event,now).await.unwrap(),(0,0));
            let hold=inventory::reserve(&db,&event,&selection,now-1740).await.unwrap();
            assert_eq!(hold.expires_at,now+120);
            let session=format!("cs_delayed_{replace}");
            orders::attach_session_with_url(&db,&hold.id,&session,Some("https://checkout.stripe.com/existing")).await.unwrap();
            let path=format!("/checkout/{}",hold.id);
            let page=request.get(&path).await;
            assert_eq!(page.status_code(),200);
            assert!(page.text().contains(&format!("data-expires-at=\"{}\"",hold.expires_at)));
            for _ in 0..2 {
                let submit=request.post(&path).form(&[("email","buyer@example.com"),("name_0","Guest")]).await;
                assert_eq!(submit.status_code(),303);
                assert_eq!(submit.header("location"),"https://checkout.stripe.com/existing");
                assert_eq!(orders::hold_details(&db,&hold.id).await.unwrap().expires_at,now+120);
            }
            assert!(mock.state.lock().unwrap().deadlines.is_empty());
            assert_eq!(request.post(&format!("{path}/cancel")).await.status_code(),303);
            assert_eq!(inventory::open_counts(&db,&event,now).await.unwrap(),(0,0));
            if replace {
                inventory::reserve(&db,&event,&selection,now).await.unwrap();
            }
            let (body,signature)=callback("checkout.session.completed",&hold.id,&session);
            for _ in 0..2 {
                let result=request.post("/webhooks/stripe").add_header("stripe-signature",&signature).add_header("content-type","application/json").text(body.clone()).await;
                assert_eq!(result.status_code(),200,"{}",result.text());
            }
            let row=db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"SELECT COUNT(*) AS count FROM tickets WHERE event_id = ?",[event.clone().into()])).await.unwrap().unwrap();
            assert_eq!(row.try_get::<i64>("","count").unwrap(),if replace {0} else {1});
            assert_eq!(inventory::open_counts(&db,&event,orders::now()).await.unwrap(),if replace {(0,1)} else {(1,0)});
            assert_eq!(mock.state.lock().unwrap().refunds,if replace {1} else {0});
            println!("delayed entry 1740s: deadline unchanged; cancelled late success replace={replace}, tickets={}, refunds={}",if replace {0} else {1},mock.state.lock().unwrap().refunds);
        }
        assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count,2);
    }).await;
}

#[tokio::test]
#[serial]
async fn cg007_provider_expired_callback_releases_capacity_without_tickets() {
    let mock = mock().await;
    request::<App, _, _>(|request, ctx| async move {
        let (db, event, kind) = fixture(&ctx, "10").await;
        let response = request
            .post("/checkout")
            .form(&[
                ("event_id".into(), event.clone()),
                (format!("quantity_{kind}"), "1".into()),
            ])
            .await;
        assert_eq!(response.status_code(), 303);
        let id = mock.state.lock().unwrap().holds[0].clone();
        let (body, signature) = callback("checkout.session.expired", &id, "cs_cg007_1");
        for _ in 0..2 {
            assert_eq!(
                request
                    .post("/webhooks/stripe")
                    .add_header("stripe-signature", &signature)
                    .add_header("content-type", "application/json")
                    .text(body.clone())
                    .await
                    .status_code(),
                200
            );
        }
        assert_eq!(
            orders::hold_details(&db, &id).await.unwrap().status,
            "Expired"
        );
        assert_eq!(
            inventory::open_counts(&db, &event, orders::now())
                .await
                .unwrap(),
            (0, 0)
        );
        assert!(orders::order_id_for_hold(&db, &id).await.unwrap().is_none());
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg007_preexisting_35_minute_hold_requires_new_selection_without_extension() {
    let mock = mock().await;
    request::<App, _, _>(|request, ctx| async move {
        let (db, event, kind) = fixture(&ctx, "10").await;
        let now = orders::now();
        let selection = [inventory::Selection {
            ticket_type_id: kind,
            quantity: 1,
        }];
        let legacy = inventory::reserve(&db, &event, &selection, now)
            .await
            .unwrap();
        orders::attach_session_with_url(
            &db,
            &legacy.id,
            "cs_legacy",
            Some("https://checkout.stripe.com/legacy"),
        )
        .await
        .unwrap();
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE holds SET expires_at = created_at + 2100 WHERE id = ?",
            [legacy.id.clone().into()],
        ))
        .await
        .unwrap();
        let details = orders::hold_details(&db, &legacy.id).await.unwrap();
        assert_eq!(details.expires_at - details.created_at, 2100);
        let path = format!("/checkout/{}", legacy.id);
        let page = request.get(&path).await;
        assert_eq!(page.status_code(), 410);
        assert!(page.text().contains("old payment deadline"));
        assert_eq!(
            request
                .post(&path)
                .form(&[("email", "buyer@example.com")])
                .await
                .status_code(),
            410
        );
        assert_eq!(
            inventory::open_counts(&db, &event, now).await.unwrap(),
            (0, 0)
        );
        assert_eq!(
            orders::hold_details(&db, &legacy.id)
                .await
                .unwrap()
                .expires_at,
            now + 2100
        );
        assert_eq!(
            orders::hold_details(&db, &legacy.id).await.unwrap().status,
            "Expired"
        );
        let replacement = inventory::reserve(&db, &event, &selection, now)
            .await
            .unwrap();
        assert_eq!(replacement.expires_at, now + 1860);
        assert!(mock.state.lock().unwrap().deadlines.is_empty());
        assert!(orders::order_id_for_hold(&db, &legacy.id)
            .await
            .unwrap()
            .is_none());
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg008_unknown_confirmation_is_not_payment_processing() {
    request::<App, _, _>(|request, _ctx| async move {
        let page = request
            .get("/checkout/missing/confirmation?session_id=cs_forged")
            .await;
        assert_eq!(page.status_code(), 404, "{}", page.text());
        assert!(page.text().contains("Checkout not found"));
        assert!(page.text().contains("href=\"/\""));
        assert!(!page.text().contains("still going through"));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg008_expired_checkout_offers_event_recovery() {
    request::<App, _, _>(|request, ctx| async move {
        let (db, event, kind) = fixture(&ctx, "0").await;
        let hold = inventory::reserve(
            &db,
            &event,
            &[inventory::Selection {
                ticket_type_id: kind,
                quantity: 1,
            }],
            orders::now() - 1900,
        )
        .await
        .unwrap();
        for suffix in ["", "/confirmation"] {
            let page = request.get(&format!("/checkout/{}{suffix}", hold.id)).await;
            assert_eq!(page.status_code(), 410);
            assert!(
                page.text().contains(&format!("href=\"/events/{event}\"")),
                "{}",
                page.text()
            );
            assert!(page.text().contains("Please choose them again"));
            std::fs::write(super::evidence_path("cg008-expired.html"), page.text()).unwrap();
        }
        assert_eq!(
            inventory::open_counts(&db, &event, orders::now())
                .await
                .unwrap(),
            (0, 0)
        );
    })
    .await;
}

async fn cg011_selection_fixture(
    ctx: &loco_rs::app::AppContext,
    free: bool,
) -> (
    tenant_sqlite_loco::TenantHandle,
    String,
    Vec<(String, String)>,
) {
    let (db, _, _) = fixture(ctx, "0").await;
    let event = events::create(
        &db,
        &EventInput {
            title: "Summer <concert> & friends".into(),
            date: "2027-06-15".into(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Hall & garden".into(),
            address: "Street".into(),
            description: String::new(),
            event_capacity: Some(10),
            max_per_order: 5,
            attendee_name_required: true,
        },
    )
    .await
    .unwrap();
    let mut form = vec![("event_id".into(), event.id.clone())];
    for (name, price, quantity) in [
        ("General <admission>", "10", "2"),
        ("Balcony & friends", "25", "1"),
    ] {
        let kind = events::add_ticket_type(
            &db,
            &event,
            &TicketTypeInput {
                name: name.into(),
                price: if free { "0" } else { price }.into(),
                capacity: 5,
                sales_open: None,
                sales_close: None,
            },
        )
        .await
        .unwrap();
        form.push((format!("quantity_{}", kind.id), quantity.into()));
    }
    events::publish(&db, &event.id, orders::now())
        .await
        .unwrap();
    (db, event.id, form)
}

#[tokio::test]
#[serial]
async fn cg011_paid_selection_summary_and_total_survive_validation_and_cancel() {
    let _mock = mock().await;
    request::<App, _, _>(|request, ctx| async move {
        let (db, event, mut form) = cg011_selection_fixture(&ctx, false).await;
        crabgate::discounts::create(&db, crabgate::discounts::NewDiscount {
            event_id: &event, code: "SUMMER-10", kind: "Percent", value: 10,
            starts_at: None, ends_at: None, max_redemptions: None,
        }, orders::now()).await.unwrap();
        let question = crabgate::questions::add(&db, &event, "Order", "Text", "Access <details> & needs", &[], true).await.unwrap();
        form.push(("discount_code".into(), "SUMMER-10".into()));
        let start = request.post("/checkout").form(&form).await;
        assert_eq!(start.status_code(), 303, "{}", start.text());
        let path = start.header("location").to_str().unwrap().to_owned();
        let id = path.strip_prefix("/checkout/").unwrap();
        let hold = orders::hold_details(&db, id).await.unwrap();
        assert_eq!((hold.subtotal_cents, hold.discount_cents, hold.total_cents), (4500, 450, 4050));
        let page = request.get(&path).await;
        assert_eq!(page.status_code(), 200);
        let assert_summary = |html: &str| {
            assert!(html.contains("Summer &lt;concert&gt; &amp; friends"));
            assert!(html.contains("Tue 15 June 2027, Hall &amp; garden"));
            assert!(html.contains("2 × General &lt;admission&gt;</span><span class=\"num\">€20</span>"));
            assert!(html.contains("1 × Balcony &amp; friends</span><span class=\"num\">€25</span>"));
            assert!(html.contains("Discount SUMMER-10</span><span class=\"num\">−€4.50</span>"));
            assert!(html.contains("Total</span><span class=\"amount hl-under\">€40.50</span>"));
            assert!(html.contains("Continue to payment — €40.50</button>"));
            assert!(html.find("id=\"sum-h\"").unwrap() < html.find("id=\"email\"").unwrap());
            for ordinal in 0..3 {
                assert!(html.contains(&format!("for=\"name_{ordinal}\">Ticket {} ", ordinal + 1)));
            }
            assert!(!html.contains("<concert>"));
            assert!(!html.contains("<admission>"));
            assert!(html.contains("Access &lt;details&gt; &amp; needs"));
        };
        assert_summary(&page.text());
        let question = crabgate::questions::list(&db, &event).await.unwrap().into_iter().find(|item| item.id == question).unwrap();
        let field = crabgate::questions::field_name(&question, 0);
        let values = vec![("email".to_owned(), "bad-email".to_owned()), ("name_0".into(), "Alex <guest>".into()), ("name_1".into(), "Bea".into()), ("name_2".into(), "Cam".into()), (field.clone(), "Wheelchair & companion".into())];
        let error = request.post(&path).form(&values).await;
        assert_eq!(error.status_code(), 400);
        assert_summary(&error.text());
        assert!(error.text().contains("role=\"alert\""));
        assert!(error.text().contains("value=\"bad-email\""));
        assert!(error.text().contains("value=\"Alex &lt;guest&gt;\""));
        assert!(error.text().contains("value=\"Wheelchair &amp; companion\""));
        let missing = request.post(&path).form(&[("email", "buyer@example.com"), ("name_0", "Alex"), ("name_1", "Bea"), ("name_2", "Cam")]).await;
        assert_eq!(missing.status_code(), 400);
        assert!(missing.text().contains("Please answer every required question."));
        assert_summary(&missing.text());
        let mut valid = values;
        valid[0].1 = "buyer@example.com".into();
        let submitted = request.post(&path).form(&valid).await;
        assert_eq!(submitted.status_code(), 303);
        assert_eq!(submitted.header("location"), "https://checkout.stripe.com/cs_cg007_1");
        assert_eq!(orders::hold_details(&db, id).await.unwrap().expires_at, hold.expires_at);
        let cancelled = request.post(&format!("{path}/cancel")).await;
        assert_eq!(cancelled.status_code(), 303);
        assert_eq!(cancelled.header("location"), format!("/events/{event}"));
        assert_eq!(inventory::open_counts(&db, &event, orders::now()).await.unwrap(), (0, 0));
    }).await;
}

#[tokio::test]
#[serial]
async fn cg011_free_selection_total_and_names_complete_without_payment() {
    request::<App, _, _>(|request, ctx| async move {
        let (db, event, form) = cg011_selection_fixture(&ctx, true).await;
        let empty = request.post("/checkout").form(&[("event_id", event.as_str())]).await;
        assert_eq!(empty.status_code(), 400);
        assert!(empty.text().contains("Choose at least one ticket."));
        let start = request.post("/checkout").form(&form).await;
        assert_eq!(start.status_code(), 303);
        let path = start.header("location").to_str().unwrap().to_owned();
        let id = path.strip_prefix("/checkout/").unwrap();
        let page = request.get(&path).await;
        assert_eq!(page.status_code(), 200);
        assert!(page.text().contains("Tue 15 June 2027, Hall &amp; garden"));
        assert!(page.text().contains("2 × General &lt;admission&gt;</span><span class=\"num\">Free</span>"));
        assert!(page.text().contains("1 × Balcony &amp; friends</span><span class=\"num\">Free</span>"));
        assert!(page.text().contains("Get tickets — Free</button>"));
        assert!(!page.text().contains("A few questions"));
        let error = request.post(&path).form(&[("email", "buyer@example.com"), ("name_0", "Alex")]).await;
        assert_eq!(error.status_code(), 400);
        assert!(error.text().contains("role=\"alert\""));
        assert!(error.text().contains("value=\"Alex\""));
        assert!(error.text().contains("Get tickets — Free</button>"));
        let submit = request.post(&path).form(&[("email", "buyer@example.com"), ("name_0", "Alex"), ("name_1", "Bea"), ("name_2", "Cam")]).await;
        assert_eq!(submit.status_code(), 303);
        assert_eq!(submit.header("location"), format!("{path}/confirmation"));
        let confirmation = request.get(&format!("{path}/confirmation")).await;
        assert_eq!(confirmation.status_code(), 200);
        let order = orders::order_id_for_hold(&db, id).await.unwrap().unwrap();
        let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "SELECT status, total_cents, (SELECT count(*) FROM tickets WHERE order_id = orders.id) AS count FROM orders WHERE id = ?", [order.into()])).await.unwrap().unwrap();
        assert_eq!(row.try_get::<String>("", "status").unwrap(), "Paid");
        assert_eq!(row.try_get::<i64>("", "total_cents").unwrap(), 0);
        assert_eq!(row.try_get::<i64>("", "count").unwrap(), 3);
    }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn concurrent_complimentary_checkouts_and_duplicate_submissions_are_idempotent() {
    request::<App, _, _>(|request, ctx| async move {
        tokio::task::LocalSet::new().run_until(async move {
        let (db, event, kind) = fixture(&ctx, "0").await;
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET event_capacity = 2 WHERE id = ?",
            [event.clone().into()],
        ))
        .await
        .unwrap();
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE ticket_types SET capacity = 2 WHERE id = ?",
            [kind.clone().into()],
        ))
        .await
        .unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..20 {
            let call = request.post("/checkout").form(&[
                ("event_id".into(), event.clone()),
                (format!("quantity_{kind}"), "1".into()),
            ]);
            tasks.spawn_local(async move { call.await });
        }
        let mut paths = Vec::new();
        let mut rejected = 0;
        while let Some(response) = tasks.join_next().await {
            let response = response.unwrap();
            match response.status_code().as_u16() {
                303 => paths.push(response.header("location").to_str().unwrap().to_owned()),
                400 => rejected += 1,
                status => panic!("Unexpected reservation {status}: {}", response.text()),
            }
        }
        assert_eq!(paths.len(), 2);
        assert_eq!(rejected, 18);
        assert_eq!(
            inventory::open_counts(&db, &event, orders::now()).await.unwrap(),
            (0, 2)
        );
        for path in &paths {
            for _ in 0..8 {
                let call = request
                    .post(path)
                    .form(&[("email", "fixture@example.invalid"), ("name_0", "Fixture")]);
                tasks.spawn_local(async move { call.await });
            }
        }
        let mut submissions = Vec::new();
        while let Some(response) = tasks.join_next().await {
            submissions.push(response.unwrap());
        }
        println!("Concurrent submission statuses: {:?}", submissions.iter().map(|response| response.status_code().as_u16()).collect::<Vec<_>>());
        assert_eq!(submissions.len(), 16);
        for response in submissions {
            assert_eq!(response.status_code(), 303, "{}", response.text());
            let location = response.header("location").to_str().unwrap().to_owned();
            assert!(paths.iter().any(|path| location == format!("{path}/confirmation")));
        }
        for path in &paths {
            let id = path.strip_prefix("/checkout/").unwrap();
            let row = db.query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS orders, (SELECT COUNT(*) FROM tickets t JOIN orders o ON o.id = t.order_id WHERE o.hold_id = ?) AS tickets FROM orders WHERE hold_id = ?",
                [id.into(), id.into()],
            )).await.unwrap().unwrap();
            assert_eq!(row.try_get::<i64>("", "orders").unwrap(), 1);
            assert_eq!(row.try_get::<i64>("", "tickets").unwrap(), 1);
            let confirmation = request.get(&format!("{path}/confirmation")).await;
            assert_eq!(confirmation.status_code(), 200, "{}", confirmation.text());
            assert!(confirmation.text().contains("Ticket 1 of 1"));
            assert!(confirmation.text().contains("fixture@example.invalid"));
        }
        assert_eq!(
            inventory::open_counts(&db, &event, orders::now()).await.unwrap(),
            (2, 0)
        );
        let deliveries = ctx.mailer.as_ref().unwrap().deliveries();
        assert_eq!(deliveries.count, 2);
        assert!(deliveries.messages.iter().all(|message| message.contains("fixture@example.invalid")));
        let registry = super::organizer_registry(&ctx);
        let scope = registry.active_dbs().await.unwrap().remove(0);
        let cipher = registry.cipher(&scope.organizer).unwrap();
        let row = db.query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id, code FROM tickets WHERE event_id = ? ORDER BY id LIMIT 1",
            [event.clone().into()],
        )).await.unwrap().unwrap();
        let ticket_id: String = row.try_get("", "id").unwrap();
        let code: String = row.try_get("", "code").unwrap();
        let check = || crabgate::door::check_in(&db, &cipher, &event, &code, "Door Link", "fixture", orders::now());
        let (first, second) = tokio::join!(check(), check());
        let outcomes = [first.unwrap(), second.unwrap()];
        assert_eq!(outcomes.iter().filter(|result| matches!(result, crabgate::door::CheckinResult::CheckedIn(_))).count(), 1);
        assert_eq!(outcomes.iter().filter(|result| matches!(result, crabgate::door::CheckinResult::AlreadyCheckedIn(_))).count(), 1);
        let row = db.query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM checkins WHERE ticket_id = ? AND undone_at IS NULL",
            [ticket_id.into()],
        )).await.unwrap().unwrap();
        assert_eq!(row.try_get::<i64>("", "count").unwrap(), 1);
        }).await;
    }).await;
}

async fn session_of(db: &tenant_sqlite_loco::TenantHandle, hold: &str) -> Option<String> {
    db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT stripe_session_id FROM holds WHERE id = ?",
        [hold.into()],
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get("", "stripe_session_id")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn cg065_slow_session_answer_keeps_the_hold_and_a_silent_provider_releases_it() {
    let mock = mock().await;
    request::<App, _, _>(|request, ctx| async move {
        let (db, event, kind) = fixture(&ctx, "10").await;
        mock.state.lock().unwrap().delay = 6;
        let started = std::time::Instant::now();
        let response = request
            .post("/checkout")
            .form(&[
                ("event_id".into(), event.clone()),
                (format!("quantity_{kind}"), "1".into()),
            ])
            .await;
        let elapsed = started.elapsed();
        assert_eq!(response.status_code(), 303, "{}", response.text());
        let slow = mock.state.lock().unwrap().holds[0].clone();
        assert_eq!(response.header("location"), format!("/checkout/{slow}"));
        assert_eq!(
            orders::hold_details(&db, &slow).await.unwrap().status,
            "Open"
        );
        assert_eq!(session_of(&db, &slow).await.as_deref(), Some("cs_cg007_1"));
        assert_eq!(
            inventory::open_counts(&db, &event, orders::now())
                .await
                .unwrap(),
            (0, 1)
        );
        println!("answer after 6 s: HTTP 303 in {elapsed:?}, Hold Open, session attached");

        let (db, event, kind) = fixture(&ctx, "10").await;
        mock.state.lock().unwrap().delay = 13;
        let started = std::time::Instant::now();
        let response = request
            .post("/checkout")
            .form(&[
                ("event_id".into(), event.clone()),
                (format!("quantity_{kind}"), "1".into()),
            ])
            .await;
        let elapsed = started.elapsed();
        assert_eq!(response.status_code(), 502, "{}", response.text());
        assert!(response.text().contains("Payment could not start"));
        assert!(response
            .text()
            .contains("If you already paid, do not book again"));
        assert!(
            elapsed >= std::time::Duration::from_secs(10)
                && elapsed < std::time::Duration::from_secs(13),
            "{elapsed:?}"
        );
        let silent = mock.state.lock().unwrap().holds[1].clone();
        assert_eq!(
            mock.state.lock().unwrap().keys,
            vec![slow.clone(), silent.clone()]
        );
        assert_eq!(
            orders::hold_details(&db, &silent).await.unwrap().status,
            "Expired"
        );
        assert_eq!(session_of(&db, &silent).await, None);
        assert_eq!(
            inventory::open_counts(&db, &event, orders::now())
                .await
                .unwrap(),
            (0, 0)
        );
        println!(
            "no answer: HTTP 502 in {elapsed:?}, Hold Expired, capacity released, one request"
        );
    })
    .await;
}
