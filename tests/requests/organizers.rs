use crabgate::{
    app::App,
    events::{self, EventInput, TicketTypeInput},
    hosted::{self, LifecycleAction},
    operator,
};
use hmac::{Hmac, Mac};
use loco_rs::{db, testing::prelude::*};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;
use sha2::Sha256;

struct AdminEmailEnvironment(Option<String>);

impl AdminEmailEnvironment {
    fn set(email: &str) -> Self {
        let prior = std::env::var("CRABGATE_ADMIN_EMAIL").ok();
        std::env::set_var("CRABGATE_ADMIN_EMAIL", email);
        Self(prior)
    }
}

impl Drop for AdminEmailEnvironment {
    fn drop(&mut self) {
        if let Some(value) = &self.0 {
            std::env::set_var("CRABGATE_ADMIN_EMAIL", value);
        } else {
            std::env::remove_var("CRABGATE_ADMIN_EMAIL");
        }
    }
}

#[tokio::test]
#[serial]
async fn self_hosted_signup_claims_only_the_installed_organizer() {
    let _email = AdminEmailEnvironment::set("installed@example.com");
    request::<App, _, _>(|request, ctx| async move {
        assert_eq!(request.get("/").await.status_code(), 303);
        let form = request.get("/signup").await;
        assert_eq!(form.status_code(), 200);
        assert!(form.text().contains("This install is for one organizer"));
        let wrong = request
            .post("/signup")
            .form(&[("name", "Wrong Arts"), ("email", "other@example.com")])
            .await;
        assert_eq!(wrong.status_code(), 200);
        assert!(wrong
            .text()
            .contains("Admin email used during installation"));
        assert!(wrong.text().contains("value=\"Wrong Arts\""));
        let claimed = request
            .post("/signup")
            .form(&[("name", "Harbour Arts"), ("email", "installed@example.com")])
            .await;
        assert_eq!(claimed.status_code(), 303);
        assert_eq!(
            claimed.header("location").to_str().unwrap(),
            "/office/sign-in?sent=1"
        );
        assert_eq!(request.get("/signup").await.status_code(), 303);
        let repeated = request
            .post("/signup")
            .form(&[("name", "Other Arts"), ("email", "installed@example.com")])
            .await;
        assert_eq!(repeated.status_code(), 303);
        let row = ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count, name, onboarding_at FROM organizers",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "count").unwrap(), 1);
        assert_eq!(row.try_get::<String>("", "name").unwrap(), "Harbour Arts");
        assert!(row
            .try_get::<Option<i64>>("", "onboarding_at")
            .unwrap()
            .is_some());
        assert_eq!(request.get("/").await.status_code(), 200);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn hosted_verification_signs_admin_in_on_organizer_subdomain() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let signup = hosted::signup(
            &registry,
            &ctx.db,
            "Harbour Arts",
            "harbour-onboard",
            "admin@example.com",
            hosted::now(),
        )
        .await
        .unwrap();
        let verify = request
            .get(&format!("/verify/{}", signup.verification_token))
            .add_header("host", "crabgate.com")
            .await;
        assert_eq!(verify.status_code(), 303);
        assert!(verify
            .header("location")
            .to_str()
            .unwrap()
            .contains("harbour-onboard.crabgate.com/office/complete-signup/"));
        let complete = request
            .get(&format!(
                "/office/complete-signup/{}",
                signup.verification_token
            ))
            .add_header("host", "harbour-onboard.crabgate.com")
            .await;
        assert_eq!(complete.status_code(), 303);
        assert_eq!(
            complete.header("location").to_str().unwrap(),
            "/office/setup"
        );
        let cookie = complete.header("set-cookie").to_str().unwrap().to_owned();
        let setup = request
            .get("/office/setup")
            .add_header("host", "harbour-onboard.crabgate.com")
            .add_header("cookie", cookie.split(';').next().unwrap())
            .await;
        assert_eq!(setup.status_code(), 200);
        assert!(setup.text().contains("Create a Draft event"));
        assert_eq!(
            request
                .get(&format!(
                    "/office/complete-signup/{}",
                    signup.verification_token
                ))
                .add_header("host", "harbour-onboard.crabgate.com")
                .await
                .status_code(),
            400
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn hosted_main_domain_owns_reserved_addresses_and_unknown_hosts() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, ctx| async move {
        let reserved = request
            .post("/signup")
            .add_header("host", "crabgate.com")
            .form(&[
                ("name", "Squatter Arts"),
                ("subdomain", "www"),
                ("email", "squatter@example.com"),
            ])
            .await;
        assert_eq!(reserved.status_code(), 200);
        assert!(reserved.text().contains("This Shop address is taken"));
        let row = ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM organizers",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "count").unwrap(), 0);

        let www = request
            .get("/pricing?ref=mail")
            .add_header("host", "www.crabgate.com")
            .await;
        assert_eq!(www.status_code(), 308);
        assert_eq!(
            www.header("location"),
            "https://crabgate.com/pricing?ref=mail"
        );

        for path in ["/", "/events/missing"] {
            let unknown = request
                .get(path)
                .add_header("host", "nobody.crabgate.com")
                .await;
            assert_eq!(unknown.status_code(), 404, "{path}");
            assert!(unknown.text().contains("Page not found."), "{path}");
        }
    })
    .await;
}

#[tokio::test]
#[serial]
async fn hosted_signup_accepts_multiple_organizers_and_explains_taken_addresses() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, ctx| async move {
        for (name, subdomain, email) in [
            ("Harbour Arts", "harbour-new", "harbour@example.com"),
            ("River Arts", "river-new", "river@example.com"),
        ] {
            let response = request
                .post("/signup")
                .add_header("host", "crabgate.com")
                .form(&[
                    ("name", name),
                    ("subdomain", subdomain),
                    ("email", email),
                    ("next_event", "Winter Revue, 12 December"),
                ])
                .await;
            assert_eq!(response.status_code(), 200);
            assert!(response.text().contains("Check your inbox"));
        }
        let deliveries = ctx.mailer.as_ref().unwrap().deliveries();
        let notice = deliveries
            .messages
            .iter()
            .find(|message| message.contains("Reply-To: river@example.com"))
            .expect("signup notice for River Arts");
        assert!(notice.contains("To: hello@crabgate.com"));
        assert!(notice.contains("From: CrabGate <hello@crabgate.com>"));
        assert!(notice.contains("Subject: New CrabGate signup: River Arts"));
        assert!(notice.contains("Next event: Winter Revue, 12 December"));
        let duplicate = request
            .post("/signup")
            .add_header("host", "crabgate.com")
            .form(&[
                ("name", "Another Arts"),
                ("subdomain", "harbour-new"),
                ("email", "another@example.com"),
            ])
            .await;
        assert_eq!(duplicate.status_code(), 200);
        assert!(duplicate.text().contains("This Shop address is taken"));
        assert!(duplicate.text().contains("value=\"Another Arts\""));
        let row = ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM organizers",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "count").unwrap(), 2);
    })
    .await;
}

struct HostedEnvironment(Option<String>);

fn billing_signature(body: &str, timestamp: i64, secret: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{timestamp}.{body}").as_bytes());
    format!(
        "t={timestamp},v1={}",
        hex::encode(mac.finalize().into_bytes())
    )
}

#[tokio::test]
#[serial]
async fn stripe_webhook_ignores_checkouts_other_tools_created() {
    let prior = std::env::var("STRIPE_TEST_WEBHOOK_SECRET").ok();
    std::env::set_var("STRIPE_TEST_WEBHOOK_SECRET", "whsec_ticket_test");
    request::<App, _, _>(|request, _| async move {
        let now = chrono::Utc::now().timestamp();
        let foreign = serde_json::json!({
            "type": "checkout.session.expired",
            "data": {"object": {"id": "cs_from_another_tool", "payment_status": "unpaid", "metadata": {}}}
        })
        .to_string();
        let accepted = request
            .post("/webhooks/stripe")
            .text(&foreign)
            .add_header(
                "Stripe-Signature",
                billing_signature(&foreign, now, "whsec_ticket_test"),
            )
            .await;
        assert_eq!(accepted.status_code(), 200);
        let forged = request
            .post("/webhooks/stripe")
            .text(&foreign)
            .add_header(
                "Stripe-Signature",
                billing_signature(&foreign, now, "whsec_wrong"),
            )
            .await;
        assert_eq!(forged.status_code(), 400);
    })
    .await;
    if let Some(value) = prior {
        std::env::set_var("STRIPE_TEST_WEBHOOK_SECRET", value);
    } else {
        std::env::remove_var("STRIPE_TEST_WEBHOOK_SECRET");
    }
}

