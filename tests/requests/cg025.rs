use crabgate::{
    app::App,
    booking_terms,
    events::{self, EventInput, TicketTypeInput},
    inventory, orders, questions, seats,
    series::{self, Frequency},
    settings,
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

async fn start_series(db: &DatabaseConnection, id: &str) -> String {
    series::start(db, id, Frequency::Weekly, None, &Ok(()), orders::now())
        .await
        .unwrap()
}

async fn dates(db: &DatabaseConnection, id: &str) -> Vec<String> {
    series::published_dates(db, id, orders::now())
        .await
        .unwrap()
        .unwrap()
        .into_iter()
        .map(|event| event.id)
        .collect()
}

async fn sql(db: &DatabaseConnection, statement: &str, id: &str) {
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        statement,
        [id.into()],
    ))
    .await
    .unwrap();
}

#[tokio::test]
#[serial]
async fn cg025_dates_are_ordered_existing_public_only_and_gets_do_not_generate() {
    request::<App,_,_>(|request,ctx| async move {
        let scope=super::organizer_registry(&ctx).active_dbs().await.unwrap().remove(0);
        let db=&scope.db;
        let (id,kind)=event(db,"0").await;
        let series=start_series(db,&id).await;
        let original=dates(db,&series).await;
        assert_eq!(original.len(),8);
        let extra=series::copy(db,&id,"2027-08-10",orders::now()).await.unwrap();
        events::publish(db,&extra.id,orders::now()).await.unwrap();
        db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"UPDATE events SET series_id = ?, series_date = date WHERE id = ?",[series.clone().into(),extra.id.clone().into()])).await.unwrap();
        let path=format!("/embed/series/{series}");
        settings::set_embed_origins(db,&["https://merchant.example".into()]).await.unwrap();
        let before=counts(db).await;
        assert_eq!(before,(9,0,0,0));
        let all=dates(db,&series).await;
        assert_eq!(all.len(),9);
        let response=request.get(&path).await;
        assert_eq!(response.status_code(),200);
        let html=response.text();
        assert_eq!(html.matches("<option value=").count(),10);
        let positions=all.iter().map(|id| html.find(&format!("<option value=\"{id}\"")).unwrap()).collect::<Vec<_>>();
        assert!(positions.windows(2).all(|p|p[0]<p[1]));
        assert!(html.contains("Tue 15 June 2027 · 19:00 · Europe/Dublin · Hall · Tickets available"));
        assert!(html.contains("target=\"_self\""));
        assert!(!html.contains(&format!("name=\"quantity_{kind}\"")));
        sql(db,"UPDATE events SET status = 'Draft', title = 'PRIVATE DRAFT' WHERE id = ?",&original[1]).await;
        sql(db,"UPDATE events SET status = 'Cancelled', title = 'PRIVATE CANCELLED' WHERE id = ?",&original[2]).await;
        sql(db,"UPDATE events SET starts_at = 1, title = 'PRIVATE PAST' WHERE id = ?",&original[3]).await;
        db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"INSERT INTO series_exceptions (series_id,date,reason,setup_json) SELECT id,'2027-09-01','PRIVATE EXCEPTION',setup_json FROM series WHERE id = ?",[series.clone().into()])).await.unwrap();
        let horizon=db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"SELECT created_through FROM series WHERE id = ?",[series.clone().into()])).await.unwrap().unwrap().try_get::<String>("","created_through").unwrap();
        for _ in 0..3 {
            for query in [String::new(),format!("?event_id={id}")] {
                let page=request.get(&format!("{path}{query}")).add_header("origin","https://evil.example").add_header("referer","https://evil.example").await;
                assert_eq!(page.status_code(),200);
                assert_eq!(page.header("cache-control"),"no-store");
                assert_eq!(page.header("referrer-policy"),"no-referrer");
                assert_eq!(page.header("x-content-type-options"),"nosniff");
                assert!(!page.headers().contains_key("x-frame-options"));
                assert!(!page.headers().contains_key("set-cookie"));
                assert!(!page.headers().contains_key("access-control-allow-origin"));
                assert!(page.header("content-security-policy").to_str().unwrap().ends_with("frame-ancestors https://merchant.example"));
                let text=page.text();
                assert!(!text.contains("PRIVATE"));
                for unavailable in &original[1..4] { assert!(!text.contains(unavailable)); }
                if !query.is_empty() {assert!(text.contains(&format!("name=\"quantity_{kind}\"")));}
            }
        }
        let head=request.method(axum::http::Method::HEAD,&format!("{path}?event_id={id}")).await;
        assert_eq!(head.status_code(),200);
        assert_eq!(head.text(),"");
        assert!(!head.headers().contains_key("x-frame-options"));
        let hosted=request.get(&format!("/series/{series}?event_id={id}")).await;
        assert_eq!(hosted.status_code(),200);
        assert_eq!(hosted.header("x-frame-options"),"DENY");
        assert!(hosted.text().contains(&format!("/events/{id}/selection")));
        series::stop(db,&id,orders::now()).await.unwrap();
        assert_eq!(request.get(&format!("{path}?event_id={id}")).await.status_code(),200);
        assert_eq!(request.get(&format!("/events/{id}/selection?quantity_{kind}=1")).await.status_code(),200);
        for query in [format!("event_id={}",original[1]),format!("event_id={}",original[2]),format!("event_id={}",original[3]),format!("event_id={id}&event_id={id}"),format!("event_id={}",id.to_uppercase()),"event_id=bad".into(),"event_id=".into(),format!("event_id={id}&origin=https://evil.example"),format!("event_id={id}&quantity_{kind}=1"),"x".repeat(129)] {
            let rejected=request.get(&format!("{path}?{query}")).await;
            assert_eq!(rejected.status_code(),400,"{query}");
            assert!(!rejected.headers().contains_key("x-frame-options"));
            assert!(rejected.header("content-security-policy").to_str().unwrap().ends_with("frame-ancestors https://merchant.example"));
            assert!(!rejected.text().contains("Continue on CrabGate</button>"));
        }
        let unknown_series=format!("/embed/series/{}",uuid::Uuid::new_v4());
        let unavailable=request.get(&unknown_series).await;
        assert_eq!(unavailable.status_code(),404);
        assert!(!unavailable.headers().contains_key("x-frame-options"));
        assert!(unavailable.header("content-security-policy").to_str().unwrap().ends_with("frame-ancestors https://merchant.example"));
        for bad in [format!("{path}/suffix"),format!("{path}/"),format!("/embed/series/{}",series.to_uppercase())] {
            let page=request.get(&bad).await;
            assert_eq!(page.status_code(),404);
            assert_eq!(page.header("x-frame-options"),"DENY");
        }
        assert_eq!(request.post(&path).await.status_code(),405);
        assert_eq!(counts(db).await,before);
        assert_eq!(inventory::open_counts(db,&id,orders::now()).await.unwrap(),(0,0));
        assert_eq!(db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"SELECT created_through FROM series WHERE id = ?",[series.clone().into()])).await.unwrap().unwrap().try_get::<String>("","created_through").unwrap(),horizon);
        assert_eq!(db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,"SELECT COUNT(*) AS n FROM series_exceptions")).await.unwrap().unwrap().try_get::<i64>("","n").unwrap(),1);
    }).await;
}

