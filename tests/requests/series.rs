use crabgate::{
    app::App,
    events::{self, EventInput, TicketTypeInput},
    members::{self, Role},
};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;

#[tokio::test]
#[serial]
async fn admin_copies_an_event_and_repeats_it_as_a_series() {
    request::<App, _, _>(|request, ctx| async move {
        let organizer = ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT id, db_path FROM organizers WHERE subdomain = 'local'".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        let id: String = organizer.try_get("", "id").unwrap();
        let path: String = organizer.try_get("", "db_path").unwrap();
        let db = sea_orm::Database::connect(format!("sqlite://{path}?mode=rw"))
            .await
            .unwrap();
        let event = events::create(
            &db,
            &EventInput {
                title: "Quiz Night".to_owned(),
                date: "2030-01-03".to_owned(),
                start_time: "20:00".to_owned(),
                end_time: "22:30".to_owned(),
                timezone: "Europe/Dublin".to_owned(),
                venue: "The Anchor".to_owned(),
                address: "2 Quay Street".to_owned(),
                description: String::new(),
                event_capacity: None,
                max_per_order: 10,
                attendee_name_required: false,
            },
        )
        .await
        .unwrap();
        events::add_ticket_type(
            &db,
            &event,
            &TicketTypeInput {
                name: "Team seat".to_owned(),
                price: "5".to_owned(),
                capacity: 60,
                sales_open: None,
                sales_close: None,
            },
        )
        .await
        .unwrap();
        members::add_member(&ctx.db, &id, "series-admin@example.com", Role::Admin)
            .await
            .unwrap();
        let session = members::create_session_for_email(
            &ctx.db,
            &id,
            "series-admin@example.com",
            chrono::Utc::now().timestamp(),
        )
        .await
        .unwrap()
        .unwrap();
        let cookie = format!("cg_session={session}");

        let editor = request
            .get(&format!("/office/events/{}", event.id))
            .add_header("cookie", &cookie)
            .await
            .text();
        assert!(editor.contains("Upcoming date preview"));
        assert!(editor.contains("2030-01-10"));
        assert!(editor.contains("<h2 id=\"copy-heading\" class=\"h-m\">Copy this event</h2>"));
        assert!(editor.contains("<option value=\"Monthly\">Every month on the first Thursday</option>"));
        assert!(editor.contains("New dates start as Drafts. If you want them on sale by themselves, publish this event before you start the Series."));

        let preview = request.get(&format!("/office/events/{}?frequency=Monthly&end_date=2030-03-07", event.id))
            .add_header("cookie", &cookie).await;
        assert_eq!(preview.status_code(), 200);
        assert!(preview.text().contains("2030-02-07"));
        assert!(preview.text().contains("2030-03-07"));
        assert!(!preview.text().contains("2030-04-04"));
        assert!(preview.text().contains("value=\"Monthly\" selected"));
        assert!(preview.text().contains("value=\"2030-03-07\""));
        let no_dates = request.get(&format!("/office/events/{}?frequency=Weekly&end_date=2030-01-03", event.id))
            .add_header("cookie", &cookie).await;
        assert!(no_dates.text().contains("No new dates before the end date."));
        let bad_preview = request.get(&format!("/office/events/{}?frequency=Monthly&end_date=2029-12-01", event.id))
            .add_header("cookie", &cookie).await;
        assert_eq!(bad_preview.status_code(), 400);
        assert!(bad_preview.text().contains("The end date is before this event"));
        assert!(bad_preview.text().contains("value=\"2029-12-01\""));
        let invalid = request.get(&format!("/office/events/{}?frequency=Unsupported&end_date=not-a-date", event.id))
            .add_header("cookie", &cookie).await;
        assert_eq!(invalid.status_code(), 400);
        assert!(invalid.text().contains("Unsupported (choose a supported repeat)"));
        assert!(invalid.text().contains("Submitted end date: not-a-date"));
        let invalid_end = request.get(&format!("/office/events/{}?frequency=Monthly&end_date=not-a-date", event.id))
            .add_header("cookie", &cookie).await;
        assert_eq!(invalid_end.status_code(), 400);
        assert!(invalid_end.text().contains("Enter a valid end date, or leave it empty."));
        assert!(invalid_end.text().contains("Submitted end date: not-a-date"));
        let fortnightly = request.get(&format!("/office/events/{}?frequency=Fortnightly&end_date=2030-01-31", event.id))
            .add_header("cookie", &cookie).await;
        assert!(fortnightly.text().contains("2030-01-17"));
        assert!(fortnightly.text().contains("2030-01-31"));
        assert!(!fortnightly.text().contains("2030-01-10"));
        let default_preview = request
            .get(&format!(
                "/office/events/{}?end_date=2030-01-17&unrelated=1",
                event.id
            ))
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(default_preview.status_code(), 200);
        assert!(default_preview.text().contains("value=\"Weekly\" selected"));
        assert!(default_preview.text().contains("2030-01-10"));
        assert!(default_preview.text().contains("2030-01-17"));
        assert!(!default_preview.text().contains("2030-01-24"));

        let incomplete_repeats: &[&[(&str, &str)]] = &[
            &[("end_date", "")],
            &[("end_date", "2030-01-17")],
            &[],
            &[("frequency", ""), ("end_date", "2030-01-17")],
            &[("frequency", "Unsupported"), ("end_date", "2030-01-17")],
        ];
        for body in incomplete_repeats {
            let rejected = request
                .post(&format!("/office/events/{}/repeat", event.id))
                .add_header("cookie", &cookie)
                .form(body)
                .await;
            assert_eq!(rejected.status_code(), 400, "repeat body: {body:?}");
            assert!(rejected
                .text()
                .contains("Choose how often this event repeats."));
            if body.contains(&("end_date", "2030-01-17")) {
                assert!(rejected.text().contains("value=\"2030-01-17\""));
            }
            if body.contains(&("frequency", "Unsupported")) {
                assert!(rejected
                    .text()
                    .contains("Unsupported (choose a supported repeat)"));
            }
            for (table, expected) in [
                ("events", 1),
                ("series", 0),
                ("holds", 0),
                ("series_exceptions", 0),
            ] {
                let count: i64 = db
                    .query_one_raw(Statement::from_string(
                        DatabaseBackend::Sqlite,
                        format!("SELECT COUNT(*) AS count FROM {table}"),
                    ))
                    .await
                    .unwrap()
                    .unwrap()
                    .try_get("", "count")
                    .unwrap();
                assert_eq!(count, expected, "{table} after repeat body: {body:?}");
            }
            let source = events::get(&db, &event.id).await.unwrap().unwrap();
            assert_eq!(source.title, "Quiz Night");
            assert_eq!(source.date, "2030-01-03");
            assert_eq!(source.status, "Draft");
            assert!(source.series_id.is_none());
        }
        let count: i64 = db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM events")).await.unwrap().unwrap().try_get("", "count").unwrap();
        assert_eq!(count, 1);
        let series_count: i64 = db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM series")).await.unwrap().unwrap().try_get("", "count").unwrap();
        assert_eq!(series_count, 0);
        let holds: i64 = db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM holds")).await.unwrap().unwrap().try_get("", "count").unwrap();
        assert_eq!(holds, 0);
        std::fs::write(super::evidence_path("cg010-admin-preview-render.html"), preview.text()).unwrap();
        let bad_copy = request
            .post(&format!("/office/events/{}/copy", event.id))
            .add_header("cookie", &cookie)
            .form(&[("date", "")])
            .await;
        assert_eq!(bad_copy.status_code(), 400);
        assert!(bad_copy
            .text()
            .contains("We couldn&#39;t copy this event. Enter a valid date for the copy."));

        let copied = request
            .post(&format!("/office/events/{}/copy", event.id))
            .add_header("cookie", &cookie)
            .form(&[("date", "2030-02-14")])
            .await;
        assert_eq!(copied.status_code(), 303);
        let location = copied.header("location").to_str().unwrap().to_owned();
        assert_ne!(location, format!("/office/events/{}", event.id));
        let copy_page = request
            .get(&location)
            .add_header("cookie", &cookie)
            .await
            .text();
        assert!(copy_page.contains("value=\"2030-02-14\""));
        assert!(copy_page.contains("<span class=\"status status-draft\">Draft</span>"));

        let bad_repeat = request
            .post(&format!("/office/events/{}/repeat", event.id))
            .add_header("cookie", &cookie)
            .form(&[("frequency", "Weekly"), ("end_date", "2029-12-01")])
            .await;
        assert_eq!(bad_repeat.status_code(), 400);
        assert!(bad_repeat.text().contains("We couldn&#39;t start the Series. The end date is before this event. Choose a later end date, or leave it empty."));

        let repeated = request
            .post(&format!("/office/events/{}/repeat", event.id))
            .add_header("cookie", &cookie)
            .form(&[("frequency", "Weekly"), ("end_date", "2030-12-19")])
            .await;
        assert_eq!(repeated.status_code(), 303);
        let schedule_snapshot = || Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT frequency || '|' || COALESCE(end_date, '') || '|' || created_through || '|' || status || '|' || setup_json || '|' || updated_at AS snapshot FROM series");
        let before: String = db.query_one_raw(schedule_snapshot()).await.unwrap().unwrap().try_get("", "snapshot").unwrap();
        let panel = request
            .get(&format!("/office/events/{}", event.id))
            .add_header("cookie", &cookie)
            .await
            .text();
        assert!(panel.contains("Every Thursday at 20:00, until 19 December 2030."));
        assert!(panel.contains("8 future dates exist. We keep the next 8 in place."));
        assert!(panel.contains("Use this date's setup for future dates"));
        assert!(!panel.contains("id=\"repeat-heading\""));

        assert!(panel.contains("Upcoming Events and saved exceptions (unchanged)"));
        assert!(panel.contains("Next dates not yet created"));
        assert!(panel.contains("2030-02-28"));
        assert!(panel.contains("Preview using this date's saved setup"));
        assert!(panel.contains("saved exception snapshots stay as they are"));
        let after: String = db.query_one_raw(schedule_snapshot()).await.unwrap().unwrap().try_get("", "snapshot").unwrap();
        assert_eq!(after, before);
        let count: i64 = db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM events WHERE series_id IS NOT NULL")).await.unwrap().unwrap().try_get("", "count").unwrap();
        assert_eq!(count, 8);
        let list = request
            .get("/office/events")
            .add_header("cookie", &cookie)
            .await
            .text();
        assert_eq!(list.matches("Repeats weekly").count(), 8);

        let stopped = request
            .post(&format!("/office/events/{}/series/stop", event.id))
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(stopped.status_code(), 303);
        let panel = request
            .get(&format!("/office/events/{}", event.id))
            .add_header("cookie", &cookie)
            .await
            .text();
        assert!(panel.contains(
            "This Series has stopped, so we won't create more dates. Every existing Event, Order, date and saved exception stays intact."
        ));
        assert!(!panel.contains("Stop repeating"));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg009_admin_corrects_invalid_date_as_draft_without_public_or_staff_leaks() {
    use crabgate::series::{self, Frequency};
    request::<App, _, _>(|request, ctx| async move {
        let organizer = ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT id, db_path FROM organizers WHERE subdomain = 'local'",
            ))
            .await
            .unwrap()
            .unwrap();
        let id: String = organizer.try_get("", "id").unwrap();
        let path: String = organizer.try_get("", "db_path").unwrap();
        let db = sea_orm::Database::connect(format!("sqlite://{path}?mode=rw"))
            .await
            .unwrap();
        let now = chrono::Utc::now().timestamp();
        members::add_member(&ctx.db, &id, "cg009-admin@example.com", Role::Admin)
            .await
            .unwrap();
        let session =
            members::create_session_for_email(&ctx.db, &id, "cg009-admin@example.com", now)
                .await
                .unwrap()
                .unwrap();
        let cookie = format!("cg_session={session}");
        let empty = request
            .get("/office/events")
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(empty.status_code(), 200);
        assert!(empty.text().contains("Your first event starts here"));
        assert!(!empty.text().contains("Series dates to review"));
        std::fs::write(
            super::evidence_path("cg009-empty-render.html"),
            empty.text(),
        )
        .unwrap();
        let source = events::create(
            &db,
            &EventInput {
                title: "DST Workshop".into(),
                date: "2027-03-07".into(),
                start_time: "01:30".into(),
                end_time: "03:00".into(),
                timezone: "Europe/Dublin".into(),
                venue: "Hall".into(),
                address: "Road".into(),
                description: String::new(),
                event_capacity: Some(2),
                max_per_order: 2,
                attendee_name_required: false,
            },
        )
        .await
        .unwrap();
        events::add_ticket_type(
            &db,
            &source,
            &TicketTypeInput {
                name: "Seat".into(),
                price: "0".into(),
                capacity: 2,
                sales_open: None,
                sales_close: None,
            },
        )
        .await
        .unwrap();
        events::publish(&db, &source.id, now).await.unwrap();
        let series_id = series::start(
            &db,
            &source.id,
            Frequency::Weekly,
            None,
            &Err("You'll need a subscription before you can publish.".into()),
            now,
        )
        .await
        .unwrap();
        let route = format!("/office/series/{series_id}/dates/2027-03-28/correct");
        let page = request
            .get("/office/events")
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(page.status_code(), 200);
        let html = page.text();
        assert!(html.contains("Series dates to review"));
        assert!(html.contains("2027-03-28"));
        assert!(html.contains("does not exist in Europe/Dublin because clocks change"));
        assert!(html.contains(
            "Saved Draft reason: You&#39;ll need a subscription before you can publish."
        ));
        assert!(html.contains("Create corrected Draft"));
        std::fs::write(super::evidence_path("cg009-admin-render.html"), &html).unwrap();
        assert_eq!(page.header("cache-control").to_str().unwrap(), "no-store");
        let public = request.get("/").await.text();
        assert!(!public.contains("2027-03-28"));
        assert!(!public.contains("Series dates to review"));
        assert!(!public.contains("subscription before you can publish"));
        assert_eq!(
            request
                .post(&route)
                .form(&[("start_time", "02:30"), ("end_time", "03:00")])
                .await
                .status_code(),
            303
        );
        members::add_member(&ctx.db, &id, "cg009-door@example.com", Role::DoorStaff)
            .await
            .unwrap();
        let staff_link = members::request_magic_link(&ctx.db, &id, "cg009-door@example.com", now)
            .await
            .unwrap()
            .unwrap();
        let staff = members::claim_magic_link(&ctx.db, &id, &staff_link, now)
            .await
            .unwrap()
            .unwrap();
        let staff_cookie = format!("cg_session={staff}");
        assert_eq!(
            request
                .get(&format!(
                    "/office/events/{}?frequency=Monthly&end_date=2027-12-31",
                    source.id
                ))
                .add_header("cookie", &staff_cookie)
                .await
                .status_code(),
            403
        );
        assert!(!public.contains("Next dates not yet created"));
        assert!(!public.contains("Upcoming date preview"));
        assert_eq!(
            request
                .get("/office/events")
                .add_header("cookie", &staff_cookie)
                .await
                .status_code(),
            403
        );
        assert_eq!(
            request
                .post(&route)
                .add_header("cookie", &staff_cookie)
                .form(&[("start_time", "02:30"), ("end_time", "03:00")])
                .await
                .status_code(),
            403
        );
        assert_eq!(
            request
                .post(&route)
                .add_header("cookie", &cookie)
                .add_header("origin", "https://evil.example")
                .form(&[("start_time", "02:30"), ("end_time", "03:00")])
                .await
                .status_code(),
            403
        );
        let invalid = request
            .post(&route)
            .add_header("cookie", &cookie)
            .form(&[("start_time", "01:45"), ("end_time", "03:00")])
            .await;
        assert_eq!(invalid.status_code(), 400);
        assert!(invalid.text().contains("01:45 does not exist"));
        assert!(invalid.text().contains("value=\"01:45\""));
        std::fs::write(
            super::evidence_path("cg009-invalid-render.html"),
            invalid.text(),
        )
        .unwrap();
        let corrected = request
            .post(&route)
            .add_header("cookie", &cookie)
            .form(&[("start_time", "02:30"), ("end_time", "03:00")])
            .await;
        assert_eq!(corrected.status_code(), 303);
        let location = corrected.header("location").to_str().unwrap().to_owned();
        let corrected_id = location.trim_start_matches("/office/events/");
        assert_eq!(
            events::get(&db, corrected_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "Draft"
        );
        assert_eq!(
            request
                .get(&format!("/events/{corrected_id}"))
                .await
                .status_code(),
            404
        );
        let corrected_type = events::list_ticket_types(&db, corrected_id).await.unwrap()[0]
            .id
            .clone();
        let checkout = request
            .post("/checkout")
            .form(&[
                ("event_id".to_owned(), corrected_id.to_owned()),
                (format!("quantity_{corrected_type}"), "1".to_owned()),
            ])
            .await;
        assert_eq!(checkout.status_code(), 400);
        assert_eq!(checkout.text(), "This ticket isn't on sale.");
        series::top_up(&db, &Ok(()), now).await.unwrap();
        assert_eq!(
            events::get(&db, corrected_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "Draft"
        );
        let editor = request.get(&location).add_header("cookie", &cookie).await;
        assert_eq!(editor.status_code(), 200);
        assert!(editor
            .text()
            .contains("<span class=\"status status-draft\">Draft</span>"));
        assert!(editor.text().contains("Publish event"));
        assert_eq!(
            request
                .post(&format!("{location}/publish"))
                .add_header("cookie", &cookie)
                .await
                .status_code(),
            303
        );
        assert_eq!(
            events::get(&db, corrected_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "Published"
        );
        assert_eq!(
            request
                .post(&route)
                .add_header("cookie", &cookie)
                .form(&[("start_time", "02:30"), ("end_time", "03:00")])
                .await
                .status_code(),
            404
        );
    })
    .await;
}

struct SeriesHostedEnvironment(Option<String>);
impl SeriesHostedEnvironment {
    fn set() -> Self {
        let previous = std::env::var("CRABGATE_EDITION").ok();
        std::env::set_var("CRABGATE_EDITION", "hosted");
        Self(previous)
    }
}
impl Drop for SeriesHostedEnvironment {
    fn drop(&mut self) {
        match &self.0 {
            Some(value) => std::env::set_var("CRABGATE_EDITION", value),
            None => std::env::remove_var("CRABGATE_EDITION"),
        }
    }
}

#[tokio::test]
#[serial]
async fn cg009_hosted_readiness_repair_requires_explicit_publish_and_preserves_held_draft() {
    use crabgate::{hosted, series, settings};
    let _hosted = SeriesHostedEnvironment::set();
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let organizer = hosted::signup(
            &registry,
            &ctx.db,
            "CG009 Hosted",
            "cg009-hosted",
            "cg009-hosted@example.com",
            chrono::Utc::now().timestamp(),
        )
        .await
        .unwrap()
        .organizer;
        let scope = registry.resolve("cg009-hosted.crabgate.com").await.unwrap();
        let now = chrono::Utc::now().timestamp();
        ctx.db
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE organizers SET verified_at = ? WHERE id = ?",
                [now.into(), organizer.id.clone().into()],
            ))
            .await
            .unwrap();
        let source = events::create(
            &scope.db,
            &EventInput {
                title: "Hosted Workshop".into(),
                date: "2027-03-07".into(),
                start_time: "20:00".into(),
                end_time: "22:00".into(),
                timezone: "Europe/Dublin".into(),
                venue: "Hall".into(),
                address: "Road".into(),
                description: String::new(),
                event_capacity: Some(2),
                max_per_order: 2,
                attendee_name_required: false,
            },
        )
        .await
        .unwrap();
        events::add_ticket_type(
            &scope.db,
            &source,
            &TicketTypeInput {
                name: "Seat".into(),
                price: "0".into(),
                capacity: 2,
                sales_open: None,
                sales_close: None,
            },
        )
        .await
        .unwrap();
        events::publish(&scope.db, &source.id, now).await.unwrap();
        members::add_member(
            &ctx.db,
            &organizer.id,
            "cg009-hosted@example.com",
            Role::Admin,
        )
        .await
        .unwrap();
        let session = members::create_session_for_email(
            &ctx.db,
            &organizer.id,
            "cg009-hosted@example.com",
            now,
        )
        .await
        .unwrap()
        .unwrap();
        let cookie = format!("cg_session={session}");
        let host = "cg009-hosted.crabgate.com";
        assert_eq!(
            request
                .post(&format!("/office/events/{}/repeat", source.id))
                .add_header("host", host)
                .add_header("cookie", &cookie)
                .form(&[("frequency", "Weekly"), ("end_date", "")])
                .await
                .status_code(),
            303
        );
        let page = request
            .get("/office/events")
            .add_header("host", host)
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(page.status_code(), 200);
        assert!(page.text().contains(
            "Saved Draft reason: You&#39;ll need a subscription before you can publish."
        ));
        std::fs::write(
            super::evidence_path("cg009-hosted-render.html"),
            page.text(),
        )
        .unwrap();
        let row = scope
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT id FROM events WHERE date = '2027-03-14'",
            ))
            .await
            .unwrap()
            .unwrap();
        let blocked: String = row.try_get("", "id").unwrap();
        let publish_route = format!("/office/events/{blocked}/publish");
        let denied = request
            .post(&publish_route)
            .add_header("host", host)
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(denied.status_code(), 403);
        assert_eq!(
            denied.text(),
            "You'll need a subscription before you can publish."
        );
        assert_eq!(
            request
                .post(&format!("/office/events/{}/draft", source.id))
                .add_header("host", host)
                .add_header("cookie", &cookie)
                .await
                .status_code(),
            303
        );
        hosted::subscription_active(
            &ctx.db,
            &organizer.id,
            "cus_synthetic",
            "sub_synthetic",
            now,
            now,
        )
        .await
        .unwrap();
        let cipher = registry.cipher(&organizer).unwrap();
        settings::set(&scope.db, &cipher, "stripe_key_tested_at", &now.to_string())
            .await
            .unwrap();
        settings::set(&scope.db, &cipher, "webhook_tested_at", &now.to_string())
            .await
            .unwrap();
        series::run(&ctx.db, &registry, now).await.unwrap();
        assert_eq!(
            events::get(&scope.db, &blocked)
                .await
                .unwrap()
                .unwrap()
                .status,
            "Draft"
        );
        assert_eq!(
            events::get(&scope.db, &source.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "Draft"
        );
        let editor = request
            .get(&format!("/office/events/{blocked}"))
            .add_header("host", host)
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(editor.status_code(), 200);
        let editor_html = editor.text();
        std::fs::write(
            super::evidence_path("cg009-hosted-ready-editor.html"),
            &editor_html,
        )
        .unwrap();
        let publish_form = editor_html
            .split("id=\"publish-form\"")
            .nth(1)
            .unwrap()
            .split("</form>")
            .next()
            .unwrap();
        assert!(publish_form.contains("Publish event"));
        assert!(!publish_form.contains("disabled"));
        let public = request.get("/").add_header("host", host).await;
        assert_eq!(public.status_code(), 200);
        assert!(!public.text().contains("Hosted Workshop"));
        assert_eq!(
            request
                .post(&publish_route)
                .add_header("host", host)
                .add_header("cookie", &cookie)
                .await
                .status_code(),
            303
        );
        assert_eq!(
            events::get(&scope.db, &blocked)
                .await
                .unwrap()
                .unwrap()
                .status,
            "Published"
        );
        assert_eq!(
            events::get(&scope.db, &source.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "Draft"
        );
    })
    .await;
}
