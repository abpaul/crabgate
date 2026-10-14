use crabgate::{
    app::App,
    booking_terms,
    events::{self, EventInput, TicketTypeInput},
    inventory, orders, questions, seats, settings,
};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

async fn event(db: &DatabaseConnection, price: &str) -> (String, String) {
    let event = events::create(
        db,
        &EventInput {
            title: "Embed test".into(),
            date: "2027-06-15".into(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Hall".into(),
            address: "1 Street".into(),
            description: String::new(),
            event_capacity: Some(4),
            max_per_order: 3,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let kind = events::add_ticket_type(
        db,
        &event,
        &TicketTypeInput {
            name: "Standard".into(),
            price: price.into(),
            capacity: 4,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(db, &event.id, orders::now()).await.unwrap();
    (event.id, kind.id)
}

async fn counts(db: &DatabaseConnection) -> (i64, i64, i64, i64) {
    let r=db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT (SELECT COUNT(*) FROM events) AS events, (SELECT COUNT(*) FROM holds) AS holds, (SELECT COUNT(*) FROM orders) AS orders, (SELECT COUNT(*) FROM tickets) AS tickets"
    )).await.unwrap().unwrap();
    (
        r.try_get("", "events").unwrap(),
        r.try_get("", "holds").unwrap(),
        r.try_get("", "orders").unwrap(),
        r.try_get("", "tickets").unwrap(),
    )
}

fn token(html: &str) -> String {
    html.split("name=\"booking_terms_token\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .into()
}

#[tokio::test]
#[serial]
async fn cg024_selector_headers_and_get_handoff_are_anonymous_read_only_and_bounded() {
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let scope = registry.active_dbs().await.unwrap().remove(0);
        let db = &scope.db;
        let (id, kind) = event(db, "12.50").await;
        let selector = format!("/embed/events/{id}");
        let handoff = format!("/events/{id}/selection");
        let before = counts(db).await;
        let stock = inventory::open_counts(db, &id, orders::now())
            .await
            .unwrap();
        let denied = request.get(&selector).await;
        assert_eq!(denied.status_code(), 200);
        assert_eq!(denied.header("x-frame-options"), "DENY");
        assert!(!denied.headers().contains_key("set-cookie"));
        settings::set_embed_origins(
            db,
            &[
                "https://Merchant.Example:443".into(),
                "https://tickets.example:8443".into(),
            ],
        )
        .await
        .unwrap();
        for _ in 0..3 {
            let response = request
                .get(&selector)
                .add_header("origin", "https://evil.example")
                .add_header("sec-fetch-site", "cross-site")
                .await;
            assert_eq!(response.status_code(), 200);
            assert!(!response.headers().contains_key("x-frame-options"));
            assert_eq!(response.header("cache-control"), "no-store");
            assert_eq!(response.header("referrer-policy"), "no-referrer");
            assert_eq!(response.header("x-content-type-options"), "nosniff");
            assert!(response
                .header("content-security-policy")
                .to_str()
                .unwrap()
                .ends_with(
                    "frame-ancestors https://merchant.example https://tickets.example:8443"
                ));
            assert!(!response
                .headers()
                .contains_key("access-control-allow-origin"));
            assert!(!response.headers().contains_key("set-cookie"));
            assert!(response.text().contains("method=\"get\""));
            assert!(response.text().contains("target=\"_top\""));
            assert!(response.text().contains("Standard quantity"));
            assert!(!response.text().contains("name=\"email\""));
            assert!(response.text().contains("€12.50"));
            let response = request.get(&format!("{handoff}?quantity_{kind}=2")).await;
            assert_eq!(response.status_code(), 200);
            assert_eq!(response.header("x-frame-options"), "DENY");
            assert!(response.text().contains("<strong>Total: €25</strong>"));
            assert!(response.text().contains("Tickets are not reserved yet"));
            assert!(response.text().contains("action=\"/checkout\""));
        }
        for query in [
            String::new(),
            format!("quantity_{kind}=-1"),
            format!("quantity_{kind}=wat"),
            format!("quantity_{kind}=101"),
            format!("quantity_{kind}=4"),
            format!("quantity_{kind}=0"),
            format!("quantity_{kind}=1&quantity_{kind}=2"),
            format!("quantity_{kind}=1&price=1"),
            format!("quantity_{kind}=1&date=2028-01-01"),
            (0..33)
                .map(|_| format!("quantity_{}=0", uuid::Uuid::new_v4()))
                .collect::<Vec<_>>()
                .join("&"),
            format!("quantity_{kind}=1&discount_code=FREE"),
            format!("quantity_{kind}=1&success_url=https://evil.example"),
            format!("quantity_{kind}=1&email=buyer@example.com"),
            format!("quantity_{}=1", uuid::Uuid::new_v4()),
            "x".repeat(4097),
        ] {
            let response = request.get(&format!("{handoff}?{query}")).await;
            assert_eq!(response.status_code(), 400, "query {query}");
            assert_eq!(response.header("x-frame-options"), "DENY");
            assert!(!response.text().contains("action=\"/checkout\""));
        }
        let (other, other_kind) = event(db, "0").await;
        assert_eq!(
            request
                .get(&format!("{handoff}?quantity_{other_kind}=1"))
                .await
                .status_code(),
            400
        );
        for path in [
            format!("{selector}/suffix"),
            format!("/embed/events/{other}/cover"),
            format!("/events/{id}"),
            handoff.clone(),
            "/office/setup".into(),
            "/operator".into(),
            "/orders/not-real".into(),
            "/checkout/not-real".into(),
        ] {
            let response = request.get(&path).await;
            assert_eq!(response.header("x-frame-options"), "DENY", "path {path}");
            assert!(response
                .header("content-security-policy")
                .to_str()
                .unwrap()
                .ends_with("frame-ancestors 'none'"));
        }
        assert_eq!(
            counts(db).await,
            (before.0 + 1, before.1, before.2, before.3)
        );
        assert_eq!(
            inventory::open_counts(db, &id, orders::now())
                .await
                .unwrap(),
            stock
        );
        for form in [
            vec![
                ("event_id".into(), id.clone()),
                (format!("quantity_{kind}"), "-1".into()),
            ],
            vec![
                ("event_id".into(), id.clone()),
                (format!("quantity_{kind}"), "1".into()),
                (format!("quantity_{kind}"), "2".into()),
            ],
            vec![
                ("event_id".into(), id.clone()),
                (format!("quantity_{kind}"), "1000000000000000000".into()),
            ],
        ] {
            assert_eq!(
                request.post("/checkout").form(&form).await.status_code(),
                400
            );
        }
        assert_eq!(
            request
                .post("/checkout")
                .add_header("origin", "https://merchant.example")
                .add_header("sec-fetch-site", "cross-site")
                .form(&[
                    ("event_id".into(), id.clone()),
                    (format!("quantity_{kind}"), "1".into())
                ])
                .await
                .status_code(),
            403
        );
        assert_eq!(counts(db).await.1, 0);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg024_free_handoff_rechecks_stock_then_uses_hosted_questions_and_terms() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = super::organizer_registry(&ctx)
            .active_dbs()
            .await
            .unwrap()
            .remove(0);
        let db = &scope.db;
        let (id, kind) = event(db, "0").await;
        let event = events::get(db, &id).await.unwrap().unwrap();
        let second = events::add_ticket_type(
            db,
            &event,
            &TicketTypeInput {
                name: "Companion".into(),
                price: "0".into(),
                capacity: 4,
                sales_open: None,
                sales_close: None,
            },
        )
        .await
        .unwrap();
        settings::set_booking_terms(db, "https://merchant.example/terms", true)
            .await
            .unwrap();
        let question_id = questions::add(db, &id, "Order", "Text", "Access needs", &[], true)
            .await
            .unwrap();
        let question = questions::list(db, &id)
            .await
            .unwrap()
            .into_iter()
            .find(|q| q.id == question_id)
            .unwrap();
        let field = questions::field_name(&question, 0);
        let handoff = format!(
            "/events/{id}/selection?quantity_{kind}=1&quantity_{}=1",
            second.id
        );
        let page = request.get(&handoff).await;
        assert_eq!(page.status_code(), 200);
        assert!(page.text().contains("Standard × 1"));
        assert!(page.text().contains("Companion × 1"));
        assert_eq!(counts(db).await, (1, 0, 0, 0));
        let competing = inventory::reserve(
            db,
            &id,
            &[inventory::Selection {
                ticket_type_id: kind.clone(),
                quantity: 3,
            }],
            orders::now(),
        )
        .await
        .unwrap();
        assert_eq!(request.get(&handoff).await.status_code(), 400);
        let form = vec![
            ("event_id".into(), id.clone()),
            (format!("quantity_{kind}"), "1".into()),
            (format!("quantity_{}", second.id), "1".into()),
        ];
        assert_eq!(
            request.post("/checkout").form(&form).await.status_code(),
            400
        );
        assert_eq!(counts(db).await.1, 1);
        orders::expire_hold(db, &competing.id).await.unwrap();
        let start = request.post("/checkout").form(&form).await;
        assert_eq!(start.status_code(), 303);
        let path = start.header("location").to_str().unwrap().to_owned();
        let hold = path.strip_prefix("/checkout/").unwrap();
        let page = request.get(&path).await;
        assert_eq!(page.status_code(), 200);
        assert_eq!(page.header("x-frame-options"), "DENY");
        assert!(page.text().contains("Access needs"));
        assert!(page.text().contains("https://merchant.example/terms"));
        let terms = token(&page.text());
        assert_eq!(
            request
                .post(&path)
                .form(&[
                    ("email", "buyer@example.com"),
                    (field.as_str(), "None"),
                    ("booking_terms_token", terms.as_str())
                ])
                .await
                .status_code(),
            400
        );
        assert_eq!(counts(db).await.2, 0);
        let submit = request
            .post(&path)
            .form(&[
                ("email", "buyer@example.com"),
                (field.as_str(), "None"),
                ("booking_terms_token", terms.as_str()),
                ("booking_terms_ack", "yes"),
            ])
            .await;
        assert_eq!(submit.status_code(), 303);
        assert_eq!(submit.header("location"), format!("{path}/confirmation"));
        let order_id = orders::order_id_for_hold(db, hold).await.unwrap().unwrap();
        let terms = booking_terms::order_evidence(db, &order_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(terms.terms.url, "https://merchant.example/terms");
        assert_eq!(counts(db).await, (1, 2, 1, 2));
        assert_eq!(
            inventory::open_counts(db, &id, orders::now())
                .await
                .unwrap(),
            (2, 0)
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg024_unavailable_seats_windows_and_invalid_origins_fail_closed() {
    request::<App,_,_>(|request,ctx| async move {
        let scope=super::organizer_registry(&ctx).active_dbs().await.unwrap().remove(0);
        let db=&scope.db;
        let (id,kind)=event(db,"0").await;
        let path=format!("/embed/events/{id}");
        settings::set_embed_origins(db,&["https://merchant.example".into()]).await.unwrap();
        for invalid in ["http://merchant.example","http://localhost:3000","https://*.example","null","https://user:password@merchant.example","https://@merchant.example","https://merchant.example/path","https://merchant.example/","https://merchant.example?x=y","https://merchant.example#fragment","https://merchant.example\r\nX: yes","https://merchant.example\\evil","https://merchant.example%2f"] {
            assert!(settings::set_embed_origins(db,&[invalid.into()]).await.is_err(),"accepted {invalid}");
        }
        assert!(settings::set_embed_origins(db,&vec!["https://merchant.example".into();17]).await.is_err());
        let before=counts(db).await;
        for (sql,notice) in [
            ("UPDATE ticket_types SET capacity = 0","Sold out"),
            ("UPDATE ticket_types SET capacity = 4, sales_open = 2000000000","Sales have not opened yet"),
            ("UPDATE ticket_types SET sales_open = NULL, sales_close = 1","Sales have ended"),
        ] {
            db.execute_raw(Statement::from_string(DatabaseBackend::Sqlite,sql)).await.unwrap();
            let page=request.get(&path).await;
            assert_eq!(page.status_code(),200);
            assert!(page.text().contains(notice));
            assert!(!page.text().contains("<form"));
            assert_eq!(request.get(&format!("/events/{id}/selection?quantity_{kind}=1")).await.status_code(),400);
        }
        db.execute_raw(Statement::from_string(DatabaseBackend::Sqlite,"UPDATE ticket_types SET sales_close = NULL")).await.unwrap();
        for (sql,status) in [("UPDATE events SET status = 'Draft'",404),("UPDATE events SET status = 'Cancelled'",409),("UPDATE events SET status = 'Published', starts_at = 1, ends_at = 2",409)] {
            db.execute_raw(Statement::from_string(DatabaseBackend::Sqlite,sql)).await.unwrap();
            let page=request.get(&path).await;
            assert_eq!(page.status_code(),status);
            assert!(!page.headers().contains_key("x-frame-options"));
            assert!(page.header("content-security-policy").to_str().unwrap().ends_with("frame-ancestors https://merchant.example"));
            assert!(page.text().contains("unavailable"));
        }
        let (seated,seated_kind)=event(db,"0").await;
        seats::add_many(db,&seated,&seated_kind,"Main","A",&["1".into()],orders::now()).await.unwrap();
        let page=request.get(&format!("/embed/events/{seated}")).await;
        assert_eq!(page.status_code(),200);
        assert!(page.header("content-security-policy").to_str().unwrap().ends_with("frame-ancestors https://merchant.example"));
        assert!(page.text().contains("uses assigned seats"));
        assert!(page.text().contains(&format!("/events/{seated}")));
        assert!(!page.text().contains("<form"));
        assert_eq!(request.post("/checkout").form(&[("event_id".into(),seated.clone()),(format!("quantity_{seated_kind}"),"1".into())]).await.status_code(),400);
        assert_eq!(counts(db).await,(before.0+1,0,0,0));
        db.execute_raw(Statement::from_string(DatabaseBackend::Sqlite,"UPDATE organizer_settings SET value = '[\"https://*.example\"]' WHERE key = 'embed_origins'")).await.unwrap();
        let (fresh,_)=event(db,"0").await;
        let page=request.get(&format!("/embed/events/{fresh}")).await;
        assert_eq!(page.status_code(),200);
        assert_eq!(page.header("x-frame-options"),"DENY");
        let fresh_event=events::get(db,&fresh).await.unwrap().unwrap();
        for index in 0..32 {
            events::add_ticket_type(db,&fresh_event,&TicketTypeInput {name:format!("Extra {index}"),price:"0".into(),capacity:4,sales_open:None,sales_close:None}).await.unwrap();
        }
        let many=request.get(&format!("/embed/events/{fresh}")).await;
        assert_eq!(many.status_code(),200);
        assert!(many.text().contains("choose from all ticket types"));
        assert!(!many.text().contains("<form"));
        assert_eq!(counts(db).await.1,0);
        settings::set_embed_origins(db,&[]).await.unwrap();
        assert_eq!(request.get(&format!("/embed/events/{fresh}")).await.header("x-frame-options"),"DENY");
    }).await;
}

#[tokio::test]
#[serial]
async fn cg024_paid_handoff_keeps_server_price_and_hosted_payment_terms_boundary() {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let _mock = super::checkout_expiry::mock().await;
    request::<App,_,_>(|request,ctx| async move {
        let registry=super::organizer_registry(&ctx);
        let scope=registry.active_dbs().await.unwrap().remove(0);
        let db=&scope.db;
        let cipher=registry.cipher(&scope.organizer).unwrap();
        settings::set(db,&cipher,"stripe_secret_key","sk_test_cg024").await.unwrap();
        settings::set(db,&cipher,"webhook_secret","whsec_cg024").await.unwrap();
        settings::set_booking_terms(db,"https://merchant.example/terms",true).await.unwrap();
        let (id,kind)=event(db,"12.50").await;
        let page=request.get(&format!("/events/{id}/selection?quantity_{kind}=2")).await;
        assert_eq!(page.status_code(),200);
        assert!(page.text().contains("<strong>Total: €25</strong>"));
        assert_eq!(counts(db).await,(1,0,0,0));
        let start=request.post("/checkout").form(&[("event_id".into(),id.clone()),(format!("quantity_{kind}"),"2".into()),("price".into(),"1".into()),("success_url".into(),"https://evil.example".into())]).await;
        assert_eq!(start.status_code(),303);
        let path=start.header("location").to_str().unwrap().to_owned();
        let hold=path.strip_prefix("/checkout/").unwrap();
        let details=orders::hold_details(db,hold).await.unwrap();
        assert_eq!(details.total_cents,2500);
        let page=request.get(&path).await;
        assert_eq!(page.header("x-frame-options"),"DENY");
        let terms=token(&page.text());
        assert_eq!(request.post(&path).form(&[("email","buyer@example.com")]).await.status_code(),400);
        let submit=request.post(&path).form(&[("email","buyer@example.com"),("booking_terms_token",terms.as_str()),("booking_terms_ack","yes")]).await;
        assert_eq!(submit.status_code(),303);
        assert_eq!(submit.header("location"),"https://checkout.stripe.com/cs_cg007_1");
        assert_eq!(counts(db).await,(1,1,0,0));
        let now=orders::now();
        let body=serde_json::json!({"type":"checkout.session.completed","data":{"object":{"id":"cs_cg007_1","payment_status":"paid","payment_intent":"pi_cg024","amount_total":2500,"currency":"eur","metadata":{"hold_id":hold}}}}).to_string();
        let mut mac=Hmac::<Sha256>::new_from_slice(b"whsec_cg024").unwrap();
        mac.update(format!("{now}.{body}").as_bytes());
        let signature=format!("t={now},v1={}",hex::encode(mac.finalize().into_bytes()));
        let webhook=request.post("/webhooks/stripe").add_header("stripe-signature",signature).add_header("content-type","application/json").text(body).await;
        assert_eq!(webhook.status_code(),200,"{}",webhook.text());
        let order=orders::order_id_for_hold(db,hold).await.unwrap().unwrap();
        assert_eq!(booking_terms::order_evidence(db,&order).await.unwrap().unwrap().terms.url,"https://merchant.example/terms");
        assert_eq!(counts(db).await,(1,1,1,2));
        assert_eq!(inventory::open_counts(db,&id,orders::now()).await.unwrap(),(2,0));
    }).await;
}

struct HostedEnvironment(Option<String>);
impl HostedEnvironment {
    fn set() -> Self {
        let prior = std::env::var("CRABGATE_EDITION").ok();
        std::env::set_var("CRABGATE_EDITION", "hosted");
        Self(prior)
    }
}
impl Drop for HostedEnvironment {
    fn drop(&mut self) {
        match &self.0 {
            Some(value) => std::env::set_var("CRABGATE_EDITION", value),
            None => std::env::remove_var("CRABGATE_EDITION"),
        }
    }
}

#[tokio::test]
#[serial]
async fn cg024_framing_and_selection_are_tenant_scoped_and_inactive_sales_fail_closed() {
    let _hosted = HostedEnvironment::set();
    request::<App,_,_>(|request,ctx| async move {
        let registry=super::organizer_registry(&ctx);
        let first=registry.create("Embed one","embed-one").await.unwrap();
        let second=registry.create("Embed two","embed-two").await.unwrap();
        for org in [&first,&second] {
            ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "INSERT INTO subscriptions (id, organizer_id, status, updated_at) VALUES (?, ?, 'Active', ?)",
                [uuid::Uuid::new_v4().to_string().into(),org.id.clone().into(),orders::now().into()]
            )).await.unwrap();
        }
        let first_db=registry.resolve("embed-one.crabgate.com").await.unwrap().db;
        let second_db=registry.resolve("embed-two.crabgate.com").await.unwrap().db;
        let (id,kind)=event(&first_db,"0").await;
        let (other,other_kind)=event(&second_db,"0").await;
        settings::set_embed_origins(&first_db,&["https://one.example".into()]).await.unwrap();
        settings::set_embed_origins(&second_db,&["https://two.example".into()]).await.unwrap();
        let before1=counts(&first_db).await;
        let before2=counts(&second_db).await;
        let page=request.get(&format!("/embed/events/{id}")).add_header("host","embed-one.crabgate.com").add_header("origin","https://two.example").add_header("referer","https://two.example/").await;
        assert_eq!(page.status_code(),200);
        assert!(page.header("content-security-policy").to_str().unwrap().ends_with("frame-ancestors https://one.example"));
        assert!(!page.text().contains("two.example"));
        assert!(page.text().contains(&format!("https://embed-one.crabgate.com/events/{id}/selection")));
        let page=request.get(&format!("/embed/events/{other}")).add_header("host","embed-two.crabgate.com").await;
        assert_eq!(page.status_code(),200);
        assert!(page.header("content-security-policy").to_str().unwrap().ends_with("frame-ancestors https://two.example"));
        let missing=request.get(&format!("/embed/events/{id}")).add_header("host","embed-two.crabgate.com").await;
        assert_eq!(missing.status_code(),404);
        assert!(!missing.headers().contains_key("x-frame-options"));
        assert!(missing.header("content-security-policy").to_str().unwrap().ends_with("frame-ancestors https://two.example"));
        let wrong=request.get(&format!("/events/{id}/selection?quantity_{other_kind}=1")).add_header("host","embed-one.crabgate.com").await;
        assert_eq!(wrong.status_code(),400);
        let selection=format!("/events/{id}/selection?quantity_{kind}=1");
        assert_eq!(request.get(&selection).add_header("host","embed-one.crabgate.com").await.status_code(),200);
        ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"UPDATE subscriptions SET status = 'Lapsed' WHERE organizer_id = ?",[first.id.into()])).await.unwrap();
        for path in [format!("/embed/events/{id}"),selection] {
            let page=request.get(&path).add_header("host","embed-one.crabgate.com").await;
            assert_eq!(page.status_code(),409);
            if path.starts_with("/embed/") {
                assert!(!page.headers().contains_key("x-frame-options"));
                assert!(page.header("content-security-policy").to_str().unwrap().ends_with("frame-ancestors https://one.example"));
            } else {
                assert_eq!(page.header("x-frame-options"),"DENY");
            }
            assert!(page.text().contains("unavailable"));
        }
        assert_eq!(request.post("/checkout").add_header("host","embed-one.crabgate.com").form(&[("event_id".into(),id),(format!("quantity_{kind}"),"1".into())]).await.status_code(),403);
        let unknown=request.get(&format!("/embed/events/{other}")).add_header("host","unknown.crabgate.com").await;
        assert_eq!(unknown.status_code(),404);
        assert_eq!(unknown.header("x-frame-options"),"DENY");
        assert_eq!(counts(&first_db).await,before1);
        assert_eq!(counts(&second_db).await,before2);
    }).await;
}

fn native_checkout_controls(html: &str) -> Vec<(String, String)> {
    fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
        tag.split_once(&format!(" {name}=\""))
            .map(|(_, value)| value.split('"').next().unwrap())
    }
    let form = html
        .split("<form")
        .find(|form| {
            let tag = form.split('>').next().unwrap();
            attribute(tag, "action") == Some("/checkout")
                && attribute(tag, "method") == Some("post")
        })
        .unwrap()
        .split("</form>")
        .next()
        .unwrap();
    form.split("<input")
        .skip(1)
        .filter_map(|input| {
            let tag = input.split('>').next().unwrap();
            if tag.split_whitespace().any(|part| part == "disabled") {
                return None;
            }
            attribute(tag, "name").map(|name| {
                (
                    name.to_owned(),
                    attribute(tag, "value").unwrap_or("").to_owned(),
                )
            })
        })
        .collect()
}

#[tokio::test]
#[serial]
async fn cg024_native_133_type_form_and_selector_fallback_reserve_one_ticket() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = super::organizer_registry(&ctx)
            .active_dbs()
            .await
            .unwrap()
            .remove(0);
        let db = &scope.db;
        let (id, kind) = event(db, "0").await;
        let event = events::get(db, &id).await.unwrap().unwrap();
        for index in 1..133 {
            events::add_ticket_type(
                db,
                &event,
                &TicketTypeInput {
                    name: format!("Type {index}"),
                    price: "0".into(),
                    capacity: 4,
                    sales_open: None,
                    sales_close: None,
                },
            )
            .await
            .unwrap();
        }
        let selector = request.get(&format!("/embed/events/{id}")).await;
        assert_eq!(selector.status_code(), 200);
        assert!(selector.text().contains("choose from all ticket types"));
        assert!(!selector.text().contains("<form"));
        let html = selector.text();
        let fallback = html
            .split("<a href=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let fallback = reqwest::Url::parse(fallback).unwrap();
        assert_eq!(fallback.path(), format!("/events/{id}"));
        let page = request.get(fallback.path()).await;
        assert_eq!(page.status_code(), 200);
        assert_eq!(page.header("x-frame-options"), "DENY");
        let mut form = native_checkout_controls(&page.text());
        assert_eq!(form.len(), 135);
        assert_eq!(
            form.iter()
                .filter(|(name, _)| name.starts_with("quantity_"))
                .count(),
            133
        );
        assert!(form.contains(&("event_id".into(), id.clone())));
        assert!(form.contains(&("discount_code".into(), String::new())));
        let chosen = form
            .iter_mut()
            .find(|(name, _)| name == &format!("quantity_{kind}"))
            .unwrap();
        assert_eq!(chosen.1, "0");
        chosen.1 = "1".into();
        assert_eq!(
            form.iter()
                .filter(|(name, value)| name.starts_with("quantity_") && value == "0")
                .count(),
            132
        );
        assert_eq!(counts(db).await, (1, 0, 0, 0));
        let response = request.post("/checkout").form(&form).await;
        assert_eq!(response.status_code(), 303, "{}", response.text());
        let path = response.header("location").to_str().unwrap().to_owned();
        let hold = orders::hold_details(db, path.strip_prefix("/checkout/").unwrap())
            .await
            .unwrap();
        assert_eq!(hold.items.len(), 1);
        assert_eq!(hold.items[0].ticket_type_id, kind);
        assert_eq!(hold.items[0].quantity, 1);
        assert_eq!(hold.total_cents, 0);
        assert_eq!(counts(db).await, (1, 1, 0, 0));
        assert_eq!(
            inventory::open_counts(db, &id, orders::now())
                .await
                .unwrap(),
            (0, 1)
        );
        assert_eq!(request.get(&path).await.status_code(), 200);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg024_native_form_rejects_invalid_controls_and_enforces_body_limit() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = super::organizer_registry(&ctx)
            .active_dbs()
            .await
            .unwrap()
            .remove(0);
        let db = &scope.db;
        let (id, kind) = event(db, "0").await;
        let (_, other_kind) = event(db, "0").await;
        let page = request.get(&format!("/events/{id}")).await;
        assert_eq!(page.status_code(), 200);
        let mut valid = native_checkout_controls(&page.text());
        valid
            .iter_mut()
            .find(|(key, _)| key == &format!("quantity_{kind}"))
            .unwrap()
            .1 = "1".into();
        let unknown = uuid::Uuid::new_v4().to_string();
        let mut bad_forms = Vec::new();
        for extra in [
            ("event_id".to_owned(), id.clone()),
            ("discount_code".into(), String::new()),
            (format!("quantity_{kind}"), "0".into()),
            (format!("quantity_{unknown}"), "0".into()),
            (format!("quantity_{other_kind}"), "0".into()),
            ("k".repeat(65), "0".into()),
            ("discount_code".into(), "x".repeat(129)),
        ] {
            let mut form = valid.clone();
            if extra.1.len() > 128 {
                form.retain(|(key, _)| key != "discount_code");
            }
            form.push(extra);
            bad_forms.push(form);
        }
        for value in ["-1", "101", "wat", "9223372036854775808"] {
            let mut form = valid.clone();
            form.iter_mut()
                .find(|(key, _)| key == &format!("quantity_{kind}"))
                .unwrap()
                .1 = value.into();
            bad_forms.push(form);
        }
        for form in bad_forms {
            let response = request.post("/checkout").form(&form).await;
            assert_eq!(response.status_code(), 400, "{form:?}");
            assert_eq!(response.header("x-frame-options"), "DENY");
            assert_eq!(counts(db).await, (2, 0, 0, 0));
        }
        assert_eq!(counts(db).await, (2, 0, 0, 0));
        assert_eq!(
            inventory::open_counts(db, &id, orders::now())
                .await
                .unwrap(),
            (0, 0)
        );
        assert_eq!(
            request.post("/checkout").form(&valid).await.status_code(),
            303
        );
        assert_eq!(counts(db).await, (2, 1, 0, 0));
    })
    .await;
    use tower::ServiceExt;
    let boot = boot_test::<App>().await.unwrap();
    let scope = super::organizer_registry(&boot.app_context)
        .active_dbs()
        .await
        .unwrap()
        .remove(0);
    let router = boot.router.unwrap();
    for (size, status) in [(2_000_000, 400), (2_000_001, 413), (3_500_001, 413)] {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/checkout")
                    .header("host", "localhost")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("content-length", size)
                    .body(axum::body::Body::from("x".repeat(size)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status, "body size {size}");
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        assert_eq!(counts(&scope.db).await, (0, 0, 0, 0));
    }
}

#[tokio::test]
#[serial]
async fn cg024_native_100_selected_seats_remain_supported() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = super::organizer_registry(&ctx)
            .active_dbs()
            .await
            .unwrap()
            .remove(0);
        let db = &scope.db;
        let event = events::create(
            db,
            &EventInput {
                title: "100 seats".into(),
                date: "2027-06-15".into(),
                start_time: "19:00".into(),
                end_time: "21:00".into(),
                timezone: "Europe/Dublin".into(),
                venue: "Hall".into(),
                address: "1 Street".into(),
                description: String::new(),
                event_capacity: Some(100),
                max_per_order: 100,
                attendee_name_required: false,
            },
        )
        .await
        .unwrap();
        let kind = events::add_ticket_type(
            db,
            &event,
            &TicketTypeInput {
                name: "Seated".into(),
                price: "0".into(),
                capacity: 100,
                sales_open: None,
                sales_close: None,
            },
        )
        .await
        .unwrap();
        seats::add_many(
            db,
            &event.id,
            &kind.id,
            "Main",
            "A",
            &(1..=100).map(|n| n.to_string()).collect::<Vec<_>>(),
            orders::now(),
        )
        .await
        .unwrap();
        events::publish(db, &event.id, orders::now()).await.unwrap();
        let selector = request.get(&format!("/embed/events/{}", event.id)).await;
        assert_eq!(selector.status_code(), 200);
        assert!(selector.text().contains("uses assigned seats"));
        assert!(!selector.text().contains("<form"));
        let page = request.get(&format!("/events/{}", event.id)).await;
        assert_eq!(page.status_code(), 200);
        let form = native_checkout_controls(&page.text());
        assert_eq!(form.len(), 102);
        assert_eq!(
            form.iter()
                .filter(|(key, value)| key.starts_with("seat_") && value == "1")
                .count(),
            100
        );
        let response = request.post("/checkout").form(&form).await;
        assert_eq!(response.status_code(), 303, "{}", response.text());
        let path = response.header("location").to_str().unwrap().to_owned();
        let hold = orders::hold_details(db, path.strip_prefix("/checkout/").unwrap())
            .await
            .unwrap();
        assert_eq!(hold.items.len(), 1);
        assert_eq!(hold.items[0].quantity, 100);
        assert_eq!(counts(db).await, (1, 1, 0, 0));
        assert_eq!(
            inventory::open_counts(db, &event.id, orders::now())
                .await
                .unwrap(),
            (0, 100)
        );
    })
    .await;
}