#[tokio::test]
#[serial]
async fn cg025_stock_windows_empty_and_stale_dates_fail_closed() {
    request::<App,_,_>(|request,ctx| async move {
        let scope=super::organizer_registry(&ctx).active_dbs().await.unwrap().remove(0);
        let db=&scope.db;
        let (id,kind)=event(db,"0").await;
        let series=start_series(db,&id).await;
        let all=dates(db,&series).await;
        let path=format!("/embed/series/{series}");
        settings::set_embed_origins(db,&["https://merchant.example".into()]).await.unwrap();
        sql(db,"UPDATE ticket_types SET capacity = 0 WHERE event_id = ?",&id).await;
        sql(db,"UPDATE ticket_types SET sales_open = 9999999999 WHERE event_id = ?",&all[1]).await;
        sql(db,"UPDATE ticket_types SET sales_close = 1 WHERE event_id = ?",&all[2]).await;
        let page=request.get(&path).await;
        assert!(page.text().contains("Sold out"));
        assert!(page.text().contains("Sales have not opened yet"));
        assert!(page.text().contains("Sales have ended"));
        for date in &all[..3] {
            let selected=request.get(&format!("{path}?event_id={date}")).await;
            assert_eq!(selected.status_code(),200);
            assert!(!selected.text().contains("Continue on CrabGate</button>"));
        }
        sql(db,"UPDATE ticket_types SET capacity = 4 WHERE event_id = ?",&id).await;
        let competing=inventory::reserve(db,&id,&[inventory::Selection{ticket_type_id:kind.clone(),quantity:3}],orders::now()).await.unwrap();
        let limited=request.get(&format!("{path}?event_id={id}")).await;
        assert!(limited.text().contains("max=\"1\""));
        assert_eq!(request.get(&format!("/events/{id}/selection?quantity_{kind}=2")).await.status_code(),400);
        orders::expire_hold(db,&competing.id).await.unwrap();
        assert_eq!(request.get(&format!("/events/{id}/selection?quantity_{kind}=1")).await.status_code(),200);
        seats::add_many(db,&all[3],&events::list_ticket_types(db,&all[3]).await.unwrap()[0].id,"Main","A",&["1".into()],orders::now()).await.unwrap();
        let seated=request.get(&format!("{path}?event_id={}",all[3])).await;
        assert!(seated.text().contains("uses assigned seats"));
        assert!(!seated.text().contains("Continue on CrabGate</button>"));
        let many_event=events::get(db,&all[4]).await.unwrap().unwrap();
        for index in 0..32 {events::add_ticket_type(db,&many_event,&TicketTypeInput{name:format!("Extra {index}"),price:"0".into(),capacity:4,sales_open:None,sales_close:None}).await.unwrap();}
        let many=request.get(&format!("{path}?event_id={}",all[4])).await;
        assert!(many.text().contains("choose from all ticket types"));
        assert!(!many.text().contains("Continue on CrabGate</button>"));
        sql(db,"UPDATE events SET status = 'Cancelled' WHERE id = ?",&id).await;
        assert_eq!(request.get(&format!("{path}?event_id={id}")).await.status_code(),400);
        assert_eq!(request.get(&format!("/events/{id}/selection?quantity_{kind}=1")).await.status_code(),409);
        assert_eq!(request.post("/checkout").form(&[("event_id".into(),id.clone()),(format!("quantity_{kind}"),"1".into())]).await.status_code(),400);
        sql(db,"UPDATE events SET status = 'Draft' WHERE series_id = ?",&series).await;
        series::stop(db,&id,orders::now()).await.unwrap();
        let empty=request.get(&path).await;
        assert_eq!(empty.status_code(),200);
        assert!(empty.text().contains("No published future dates"));
        assert!(!empty.text().contains("name=\"event_id\""));
        assert_eq!(counts(db).await,(8,1,0,0));
        settings::set_embed_origins(db,&[]).await.unwrap();
        assert_eq!(request.get(&path).await.header("x-frame-options"),"DENY");
        db.execute_raw(Statement::from_string(DatabaseBackend::Sqlite,"INSERT INTO organizer_settings (key,value) VALUES ('embed_origins','[\"https://*.example\"]') ON CONFLICT(key) DO UPDATE SET value=excluded.value")).await.unwrap();
        assert_eq!(request.get(&path).await.header("x-frame-options"),"DENY");
    }).await;
}
#[tokio::test]
#[serial]
async fn cg025_free_handoff_rechecks_stock_then_uses_hosted_questions_and_terms() {
    request::<App, _, _>(|request, ctx| async move {
        let scope = super::organizer_registry(&ctx)
            .active_dbs()
            .await
            .unwrap()
            .remove(0);
        let db = &scope.db;
        let (id, kind) = event(db, "0").await;
        let series = start_series(db, &id).await;
        let other = dates(db, &series).await[1].clone();
        let other_kind = events::list_ticket_types(db, &other).await.unwrap()[0]
            .id
            .clone();
        let picker = request
            .get(&format!("/embed/series/{series}?event_id={id}"))
            .await;
        assert_eq!(picker.status_code(), 200);
        assert!(picker.text().contains(&format!("/events/{id}/selection")));
        assert!(!picker
            .text()
            .contains(&format!("name=\"quantity_{other_kind}\"")));
        let before = counts(db).await;
        assert_eq!(
            request
                .get(&format!("/events/{id}/selection?quantity_{other_kind}=1"))
                .await
                .status_code(),
            400
        );
        assert_eq!(
            request
                .post("/checkout")
                .form(&[
                    ("event_id".into(), id.clone()),
                    (format!("quantity_{other_kind}"), "0".into()),
                    (format!("quantity_{}", kind), "1".into())
                ])
                .await
                .status_code(),
            400
        );
        assert_eq!(counts(db).await, before);

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
        assert_eq!(counts(db).await, (8, 0, 0, 0));
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
        assert_eq!(counts(db).await, (8, 2, 1, 2));
        assert_eq!(
            inventory::open_counts(db, &id, orders::now())
                .await
                .unwrap(),
            (2, 0)
        );
        assert_eq!(
            inventory::open_counts(db, &other, orders::now())
                .await
                .unwrap(),
            (0, 0)
        );
        let persisted = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT event_id FROM orders",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<String>("", "event_id")
            .unwrap();
        assert_eq!(persisted, id);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg025_paid_handoff_keeps_server_price_and_hosted_payment_terms_boundary() {
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
        let event=events::get(db,&id).await.unwrap().unwrap();
        let companion=events::add_ticket_type(db,&event,&TicketTypeInput{name:"Companion".into(),price:"12.50".into(),capacity:4,sales_open:None,sales_close:None}).await.unwrap();
        let second=companion.id;
        let series=start_series(db,&id).await;
        let other=dates(db,&series).await[1].clone();
        let other_kind=events::list_ticket_types(db,&other).await.unwrap()[0].id.clone();
        let picker=request.get(&format!("/embed/series/{series}?event_id={id}")).await;
        assert_eq!(picker.status_code(),200);
        assert!(picker.text().contains(&format!("/events/{id}/selection")));
        assert!(!picker.text().contains(&format!("name=\"quantity_{other_kind}\"")));
        let before=counts(db).await;
        assert_eq!(request.get(&format!("/events/{id}/selection?quantity_{other_kind}=1")).await.status_code(),400);
        assert_eq!(request.post("/checkout").form(&[("event_id".into(),id.clone()),(format!("quantity_{other_kind}"),"0".into()),(format!("quantity_{}",kind),"1".into())]).await.status_code(),400);
        assert_eq!(counts(db).await,before);

        let page=request.get(&format!("/events/{id}/selection?quantity_{kind}=1&quantity_{second}=1")).await;
        assert_eq!(page.status_code(),200);
        assert!(page.text().contains("<strong>Total: €25</strong>"));
        assert_eq!(counts(db).await,(8,0,0,0));
        let start=request.post("/checkout").form(&[("event_id".into(),id.clone()),(format!("quantity_{kind}"),"1".into()),(format!("quantity_{second}"),"1".into()),("price".into(),"1".into()),("success_url".into(),"https://evil.example".into())]).await;
        assert_eq!(start.status_code(),303);
        let path=start.header("location").to_str().unwrap().to_owned();
        let hold=path.strip_prefix("/checkout/").unwrap();
        let details=orders::hold_details(db,hold).await.unwrap();
        assert_eq!(details.total_cents,2500);
        assert_eq!(details.event_id,id);
        let page=request.get(&path).await;
        assert_eq!(page.header("x-frame-options"),"DENY");
        let terms=token(&page.text());
        assert_eq!(request.post(&path).form(&[("email","buyer@example.com")]).await.status_code(),400);
        let submit=request.post(&path).form(&[("email","buyer@example.com"),("booking_terms_token",terms.as_str()),("booking_terms_ack","yes")]).await;
        assert_eq!(submit.status_code(),303);
        assert_eq!(submit.header("location"),"https://checkout.stripe.com/cs_cg007_1");
        assert_eq!(counts(db).await,(8,1,0,0));
        let now=orders::now();
        let body=serde_json::json!({"type":"checkout.session.completed","data":{"object":{"id":"cs_cg007_1","payment_status":"paid","payment_intent":"pi_cg024","amount_total":2500,"currency":"eur","metadata":{"hold_id":hold}}}}).to_string();
        let mut mac=Hmac::<Sha256>::new_from_slice(b"whsec_cg024").unwrap();
        mac.update(format!("{now}.{body}").as_bytes());
        let signature=format!("t={now},v1={}",hex::encode(mac.finalize().into_bytes()));
        let webhook=request.post("/webhooks/stripe").add_header("stripe-signature",signature).add_header("content-type","application/json").text(body).await;
        assert_eq!(webhook.status_code(),200,"{}",webhook.text());
        let order=orders::order_id_for_hold(db,hold).await.unwrap().unwrap();
        assert_eq!(booking_terms::order_evidence(db,&order).await.unwrap().unwrap().terms.url,"https://merchant.example/terms");
        assert_eq!(counts(db).await,(8,1,1,2));
        assert_eq!(inventory::open_counts(db,&id,orders::now()).await.unwrap(),(2,0));
        assert_eq!(inventory::open_counts(db,&other,orders::now()).await.unwrap(),(0,0));
        assert_eq!(db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,"SELECT event_id FROM orders")).await.unwrap().unwrap().try_get::<String>("","event_id").unwrap(),id);
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
async fn cg025_framing_membership_tenant_and_inactive_sales_fail_closed() {
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
        let first_series=start_series(&first_db,&id).await;
        let second_series=start_series(&second_db,&other).await;
        let (elsewhere,_)=event(&first_db,"0").await;
        let elsewhere_series=start_series(&first_db,&elsewhere).await;
        assert_ne!(elsewhere_series,first_series);
        for selection in [&elsewhere,&other] {
            let rejected=request.get(&format!("/embed/series/{first_series}?event_id={selection}")).add_header("host","embed-one.crabgate.com").await;
            assert_eq!(rejected.status_code(),400);
            assert_eq!(rejected.header("x-frame-options"),"DENY");
            assert!(!rejected.text().contains("Continue on CrabGate</button>"));
        }

        settings::set_embed_origins(&first_db,&["https://one.example".into()]).await.unwrap();
        settings::set_embed_origins(&second_db,&["https://two.example".into()]).await.unwrap();
        let before1=counts(&first_db).await;
        let before2=counts(&second_db).await;
        let page=request.get(&format!("/embed/series/{first_series}?event_id={id}")).add_header("host","embed-one.crabgate.com").add_header("origin","https://two.example").add_header("referer","https://two.example/").await;
        assert_eq!(page.status_code(),200);
        assert!(page.header("content-security-policy").to_str().unwrap().ends_with("frame-ancestors https://one.example"));
        assert!(!page.text().contains("two.example"));
        assert!(page.text().contains(&format!("https://embed-one.crabgate.com/events/{id}/selection")));
        let page=request.get(&format!("/embed/series/{second_series}?event_id={other}")).add_header("host","embed-two.crabgate.com").await;
        assert_eq!(page.status_code(),200);
        assert!(page.header("content-security-policy").to_str().unwrap().ends_with("frame-ancestors https://two.example"));
        let missing=request.get(&format!("/embed/series/{first_series}?event_id={id}")).add_header("host","embed-two.crabgate.com").await;
        assert_eq!(missing.status_code(),404);
        assert!(!missing.headers().contains_key("x-frame-options"));
        assert!(missing.header("content-security-policy").to_str().unwrap().ends_with("frame-ancestors https://two.example"));
        let wrong=request.get(&format!("/events/{id}/selection?quantity_{other_kind}=1")).add_header("host","embed-one.crabgate.com").await;
        assert_eq!(wrong.status_code(),400);
        let selection=format!("/events/{id}/selection?quantity_{kind}=1");
        assert_eq!(request.get(&selection).add_header("host","embed-one.crabgate.com").await.status_code(),200);
        ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"UPDATE subscriptions SET status = 'Lapsed' WHERE organizer_id = ?",[first.id.into()])).await.unwrap();
        for path in [format!("/embed/series/{first_series}?event_id={id}"),selection] {
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
        let unknown=request.get(&format!("/embed/series/{second_series}?event_id={other}")).add_header("host","unknown.crabgate.com").await;
        assert_eq!(unknown.status_code(),404);
        assert_eq!(unknown.header("x-frame-options"),"DENY");
        assert_eq!(counts(&first_db).await,before1);
        assert_eq!(counts(&second_db).await,before2);
    }).await;
}
