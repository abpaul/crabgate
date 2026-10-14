use crabgate::{
    app::App,
    events::{self, Event, EventInput, TicketTypeInput},
};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::Value;
use serial_test::serial;

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
        if let Some(prior) = &self.0 {
            std::env::set_var("CRABGATE_EDITION", prior);
        } else {
            std::env::remove_var("CRABGATE_EDITION");
        }
    }
}

async fn local_db(ctx: &loco_rs::app::AppContext) -> DatabaseConnection {
    let row = ctx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT db_path FROM organizers WHERE subdomain = 'local'".to_owned(),
        ))
        .await
        .unwrap()
        .unwrap();
    let path: String = row.try_get("", "db_path").unwrap();
    sea_orm::Database::connect(format!("sqlite://{path}?mode=rw"))
        .await
        .unwrap()
}

async fn create_event(db: &DatabaseConnection, title: &str, description: &str) -> Event {
    events::create(
        db,
        &EventInput {
            title: title.to_owned(),
            date: "2030-10-17".to_owned(),
            start_time: "19:30".to_owned(),
            end_time: "22:00".to_owned(),
            timezone: "Europe/Dublin".to_owned(),
            venue: "Harbour Hall".to_owned(),
            address: "1 Quay Street, Galway".to_owned(),
            description: description.to_owned(),
            event_capacity: None,
            max_per_order: 10,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap()
}

async fn add_type(
    db: &DatabaseConnection,
    event: &Event,
    name: &str,
    price: &str,
    capacity: i64,
    sales_open: Option<&str>,
) {
    events::add_ticket_type(
        db,
        event,
        &TicketTypeInput {
            name: name.to_owned(),
            price: price.to_owned(),
            capacity,
            sales_open: sales_open.map(str::to_owned),
            sales_close: None,
        },
    )
    .await
    .unwrap();
}

fn json_ld(html: &str) -> Value {
    let open = "<script type=\"application/ld+json\">";
    let start = html.find(open).expect("JSON-LD script") + open.len();
    let end = start + html[start..].find("</script>").unwrap();
    serde_json::from_str(&html[start..end]).unwrap()
}

#[tokio::test]
#[serial]
async fn event_page_describes_the_event_for_search_engines() {
    request::<App, _, _>(|request, ctx| async move {
        let db = local_db(&ctx).await;
        let event = create_event(&db, "Harbour Lights", "A night of sea songs.").await;
        add_type(&db, &event, "Standard", "15.5", 20, None).await;
        add_type(&db, &event, "Front row", "30", 0, None).await;
        add_type(
            &db,
            &event,
            "Late release",
            "12",
            5,
            Some("2030-09-01T10:00"),
        )
        .await;
        add_type(&db, &event, "Early bird", "10", 5, None).await;
        sea_orm::ConnectionTrait::execute_raw(
            &db,
            sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Sqlite,
                "UPDATE ticket_types SET sales_close = ? WHERE event_id = ? AND name = 'Early bird'",
                [
                    (chrono::Utc::now().timestamp() - 3600).into(),
                    event.id.clone().into(),
                ],
            ),
        )
        .await
        .unwrap();
        events::publish(&db, &event.id, chrono::Utc::now().timestamp())
            .await
            .unwrap();

        let page = request.get(&format!("/events/{}", event.id)).await;
        assert_eq!(page.status_code(), 200);
        let data = json_ld(&page.text());
        let url = format!("http://localhost:5150/events/{}", event.id);
        assert_eq!(data["@context"], "https://schema.org");
        assert_eq!(data["@type"], "Event");
        assert_eq!(data["name"], "Harbour Lights");
        assert_eq!(data["description"], "A night of sea songs.");
        assert_eq!(data["startDate"], "2030-10-17T19:30:00+01:00");
        assert_eq!(data["endDate"], "2030-10-17T22:00:00+01:00");
        assert_eq!(data["eventStatus"], "https://schema.org/EventScheduled");
        assert_eq!(
            data["eventAttendanceMode"],
            "https://schema.org/OfflineEventAttendanceMode"
        );
        assert_eq!(data["location"]["@type"], "Place");
        assert_eq!(data["location"]["name"], "Harbour Hall");
        assert_eq!(data["location"]["address"]["@type"], "PostalAddress");
        assert_eq!(
            data["location"]["address"]["streetAddress"],
            "1 Quay Street, Galway"
        );
        assert_eq!(data["organizer"]["@type"], "Organization");
        assert_eq!(data["organizer"]["url"], "http://localhost:5150");
        assert!(data.get("image").is_none());

        let offers = data["offers"].as_array().unwrap();
        assert_eq!(offers.len(), 3);
        assert!(offers.iter().all(|offer| offer["name"] != "Early bird"));
        assert_eq!(offers[0]["@type"], "Offer");
        assert_eq!(offers[0]["name"], "Standard");
        assert_eq!(offers[0]["price"], "15.50");
        assert_eq!(offers[0]["priceCurrency"], "EUR");
        assert_eq!(offers[0]["availability"], "https://schema.org/InStock");
        assert_eq!(offers[0]["url"], url.as_str());
        assert!(offers[0].get("validFrom").is_none());
        assert_eq!(offers[1]["name"], "Front row");
        assert_eq!(offers[1]["price"], "30.00");
        assert_eq!(offers[1]["availability"], "https://schema.org/SoldOut");
        assert_eq!(offers[2]["price"], "12.00");
        assert_eq!(offers[2]["validFrom"], "2030-09-01T10:00:00+01:00");
    })
    .await;
}

#[tokio::test]
#[serial]
async fn event_title_cannot_close_the_json_ld_script() {
    request::<App, _, _>(|request, ctx| async move {
        let db = local_db(&ctx).await;
        let title = "Gig </script><script>alert(1)</script> <!-- night";
        let event = create_event(&db, title, "").await;
        add_type(&db, &event, "Standard", "10", 20, None).await;
        events::publish(&db, &event.id, chrono::Utc::now().timestamp())
            .await
            .unwrap();

        let html = request.get(&format!("/events/{}", event.id)).await.text();
        assert!(
            html.contains(r#""name":"Gig <\/script><script>alert(1)<\/script> \u003c!-- night""#)
        );
        let data = json_ld(&html);
        assert_eq!(data["name"], title);
        assert!(data.get("description").is_none());
    })
    .await;
}

#[tokio::test]
#[serial]
async fn shop_sitemap_lists_only_published_events_that_have_not_ended() {
    request::<App, _, _>(|request, ctx| async move {
        let db = local_db(&ctx).await;
        let now = chrono::Utc::now().timestamp();
        let published = create_event(&db, "Published show", "").await;
        add_type(&db, &published, "Standard", "10", 20, None).await;
        events::publish(&db, &published.id, now).await.unwrap();
        let draft = create_event(&db, "Draft show", "").await;
        let past = create_event(&db, "Past show", "").await;
        add_type(&db, &past, "Standard", "10", 20, None).await;
        events::publish(&db, &past.id, now).await.unwrap();
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET starts_at = 1000, ends_at = 2000 WHERE id = ?",
            [past.id.clone().into()],
        ))
        .await
        .unwrap();
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET updated_at = 1893456000 WHERE id = ?",
            [published.id.clone().into()],
        ))
        .await
        .unwrap();

        let sitemap = request.get("/sitemap.xml").await;
        assert_eq!(sitemap.status_code(), 200);
        assert_eq!(
            sitemap.header("content-type"),
            "application/xml; charset=utf-8"
        );
        let xml = sitemap.text();
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.contains("<url><loc>http://localhost:5150/</loc></url>"));
        assert!(xml.contains(&format!(
            "<url><loc>http://localhost:5150/events/{}</loc><lastmod>2030-01-01T00:00:00Z</lastmod></url>",
            published.id
        )));
        assert!(!xml.contains(&draft.id));
        assert!(!xml.contains(&past.id));

        let robots = request.get("/robots.txt").await;
        assert_eq!(robots.status_code(), 200);
        assert_eq!(robots.header("content-type"), "text/plain; charset=utf-8");
        assert_eq!(
            robots.text(),
            "User-agent: *\nAllow: /\nDisallow: /office\nDisallow: /checkout\nDisallow: /orders\nDisallow: /door\n\nSitemap: http://localhost:5150/sitemap.xml\n"
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn hosted_marketing_and_shop_hosts_have_their_own_sitemap_and_robots() {
    let _hosted = HostedEnvironment::set();
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        registry.create("Harbour Arts", "harbour-seo").await.unwrap();
        let shop = registry.resolve("harbour-seo.crabgate.com").await.unwrap();
        let event = create_event(&shop.db, "Hosted show", "").await;
        add_type(&shop.db, &event, "Standard", "10", 20, None).await;
        events::publish(&shop.db, &event.id, chrono::Utc::now().timestamp())
            .await
            .unwrap();

        let sitemap = request
            .get("/sitemap.xml")
            .add_header("host", "crabgate.com")
            .await;
        assert_eq!(sitemap.status_code(), 200);
        assert_eq!(
            sitemap.header("content-type"),
            "application/xml; charset=utf-8"
        );
        let xml = sitemap.text();
        for path in [
            "/",
            "/pricing",
            "/eventbrite-alternative",
            "/for-rooms-with-an-audience",
            "/for-conferences",
            "/for-workshops",
            "/privacy",
            "/terms",
        ] {
            assert!(
                xml.contains(&format!("<url><loc>https://crabgate.com{path}</loc></url>")),
                "{path}"
            );
        }
        assert_eq!(xml.matches("<url>").count(), 21);
        assert!(!xml.contains("/signup"));

        let robots = request
            .get("/robots.txt")
            .add_header("host", "crabgate.com")
            .await;
        assert_eq!(robots.status_code(), 200);
        assert_eq!(
            robots.text(),
            "User-agent: *\nAllow: /\nDisallow: /signup\nDisallow: /verify\nDisallow: /office\nDisallow: /operator\n\nSitemap: https://crabgate.com/sitemap.xml\n"
        );

        let shop_sitemap = request
            .get("/sitemap.xml")
            .add_header("host", "harbour-seo.crabgate.com")
            .await;
        assert_eq!(shop_sitemap.status_code(), 200);
        assert!(shop_sitemap.text().contains(&format!(
            "<loc>https://harbour-seo.crabgate.com/events/{}</loc>",
            event.id
        )));
        let shop_robots = request
            .get("/robots.txt")
            .add_header("host", "harbour-seo.crabgate.com")
            .await;
        assert_eq!(shop_robots.status_code(), 200);
        let shop_robots = shop_robots.text();
        assert!(shop_robots.contains("Disallow: /checkout\n"));
        assert!(shop_robots.contains("Disallow: /door\n"));
        assert!(shop_robots.ends_with("\nSitemap: https://harbour-seo.crabgate.com/sitemap.xml\n"));

        let page = request
            .get(&format!("/events/{}", event.id))
            .add_header("host", "harbour-seo.crabgate.com")
            .await;
        let data = json_ld(&page.text());
        assert_eq!(
            data["offers"][0]["url"],
            format!("https://harbour-seo.crabgate.com/events/{}", event.id).as_str()
        );
        assert_eq!(data["organizer"]["name"], "Harbour Arts");
        assert_eq!(data["organizer"]["url"], "https://harbour-seo.crabgate.com");

        for path in ["/sitemap.xml", "/robots.txt"] {
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
async fn every_host_serves_the_logo_as_its_icon() {
    request::<App, _, _>(|request, _ctx| async move {
        let icon = request.get("/favicon.ico").await;
        assert_eq!(icon.status_code(), 200);
        assert_eq!(icon.header("content-type"), "image/x-icon");
        assert_eq!(icon.as_bytes()[..4], [0, 0, 1, 0]);

        let touch = request.get("/apple-touch-icon.png").await;
        assert_eq!(touch.status_code(), 200);
        assert_eq!(touch.header("content-type"), "image/png");
        assert_eq!(touch.as_bytes()[..8], *b"\x89PNG\r\n\x1a\n");

        let page = request.get("/office/sign-in").await.text();
        assert!(page.contains(r#"<link rel="icon" href="/favicon.ico" sizes="32x32">"#));
        assert!(page.contains(
            r#"<link rel="icon" type="image/svg+xml" href="/static/shared/favicon.svg">"#
        ));
        assert!(page.contains(r#"<link rel="apple-touch-icon" href="/apple-touch-icon.png">"#));

        let svg = request.get("/static/shared/favicon.svg").await;
        assert_eq!(svg.status_code(), 200);
    })
    .await;
}
