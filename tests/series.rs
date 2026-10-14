use std::path::PathBuf;

use crabgate::{
    app::App,
    crypto::MasterKey,
    discounts, door,
    events::{self, EventInput, TicketTypeInput},
    hosted,
    inventory::{self, Selection},
    organizers::{Edition, OrganizerRegistry},
    questions, seats,
    series::{self, Frequency},
};
use loco_rs::{db, testing::prelude::*};
use migration::OrganizerMigrator;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

const DAY: i64 = 86_400;
const JAN_1_2027_NOON: i64 = 1_798_804_800;
const TEST_MASTER_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

async fn database() -> (tempfile::TempDir, DatabaseConnection) {
    let dir = tempfile::tempdir().unwrap();
    let connection = connect(&dir.path().join("series.db")).await;
    db::migrate::<OrganizerMigrator>(&connection).await.unwrap();
    (dir, connection)
}

async fn connect(path: &std::path::Path) -> DatabaseConnection {
    sea_orm::Database::connect(format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap()
}

fn input(date: &str) -> EventInput {
    EventInput {
        title: "Thursday Jazz".to_owned(),
        date: date.to_owned(),
        start_time: "20:00".to_owned(),
        end_time: "23:00".to_owned(),
        timezone: "Europe/Dublin".to_owned(),
        venue: "Harbour Arts Centre".to_owned(),
        address: "1 Harbour Road".to_owned(),
        description: "Live jazz in the bar".to_owned(),
        event_capacity: Some(80),
        max_per_order: 6,
        attendee_name_required: true,
    }
}

async fn event_with_tickets(db: &DatabaseConnection, date: &str) -> events::Event {
    let event = events::create(db, &input(date)).await.unwrap();
    events::add_ticket_type(
        db,
        &event,
        &TicketTypeInput {
            name: "General admission".to_owned(),
            price: "15".to_owned(),
            capacity: 80,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    event
}

async fn series_dates(db: &DatabaseConnection, series_id: &str) -> Vec<(String, String, String)> {
    db.query_all_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT date, status, title FROM events WHERE series_id = ? ORDER BY series_date",
        [series_id.into()],
    ))
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        (
            row.try_get("", "date").unwrap(),
            row.try_get("", "status").unwrap(),
            row.try_get("", "title").unwrap(),
        )
    })
    .collect()
}

async fn count(db: &DatabaseConnection, sql: &str, id: &str) -> i64 {
    db.query_one_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        sql,
        [id.into()],
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get("", "count")
    .unwrap()
}

fn dates(rows: &[(String, String, String)]) -> Vec<&str> {
    rows.iter().map(|(date, _, _)| date.as_str()).collect()
}