#[tokio::test]
#[serial]
async fn signed_billing_webhooks_update_subscription_state() {
    let _hosted = HostedEnvironment::set();
    let prior_secret = std::env::var("CRABGATE_BILLING_WEBHOOK_SECRET").ok();
    std::env::set_var("CRABGATE_BILLING_WEBHOOK_SECRET", "whsec_billing_test");
    for modern_invoice in [false, true] {
        request::<App, _, _>(|request, ctx| async move {
            let registry = super::organizer_registry(&ctx);
            let signup = hosted::signup(&registry, &ctx.db, "Billing Arts", "billing-arts", "billing@example.com", hosted::now()).await.unwrap();
            let id = signup.organizer.id;
            let now = hosted::now();
            let first = hosted::billing_attempt(&ctx.db, &id, now).await.unwrap().unwrap();
            assert!(hosted::save_billing_checkout(&ctx.db, &id, &first.key, "cs_billing", "https://checkout.stripe.com/c/pay/cs_billing").await.unwrap());
            let wrong = serde_json::json!({"type":"checkout.session.completed","created":now,"data":{"object":{"id":"cs_other", "mode":"subscription", "payment_status":"paid", "client_reference_id":id, "customer":"cus_billing", "subscription":"sub_billing"}}}).to_string();
            assert_eq!(request.post("/webhooks/billing").text(&wrong).add_header("Stripe-Signature", &billing_signature(&wrong, now, "whsec_billing_test")).await.status_code(), 400);
            let completed = serde_json::json!({"type":"checkout.session.completed","created":now,"data":{"object":{"id":"cs_billing", "mode":"subscription", "payment_status":"paid", "client_reference_id":id, "customer":"cus_billing", "subscription":"sub_billing"}}}).to_string();
            assert_eq!(request.post("/webhooks/billing").text(&completed).add_header("Stripe-Signature", "t=0,v1=bad").await.status_code(), 400);
            assert_eq!(request.post("/webhooks/billing").text(&completed).add_header("Stripe-Signature", &billing_signature(&completed, now, "whsec_billing_test")).await.status_code(), 200);
            assert_eq!(hosted::access_state(&ctx.db, &id).await.unwrap().subscription_status, "Active");
            let invoice = if modern_invoice {
                serde_json::json!({"id":"in_billing", "parent":{"type":"subscription_details", "subscription_details":{"subscription":"sub_billing"}}})
            } else {
                serde_json::json!({"id":"in_billing", "subscription":"sub_billing"})
            };
            let failed = serde_json::json!({"type":"invoice.payment_failed","created":now+1,"data":{"object":invoice}}).to_string();
            assert_eq!(request.post("/webhooks/billing").text(&failed).add_header("Stripe-Signature", &billing_signature(&failed, now, "whsec_billing_test")).await.status_code(), 200);
            assert_eq!(hosted::access_state(&ctx.db, &id).await.unwrap().subscription_status, "Grace");
            let paid = serde_json::json!({"type":"invoice.paid","created":now+2,"data":{"object":{"id":"in_billing2", "subscription":"sub_billing"}}}).to_string();
            assert_eq!(request.post("/webhooks/billing").text(&paid).add_header("Stripe-Signature", &billing_signature(&paid, now, "whsec_billing_test")).await.status_code(), 200);
            assert_eq!(hosted::access_state(&ctx.db, &id).await.unwrap().subscription_status, "Grace");
            let foreign = serde_json::json!({"type":"checkout.session.completed","created":now,"data":{"object":{"id":"cs_other_product","mode":"subscription","payment_status":"paid","client_reference_id":"another-products-customer","subscription":"sub_other"}}}).to_string();
            assert_eq!(request.post("/webhooks/billing").text(&foreign).add_header("Stripe-Signature", &billing_signature(&foreign, now, "whsec_billing_test")).await.status_code(), 200);
            let unlabelled = serde_json::json!({"type":"checkout.session.completed","created":now,"data":{"object":{"id":"cs_no_reference","mode":"subscription","payment_status":"paid","subscription":"sub_other"}}}).to_string();
            assert_eq!(request.post("/webhooks/billing").text(&unlabelled).add_header("Stripe-Signature", &billing_signature(&unlabelled, now, "whsec_billing_test")).await.status_code(), 200);
            let active = serde_json::json!({"type":"customer.subscription.updated","created":now+3,"data":{"object":{"id":"sub_billing", "status":"active"}}}).to_string();
            assert_eq!(request.post("/webhooks/billing").text(&active).add_header("Stripe-Signature", &billing_signature(&active, now, "whsec_billing_test")).await.status_code(), 200);
            assert_eq!(hosted::access_state(&ctx.db, &id).await.unwrap().subscription_status, "Active");
            let deleted = serde_json::json!({"type":"customer.subscription.deleted","created":now+4,"data":{"object":{"id":"sub_billing"}}}).to_string();
            assert_eq!(request.post("/webhooks/billing").text(&deleted).add_header("Stripe-Signature", &billing_signature(&deleted, now, "whsec_billing_test")).await.status_code(), 200);
            assert_eq!(hosted::access_state(&ctx.db, &id).await.unwrap().subscription_status, "Lapsed");
            assert_eq!(request.post("/webhooks/billing").text(&paid).add_header("Stripe-Signature", &billing_signature(&paid, now, "whsec_billing_test")).await.status_code(), 200);
            assert_eq!(request.post("/webhooks/billing").text(&active).add_header("Stripe-Signature", &billing_signature(&active, now, "whsec_billing_test")).await.status_code(), 200);
            assert_eq!(request.post("/webhooks/billing").text(&completed).add_header("Stripe-Signature", &billing_signature(&completed, now, "whsec_billing_test")).await.status_code(), 200);
            assert_eq!(hosted::access_state(&ctx.db, &id).await.unwrap().subscription_status, "Lapsed");
            let retry = hosted::billing_attempt(&ctx.db, &id, now + 5).await.unwrap().unwrap();
            assert!(retry.checkout_url.is_none());
            assert_ne!(retry.key, first.key);
            assert!(hosted::save_billing_checkout(&ctx.db, &id, &retry.key, "cs_reactivated", "https://checkout.stripe.com/c/pay/cs_reactivated").await.unwrap());
            let mut reactivated = serde_json::json!({"type":"checkout.session.completed","created":now+4,"data":{"object":{"id":"cs_reactivated", "mode":"subscription", "payment_status":"paid", "client_reference_id":id, "customer":"cus_billing", "subscription":"sub_reactivated"}}});
            let equal_time = reactivated.to_string();
            assert_eq!(request.post("/webhooks/billing").text(&equal_time).add_header("Stripe-Signature", &billing_signature(&equal_time, now, "whsec_billing_test")).await.status_code(), 200);
            assert_eq!(hosted::access_state(&ctx.db, &id).await.unwrap().subscription_status, "Lapsed");
            reactivated["created"] = serde_json::json!(now+5);
            let reactivated = reactivated.to_string();
            for _ in 0..2 {
                assert_eq!(request.post("/webhooks/billing").text(&reactivated).add_header("Stripe-Signature", &billing_signature(&reactivated, now, "whsec_billing_test")).await.status_code(), 200);
            }
            assert_eq!(request.post("/webhooks/billing").text(&deleted).add_header("Stripe-Signature", &billing_signature(&deleted, now, "whsec_billing_test")).await.status_code(), 200);
            let state = hosted::access_state(&ctx.db, &id).await.unwrap();
            assert_eq!(state.subscription_status, "Active");
            assert_eq!(state.grace_until, None);
            let row = ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT stripe_subscription_id, lapsed_at FROM subscriptions WHERE organizer_id = ?", [id.into()]
            )).await.unwrap().unwrap();
            assert_eq!(row.try_get::<String>("", "stripe_subscription_id").unwrap(), "sub_reactivated");
            assert_eq!(row.try_get::<Option<i64>>("", "lapsed_at").unwrap(), None);
        }).await;
    }
    if let Some(value) = prior_secret {
        std::env::set_var("CRABGATE_BILLING_WEBHOOK_SECRET", value);
    } else {
        std::env::remove_var("CRABGATE_BILLING_WEBHOOK_SECRET");
    }
}

#[tokio::test]
#[serial]
async fn hosted_marketing_uses_configured_price_on_base_domain() {
    let _hosted = HostedEnvironment::set();
    let prior_price = std::env::var("CRABGATE_ANNUAL_PRICE").ok();
    std::env::set_var("CRABGATE_ANNUAL_PRICE", "$311");
    request::<App, _, _>(|request, _| async move {
        for path in [
            "/",
            "/pricing",
            "/eventbrite-alternative",
            "/for-rooms-with-an-audience",
            "/for-conferences",
            "/for-workshops",
            "/signup",
            "/tools/stripe-fee-calculator",
            "/tools/eventbrite-fee-calculator",
        ] {
            let page = request.get(path).add_header("host", "crabgate.com").await;
            assert_eq!(page.status_code(), 200, "{path}");
            assert!(page.text().contains("$311 a year"), "{path}");
            assert!(!page.text().contains("$99"), "{path}");
        }
    })
    .await;
    if let Some(value) = prior_price {
        std::env::set_var("CRABGATE_ANNUAL_PRICE", value);
    } else {
        std::env::remove_var("CRABGATE_ANNUAL_PRICE");
    }
}