const CG059_ALLOWED: &str = "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; font-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors https://merchant.example https://tickets.example:8443";
const CG059_DENIED: &str = "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; font-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'";

macro_rules! cg059_response {
    ($page:expr, $status:expr, $policy:expr) => {{
        let page = &$page;
        assert_eq!(page.status_code(), $status, "{}", page.text());
        assert_eq!(page.header("content-security-policy"), $policy);
        assert_eq!(
            page.headers().contains_key("x-frame-options"),
            $policy == CG059_DENIED
        );
        if $policy == CG059_DENIED {
            assert_eq!(page.header("x-frame-options"), "DENY");
        }
        assert!(!page.headers().contains_key("set-cookie"));
        assert!(!page.headers().contains_key("access-control-allow-origin"));
        assert_eq!(page.header("referrer-policy"), "no-referrer");
        assert_eq!(page.header("x-content-type-options"), "nosniff");
        for secret in [
            "sk_test_CG059_PRIVATE",
            "CG059_DRAFT_TITLE",
            "CG059_PRIVATE_TICKET",
            "buyer-private@example.com",
            "<iframe",
            "checkout.stripe.com",
        ] {
            assert!(!page.text().contains(secret), "disclosed {secret}");
        }
    }};
}

#[tokio::test]
#[serial]
async fn cg059_event_series_get_head_safe_states_use_exact_configured_origins() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = super::organizer_registry(&ctx).active_dbs().await.unwrap().remove(0);
        let db = &scope.db;
        let (id, _) = event(db, "12.50").await;
        let series = crabgate::series::start(db, &id, crabgate::series::Frequency::Weekly, None, &Ok(()), orders::now()).await.unwrap();
        settings::set_embed_origins(db, &["https://Merchant.Example:443".into(), "https://tickets.example:8443".into()]).await.unwrap();
        let before = counts(db).await;
        let event_path = format!("/embed/events/{id}");
        let series_path = format!("/embed/series/{series}");
        macro_rules! probe {
            ($path:expr, $status:expr, $notice:expr) => {{
                let path = $path;
                for source in [None, Some("https://merchant.example"), Some("https://evil.example")] {
                    let mut get = request.get(&path);
                    let mut head = request.method(axum::http::Method::HEAD, &path);
                    if let Some(origin) = source {
                        get = get.add_header("origin", origin).add_header("referer", format!("{origin}/"));
                        head = head.add_header("origin", origin).add_header("referer", format!("{origin}/"));
                    }
                    let page = get.await;
                    cg059_response!(page, $status, CG059_ALLOWED);
                    assert_eq!(page.header("cache-control"), "no-store");
                    assert!(page.text().contains($notice), "{}", page.text());
                    if $status != 200 { assert!(!page.text().contains("Continue on CrabGate</button>")); }
                    let empty = head.await;
                    cg059_response!(empty, $status, CG059_ALLOWED);
                    assert_eq!(empty.header("cache-control"), "no-store");
                    assert_eq!(empty.text(), "");
                    assert_eq!(empty.header("content-type"), page.header("content-type"));
                    println!("CG059 {path} source={source:?} GET={} HEAD={} headers={:?} body={}", page.status_code(), empty.status_code(), page.headers(), page.text());
                }
            }};
        }
        probe!(event_path.clone(), 200, "Standard quantity");
        probe!(series_path.clone(), 200, "Choose a workshop date");
        probe!(format!("{event_path}?email=buyer-private@example.com"), 400, "Invalid ticket selection");
        probe!(format!("{event_path}?origin=https://evil.example"), 400, "Invalid ticket selection");
        probe!(format!("{series_path}?event_id=bad"), 400, "Invalid date selection");
        probe!(format!("{series_path}?event_id={id}&event_id={id}"), 400, "Invalid date selection");
        probe!(format!("/embed/events/{}", uuid::Uuid::new_v4()), 404, "Event unavailable");
        probe!(format!("/embed/series/{}", uuid::Uuid::new_v4()), 404, "Series unavailable");
        for (sql, notice) in [
            ("UPDATE ticket_types SET capacity = 0", "Sold out"),
            ("UPDATE ticket_types SET capacity = 4, sales_open = 9999999999", "Sales have not opened yet"),
            ("UPDATE ticket_types SET sales_open = NULL, sales_close = 1", "Sales have ended"),
        ] {
            db.execute_unprepared(sql).await.unwrap();
            probe!(event_path.clone(), 200, notice);
            probe!(format!("{series_path}?event_id={id}"), 200, notice);
        }
        db.execute_unprepared("UPDATE ticket_types SET sales_close = NULL").await.unwrap();
        for sql in [
            format!("UPDATE events SET starts_at = {}, ends_at = {} WHERE id = '{id}'", orders::now() - 1, orders::now() + 3600),
            format!("UPDATE events SET starts_at = 1, ends_at = 2 WHERE id = '{id}'"),
            format!("UPDATE events SET status = 'Cancelled' WHERE id = '{id}'"),
        ] {
            db.execute_unprepared(&sql).await.unwrap();
            probe!(event_path.clone(), 409, "Tickets are unavailable");
            probe!(format!("{series_path}?event_id={id}"), 400, "Invalid date selection");
        }
        db.execute_unprepared(&format!("UPDATE events SET status = 'Draft', title = 'CG059_DRAFT_TITLE' WHERE id = '{id}'; UPDATE ticket_types SET name = 'CG059_PRIVATE_TICKET', price_cents = 7123 WHERE event_id = '{id}'")).await.unwrap();
        probe!(event_path, 404, "Event unavailable");
        probe!(format!("{series_path}?event_id={id}"), 400, "Invalid date selection");
        assert_eq!(counts(db).await, before);
    }).await;
}