#[tokio::test]
async fn a_copy_carries_the_setup_as_a_draft_and_leaves_sales_behind() {
    let (_dir, db) = database().await;
    let mut original_input = input("2027-06-15");
    original_input.start_time = "19:00".to_owned();
    original_input.end_time = "21:00".to_owned();
    let original = events::create(&db, &original_input).await.unwrap();
    let early = events::add_ticket_type(
        &db,
        &original,
        &TicketTypeInput {
            name: "Early bird".to_owned(),
            price: "12.50".to_owned(),
            capacity: 20,
            sales_open: Some("2027-06-01T10:00".to_owned()),
            sales_close: Some("2027-06-14T18:00".to_owned()),
        },
    )
    .await
    .unwrap();
    assert_eq!(early.sales_open, Some(1_811_840_400));
    events::add_ticket_type(
        &db,
        &original,
        &TicketTypeInput {
            name: "On the door".to_owned(),
            price: "20".to_owned(),
            capacity: 10,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    questions::add(
        &db,
        &original.id,
        "Order",
        "Text",
        "Any access needs?",
        &[],
        false,
    )
    .await
    .unwrap();
    questions::add(
        &db,
        &original.id,
        "Attendee",
        "Choice",
        "T-shirt size",
        &["S".to_owned(), "M".to_owned(), "L".to_owned()],
        true,
    )
    .await
    .unwrap();
    let now = 1_809_129_600;
    seats::add_many(
        &db,
        &original.id,
        &early.id,
        "Stalls",
        "A",
        &["1".to_owned(), "2".to_owned(), "3".to_owned()],
        now,
    )
    .await
    .unwrap();
    let third = seats::list(&db, &original.id, now)
        .await
        .unwrap()
        .into_iter()
        .find(|seat| seat.label == "3")
        .unwrap();
    seats::deactivate(&db, &original.id, &third.id, now)
        .await
        .unwrap();
    let cover = include_bytes!("fixtures/cover.png").to_vec();
    events::save_cover(&db, &original.id, "image/png", cover.clone())
        .await
        .unwrap();
    events::publish(&db, &original.id, now).await.unwrap();
    discounts::create(
        &db,
        discounts::NewDiscount {
            event_id: &original.id,
            code: "FRIENDS",
            kind: "Percent",
            value: 10,
            starts_at: None,
            ends_at: None,
            max_redemptions: None,
        },
        now,
    )
    .await
    .unwrap();
    let cipher = MasterKey::from_base64(TEST_MASTER_KEY)
        .unwrap()
        .generate_organizer_key("series-test")
        .unwrap()
        .1;
    door::create_link(&db, &cipher, &original.id, "Front door", now)
        .await
        .unwrap();
    let first_seat = seats::list(&db, &original.id, now).await.unwrap().remove(0);
    inventory::reserve_with_options(
        &db,
        &original.id,
        &[Selection {
            ticket_type_id: early.id.clone(),
            quantity: 1,
        }],
        None,
        &[first_seat.id],
        1_812_585_600,
    )
    .await
    .unwrap();
    let original = events::get(&db, &original.id).await.unwrap().unwrap();

    let copy = series::copy(&db, &original.id, "2027-07-20", now)
        .await
        .unwrap();

    assert_ne!(copy.id, original.id);
    assert_eq!(copy.status, "Draft");
    assert_eq!(copy.title, "Thursday Jazz");
    assert_eq!(copy.description, "Live jazz in the bar");
    assert_eq!(copy.venue, "Harbour Arts Centre");
    assert_eq!(copy.address, "1 Harbour Road");
    assert_eq!(copy.date, "2027-07-20");
    assert_eq!(copy.start_time, "19:00");
    assert_eq!(copy.end_time, "21:00");
    assert_eq!(copy.timezone, "Europe/Dublin");
    assert_eq!(copy.starts_at, 1_816_106_400);
    assert_eq!(copy.ends_at, 1_816_113_600);
    assert_eq!(copy.event_capacity, Some(80));
    assert_eq!(copy.max_per_order, 6);
    assert!(copy.attendee_name_required);
    assert_eq!(copy.series_id, None);
    assert_eq!(
        events::cover(&db, &copy.id).await.unwrap(),
        Some(("image/png".to_owned(), cover))
    );
    let types = events::list_ticket_types(&db, &copy.id).await.unwrap();
    let carried = types
        .iter()
        .map(|item| {
            (
                item.name.as_str(),
                item.price_cents,
                item.capacity,
                item.sales_open,
                item.sales_close,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        carried,
        vec![
            (
                "Early bird",
                1250,
                20,
                Some(1_814_864_400),
                Some(1_816_016_400)
            ),
            ("On the door", 2000, 10, None, None),
        ]
    );
    assert_eq!(
        types[0].sales_open.unwrap() - early.sales_open.unwrap(),
        35 * DAY
    );
    let copied_questions = questions::list(&db, &copy.id)
        .await
        .unwrap()
        .into_iter()
        .map(|question| {
            (
                question.scope,
                question.kind,
                question.label,
                question.options,
                question.required,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        copied_questions,
        vec![
            (
                "Order".to_owned(),
                "Text".to_owned(),
                "Any access needs?".to_owned(),
                vec![],
                false
            ),
            (
                "Attendee".to_owned(),
                "Choice".to_owned(),
                "T-shirt size".to_owned(),
                vec!["S".to_owned(), "M".to_owned(), "L".to_owned()],
                true
            ),
        ]
    );
    let copied_seats = seats::list(&db, &copy.id, now)
        .await
        .unwrap()
        .into_iter()
        .map(|seat| {
            (
                seat.ticket_type_id == types[0].id,
                seat.section,
                seat.row,
                seat.label,
                seat.active,
                seat.available,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        copied_seats,
        vec![
            (
                true,
                "Stalls".to_owned(),
                "A".to_owned(),
                "1".to_owned(),
                true,
                true
            ),
            (
                true,
                "Stalls".to_owned(),
                "A".to_owned(),
                "2".to_owned(),
                true,
                true
            ),
        ]
    );
    for table in [
        "discount_codes",
        "door_links",
        "holds",
        "orders",
        "tickets",
        "checkins",
        "event_change_notices",
    ] {
        let sql = format!("SELECT COUNT(*) AS count FROM {table} WHERE event_id = ?");
        assert_eq!(count(&db, &sql, &copy.id).await, 0, "{table}");
    }
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) AS count FROM holds WHERE event_id = ?",
            &original.id
        )
        .await,
        1
    );
}

#[tokio::test]
async fn a_copy_drops_closed_sales_windows_and_opens_past_ones_now() {
    let (_dir, db) = database().await;
    let mut original_input = input("2027-06-15");
    original_input.start_time = "19:00".to_owned();
    let original = events::create(&db, &original_input).await.unwrap();
    events::add_ticket_type(
        &db,
        &original,
        &TicketTypeInput {
            name: "Early bird".to_owned(),
            price: "12.50".to_owned(),
            capacity: 20,
            sales_open: Some("2027-06-01T10:00".to_owned()),
            sales_close: Some("2027-06-14T18:00".to_owned()),
        },
    )
    .await
    .unwrap();
    let open_in_past = series::copy(&db, &original.id, "2027-06-20", 1_812_585_600)
        .await
        .unwrap();
    let window = &events::list_ticket_types(&db, &open_in_past.id)
        .await
        .unwrap()[0];
    assert_eq!(
        (window.sales_open, window.sales_close),
        (None, Some(1_813_424_400))
    );
    let closed = series::copy(&db, &original.id, "2027-06-16", 1_813_089_600)
        .await
        .unwrap();
    let window = &events::list_ticket_types(&db, &closed.id).await.unwrap()[0];
    assert_eq!((window.sales_open, window.sales_close), (None, None));
}

#[tokio::test]
async fn a_failed_copy_leaves_no_event_behind() {
    let (_dir, db) = database().await;
    let mut late_night = input("2027-03-20");
    late_night.start_time = "02:30".to_owned();
    late_night.end_time = "04:00".to_owned();
    let original = events::create(&db, &late_night).await.unwrap();
    events::add_ticket_type(
        &db,
        &original,
        &TicketTypeInput {
            name: "Advance".to_owned(),
            price: "8".to_owned(),
            capacity: 40,
            sales_open: None,
            sales_close: Some("2027-03-20T01:45".to_owned()),
        },
    )
    .await
    .unwrap();
    let error = series::copy(&db, &original.id, "2027-03-28", JAN_1_2027_NOON)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Sales window must close before the event starts"
    );
    assert_eq!(events::list(&db).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_weekly_series_keeps_eight_published_dates_ahead() {
    let (_dir, db) = database().await;
    let source = event_with_tickets(&db, "2027-01-07").await;
    events::publish(&db, &source.id, JAN_1_2027_NOON)
        .await
        .unwrap();
    let series_id = series::start(
        &db,
        &source.id,
        Frequency::Weekly,
        None,
        &Ok(()),
        JAN_1_2027_NOON,
    )
    .await
    .unwrap();
    let rows = series_dates(&db, &series_id).await;
    assert_eq!(
        dates(&rows),
        vec![
            "2027-01-07",
            "2027-01-14",
            "2027-01-21",
            "2027-01-28",
            "2027-02-04",
            "2027-02-11",
            "2027-02-18",
            "2027-02-25"
        ]
    );
    assert!(rows.iter().all(|(_, status, _)| status == "Published"));
    assert_eq!(
        events::get(&db, &source.id)
            .await
            .unwrap()
            .unwrap()
            .series_id,
        Some(series_id.clone())
    );
    assert_eq!(
        series::top_up(&db, &Ok(()), JAN_1_2027_NOON).await.unwrap(),
        0
    );
    assert_eq!(series_dates(&db, &series_id).await.len(), 8);

    let week_later = JAN_1_2027_NOON + 7 * DAY;
    assert_eq!(series::top_up(&db, &Ok(()), week_later).await.unwrap(), 1);
    let rows = series_dates(&db, &series_id).await;
    assert_eq!(rows.len(), 9);
    assert_eq!(
        rows.last().unwrap(),
        &(
            "2027-03-04".to_owned(),
            "Published".to_owned(),
            "Thursday Jazz".to_owned()
        )
    );
    let panel = series::panel(
        &db,
        &events::get(&db, &source.id).await.unwrap().unwrap(),
        week_later,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(panel.rule, "Every Thursday at 20:00, with no end date");
    assert_eq!(panel.future_dates, 8);
    assert!(panel.active);
    assert_eq!(
        series::labels(&db).await.unwrap().get(&series_id).copied(),
        Some("Repeats weekly")
    );
}

#[tokio::test]
async fn a_weekly_date_keeps_its_local_time_across_a_clock_change() {
    let (_dir, db) = database().await;
    let source = event_with_tickets(&db, "2027-03-18").await;
    let series_id = series::start(
        &db,
        &source.id,
        Frequency::Weekly,
        Some("2027-04-01"),
        &Ok(()),
        JAN_1_2027_NOON,
    )
    .await
    .unwrap();
    let starts = db
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT date, start_time, starts_at FROM events WHERE series_id = ? ORDER BY series_date",
            [series_id.into()],
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.try_get::<String>("", "date").unwrap(),
                row.try_get::<String>("", "start_time").unwrap(),
                row.try_get::<i64>("", "starts_at").unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        starts[1..],
        [
            ("2027-03-25".to_owned(), "20:00".to_owned(), 1_806_004_800),
            ("2027-04-01".to_owned(), "20:00".to_owned(), 1_806_606_000),
        ]
    );
}

#[tokio::test]
async fn a_monthly_series_repeats_the_same_weekday_of_the_month() {
    let (_dir, db) = database().await;
    let source = event_with_tickets(&db, "2027-01-14").await;
    let series_id = series::start(
        &db,
        &source.id,
        Frequency::Monthly,
        None,
        &Ok(()),
        JAN_1_2027_NOON,
    )
    .await
    .unwrap();
    let rows = series_dates(&db, &series_id).await;
    assert_eq!(
        dates(&rows),
        vec![
            "2027-01-14",
            "2027-02-11",
            "2027-03-11",
            "2027-04-08",
            "2027-05-13",
            "2027-06-10",
            "2027-07-08",
            "2027-08-12"
        ]
    );
    assert!(rows.iter().all(|(_, status, _)| status == "Draft"));
    assert_eq!(
        series::panel(
            &db,
            &events::get(&db, &source.id).await.unwrap().unwrap(),
            JAN_1_2027_NOON
        )
        .await
        .unwrap()
        .unwrap()
        .rule,
        "Every month on the second Thursday at 20:00, with no end date"
    );
}

#[tokio::test]
async fn a_fifth_weekday_repeats_on_the_last_weekday_of_each_month() {
    let (_dir, db) = database().await;
    let source = event_with_tickets(&db, "2027-04-29").await;
    let series_id = series::start(
        &db,
        &source.id,
        Frequency::Monthly,
        Some("2027-12-17"),
        &Ok(()),
        JAN_1_2027_NOON,
    )
    .await
    .unwrap();
    assert_eq!(
        dates(&series_dates(&db, &series_id).await),
        vec![
            "2027-04-29",
            "2027-05-27",
            "2027-06-24",
            "2027-07-29",
            "2027-08-26",
            "2027-09-30",
            "2027-10-28",
            "2027-11-25"
        ]
    );
    assert_eq!(
        series::panel(
            &db,
            &events::get(&db, &source.id).await.unwrap().unwrap(),
            JAN_1_2027_NOON
        )
        .await
        .unwrap()
        .unwrap()
        .rule,
        "Every month on the last Thursday at 20:00, until 17 December"
    );
}

#[tokio::test]
async fn a_series_creates_no_date_after_its_end_date() {
    let (_dir, db) = database().await;
    let source = event_with_tickets(&db, "2027-01-07").await;
    let series_id = series::start(
        &db,
        &source.id,
        Frequency::Fortnightly,
        Some("2027-02-17"),
        &Ok(()),
        JAN_1_2027_NOON,
    )
    .await
    .unwrap();
    assert_eq!(
        dates(&series_dates(&db, &series_id).await),
        vec!["2027-01-07", "2027-01-21", "2027-02-04"]
    );
    assert_eq!(
        series::top_up(&db, &Ok(()), JAN_1_2027_NOON + 30 * DAY)
            .await
            .unwrap(),
        0
    );
    assert_eq!(series_dates(&db, &series_id).await.len(), 3);
}

#[tokio::test]
async fn a_stopped_series_creates_no_new_dates() {
    let (_dir, db) = database().await;
    let source = event_with_tickets(&db, "2027-01-07").await;
    let series_id = series::start(
        &db,
        &source.id,
        Frequency::Weekly,
        None,
        &Ok(()),
        JAN_1_2027_NOON,
    )
    .await
    .unwrap();
    series::stop(&db, &source.id, JAN_1_2027_NOON)
        .await
        .unwrap();
    assert_eq!(
        series::top_up(&db, &Ok(()), JAN_1_2027_NOON + 21 * DAY)
            .await
            .unwrap(),
        0
    );
    assert_eq!(series_dates(&db, &series_id).await.len(), 8);
    let panel = series::panel(
        &db,
        &events::get(&db, &source.id).await.unwrap().unwrap(),
        JAN_1_2027_NOON,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!panel.active);
    assert_eq!(
        series::labels(&db).await.unwrap().get(&series_id).copied(),
        Some("Series stopped")
    );
}

#[tokio::test]
async fn using_a_dates_setup_changes_only_dates_created_later() {
    let (_dir, db) = database().await;
    let source = event_with_tickets(&db, "2027-01-07").await;
    let series_id = series::start(
        &db,
        &source.id,
        Frequency::Weekly,
        None,
        &Ok(()),
        JAN_1_2027_NOON,
    )
    .await
    .unwrap();
    let third = events::list(&db)
        .await
        .unwrap()
        .into_iter()
        .find(|event| event.date == "2027-01-21")
        .unwrap();
    let mut renamed = input("2027-01-21");
    renamed.title = "Thursday Jazz and Blues".to_owned();
    events::update(&db, &third.id, &renamed).await.unwrap();
    series::use_setup_from(&db, &third.id, JAN_1_2027_NOON)
        .await
        .unwrap();
    assert_eq!(
        series::top_up(&db, &Ok(()), JAN_1_2027_NOON + 7 * DAY)
            .await
            .unwrap(),
        1
    );
    let titles = series_dates(&db, &series_id)
        .await
        .into_iter()
        .map(|(date, _, title)| format!("{date} {title}"))
        .collect::<Vec<_>>();
    assert_eq!(
        titles,
        vec![
            "2027-01-07 Thursday Jazz",
            "2027-01-14 Thursday Jazz",
            "2027-01-21 Thursday Jazz and Blues",
            "2027-01-28 Thursday Jazz",
            "2027-02-04 Thursday Jazz",
            "2027-02-11 Thursday Jazz",
            "2027-02-18 Thursday Jazz",
            "2027-02-25 Thursday Jazz",
            "2027-03-04 Thursday Jazz and Blues",
        ]
    );
}

#[tokio::test]
async fn two_top_ups_at_once_create_each_date_once() {
    let (dir, db) = database().await;
    let source = event_with_tickets(&db, "2027-01-07").await;
    let series_id = series::start(
        &db,
        &source.id,
        Frequency::Weekly,
        None,
        &Ok(()),
        JAN_1_2027_NOON,
    )
    .await
    .unwrap();
    let first = connect(&dir.path().join("series.db")).await;
    let second = connect(&dir.path().join("series.db")).await;
    let later = JAN_1_2027_NOON + 14 * DAY;
    let (left, right) = tokio::join!(
        series::top_up(&first, &Ok(()), later),
        series::top_up(&second, &Ok(()), later)
    );
    assert_eq!(left.unwrap() + right.unwrap(), 2);
    let rows = series_dates(&db, &series_id).await;
    assert_eq!(dates(&rows[8..]), vec!["2027-03-04", "2027-03-11"]);
    assert_eq!(rows.len(), 10);

    let duplicate = event_with_tickets(&db, "2027-03-11").await;
    let result = db
        .execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET series_id = ?, series_date = '2027-03-11' WHERE id = ?",
            [series_id.into(), duplicate.id.into()],
        ))
        .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("UNIQUE constraint failed: events.series_id, events.series_date"));
}

#[tokio::test]
#[serial]
async fn hosted_dates_stay_drafts_without_a_subscription() {
    request::<App, _, _>(|_request, ctx| async move {
        let dir = tempfile::tempdir().unwrap();
        let registry = registry(&ctx, dir.path().to_owned());
        let organizer = registry
            .create("Harbour Arts", "harbour-series")
            .await
            .unwrap();
        ctx.db
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE organizers SET verified_at = 1 WHERE id = ?",
                [organizer.id.clone().into()],
            ))
            .await
            .unwrap();
        let scope = registry
            .resolve("harbour-series.crabgate.com")
            .await
            .unwrap();
        let source = event_with_tickets(&scope.db, "2027-01-07").await;
        events::publish(&scope.db, &source.id, JAN_1_2027_NOON)
            .await
            .unwrap();
        let check = hosted::publish_check(
            &ctx.db,
            &registry,
            &scope.organizer,
            &scope.db,
            JAN_1_2027_NOON,
        )
        .await
        .map_err(|error| error.to_string());
        assert_eq!(
            check,
            Err("You'll need a subscription before you can publish.".to_owned())
        );
        let series_id = series::start(
            &scope.db,
            &source.id,
            Frequency::Weekly,
            None,
            &check,
            JAN_1_2027_NOON,
        )
        .await
        .unwrap();
        series::run(&ctx.db, &registry, JAN_1_2027_NOON + 7 * DAY)
            .await
            .unwrap();
        let statuses = series_dates(&scope.db, &series_id)
            .await
            .into_iter()
            .map(|(_, status, _)| status)
            .collect::<Vec<_>>();
        assert_eq!(statuses.len(), 9);
        assert_eq!(statuses[0], "Published");
        assert!(statuses[1..].iter().all(|status| status == "Draft"));
    })
    .await;
}

fn registry(ctx: &loco_rs::app::AppContext, dir: PathBuf) -> OrganizerRegistry {
    OrganizerRegistry::new(
        ctx.db.clone(),
        ctx.config.database.clone(),
        Edition::Hosted,
        "crabgate.com".to_owned(),
        dir,
        MasterKey::from_base64(TEST_MASTER_KEY).unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn cg009_invalid_dates_count_towards_the_eight_date_horizon() {
    let (_dir, db) = database().await;
    let mut setup = input("2027-03-07");
    setup.start_time = "01:30".into();
    setup.end_time = "03:00".into();
    let event = events::create(&db, &setup).await.unwrap();
    let id = series::start(
        &db,
        &event.id,
        Frequency::Weekly,
        None,
        &Ok(()),
        JAN_1_2027_NOON,
    )
    .await
    .unwrap();
    assert_eq!(
        dates(&series_dates(&db, &id).await),
        vec![
            "2027-03-07",
            "2027-03-14",
            "2027-03-21",
            "2027-04-04",
            "2027-04-11",
            "2027-04-18",
            "2027-04-25"
        ]
    );
    assert_eq!(
        series::top_up(&db, &Ok(()), JAN_1_2027_NOON).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn cg009_gap_and_fold_persist_until_explicit_correction_and_publish() {
    for (first, invalid, cause) in [
        ("2027-03-07", "2027-03-28", "does not exist"),
        ("2027-10-03", "2027-10-31", "occurs twice"),
    ] {
        let (dir, db) = database().await;
        let mut setup = input(first);
        setup.start_time = "01:30".into();
        setup.end_time = "03:00".into();
        let source = events::create(&db, &setup).await.unwrap();
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
        events::publish(&db, &source.id, JAN_1_2027_NOON)
            .await
            .unwrap();
        let id = series::start(
            &db,
            &source.id,
            Frequency::Weekly,
            None,
            &Ok(()),
            JAN_1_2027_NOON,
        )
        .await
        .unwrap();
        let reviews = series::reviews(&db).await.unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].date, invalid);
        assert_eq!(reviews[0].start_time, "01:30");
        assert!(reviews[0].reason.contains(cause));
        assert!(reviews[0].event_id.is_none());
        assert_eq!(
            series::top_up(&db, &Ok(()), JAN_1_2027_NOON).await.unwrap(),
            0
        );
        let reopened = connect(&dir.path().join("series.db")).await;
        assert_eq!(series::reviews(&reopened).await.unwrap()[0].date, invalid);
        let error = series::correct_exception(&db, &id, invalid, "01:30", "03:00", JAN_1_2027_NOON)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains(cause));
        assert_eq!(series::reviews(&db).await.unwrap().len(), 1);
        let mut changed = setup.clone();
        changed.title = "Later setup".into();
        changed.start_time = "19:00".into();
        changed.end_time = "21:00".into();
        events::update(&db, &source.id, &changed).await.unwrap();
        series::use_setup_from(&db, &source.id, JAN_1_2027_NOON)
            .await
            .unwrap();
        assert_eq!(
            series::reviews(&db).await.unwrap()[0].title,
            "Thursday Jazz"
        );
        let corrected =
            series::correct_exception(&db, &id, invalid, "02:30", "03:00", JAN_1_2027_NOON)
                .await
                .unwrap();
        assert_eq!(corrected.status, "Draft");
        assert_eq!(corrected.date, invalid);
        assert_eq!(corrected.title, "Thursday Jazz");
        assert_eq!(corrected.series_id, Some(id.clone()));
        assert_eq!(
            events::list_ticket_types(&db, &corrected.id).await.unwrap()[0].capacity,
            2
        );
        assert_eq!(series_dates(&db, &id).await.len(), 8);
        assert_eq!(
            series::top_up(&db, &Ok(()), JAN_1_2027_NOON).await.unwrap(),
            0
        );
        assert_eq!(
            events::get(&db, &corrected.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "Draft"
        );
        assert!(
            series::correct_exception(&db, &id, invalid, "02:30", "03:00", JAN_1_2027_NOON)
                .await
                .is_err()
        );
        events::publish(&db, &corrected.id, JAN_1_2027_NOON)
            .await
            .unwrap();
        assert_eq!(
            events::get(&db, &corrected.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "Published"
        );
        assert!(series::reviews(&db).await.unwrap().is_empty());
        let corrected_type = events::list_ticket_types(&db, &corrected.id).await.unwrap()[0]
            .id
            .clone();
        let source_type = events::list_ticket_types(&db, &source.id).await.unwrap()[0]
            .id
            .clone();
        assert_ne!(corrected_type, source_type);
        let hold = inventory::reserve(
            &db,
            &corrected.id,
            &[Selection {
                ticket_type_id: corrected_type.clone(),
                quantity: 2,
            }],
            JAN_1_2027_NOON,
        )
        .await
        .unwrap();
        assert_eq!(hold.event_id, corrected.id);
        assert!(matches!(
            inventory::reserve(
                &db,
                &corrected.id,
                &[Selection {
                    ticket_type_id: corrected_type,
                    quantity: 1
                }],
                JAN_1_2027_NOON
            )
            .await,
            Err(inventory::InventoryError::SoldOut)
        ));
        let other_hold = inventory::reserve(
            &db,
            &source.id,
            &[Selection {
                ticket_type_id: source_type,
                quantity: 2,
            }],
            JAN_1_2027_NOON,
        )
        .await
        .unwrap();
        assert_eq!(other_hold.event_id, source.id);
        assert_ne!(hold.id, other_hold.id);
    }
}

#[tokio::test]
async fn cg009_readiness_recovery_and_time_edits_never_publish_existing_drafts() {
    let (_dir, db) = database().await;
    let source = event_with_tickets(&db, "2027-01-07").await;
    events::publish(&db, &source.id, JAN_1_2027_NOON)
        .await
        .unwrap();
    let id = series::start(
        &db,
        &source.id,
        Frequency::Weekly,
        None,
        &Err("You'll need a subscription before you can publish.".into()),
        JAN_1_2027_NOON,
    )
    .await
    .unwrap();
    let reviews = series::reviews(&db).await.unwrap();
    assert_eq!(reviews.len(), 7);
    assert!(reviews
        .iter()
        .all(|review| review.reason == "You'll need a subscription before you can publish."));
    events::return_to_draft(&db, &source.id).await.unwrap();
    assert_eq!(
        series::top_up(&db, &Ok(()), JAN_1_2027_NOON + 7 * DAY)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        events::get(&db, &source.id).await.unwrap().unwrap().status,
        "Draft"
    );
    assert_eq!(
        series_dates(&db, &id)
            .await
            .iter()
            .filter(|(_, status, _)| status == "Draft")
            .count(),
        8
    );
    let blocked = reviews[0].event_id.as_ref().unwrap();
    let mut edited = input(&reviews[0].date);
    edited.start_time = "21:00".into();
    events::update(&db, blocked, &edited).await.unwrap();
    series::top_up(&db, &Ok(()), JAN_1_2027_NOON + 7 * DAY)
        .await
        .unwrap();
    assert_eq!(
        events::get(&db, blocked).await.unwrap().unwrap().status,
        "Draft"
    );
    events::publish(&db, blocked, JAN_1_2027_NOON + 7 * DAY)
        .await
        .unwrap();
    assert_eq!(
        events::get(&db, blocked).await.unwrap().unwrap().status,
        "Published"
    );
}

#[tokio::test]
async fn cg009_stop_and_end_keep_exceptions_without_extra_dates() {
    let (_dir, db) = database().await;
    let mut setup = input("2027-03-07");
    setup.start_time = "01:30".into();
    setup.end_time = "03:00".into();
    let source = events::create(&db, &setup).await.unwrap();
    let id = series::start(
        &db,
        &source.id,
        Frequency::Weekly,
        Some("2027-03-28"),
        &Ok(()),
        JAN_1_2027_NOON,
    )
    .await
    .unwrap();
    assert_eq!(
        dates(&series_dates(&db, &id).await),
        vec!["2027-03-07", "2027-03-14", "2027-03-21"]
    );
    assert_eq!(
        series::panel(
            &db,
            &events::get(&db, &source.id).await.unwrap().unwrap(),
            JAN_1_2027_NOON
        )
        .await
        .unwrap()
        .unwrap()
        .future_dates,
        4
    );
    series::stop(&db, &source.id, JAN_1_2027_NOON)
        .await
        .unwrap();
    assert_eq!(
        series::top_up(&db, &Ok(()), JAN_1_2027_NOON).await.unwrap(),
        0
    );
    assert_eq!(
        series::reviews(&db)
            .await
            .unwrap()
            .into_iter()
            .find(|review| review.event_id.is_none())
            .unwrap()
            .date,
        "2027-03-28"
    );
    let corrected =
        series::correct_exception(&db, &id, "2027-03-28", "02:30", "03:00", JAN_1_2027_NOON)
            .await
            .unwrap();
    assert_eq!(corrected.status, "Draft");
    assert_eq!(series_dates(&db, &id).await.len(), 4);
}

#[tokio::test]
async fn cg009_additive_migration_preserves_existing_events() {
    use migration::MigratorTrait;
    let dir = tempfile::tempdir().unwrap();
    let db = connect(&dir.path().join("upgrade.db")).await;
    let migrations = OrganizerMigrator::migrations();
    let series_migration = migrations
        .iter()
        .position(|migration| migration.name() == "m20260930_000019_series")
        .expect("series migration is registered");
    let rollback_steps = u32::try_from(migrations.len() - series_migration).unwrap();
    OrganizerMigrator::up(&db, None).await.unwrap();
    let source = event_with_tickets(&db, "2027-01-07").await;
    events::publish(&db, &source.id, JAN_1_2027_NOON)
        .await
        .unwrap();
    OrganizerMigrator::down(&db, Some(rollback_steps))
        .await
        .unwrap();
    let tables = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM sqlite_master WHERE type = 'table' AND name = 'series'",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tables.try_get::<i64>("", "count").unwrap(), 0);
    OrganizerMigrator::up(&db, None).await.unwrap();
    assert_eq!(
        events::get(&db, &source.id).await.unwrap().unwrap().status,
        "Published"
    );
    assert_eq!(
        events::list_ticket_types(&db, &source.id).await.unwrap()[0].capacity,
        80
    );
    assert!(series::reviews(&db).await.unwrap().is_empty());
    OrganizerMigrator::down(&db, Some(rollback_steps))
        .await
        .unwrap();
    let legacy = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT e.date, e.status, tt.capacity FROM events e JOIN ticket_types tt ON tt.event_id = e.id WHERE e.id = ?",
            [source.id.clone().into()],
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(legacy.try_get::<String>("", "date").unwrap(), "2027-01-07");
    assert_eq!(legacy.try_get::<String>("", "status").unwrap(), "Published");
    assert_eq!(legacy.try_get::<i64>("", "capacity").unwrap(), 80);
    OrganizerMigrator::up(&db, None).await.unwrap();
    assert!(series::reviews(&db).await.unwrap().is_empty());
}

#[tokio::test]
async fn cg009_setup_timezone_edits_do_not_change_existing_exception_horizon() {
    let (_dir, db) = database().await;
    let mut setup = input("2027-03-07");
    setup.start_time = "01:30".into();
    setup.end_time = "03:00".into();
    let source = events::create(&db, &setup).await.unwrap();
    series::start(
        &db,
        &source.id,
        Frequency::Weekly,
        None,
        &Ok(()),
        JAN_1_2027_NOON,
    )
    .await
    .unwrap();
    let afternoon_in_dublin = 1_806_247_800;
    let before = series::panel(
        &db,
        &events::get(&db, &source.id).await.unwrap().unwrap(),
        afternoon_in_dublin,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(before.future_dates, 5);
    setup.timezone = "Asia/Tokyo".into();
    events::update(&db, &source.id, &setup).await.unwrap();
    series::use_setup_from(&db, &source.id, afternoon_in_dublin)
        .await
        .unwrap();
    let after = series::panel(
        &db,
        &events::get(&db, &source.id).await.unwrap().unwrap(),
        afternoon_in_dublin,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(after.future_dates, 5);
}

#[tokio::test]
async fn cg010_preview_uses_literal_calendar_dates_without_creating_anything() {
    let (_dir, db) = database().await;
    for (first, frequency, expected) in [
        (
            "2027-12-23",
            Frequency::Weekly,
            vec![
                "2027-12-30",
                "2028-01-06",
                "2028-01-13",
                "2028-01-20",
                "2028-01-27",
                "2028-02-03",
                "2028-02-10",
            ],
        ),
        (
            "2027-12-23",
            Frequency::Fortnightly,
            vec![
                "2028-01-06",
                "2028-01-20",
                "2028-02-03",
                "2028-02-17",
                "2028-03-02",
                "2028-03-16",
                "2028-03-30",
            ],
        ),
        (
            "2027-01-12",
            Frequency::Monthly,
            vec![
                "2027-02-09",
                "2027-03-09",
                "2027-04-13",
                "2027-05-11",
                "2027-06-08",
                "2027-07-13",
                "2027-08-10",
            ],
        ),
        (
            "2027-12-31",
            Frequency::Monthly,
            vec![
                "2028-01-28",
                "2028-02-25",
                "2028-03-31",
                "2028-04-28",
                "2028-05-26",
                "2028-06-30",
                "2028-07-28",
            ],
        ),
    ] {
        let source = event_with_tickets(&db, first).await;
        let preview = series::preview_start(&db, &source, frequency, None, JAN_1_2027_NOON)
            .await
            .unwrap();
        assert_eq!(
            preview
                .iter()
                .map(|date| date.date.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(preview
            .iter()
            .all(|date| date.status == "Planned; checked when created"));
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) AS count FROM events WHERE id = ?",
                &source.id
            )
            .await,
            1
        );
        assert!(events::get(&db, &source.id)
            .await
            .unwrap()
            .unwrap()
            .series_id
            .is_none());
        let id = series::start(&db, &source.id, frequency, None, &Ok(()), JAN_1_2027_NOON)
            .await
            .unwrap();
        let generated = series_dates(&db, &id).await;
        assert_eq!(generated.len(), 8);
        assert_eq!(dates(&generated)[1..], expected);
    }
}

#[tokio::test]
async fn cg010_preview_bounds_and_clock_changes_match_intended_generation() {
    let (_dir, db) = database().await;
    for (first, invalid, end) in [
        ("2027-03-07", "2027-03-28", "2027-04-04"),
        ("2027-10-10", "2027-10-31", "2027-11-07"),
    ] {
        let mut event_input = input(first);
        event_input.start_time = "00:30".into();
        event_input.end_time = "01:30".into();
        let source = events::create(&db, &event_input).await.unwrap();
        let preview =
            series::preview_start(&db, &source, Frequency::Weekly, Some(end), JAN_1_2027_NOON)
                .await
                .unwrap();
        assert_eq!(preview.len(), 4);
        assert_eq!(preview.last().unwrap().date, end);
        assert!(preview
            .iter()
            .find(|date| date.date == invalid)
            .unwrap()
            .status
            .starts_with("Needs correction:"));
        let id = series::start(
            &db,
            &source.id,
            Frequency::Weekly,
            Some(end),
            &Ok(()),
            JAN_1_2027_NOON,
        )
        .await
        .unwrap();
        assert!(series::reviews(&db)
            .await
            .unwrap()
            .iter()
            .any(|date| date.series_id == id && date.date == invalid && date.event_id.is_none()));
        let linked = events::get(&db, &source.id).await.unwrap().unwrap();
        let panel = series::panel(&db, &linked, JAN_1_2027_NOON)
            .await
            .unwrap()
            .unwrap();
        assert!(panel.upcoming.is_empty());
        assert!(panel.proposed.is_empty());
    }
    let source = event_with_tickets(&db, "2027-01-07").await;
    assert!(series::preview_start(
        &db,
        &source,
        Frequency::Weekly,
        Some("2027-01-07"),
        JAN_1_2027_NOON
    )
    .await
    .unwrap()
    .is_empty());
    assert!(series::preview_start(
        &db,
        &source,
        Frequency::Weekly,
        Some("invalid"),
        JAN_1_2027_NOON
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("valid end date"));
    let preview = series::preview_start(
        &db,
        &source,
        Frequency::Weekly,
        None,
        JAN_1_2027_NOON + 7 * DAY,
    )
    .await
    .unwrap();
    assert_eq!(preview.len(), 8);
    assert_eq!(preview[0].date, "2027-01-14");
}

#[tokio::test]
async fn cg010_setup_preview_and_stop_preserve_existing_dates_and_exception_snapshots() {
    let (_dir, db) = database().await;
    let mut original = input("2027-03-07");
    original.start_time = "01:30".into();
    original.end_time = "03:00".into();
    let source = events::create(&db, &original).await.unwrap();
    let kind = events::add_ticket_type(
        &db,
        &source,
        &TicketTypeInput {
            name: "Workshop seat".into(),
            price: "0".into(),
            capacity: 2,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    let cipher = MasterKey::from_base64(TEST_MASTER_KEY)
        .unwrap()
        .generate_organizer_key("cg010")
        .unwrap()
        .1;
    let order = crabgate::admin_orders::complimentary(
        &db,
        &cipher,
        &source.id,
        &kind.id,
        "buyer@example.com",
        "Buyer",
        JAN_1_2027_NOON,
    )
    .await
    .unwrap();
    let id = series::start(
        &db,
        &source.id,
        Frequency::Weekly,
        None,
        &Ok(()),
        JAN_1_2027_NOON,
    )
    .await
    .unwrap();
    let mut edited = original.clone();
    edited.start_time = "04:00".into();
    edited.end_time = "05:00".into();
    edited.timezone = "Asia/Tokyo".into();
    events::update(&db, &source.id, &edited).await.unwrap();
    let source = events::get(&db, &source.id).await.unwrap().unwrap();
    let rows = series_dates(&db, &id).await;
    let exception_json = || {
        Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT setup_json FROM series_exceptions WHERE series_id = ?",
            [id.clone().into()],
        )
    };
    let saved: String = db
        .query_one_raw(exception_json())
        .await
        .unwrap()
        .unwrap()
        .try_get("", "setup_json")
        .unwrap();
    let before = series::panel(&db, &source, JAN_1_2027_NOON)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.existing.len(), 7);
    assert_eq!(before.upcoming[0].date, "2027-05-02");
    assert_eq!(before.upcoming[0].start_time, "01:30");
    assert_eq!(before.proposed[0].date, "2027-05-02");
    assert_eq!(before.proposed[0].start_time, "04:00");
    assert_eq!(before.proposed[0].timezone, "Asia/Tokyo");
    series::use_setup_from(&db, &source.id, JAN_1_2027_NOON)
        .await
        .unwrap();
    assert_eq!(series_dates(&db, &id).await, rows);
    let after_saved: String = db
        .query_one_raw(exception_json())
        .await
        .unwrap()
        .unwrap()
        .try_get("", "setup_json")
        .unwrap();
    assert_eq!(after_saved, saved);
    assert_eq!(
        series::top_up(&db, &Ok(()), JAN_1_2027_NOON).await.unwrap(),
        0
    );
    series::stop(&db, &source.id, JAN_1_2027_NOON)
        .await
        .unwrap();
    let stopped = series::panel(&db, &source, JAN_1_2027_NOON)
        .await
        .unwrap()
        .unwrap();
    assert!(!stopped.active);
    assert!(stopped.upcoming.is_empty());
    assert!(stopped.proposed.is_empty());
    assert_eq!(stopped.existing.len(), 7);
    assert_eq!(
        serde_json::to_value(&stopped.existing).unwrap(),
        serde_json::to_value(&before.existing).unwrap()
    );
    assert_eq!(
        series::top_up(&db, &Ok(()), JAN_1_2027_NOON + 100 * DAY)
            .await
            .unwrap(),
        0
    );
    assert_eq!(series_dates(&db, &id).await, rows);
    let after_saved: String = db
        .query_one_raw(exception_json())
        .await
        .unwrap()
        .unwrap()
        .try_get("", "setup_json")
        .unwrap();
    assert_eq!(after_saved, saved);
    let retained = crabgate::admin_orders::order(&db, &cipher, &order)
        .await
        .unwrap();
    assert_eq!(retained.event_id, source.id);
    assert_eq!(retained.status, "Paid");
    assert_eq!(retained.tickets.len(), 1);
    assert_eq!(retained.tickets[0].status, "Valid");
    assert_eq!(
        events::list_ticket_types(&db, &source.id).await.unwrap()[0].capacity,
        2
    );
}