#[tokio::test]
#[serial]
async fn hosted_eventbrite_alternative_has_switching_content_and_contextual_links() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, ctx| async move {
        let page = request
            .get("/eventbrite-alternative")
            .add_header("host", "crabgate.com")
            .await;
        assert_eq!(page.status_code(), 200);
        let html = page.text();
        for expected in [
            "<title>Eventbrite alternative for your next event | CrabGate</title>",
            "href=\"https://crabgate.com/eventbrite-alternative\"",
            "small rooms, independent conferences, and recurring workshops",
            "Removing that buyer charge does not make it an organizer saving.",
            "The calculator excludes tax and assumes one ticket per card order",
            "not an event discovery marketplace",
            "These examples are not customer endorsements.",
            "href=\"/signup\">Set up your next event</a>",
            "href=\"/tools/eventbrite-fee-calculator\"",
            "href=\"/pricing\"",
            "href=\"/for-rooms-with-an-audience\"",
            "href=\"/for-conferences\"",
            "href=\"/for-workshops\"",
        ] {
            assert!(html.contains(expected), "Missing {expected}");
        }
        assert!(!html.contains("noindex"));
        for path in ["/pricing", "/tools/eventbrite-fee-calculator"] {
            let linked = request.get(path).add_header("host", "crabgate.com").await;
            assert_eq!(linked.status_code(), 200);
            assert!(linked.text().contains("href=\"/eventbrite-alternative\""));
        }
        let other_tool = request
            .get("/tools/tito-fee-calculator")
            .add_header("host", "crabgate.com")
            .await;
        assert_eq!(other_tool.status_code(), 200);
        assert!(!other_tool
            .text()
            .contains("href=\"/eventbrite-alternative\""));
        super::organizer_registry(&ctx)
            .create("Alternative page test", "alternative-page-test")
            .await
            .unwrap();
        assert_eq!(
            request
                .get("/eventbrite-alternative")
                .add_header("host", "alternative-page-test.crabgate.com")
                .await
                .status_code(),
            404
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn hosted_marketing_keeps_public_contact_links_out_of_email_obfuscation() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, _| async move {
        for path in std::iter::once("/")
            .chain(
                crabgate::controllers::hosted::LANDING_PAGES
                    .iter()
                    .map(|(path, _)| *path),
            )
            .chain(std::iter::once("/sign-in"))
        {
            let page = request.get(path).add_header("host", "crabgate.com").await;
            assert_eq!(page.status_code(), 200, "{path}");
            let html = page.text();
            let (before, body) = html.split_once("<!--email_off-->").expect(path);
            let (public_contact, after) = body.split_once("<!--/email_off-->").expect(path);
            assert!(
                public_contact.contains(
                    "href=\"mailto:hello@crabgate.com?subject=Feedback%20on%20CrabGate\""
                ),
                "{path}"
            );
            assert!(!before.contains("href=\"mailto:"), "{path}");
            assert!(!after.contains("href=\"mailto:"), "{path}");
            assert!(!html.contains("/cdn-cgi/l/email-protection"), "{path}");
            assert_eq!(
                html.contains("<meta name=\"robots\" content=\"noindex\">"),
                path == "/sign-in",
                "{path}"
            );
        }
    })
    .await;
}

#[tokio::test]
#[serial]
async fn hosted_pricing_has_standard_price_and_manual_free_access() {
    let _hosted = HostedEnvironment::set();
    let prior = std::env::var("CRABGATE_ANNUAL_PRICE").ok();
    std::env::remove_var("CRABGATE_ANNUAL_PRICE");
    request::<App, _, _>(|request, _| async move {
        let page = request
            .get("/pricing")
            .add_header("host", "crabgate.com")
            .await;
        assert_eq!(page.status_code(), 200);
        let html = page.text();
        for included in [
            "Simple ticketing. <mark>$99 a year.</mark>",
            "Unlimited events.",
            "Unlimited tickets.",
            "0% of ticket sales",
            "No CrabGate transaction fee, no setup fee, and no percentage of ticket sales.",
            "Payments go directly to you through Stripe.",
            "Free for charities, nonprofits, and community organisations",
            "href=\"mailto:hello@crabgate.com\">Email hello@crabgate.com</a> to get access.",
        ] {
            assert!(html.contains(included), "Missing {included}");
        }
        assert!(!html.contains("$299"));
        assert!(!html.contains("hefty discount"));
    })
    .await;
    if let Some(value) = prior {
        std::env::set_var("CRABGATE_ANNUAL_PRICE", value);
    } else {
        std::env::remove_var("CRABGATE_ANNUAL_PRICE");
    }
}

#[tokio::test]
#[serial]
async fn hosted_setup_offer_shows_its_closing_date_until_it_passes() {
    let _hosted = HostedEnvironment::set();
    let prior = std::env::var("CRABGATE_SETUP_OFFER_ENDS").ok();
    let today = chrono::Utc::now().date_naive();
    request::<App, _, _>(|request, _| async move {
        let ends = today + chrono::Duration::days(10);
        std::env::set_var(
            "CRABGATE_SETUP_OFFER_ENDS",
            ends.format("%Y-%m-%d").to_string(),
        );
        let open = request
            .get("/pricing")
            .add_header("host", "crabgate.com")
            .await;
        let date = ends.format("%-d %B");
        assert!(open.text().contains(&format!(
            "<b>This offer closes on {date}.</b> That's 10 days from today."
        )));
        assert!(open
            .text()
            .contains(&format!("setup call with Paul, until {date}.")));

        let passed = today - chrono::Duration::days(1);
        std::env::set_var(
            "CRABGATE_SETUP_OFFER_ENDS",
            passed.format("%Y-%m-%d").to_string(),
        );
        let closed = request
            .get("/pricing")
            .add_header("host", "crabgate.com")
            .await;
        assert!(!closed.text().contains("This offer closes on"));
        assert!(closed.text().contains("setup call with Paul."));
    })
    .await;
    if let Some(value) = prior {
        std::env::set_var("CRABGATE_SETUP_OFFER_ENDS", value);
    } else {
        std::env::remove_var("CRABGATE_SETUP_OFFER_ENDS");
    }
}

#[tokio::test]
#[serial]
async fn hosted_landing_pages_render_on_the_marketing_host_only() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, _| async move {
        for (path, heading, title) in [
            (
                "/",
                "Sell tickets. Manage attendees. <mark>Run the door.</mark></h1>",
                "<title>CrabGate | Tickets for rooms, conferences and recurring workshops</title>",
            ),
            (
                "/pricing",
                "Simple ticketing. <mark>$99 a year.</mark></h1>",
                "<title>Simple ticketing. $99 a year. | CrabGate</title>",
            ),
            (
                "/for-rooms-with-an-audience",
                "Put your nights on sale. <mark>Keep the door moving.</mark></h1>",
                "<title>Ticketing for small rooms with their own list | CrabGate</title>",
            ),
            (
                "/for-conferences",
                "Sell conference tickets. <mark>Run registration.</mark></h1>",
                "<title>Ticketing for independent conferences | CrabGate</title>",
            ),
            (
                "/for-workshops",
                "Schedule your workshops. <mark>Sell each date.</mark></h1>",
                "<title>Ticketing for recurring workshops | CrabGate</title>",
            ),
        ] {
            let page = request.get(path).add_header("host", "crabgate.com").await;
            assert_eq!(page.status_code(), 200, "{path}");
            let html = page.text();
            assert!(html.contains(heading), "{path}");
            assert!(html.contains(title), "{path}");
            assert_eq!(html.matches("<h1").count(), 1, "{path}");
            assert!(
                html.contains(&format!(
                    "<link rel=\"canonical\" href=\"https://crabgate.com{path}\">"
                )),
                "{path}"
            );
            let no_cut = match path {
                "/" => "no percentage of ticket sales",
                "/pricing" => "CrabGate never takes a percentage of your sales.",
                _ => "we never take a cut",
            };
            assert!(html.contains(no_cut), "{path}");
            assert!(html.contains("within 30 days of paying"), "{path}");
            assert!(
                html.contains("href=\"/signup\">Set up your next event</a>"),
                "{path}"
            );
            assert!(html.contains("href=\"/sign-in\" >Sign in</a>"), "{path}");
            assert!(!html.contains("conferences, and charities"), "{path}");
            if path != "/" {
                assert!(html.contains("card processor's fee"), "{path}");
                assert_eq!(
                    request
                        .get(path)
                        .add_header("host", "other.crabgate.com")
                        .await
                        .status_code(),
                    404,
                    "{path}"
                );
            }
        }
    })
    .await;
}