#[tokio::test]
#[serial]
async fn cg059_selector_errors_require_readable_exact_origin_configuration() {
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let scope = registry.active_dbs().await.unwrap().remove(0);
        let db = &scope.db;
        let (id, _) = event(db, "0").await;
        let paths = [format!("/embed/events/{id}"), format!("/embed/series/{}", uuid::Uuid::new_v4())];
        settings::set(db, &registry.cipher(&scope.organizer).unwrap(), "stripe_secret_key", "sk_test_CG059_PRIVATE").await.unwrap();
        settings::set_embed_origins(db, &["https://merchant.example".into(), "https://tickets.example:8443".into()]).await.unwrap();
        db.execute_unprepared("ALTER TABLE events RENAME TO cg059_saved_events").await.unwrap();
        for configuration in ["approved", "missing", "corrupt", "wildcard", "failed-read"] {
            match configuration {
                "missing" => { db.execute_unprepared("DELETE FROM organizer_settings WHERE key = 'embed_origins'").await.unwrap(); }
                "corrupt" => { db.execute_unprepared("INSERT INTO organizer_settings (key, value) VALUES ('embed_origins', 'not-json')").await.unwrap(); }
                "wildcard" => { db.execute_unprepared("UPDATE organizer_settings SET value = '[\"https://*.example\"]' WHERE key = 'embed_origins'").await.unwrap(); }
                "failed-read" => { db.execute_unprepared("ALTER TABLE organizer_settings RENAME TO cg059_saved_settings").await.unwrap(); }
                _ => {}
            }
            for path in &paths {
                let page = request.get(path).await;
                let empty = request.method(axum::http::Method::HEAD, path).await;
                let policy = if configuration == "approved" { CG059_ALLOWED } else { CG059_DENIED };
                cg059_response!(page, 500, policy);
                cg059_response!(empty, 500, policy);
                assert_eq!(empty.text(), "");
                assert!(!page.text().contains("cg059_saved_events"));
                assert!(!page.text().contains("organizer_settings"));
                if configuration != "failed-read" {
                    assert!(page.text().contains("Tickets are unavailable"));
                    assert!(page.text().contains("target=\"_top\""));
                    assert!(page.text().contains("target=\"_blank\" rel=\"noopener\""));
                    assert!(!page.text().contains("<form"));
                }
                println!("CG059 error config={configuration} {path} GET={} HEAD={} headers={:?} body={}", page.status_code(), empty.status_code(), page.headers(), page.text());
            }
        }
        db.execute_unprepared("ALTER TABLE cg059_saved_settings RENAME TO organizer_settings; ALTER TABLE cg059_saved_events RENAME TO events").await.unwrap();
    }).await;
}