#[tokio::test]
#[serial]
async fn marketing_sign_in_opens_only_an_existing_shop_sign_in() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        registry
            .create("Harbour Arts", "harbour-sign-in")
            .await
            .unwrap();
        let page = request
            .get("/sign-in")
            .add_header("host", "crabgate.com")
            .await;
        assert_eq!(page.status_code(), 200);
        assert!(page
            .text()
            .contains("<h1 class=\"h-l\" id=\"sign-in-heading\">Sign in to your shop</h1>"));
        assert!(page
            .text()
            .contains("<meta name=\"robots\" content=\"noindex\">"));
        assert!(page
            .text()
            .contains("<link rel=\"canonical\" href=\"https://crabgate.com/sign-in\">"));
        let target = request
            .get("/sign-in?shop=HARBOUR-SIGN-IN")
            .add_header("host", "crabgate.com")
            .await;
        assert_eq!(target.status_code(), 303);
        assert_eq!(
            target.header("location").to_str().unwrap(),
            "https://harbour-sign-in.crabgate.com/office/sign-in"
        );
        let form = request
            .get("/office/sign-in")
            .add_header("host", "harbour-sign-in.crabgate.com")
            .await;
        assert_eq!(form.status_code(), 200);
        assert!(form.text().contains("Email me a sign-in link"));
        for shop in [
            "missing",
            "",
            "https://example.com",
            "harbour-sign-in.evil",
            &"a".repeat(64),
        ] {
            let failed = request
                .get(&format!("/sign-in?shop={shop}"))
                .add_header("host", "crabgate.com")
                .await;
            assert_eq!(failed.status_code(), 400, "{shop}");
            assert!(failed.text().contains("We couldn&#39;t find that shop."));
            assert!(failed.text().contains("aria-invalid=\"true\""));
        }
        assert_eq!(
            request
                .get("/sign-in")
                .add_header("host", "harbour-sign-in.crabgate.com")
                .await
                .status_code(),
            404
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn hosted_legal_pages_are_served_on_the_base_domain_and_linked() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, _| async move {
        for (path, heading, title, fact) in [
            (
                "/privacy",
                "We keep what the shop needs. <mark>Nothing else.</mark></h1>",
                "<title>Privacy | CrabGate</title>",
                "CrabGate runs on a server in Stockholm, Sweden.",
            ),
            (
                "/terms",
                "The deal, <mark>in plain words.</mark></h1>",
                "<title>Terms | CrabGate</title>",
                "CrabGate costs $99 a year, taxes included.",
            ),
        ] {
            let page = request.get(path).add_header("host", "crabgate.com").await;
            assert_eq!(page.status_code(), 200, "{path}");
            let html = page.text();
            assert!(html.contains(heading), "{path}");
            assert!(html.contains(title), "{path}");
            assert!(html.contains(fact), "{path}");
            assert!(
                html.contains("registration number (CUI) 55013997, trade register number J2026041446003, registered at Bd. Dâmbovița, nr. 60, sc. A, et. 4, ap. 14, Timișoara, Timiș, Romania."),
                "{path}"
            );
            assert_eq!(html.matches("<h1").count(), 1, "{path}");
            assert!(
                html.contains(&format!(
                    "<link rel=\"canonical\" href=\"https://crabgate.com{path}\">"
                )),
                "{path}"
            );
            assert_eq!(
                request
                    .get(path)
                    .add_header("host", "other.crabgate.com")
                    .await
                    .status_code(),
                404,
                "{path}"
            );
        }
        for path in ["/", "/signup"] {
            let html = request
                .get(path)
                .add_header("host", "crabgate.com")
                .await
                .text();
            assert!(html.contains("<a href=\"/privacy\">Privacy</a>"), "{path}");
            assert!(html.contains("<a href=\"/terms\">Terms</a>"), "{path}");
        }
        let signup = request
            .get("/signup")
            .add_header("host", "crabgate.com")
            .await
            .text();
        assert!(signup.contains(
            "By creating a shop you agree to our <a href=\"/terms\">terms</a> and <a href=\"/privacy\">privacy page</a>."
        ));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn hosted_event_editor_shows_subscription_and_setup_blockers() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let signup = hosted::signup(
            &registry,
            &ctx.db,
            "Checklist Arts",
            "checklist-arts",
            "admin@example.com",
            hosted::now(),
        )
        .await
        .unwrap();
        let db =
            sea_orm::Database::connect(format!("sqlite://{}?mode=rw", signup.organizer.db_path))
                .await
                .unwrap();
        let event = events::create(
            &db,
            &EventInput {
                title: "Checklist Event".to_owned(),
                date: "2027-06-15".to_owned(),
                start_time: "19:00".to_owned(),
                end_time: "21:00".to_owned(),
                timezone: "Europe/Dublin".to_owned(),
                venue: "Hall".to_owned(),
                address: "Street".to_owned(),
                description: String::new(),
                event_capacity: None,
                max_per_order: 10,
                attendee_name_required: false,
            },
        )
        .await
        .unwrap();
        let session = crabgate::members::create_session_for_email(
            &ctx.db,
            &signup.organizer.id,
            "admin@example.com",
            hosted::now(),
        )
        .await
        .unwrap()
        .unwrap();
        let response = request
            .get(&format!("/office/events/{}", event.id))
            .add_header("host", "checklist-arts.crabgate.com")
            .add_header("cookie", format!("cg_session={session}"))
            .await;
        assert_eq!(response.status_code(), 200);
        assert!(response.text().contains("Activate the"));
        assert!(response.text().contains("(Pending)"));
        assert!(response.text().contains("Pass the webhook check"));
        assert!(response
            .text()
            .contains("disabled aria-describedby=\"publish-note\""));
        let requested = request
            .post("/office/setup/request")
            .add_header("host", "checklist-arts.crabgate.com")
            .add_header("cookie", format!("cg_session={session}"))
            .await;
        assert_eq!(requested.status_code(), 303);
        let setup = request
            .get("/office/setup")
            .add_header("host", "checklist-arts.crabgate.com")
            .add_header("cookie", format!("cg_session={session}"))
            .await;
        assert!(setup
            .text()
            .contains("Your onboarding call: <strong>Requested</strong>"));
        assert!(setup.text().contains("Connect Stripe</h2>"));
        assert!(!setup.text().contains("Check ticket emails"));
        assert!(!setup.text().contains("SQLite"));
        let bad_key = request
            .post("/office/setup/stripe")
            .add_header("host", "checklist-arts.crabgate.com")
            .add_header("cookie", format!("cg_session={session}"))
            .form(&[("secret_key", "not-a-key")])
            .await;
        assert_eq!(bad_key.status_code(), 303);
        assert_eq!(
            bad_key.header("location"),
            "/office/setup?step=stripe&error=key_format"
        );
        let explained = request
            .get("/office/setup?step=stripe&error=key_format")
            .add_header("host", "checklist-arts.crabgate.com")
            .add_header("cookie", format!("cg_session={session}"))
            .await;
        assert!(explained
            .text()
            .contains("That doesn't look like a Stripe key. It should start with sk_ or rk_."));
        assert_eq!(
            hosted::setup_status(&ctx.db, &signup.organizer.id)
                .await
                .unwrap()
                .as_deref(),
            Some("Requested")
        );
        assert!(hosted::update_setup_status(
            &ctx.db,
            &signup.organizer.id,
            "Contacted",
            hosted::now()
        )
        .await
        .unwrap());
        assert!(
            !hosted::request_setup(&ctx.db, &signup.organizer.id, hosted::now())
                .await
                .unwrap()
        );
        assert_eq!(
            hosted::setup_status(&ctx.db, &signup.organizer.id)
                .await
                .unwrap()
                .as_deref(),
            Some("Contacted")
        );
    })
    .await;
}

impl HostedEnvironment {
    fn set() -> Self {
        let prior = std::env::var("CRABGATE_EDITION").ok();
        std::env::set_var("CRABGATE_EDITION", "hosted");
        Self(prior)
    }
}

#[tokio::test]
#[serial]
async fn operator_needs_active_grant_for_organizer_data() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let organizer = registry.create("Harbour Arts", "harbour").await.unwrap();
        ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO operators (id, email) VALUES ('operator-test', 'operator@example.com')", []
        )).await.unwrap();
        let now = chrono::Utc::now().timestamp();
        let token = operator::request_link(&ctx.db, "operator@example.com", now)
            .await
            .unwrap()
            .unwrap();
        assert!(operator::request_link(&ctx.db, "operator@example.com", now)
            .await
            .unwrap()
            .is_some());
        assert!(operator::request_link(&ctx.db, "operator@example.com", now)
            .await
            .unwrap()
            .is_some());
        assert!(operator::request_link(&ctx.db, "operator@example.com", now)
            .await
            .unwrap()
            .is_none());
        let session = operator::claim_link(&ctx.db, &token, now)
            .await
            .unwrap()
            .unwrap();
        let cookie = format!("cg_operator_session={session}");
        assert_eq!(
            request
                .post("/operator/organizers")
                .form(&[
                    ("name", "Blocked Arts"),
                    ("subdomain", "blocked-arts"),
                    ("email", "blocked@example.com"),
                ])
                .await
                .status_code(),
            403
        );
        let operator_page = request
            .get("/operator")
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(operator_page.status_code(), 200);
        assert!(operator_page.text().contains("Create organizer"));
        let created = request
            .post("/operator/organizers")
            .add_header("cookie", &cookie)
            .form(&[
                ("name", "Created Arts"),
                ("subdomain", "created-arts"),
                ("email", "created@example.com"),
            ])
            .await;
        assert_eq!(created.status_code(), 303);
        let created_row = ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT s.status FROM subscriptions s JOIN organizers o ON o.id = s.organizer_id WHERE o.subdomain = 'created-arts'".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(created_row.try_get::<String>("", "status").unwrap(), "Pending");
        let response = request
            .get("/office/events")
            .add_header("host", "harbour.crabgate.com")
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(response.status_code(), 403);
        let member = crabgate::members::add_member(
            &ctx.db,
            &organizer.id,
            "admin@example.com",
            crabgate::members::Role::Admin,
        )
        .await
        .unwrap();
        let grant_id = operator::grant(&ctx.db, &organizer.id, &member, 24, now)
            .await
            .unwrap();
        let response = request
            .get("/office/events")
            .add_header("host", "harbour.crabgate.com")
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(response.status_code(), 200);
        let audit = ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT method, path FROM audit_log WHERE operator_id = 'operator-test' AND path = '/office/events'".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(audit.try_get::<String>("", "method").unwrap(), "GET");
        assert_eq!(
            audit.try_get::<String>("", "path").unwrap(),
            "/office/events"
        );
        operator::revoke(&ctx.db, &organizer.id, &grant_id, now)
            .await
            .unwrap();
        let response = request
            .get("/office/events")
            .add_header("host", "harbour.crabgate.com")
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(response.status_code(), 403);
        assert_eq!(
            request
                .post("/operator/sign-out")
                .add_header("cookie", &cookie)
                .await
                .status_code(),
            303
        );
        assert!(operator::session(&ctx.db, &session, now)
            .await
            .unwrap()
            .is_none());
    })
    .await;
}

#[tokio::test]
#[serial]
async fn hosted_verification_and_lifecycle_follow_injected_time() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|_, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let start = 1_800_000_000;
        let signup = hosted::signup(
            &registry,
            &ctx.db,
            "Harbour Arts",
            "harbour",
            "ADMIN@EXAMPLE.COM",
            start,
        )
        .await
        .unwrap();
        let id = &signup.organizer.id;
        let first = hosted::billing_attempt(&ctx.db, id, start)
            .await
            .unwrap()
            .unwrap();
        assert!(first.checkout_url.is_none());
        assert!(hosted::save_billing_checkout(
            &ctx.db,
            id,
            &first.key,
            "cs_first",
            "https://checkout.stripe.test/first"
        )
        .await
        .unwrap());
        let retry = hosted::billing_attempt(&ctx.db, id, start + 60)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retry.key, first.key);
        assert_eq!(
            retry.checkout_url.as_deref(),
            Some("https://checkout.stripe.test/first")
        );
        let fresh = hosted::billing_attempt(&ctx.db, id, start + 70 * 60)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(fresh.key, first.key);
        assert!(fresh.checkout_url.is_none());
        assert!(!hosted::save_billing_checkout(
            &ctx.db,
            id,
            &first.key,
            "cs_stale",
            "https://checkout.stripe.test/stale"
        )
        .await
        .unwrap());
        let state = hosted::access_state(&ctx.db, id).await.unwrap();
        assert!(!state.verified);
        assert!(!hosted::subscription_allows_sales(&state, start));
        assert!(matches!(
            hosted::verify_email(&ctx.db, &signup.verification_token, start + 86_400).await,
            Err(hosted::HostedError::InvalidVerification)
        ));
        assert_eq!(
            hosted::verify_email(&ctx.db, &signup.verification_token, start + 1)
                .await
                .unwrap(),
            *id
        );
        assert!(matches!(
            hosted::verify_email(&ctx.db, &signup.verification_token, start + 2).await,
            Err(hosted::HostedError::InvalidVerification)
        ));
        hosted::subscription_active(&ctx.db, id, "cus_test", "sub_test", start + 3, start + 3)
            .await
            .unwrap();
        assert!(hosted::billing_attempt(&ctx.db, id, start + 4)
            .await
            .unwrap()
            .is_none());
        let state = hosted::access_state(&ctx.db, id).await.unwrap();
        assert!(hosted::publish_allowed(&state, true, true, start + 3).is_ok());
        assert!(matches!(
            hosted::publish_allowed(&state, false, true, start + 3),
            Err(hosted::HostedError::SetupRequired)
        ));
        hosted::subscription_failed(&ctx.db, "sub_test", start + 4, start + 4)
            .await
            .unwrap();
        let state = hosted::access_state(&ctx.db, id).await.unwrap();
        let deadline = start + 4 + 14 * 86_400;
        assert!(hosted::subscription_allows_sales(&state, deadline - 1));
        assert!(!hosted::subscription_allows_sales(&state, deadline));
        assert!(hosted::due_lifecycle(&ctx.db, deadline)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            hosted::access_state(&ctx.db, id)
                .await
                .unwrap()
                .subscription_status,
            "Lapsed"
        );
        let warning_at = deadline + 60 * 86_400;
        assert_eq!(
            hosted::due_lifecycle(&ctx.db, warning_at).await.unwrap(),
            vec![LifecycleAction::Warn {
                organizer_id: id.clone(),
                days_left: 30,
                lapse_generation: 1
            }]
        );
        crabgate::hosted_jobs::run(&ctx, &registry, warning_at)
            .await
            .unwrap();
        assert!(hosted::due_lifecycle(&ctx.db, warning_at)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            hosted::due_lifecycle(&ctx.db, deadline + 83 * 86_400)
                .await
                .unwrap(),
            vec![LifecycleAction::Warn {
                organizer_id: id.clone(),
                days_left: 7,
                lapse_generation: 1
            }]
        );
        assert_eq!(
            hosted::due_lifecycle(&ctx.db, deadline + 89 * 86_400)
                .await
                .unwrap(),
            vec![LifecycleAction::Warn {
                organizer_id: id.clone(),
                days_left: 1,
                lapse_generation: 1
            }]
        );
        assert_eq!(
            hosted::due_lifecycle(&ctx.db, deadline + 90 * 86_400)
                .await
                .unwrap(),
            vec![LifecycleAction::Delete {
                organizer_id: id.clone()
            }]
        );
        hosted::mark_deleted(&ctx.db, id, deadline + 90 * 86_400)
            .await
            .unwrap();
        assert_eq!(
            hosted::due_lifecycle(&ctx.db, deadline + 90 * 86_400)
                .await
                .unwrap(),
            vec![LifecycleAction::Delete {
                organizer_id: id.clone()
            }]
        );
        assert!(registry.resolve("harbour.crabgate.com").await.is_err());
    })
    .await;
}

#[tokio::test]
#[serial]
async fn hosted_job_deletes_lapsed_organizer_database_at_ninety_days() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|_, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let organizer = registry.create("Old Arts", "old-arts").await.unwrap();
        let path = organizer.db_path.clone();
        assert!(std::path::Path::new(&path).exists());
        let start = 1_800_000_000;
        ctx.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO subscriptions (id, organizer_id, status, lapsed_at) VALUES (?, ?, 'Lapsed', ?)",
            ["old-subscription".into(), organizer.id.clone().into(), start.into()]
        )).await.unwrap();
        crabgate::hosted_jobs::run(&ctx, &registry, start + 90 * 86_400).await.unwrap();
        assert!(!std::path::Path::new(&path).exists());
        assert_eq!(hosted::access_state(&ctx.db, &organizer.id).await.unwrap().subscription_status, "Deleted");
    }).await;
}

impl Drop for HostedEnvironment {
    fn drop(&mut self) {
        if let Some(prior) = &self.0 {
            std::env::set_var("CRABGATE_EDITION", prior);
        } else {
            std::env::remove_var("CRABGATE_EDITION");
        }
    }
}