#[tokio::test]
#[serial]
async fn cg059_hosted_lapse_and_tenant_route_method_boundaries() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let first = registry.create("CG059 one", "cg059-one").await.unwrap();
        let second = registry.create("CG059 two", "cg059-two").await.unwrap();
        for org in [&first, &second] {
            ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "INSERT INTO subscriptions (id, organizer_id, status, updated_at) VALUES (?, ?, 'Active', ?)", [uuid::Uuid::new_v4().to_string().into(), org.id.clone().into(), orders::now().into()])).await.unwrap();
        }
        let scope = registry.resolve("cg059-one.crabgate.com").await.unwrap();
        let second_scope = registry.resolve("cg059-two.crabgate.com").await.unwrap();
        let (id, kind) = event(&scope.db, "12.50").await;
        let series = crabgate::series::start(&scope.db, &id, crabgate::series::Frequency::Weekly, None, &Ok(()), orders::now()).await.unwrap();
        settings::set_embed_origins(&scope.db, &["https://merchant.example".into(), "https://tickets.example:8443".into()]).await.unwrap();
        let before = counts(&scope.db).await;
        ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE subscriptions SET status = 'Lapsed' WHERE organizer_id = ?", [first.id.clone().into()])).await.unwrap();
        let paths = [format!("/embed/events/{id}"), format!("/embed/series/{series}?event_id={id}")];
        for path in &paths {
            let page = request.get(path).add_header("host", "cg059-one.crabgate.com").await;
            let empty = request.method(axum::http::Method::HEAD, path).add_header("host", "cg059-one.crabgate.com").await;
            cg059_response!(page, 409, CG059_ALLOWED);
            cg059_response!(empty, 409, CG059_ALLOWED);
            assert_eq!(empty.text(), "");
            assert!(page.text().contains("unavailable"));
            assert!(!page.text().contains("Standard"));
            assert!(!page.text().contains("€12.50"));
            for host in ["unknown.crabgate.com", "crabgate.com", "operator.crabgate.com", "help.crabgate.com", "evil.example", "cg059-one.evil.example", "cg059-two.crabgate.com"] {
                let page = request.get(path).add_header("host", host).add_header("origin", "https://merchant.example").await;
                let empty = request.method(axum::http::Method::HEAD, path).add_header("host", host).await;
                cg059_response!(page, 404, CG059_DENIED);
                cg059_response!(empty, 404, CG059_DENIED);
                assert_eq!(empty.text(), "");
                assert!(!page.text().contains("CG059 one"));
            }
        }
        for path in [format!("/embed/events/{id}/unknown"), format!("/embed/events/{}", id.to_uppercase()), "/embed/not-real".into(), format!("/events/{id}/selection?quantity_{kind}=1"), "/office/setup".into(), "/operator".into(), "/checkout/not-real".into(), "/orders/not-real".into()] {
            for head in [false, true] {
                let page = if head { request.method(axum::http::Method::HEAD, &path).add_header("host", "cg059-one.crabgate.com").await } else { request.get(&path).add_header("host", "cg059-one.crabgate.com").await };
                assert_eq!(page.header("content-security-policy"), CG059_DENIED);
                assert_eq!(page.header("x-frame-options"), "DENY");
                if head { assert_eq!(page.text(), ""); }
            }
        }
        for path in &paths {
            let page = request.post(path).add_header("host", "cg059-one.crabgate.com").add_header("origin", "https://merchant.example").await;
            cg059_response!(page, 403, CG059_DENIED);
        }
        let page = request.post("/checkout").add_header("host", "cg059-one.crabgate.com").form(&[("event_id".into(), id), (format!("quantity_{kind}"), "1".into())]).await;
        cg059_response!(page, 403, CG059_DENIED);
        assert_eq!(counts(&scope.db).await, before);
        assert_eq!(counts(&second_scope.db).await, (0, 0, 0, 0));
        for (change, status) in [
            ("suspension_reason = 'CG059 synthetic suspension'", 503),
            ("suspension_reason = NULL, data_key = NULL", 404),
        ] {
            ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                format!("UPDATE organizers SET {change} WHERE id = ?"), [first.id.clone().into()]
            )).await.unwrap();
            for path in &paths {
                let page = request.get(path).add_header("host", "cg059-one.crabgate.com").await;
                let empty = request.method(axum::http::Method::HEAD, path).add_header("host", "cg059-one.crabgate.com").await;
                cg059_response!(page, status, CG059_DENIED);
                cg059_response!(empty, status, CG059_DENIED);
                assert_eq!(empty.text(), "");
                assert!(!page.text().contains("CG059 synthetic suspension"));
            }
        }
    }).await;
}