#[tokio::test]
#[serial]
async fn each_host_reads_only_its_organizer_database() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        registry.create("Alpha Arts", "alpha").await.unwrap();
        registry.create("Beta Arts", "beta").await.unwrap();
        for (subdomain, title) in [
            ("alpha.crabgate.com", "Only Alpha Events"),
            ("beta.crabgate.com", "Only Beta Events"),
        ] {
            let organizer = registry.resolve(subdomain).await.unwrap();
            let event = events::create(
                &organizer.db,
                &EventInput {
                    title: title.to_owned(),
                    date: "2027-06-15".to_owned(),
                    start_time: "19:00".to_owned(),
                    end_time: "21:00".to_owned(),
                    timezone: "Europe/Dublin".to_owned(),
                    venue: "Hall".to_owned(),
                    address: "1 Main Street".to_owned(),
                    description: String::new(),
                    event_capacity: None,
                    max_per_order: 10,
                    attendee_name_required: false,
                },
            )
            .await
            .unwrap();
            events::add_ticket_type(
                &organizer.db,
                &event,
                &TicketTypeInput {
                    name: "General admission".to_owned(),
                    price: "10".to_owned(),
                    capacity: 20,
                    sales_open: None,
                    sales_close: None,
                },
            )
            .await
            .unwrap();
            events::publish(&organizer.db, &event.id, chrono::Utc::now().timestamp())
                .await
                .unwrap();
        }
        let alpha = request
            .get("/")
            .add_header("host", "alpha.crabgate.com")
            .await;
        assert_eq!(alpha.status_code(), 200);
        assert!(alpha.text().contains("Only Alpha Events"));
        assert!(!alpha.text().contains("Only Beta Events"));
        let beta = request
            .get("/")
            .add_header("host", "beta.crabgate.com")
            .await;
        assert_eq!(beta.status_code(), 200);
        assert!(beta.text().contains("Only Beta Events"));
        assert!(!beta.text().contains("Only Alpha Events"));
        assert_eq!(
            request
                .get("/")
                .add_header("host", "gamma.crabgate.com")
                .await
                .status_code(),
            404
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn failed_migration_suspends_only_its_organizer() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        registry.create("Working Arts", "working").await.unwrap();
        let bad = registry.create("Broken Arts", "broken").await.unwrap();
        let mut config = ctx.config.database.clone();
        config.uri = format!("sqlite://{}?mode=rw", bad.db_path);
        let bad_db = db::connect(&config).await.unwrap();
        bad_db
            .execute_unprepared(
                "DELETE FROM seaql_migrations WHERE version = 'm20260930_000003_events'",
            )
            .await
            .unwrap();
        bad_db.close().await.unwrap();
        registry.prepare().await.unwrap();
        let reason: String = ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT suspension_reason FROM organizers WHERE subdomain = 'broken'",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "suspension_reason")
            .unwrap();
        assert_eq!(reason, "migration failed");
        assert_eq!(
            request
                .get("/")
                .add_header("host", "broken.crabgate.com")
                .await
                .status_code(),
            503
        );
        assert_eq!(
            request
                .get("/")
                .add_header("host", "working.crabgate.com")
                .await
                .status_code(),
            200
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn free_tools_render_only_on_the_marketing_host_and_appear_in_sitemap() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, _| async move {
        let sitemap = request
            .get("/sitemap.xml")
            .add_header("host", "crabgate.com")
            .await;
        assert_eq!(sitemap.status_code(), 200);
        for (slug, heading, script) in [
            (
                "ticketsource-fee-calculator",
                "TicketSource fee calculator",
                "fees",
            ),
            (
                "humanitix-fee-calculator",
                "Humanitix fee calculator",
                "fees",
            ),
            ("tito-fee-calculator", "Tito fee calculator", "fees"),
            (
                "ticket-tailor-fee-calculator",
                "Ticket Tailor fee calculator",
                "fees",
            ),
            (
                "eventbrite-fee-calculator",
                "Eventbrite fee calculator",
                "fees",
            ),
            (
                "door-split-calculator",
                "Door split calculator",
                "door-split",
            ),
            (
                "ticket-price-calculator",
                "Ticket price calculator",
                "ticket-price",
            ),
            ("stripe-fee-calculator", "Stripe fee calculator", "stripe"),
            ("ticket-maker", "Printable ticket maker", "ticket-maker"),
        ] {
            let path = format!("/tools/{slug}");
            let page = request.get(&path).add_header("host", "crabgate.com").await;
            assert_eq!(page.status_code(), 200, "{path}");
            let html = page.text();
            assert_eq!(html.matches("<h1").count(), 1, "{path}");
            assert!(
                html.contains(&format!("<h1 class=\"h-xl lp-h1\">{heading}</h1>")),
                "{path}"
            );
            assert!(
                html.contains(&format!(
                    "<link rel=\"canonical\" href=\"https://crabgate.com{path}\">"
                )),
                "{path}"
            );
            let script_url = if script == "fees" {
                "/static/marketing/tools/fees.js?v=4b648303a9a0".to_owned()
            } else {
                format!("/static/marketing/tools/{script}.js")
            };
            assert!(
                html.contains(&format!("<script src=\"{script_url}\" defer></script>")),
                "{path}"
            );
            assert!(html.contains("aria-live=\"polite\""), "{path}");
            assert!(html.contains("Worked example"), "{path}");
            assert!(
                sitemap
                    .text()
                    .contains(&format!("<loc>https://crabgate.com{path}</loc>")),
                "{path}"
            );
            assert_eq!(
                request
                    .get(&path)
                    .add_header("host", "other.crabgate.com")
                    .await
                    .status_code(),
                404,
                "{path}"
            );
        }
        let index = request
            .get("/tools")
            .add_header("host", "crabgate.com")
            .await;
        assert_eq!(index.status_code(), 200);
        assert!(index
            .text()
            .contains("<h1 class=\"h-xl lp-h1\">Free tools for event organisers</h1>"));
        assert!(sitemap
            .text()
            .contains("<loc>https://crabgate.com/tools</loc>"));
        assert_eq!(
            request
                .get("/tools")
                .add_header("host", "other.crabgate.com")
                .await
                .status_code(),
            404
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn ticket_maker_qr_is_svg_and_has_a_character_cap() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, _| async move {
        for text in [
            "hello".to_owned(),
            "x".repeat(200),
            "é".repeat(200),
            "<script>alert(1)</script>".to_owned(),
        ] {
            let response = request
                .get("/tools/ticket-qr")
                .add_query_param("text", &text)
                .add_header("host", "crabgate.com")
                .await;
            assert_eq!(response.status_code(), 200);
            assert_eq!(response.header("content-type"), "image/svg+xml");
            assert_eq!(response.header("cache-control"), "private, max-age=86400");
            assert!(response.text().contains("<svg"));
            assert!(!response.text().contains("<script>"));
        }
        for text in ["x".repeat(201), "é".repeat(201), String::new()] {
            assert_eq!(
                request
                    .get("/tools/ticket-qr")
                    .add_query_param("text", &text)
                    .add_header("host", "crabgate.com")
                    .await
                    .status_code(),
                400
            );
        }
        assert_eq!(
            request
                .get("/tools/ticket-qr?text=hello")
                .add_header("host", "other.crabgate.com")
                .await
                .status_code(),
            404
        );
    })
    .await;
}
#[tokio::test]
#[serial]
async fn billing_start_recovers_a_lost_checkout_response_without_a_new_attempt() {
    use axum::{
        body::Bytes,
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::post,
        Json, Router,
    };
    use std::sync::{Arc, Mutex};
    type RetrySeen = Arc<Mutex<Vec<(String, String)>>>;
    async fn provider(
        State(seen): State<RetrySeen>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let key = headers["idempotency-key"].to_str().unwrap().to_owned();
        let body = String::from_utf8(body.to_vec()).unwrap();
        let mut records = seen.lock().unwrap();
        let same = records.first().is_none_or(|(k, b)| k == &key && b == &body);
        records.push((key, body));
        if records.len() == 1 {
            let expiry: i64 = body_expiry(&records[0].1);
            assert!(expiry >= hosted::now() + 30 * 60);

            return (
                StatusCode::OK,
                Json(serde_json::json!({"id":"cs_retry_accepted"})),
            );
        }
        if !same {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({"error":{"type":"idempotency_error", "message":"same key, different parameters"}}),
                ),
            );
        }
        (
            StatusCode::OK,
            Json(
                serde_json::json!({"id":"cs_retry_accepted","url":"https://checkout.stripe.test/recovered"}),
            ),
        )
    }
    fn body_expiry(body: &str) -> i64 {
        body.split('&')
            .find_map(|p| p.strip_prefix("expires_at="))
            .unwrap()
            .parse()
            .unwrap()
    }
    let _hosted = HostedEnvironment::set();
    let seen: RetrySeen = Arc::default();
    let app = Router::new()
        .route("/v1/checkout/sessions", post(provider))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var("STRIPE_API_BASE", base);
    std::env::set_var("CRABGATE_BILLING_SECRET_KEY", "sk_test_example");
    std::env::set_var("CRABGATE_BILLING_PRICE_ID", "price_retry");
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let other = hosted::signup(&registry, &ctx.db, "Other Retry", "other-retry", "other-retry@example.com", hosted::now()).await.unwrap();
        let signup = hosted::signup(&registry, &ctx.db, "Retry Edge", "retry-edge", "retry@example.com", hosted::now()).await.unwrap();
        let complete = request.get(&format!("/office/complete-signup/{}", signup.verification_token)).add_header("host", "retry-edge.crabgate.com").await;
        assert_eq!(complete.status_code(), 303);
        let cookie = complete.header("set-cookie").to_str().unwrap().split(';').next().unwrap().to_owned();
        let seeded = hosted::billing_attempt(&ctx.db, &signup.organizer.id, hosted::now() - 35 * 60).await.unwrap().unwrap();
        assert!(seeded.can_request_checkout(hosted::now()));
        let first = request.post("/office/billing/start").add_header("host", "retry-edge.crabgate.com").add_header("cookie", &cookie).await;
        assert_eq!(first.status_code(), 500);
        let before = hosted::billing_attempt(&ctx.db, &signup.organizer.id, hosted::now()).await.unwrap().unwrap();
        assert_eq!(before.checkout_url, None);
        let row = ctx.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "SELECT billing_attempt_started_at FROM subscriptions WHERE organizer_id = ?", [signup.organizer.id.clone().into()])).await.unwrap().unwrap();
        assert_eq!(before.expires_at, row.try_get::<i64>("", "billing_attempt_started_at").unwrap() + 3960);
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let second = request.post("/office/billing/start").add_header("host", "retry-edge.crabgate.com").add_header("cookie", &cookie).await;
        let records = seen.lock().unwrap().clone();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].0, before.key);
        assert_eq!(records[1].0, before.key);
        assert_eq!(records[0].1, records[1].1);
        assert_eq!(body_expiry(&records[0].1), before.expires_at);
        let first_params: std::collections::BTreeMap<_,_> = records[0].1.split('&').map(|p| p.split_once('=').unwrap()).collect();
        let second_params: std::collections::BTreeMap<_,_> = records[1].1.split('&').map(|p| p.split_once('=').unwrap()).collect();
        let differences: Vec<_> = first_params.iter().filter(|(k,v)| second_params.get(*k) != Some(*v)).map(|(k,_)| *k).collect();
        assert!(differences.is_empty());
        assert_eq!(second.status_code(), 303, "same-attempt retry should recover the already-created Checkout");
        assert_eq!(second.header("location").to_str().unwrap(), "https://checkout.stripe.test/recovered");
        let cached = hosted::billing_attempt(&ctx.db, &signup.organizer.id, hosted::now()).await.unwrap().unwrap();
        assert_eq!(cached.key, before.key);
        assert_eq!(cached.checkout_url.as_deref(), Some("https://checkout.stripe.test/recovered"));
        let third = request.post("/office/billing/start").add_header("host", "retry-edge.crabgate.com").add_header("cookie", &cookie).await;
        assert_eq!(third.status_code(), 303);
        assert_eq!(third.header("location").to_str().unwrap(), "https://checkout.stripe.test/recovered");
        assert_eq!(seen.lock().unwrap().len(), 2);
        let rows = ctx.db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT organizer_id, stripe_checkout_session_id, billing_attempt_key FROM subscriptions WHERE organizer_id IN (?, ?)",
            [signup.organizer.id.clone().into(), other.organizer.id.clone().into()])).await.unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            let id: String = row.try_get("", "organizer_id").unwrap();
            let session: Option<String> = row.try_get("", "stripe_checkout_session_id").unwrap();
            let key: Option<String> = row.try_get("", "billing_attempt_key").unwrap();
            if id == signup.organizer.id {
                assert_eq!(session.as_deref(), Some("cs_retry_accepted"));
                assert_eq!(key.as_deref(), Some(before.key.as_str()));
            } else {
                assert_eq!(id, other.organizer.id);
                assert_eq!(session, None);
                assert_eq!(key, None);
            }
        }
    }).await;
    server.abort();
    if let Some(value) = prior {
        std::env::set_var("STRIPE_API_BASE", value);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }
    std::env::remove_var("CRABGATE_BILLING_SECRET_KEY");
    std::env::remove_var("CRABGATE_BILLING_PRICE_ID");
}

#[tokio::test]
#[serial]
async fn billing_attempt_keeps_fixed_expiry_and_rotates_only_after_seventy_minutes() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|_, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let start = 1_800_000_000;
        let signup = hosted::signup(
            &registry,
            &ctx.db,
            "Timer",
            "timer",
            "timer@example.com",
            start,
        )
        .await
        .unwrap();
        let id = &signup.organizer.id;
        let first = hosted::billing_attempt(&ctx.db, id, start)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.expires_at, 1_800_003_960);
        assert_eq!(first.checkout_url, None);
        for age in [0, 120, 2099, 2100, 2159, 2160, 2161, 3959, 3960, 4199] {
            let retry = hosted::billing_attempt(&ctx.db, id, start + age)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(retry.key, first.key);
            assert_eq!(retry.expires_at, 1_800_003_960);
            assert_eq!(retry.checkout_url, None);
        }
        assert!(first.can_request_checkout(start + 2160));
        assert!(!first.can_request_checkout(start + 2161));
        assert!(!first.is_expired(start + 3959));
        assert!(first.is_expired(start + 3960));
        assert!(hosted::save_billing_checkout(
            &ctx.db,
            id,
            &first.key,
            "cs_timer",
            "https://checkout.stripe.test/timer"
        )
        .await
        .unwrap());
        for age in [3959, 3960, 4199] {
            let cached = hosted::billing_attempt(&ctx.db, id, start + age)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(cached.key, first.key);
            assert_eq!(
                cached.checkout_url.as_deref(),
                Some("https://checkout.stripe.test/timer")
            );
            assert_eq!(cached.is_expired(start + age), age >= 3960);
        }
        let fresh = hosted::billing_attempt(&ctx.db, id, start + 4200)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(fresh.key, first.key);
        assert_eq!(fresh.expires_at, 1_800_008_160);
        assert_eq!(fresh.checkout_url, None);
        assert!(first.is_expired(start + 4200));
        assert!(!hosted::save_billing_checkout(
            &ctx.db,
            id,
            &first.key,
            "cs_stale",
            "https://checkout.stripe.test/stale"
        )
        .await
        .unwrap());
        let row = ctx
            .db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT stripe_checkout_session_id FROM subscriptions WHERE organizer_id = ?",
                [id.clone().into()],
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.try_get::<Option<String>>("", "stripe_checkout_session_id")
                .unwrap(),
            None
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn billing_start_observes_creation_expiry_and_retention_windows() {
    use axum::{
        body::Bytes,
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::post,
        Json, Router,
    };
    use std::sync::{Arc, Mutex};
    #[derive(Default)]
    struct Provider {
        calls: Vec<(String, String)>,
        accepted: std::collections::BTreeMap<String, (String, i64, String)>,
    }
    async fn provider(
        State(state): State<Arc<Mutex<Provider>>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let key = headers["idempotency-key"].to_str().unwrap().to_owned();
        let body = String::from_utf8(body.to_vec()).unwrap();
        let fields: std::collections::BTreeMap<_, _> = body
            .split('&')
            .map(|p| p.split_once('=').unwrap())
            .collect();
        let expiry: i64 = fields["expires_at"].parse().unwrap();
        let owner = fields["client_reference_id"].to_owned();
        let mut state = state.lock().unwrap();
        state.calls.push((key.clone(), body.clone()));
        if let Some((original, _, _)) = state.accepted.get(&key) {
            if original != &body {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":{"message":"different parameters"}})),
                );
            }
        } else {
            let now = hosted::now();
            if expiry < now + 1800 || expiry > now + 86400 {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":{"message":"invalid expiry"}})),
                );
            }
            assert!(
                !state
                    .accepted
                    .values()
                    .any(|(_, until, organizer)| organizer == &owner && *until > now),
                "accepted Sessions must not overlap for one organizer"
            );
            state.accepted.insert(key.clone(), (body, expiry, owner));
        }
        (
            StatusCode::OK,
            Json(
                serde_json::json!({"id":format!("cs_{key}"),"url":"https://checkout.stripe.test/window"}),
            ),
        )
    }
    let _hosted = HostedEnvironment::set();
    let state = Arc::new(Mutex::new(Provider::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/checkout/sessions", post(provider))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let prior = std::env::var("STRIPE_API_BASE").ok();
    std::env::set_var("STRIPE_API_BASE", base);
    std::env::set_var("CRABGATE_BILLING_SECRET_KEY", "sk_test_example");
    std::env::set_var("CRABGATE_BILLING_PRICE_ID", "price_window");
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        for age in [0, 120, 2099, 2100, 2150, 2161, 3950, 3960, 4190, 4200] {
            let now = hosted::now();
            let subdomain = format!("window-{age}");
            let host = format!("{subdomain}.crabgate.com");
            let signup = hosted::signup(
                &registry,
                &ctx.db,
                "Window",
                &subdomain,
                &format!("{subdomain}@example.com"),
                now,
            )
            .await
            .unwrap();
            let complete = request
                .get(&format!(
                    "/office/complete-signup/{}",
                    signup.verification_token
                ))
                .add_header("host", &host)
                .await;
            assert_eq!(complete.status_code(), 303);
            let cookie = complete
                .header("set-cookie")
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned();
            let attempt = hosted::billing_attempt(&ctx.db, &signup.organizer.id, now - age)
                .await
                .unwrap()
                .unwrap();
            if age == 4200 {
                state.lock().unwrap().accepted.insert(
                    attempt.key.clone(),
                    (String::new(), now - 240, signup.organizer.id.clone()),
                );
                assert!(hosted::save_billing_checkout(
                    &ctx.db,
                    &signup.organizer.id,
                    &attempt.key,
                    "cs_old_expired",
                    "https://checkout.stripe.test/expired"
                )
                .await
                .unwrap());
            }
            let count = state.lock().unwrap().calls.len();
            let response = request
                .post("/office/billing/start")
                .add_header("host", &host)
                .add_header("cookie", &cookie)
                .await;
            assert_eq!(response.status_code(), 303);
            let location = response.header("location").to_str().unwrap().to_owned();
            if age <= 2150 || age == 4200 {
                assert_eq!(location, "https://checkout.stripe.test/window");
                let current = hosted::billing_attempt(&ctx.db, &signup.organizer.id, hosted::now())
                    .await
                    .unwrap()
                    .unwrap();
                if age == 4200 {
                    assert_ne!(current.key, attempt.key);
                    assert!(!hosted::save_billing_checkout(
                        &ctx.db,
                        &signup.organizer.id,
                        &attempt.key,
                        "cs_stale",
                        "https://checkout.stripe.test/stale"
                    )
                    .await
                    .unwrap());
                } else {
                    assert_eq!(current.key, attempt.key);
                    assert_eq!(current.expires_at, now - age + 3960);
                }
                let cached = request
                    .post("/office/billing/start")
                    .add_header("host", &host)
                    .add_header("cookie", &cookie)
                    .await;
                assert_eq!(cached.status_code(), 303);
                assert_eq!(
                    cached.header("location").to_str().unwrap(),
                    "https://checkout.stripe.test/window"
                );
                assert_eq!(state.lock().unwrap().calls.len(), count + 1);
            } else {
                let error = if age >= 3960 {
                    "billing_expired"
                } else {
                    "billing_retry_wait"
                };
                assert_eq!(
                    location,
                    format!("/office/setup?step=subscription&error={error}")
                );
                let feedback = request
                    .get(&location)
                    .add_header("host", &host)
                    .add_header("cookie", &cookie)
                    .await;
                assert_eq!(feedback.status_code(), 200);
                assert!(feedback.text().contains(if age >= 3960 {
                    "Wait up to 4 minutes"
                } else {
                    "Wait up to 34 minutes"
                }));
                assert!(feedback.text().contains("action=\"/office/billing/start\""));
                assert_eq!(state.lock().unwrap().calls.len(), count);
                assert_eq!(
                    hosted::billing_attempt(&ctx.db, &signup.organizer.id, hosted::now())
                        .await
                        .unwrap()
                        .unwrap()
                        .key,
                    attempt.key
                );
            }
            if age == 3950 {
                assert!(hosted::save_billing_checkout(
                    &ctx.db,
                    &signup.organizer.id,
                    &attempt.key,
                    "cs_near_expiry",
                    "https://checkout.stripe.test/near-expiry"
                )
                .await
                .unwrap());
                let cached = request
                    .post("/office/billing/start")
                    .add_header("host", &host)
                    .add_header("cookie", &cookie)
                    .await;
                assert_eq!(cached.status_code(), 303);
                assert_eq!(
                    cached.header("location").to_str().unwrap(),
                    "https://checkout.stripe.test/near-expiry"
                );
                assert_eq!(state.lock().unwrap().calls.len(), count);
            }
            if age == 3960 || age == 4190 {
                assert!(hosted::save_billing_checkout(
                    &ctx.db,
                    &signup.organizer.id,
                    &attempt.key,
                    "cs_expired",
                    "https://checkout.stripe.test/expired"
                )
                .await
                .unwrap());
                let expired = request
                    .post("/office/billing/start")
                    .add_header("host", &host)
                    .add_header("cookie", &cookie)
                    .await;
                assert_eq!(expired.status_code(), 303);
                assert_eq!(
                    expired.header("location").to_str().unwrap(),
                    "/office/setup?step=subscription&error=billing_expired"
                );
                assert_eq!(state.lock().unwrap().calls.len(), count);
            }
        }
        assert_eq!(state.lock().unwrap().calls.len(), 6);
        assert_eq!(state.lock().unwrap().accepted.len(), 7);
    })
    .await;
    server.abort();
    if let Some(value) = prior {
        std::env::set_var("STRIPE_API_BASE", value);
    } else {
        std::env::remove_var("STRIPE_API_BASE");
    }
    std::env::remove_var("CRABGATE_BILLING_SECRET_KEY");
    std::env::remove_var("CRABGATE_BILLING_PRICE_ID");
}