#[tokio::test]
#[serial]
async fn cg059_same_baseline_flow_now_frames_only_configured_merchant() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = super::organizer_registry(&ctx)
            .active_dbs()
            .await
            .unwrap()
            .remove(0);
        let (id, _) = event(&scope.db, "12.50").await;
        settings::set_embed_origins(&scope.db, &["https://merchant.example".into()])
            .await
            .unwrap();
        scope
            .db
            .execute_unprepared("UPDATE events SET status = 'Cancelled'")
            .await
            .unwrap();
        for (path, status) in [
            (format!("/embed/events/{id}"), 409),
            (format!("/embed/events/{id}?bad=1"), 400),
            (format!("/embed/series/{}", uuid::Uuid::new_v4()), 404),
        ] {
            for method in ["GET", "HEAD"] {
                let response = if method == "GET" {
                    request.get(&path).await
                } else {
                    request.method(axum::http::Method::HEAD, &path).await
                };
                assert_eq!(response.status_code(), status);
                assert!(!response.headers().contains_key("x-frame-options"));
                assert!(response
                    .header("content-security-policy")
                    .to_str()
                    .unwrap()
                    .ends_with("frame-ancestors https://merchant.example"));
                assert!(!response.headers().contains_key("set-cookie"));
                if method == "HEAD" {
                    assert_eq!(response.text(), "");
                } else {
                    assert!(
                        response.text().contains("unavailable")
                            || response.text().contains("Invalid ticket selection")
                    );
                }
                println!(
                    "FINAL-SAME-FLOW {method} {path} status={} headers={:?} body={}",
                    response.status_code(),
                    response.headers(),
                    response.text()
                );
            }
        }
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg059_public_series_get_head_operation_error_is_safe_denied_and_no_store() {
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let scope = registry.active_dbs().await.unwrap().remove(0);
        let db = &scope.db;
        let (id, _) = event(db, "12.50").await;
        let series = crabgate::series::start(
            db,
            &id,
            crabgate::series::Frequency::Weekly,
            None,
            &Ok(()),
            orders::now(),
        )
        .await
        .unwrap();
        settings::set(
            db,
            &registry.cipher(&scope.organizer).unwrap(),
            "stripe_secret_key",
            "sk_test_CG059_PRIVATE",
        )
        .await
        .unwrap();
        settings::set_embed_origins(db, &["https://merchant.example".into()])
            .await
            .unwrap();
        let path = format!("/series/{series}");
        let normal = request.get(&path).await;
        cg059_response!(normal, 200, CG059_DENIED);
        assert_eq!(normal.header("cache-control"), "no-store");
        let before = counts(db).await;
        db.execute_unprepared("ALTER TABLE events RENAME TO cg059_public_series_saved_events")
            .await
            .unwrap();
        for method in [axum::http::Method::GET, axum::http::Method::HEAD] {
            let response = request.method(method.clone(), &path).await;
            cg059_response!(response, 500, CG059_DENIED);
            assert_eq!(response.header("cache-control"), "no-store");
            assert!(response
                .header("content-type")
                .to_str()
                .unwrap()
                .starts_with("text/html"));
            if method == axum::http::Method::HEAD {
                assert_eq!(response.text(), "");
            } else {
                assert!(response
                    .text()
                    .contains("Tickets are unavailable right now"));
                assert!(!response.text().contains("cg059_public_series_saved_events"));
                assert!(!response.text().contains("organizer_settings"));
                assert!(!response.text().contains("<form"));
                assert!(!response.text().contains("Embed test"));
                assert!(!response.text().contains("Standard"));
            }
            println!(
                "CG059-PUBLIC-CACHE-TCP {method} {path} status={} headers={:?} body={}",
                response.status_code(),
                response.headers(),
                response.text()
            );
        }
        db.execute_unprepared("ALTER TABLE cg059_public_series_saved_events RENAME TO events")
            .await
            .unwrap();
        assert_eq!(counts(db).await, before);
    })
    .await;
}