#[tokio::test]
#[serial]
async fn hosted_guides_render_with_downloads_and_signup_handoff() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, _| async move {
        let hub = request
            .get("/guides")
            .add_header("host", "crabgate.com")
            .await;
        assert_eq!(hub.status_code(), 200);
        assert!(hub.text().contains("Practical guides for your next event"));
        let sitemap = request
            .get("/sitemap.xml")
            .add_header("host", "crabgate.com")
            .await;
        for (path, title, download, artifact_text) in [
            (
                "/guides/workshop-registration-form",
                "Workshop registration form: free template",
                "/static/marketing/guides/workshop-registration-template.txt",
                "Place 2 name:",
            ),
            (
                "/guides/sell-tickets-small-event",
                "How to sell tickets for a small event",
                "/static/marketing/guides/small-event-launch-checklist.txt",
                "Check one place and a multi-place order.",
            ),
        ] {
            let page = request.get(path).add_header("host", "crabgate.com").await;
            assert_eq!(page.status_code(), 200, "{path}");
            let html = page.text();
            assert!(
                html.contains(&format!("<title>{title} | CrabGate</title>")),
                "{path}"
            );
            assert_eq!(html.matches("<h1").count(), 1, "{path}");
            assert!(html.contains("<html lang=\"en-US\">"), "{path}");
            assert!(
                html.contains(&format!("href=\"https://crabgate.com{path}\"")),
                "{path}"
            );
            assert!(
                html.contains(&format!("href=\"{download}\" download")),
                "{path}"
            );
            assert!(
                html.contains("href=\"/signup\">Set up your next event</a>"),
                "{path}"
            );
            assert!(html.contains("$99 a year"), "{path}");
            let schema = html
                .split("<script type=\"application/ld+json\">")
                .nth(1)
                .unwrap()
                .split("</script>")
                .next()
                .unwrap();
            let schema: serde_json::Value = serde_json::from_str(schema).unwrap();
            assert_eq!(schema["@type"], "Article");
            assert_eq!(
                schema["mainEntityOfPage"],
                format!("https://crabgate.com{path}")
            );
            assert_eq!(schema["author"]["name"], "Paul Bădărău");
            assert!(
                !html.contains("$299") && !html.contains("noindex"),
                "{path}"
            );
            assert!(html.contains("mailto:hello@crabgate.com"), "{path}");
            assert!(hub.text().contains(&format!("href=\"{path}\"")), "{path}");
            assert!(
                sitemap
                    .text()
                    .contains(&format!("<loc>https://crabgate.com{path}</loc>")),
                "{path}"
            );
            let file = request
                .get(download)
                .add_header("host", "crabgate.com")
                .await;
            assert_eq!(file.status_code(), 200, "{download}");
            assert!(file.text().contains(artifact_text), "{download}");
            let shop_page = request
                .get(path)
                .add_header("host", "shop.crabgate.com")
                .await;
            assert_eq!(shop_page.status_code(), 404, "{path}");
        }
        let signup = request
            .get("/signup")
            .add_header("host", "crabgate.com")
            .await;
        assert_eq!(signup.status_code(), 200);
        assert!(signup.text().contains("Create my shop"));
        assert!(signup.text().contains("Your next event and its date"));
    })
    .await;
}
