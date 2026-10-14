use chrono::Utc;
use crabgate::{
    admin_orders,
    app::App,
    crypto::OrganizerCipher,
    events::{self, EventInput, TicketTypeInput},
    members::{self, Role},
};
use loco_rs::{app::AppContext, testing::prelude::*};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

async fn fixture(db: &DatabaseConnection, cipher: &OrganizerCipher) -> String {
    let event = events::create(
        db,
        &EventInput {
            title: "Page fixture".to_owned(),
            date: "2027-06-15".to_owned(),
            start_time: "19:00".to_owned(),
            end_time: "21:00".to_owned(),
            timezone: "Europe/Dublin".to_owned(),
            venue: "Hall".to_owned(),
            address: "Street".to_owned(),
            description: String::new(),
            event_capacity: Some(200),
            max_per_order: 2,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let kind = events::add_ticket_type(
        db,
        &event,
        &TicketTypeInput {
            name: "Standard".to_owned(),
            price: "0".to_owned(),
            capacity: 200,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    let now = Utc::now().timestamp();
    events::publish(db, &event.id, now).await.unwrap();
    let base = admin_orders::complimentary(
        db,
        cipher,
        &event.id,
        &kind.id,
        "zero@example.com",
        "Zero",
        now,
    )
    .await
    .unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE orders SET number='ORDER-000' WHERE id=?",
        [base.clone().into()],
    ))
    .await
    .unwrap();
    for insertion in 1..=100 {
        let key = (insertion * 37) % 101;
        let hold = format!("page-hold-{key:03}");
        let order = format!("page-order-{key:03}");
        let session = format!("page-session-{key:03}");
        let email = cipher
            .encrypt("buyer_email", &format!("buyer-{key:03}@example.com"))
            .unwrap();
        db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO holds (id,event_id,status,expires_at,stripe_session_id,buyer_email_encrypted,created_at) VALUES (?,?,'Paid',?,?,?,?)",
            [hold.clone().into(),event.id.clone().into(),(now+60).into(),session.clone().into(),email.clone().into(),now.into()]
        )).await.unwrap();
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "INSERT INTO hold_items (hold_id,ticket_type_id,quantity,price_cents) VALUES (?,?,1,0)",
            [hold.clone().into(), kind.id.clone().into()],
        ))
        .await
        .unwrap();
        db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO orders (id,number,event_id,hold_id,stripe_session_id,buyer_email_encrypted,total_cents,status,is_complimentary,created_at) VALUES (?,?,?,?,?,?,0,'Paid',1,?)",
            [order.clone().into(),format!("ORDER-{key:03}").into(),event.id.clone().into(),hold.into(),session.into(),email.into(),now.into()]
        )).await.unwrap();
        db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO tickets (id,order_id,event_id,ticket_type_id,code,attendee_name_encrypted,status,created_at) VALUES (?,?,?,?,?,?,'Valid',?)",
            [format!("page-ticket-{key:03}").into(),order.into(),event.id.clone().into(),kind.id.clone().into(),format!("PAGE-{key:03}").into(),cipher.encrypt("attendee_name",&format!("Guest-{key:03}")).unwrap().into(),now.into()]
        )).await.unwrap();
    }
    event.id
}

async fn admin_session(ctx: &AppContext, organizer: &str) -> String {
    let email = "pages-admin@example.com";
    members::add_member(&ctx.db, organizer, email, Role::Admin)
        .await
        .unwrap();
    let token = members::request_magic_link(&ctx.db, organizer, email, Utc::now().timestamp())
        .await
        .unwrap()
        .unwrap();
    let session = members::claim_magic_link(&ctx.db, organizer, &token, Utc::now().timestamp())
        .await
        .unwrap()
        .unwrap();
    format!("cg_session={session}")
}

fn row_numbers(body: &str) -> Vec<String> {
    body.split("<tbody>")
        .nth(1)
        .unwrap()
        .split("</tbody>")
        .next()
        .unwrap()
        .split("<td><a href=\"/office/orders/")
        .skip(1)
        .map(|row| {
            row.split("\">")
                .nth(1)
                .unwrap()
                .split("</a>")
                .next()
                .unwrap()
                .to_owned()
        })
        .collect()
}

#[tokio::test]
#[serial]
async fn orders_http_pages_are_bounded_and_searches_keep_older_matches() {
    request::<App, _, _>(|request, ctx| async move {
        let registry = ctx
            .shared_store
            .get::<std::sync::Arc<crabgate::initializers::organizers::OrganizerRuntime>>()
            .unwrap()
            .registry();
        let scope = registry.active_dbs().await.unwrap().remove(0);
        let cipher = registry.cipher(&scope.organizer).unwrap();
        let event = fixture(&scope.db, &cipher).await;
        let cookie = admin_session(&ctx, &scope.organizer.id).await;
        let response = request
            .get(&format!("/office/orders?event={event}"))
            .add_header("cookie", &cookie)
            .await;
        if let Some(root) = std::env::var_os("CRABGATE_TEST_EVIDENCE_DIR") {
            let directory = tempfile::Builder::new()
                .prefix("office-page-")
                .tempdir_in(root)
                .unwrap()
                .keep();
            std::fs::write(directory.join("orders.html"), response.text()).unwrap();
            println!("Office HTML evidence: {}", directory.display());
        }
        assert_eq!(response.status_code(), 200);
        let numbers = row_numbers(&response.text());
        assert_eq!(numbers.len(), 50);
        assert_eq!(
            numbers,
            (51..=100)
                .rev()
                .map(|i| format!("ORDER-{i:03}"))
                .collect::<Vec<_>>()
        );
        assert!(response.text().contains("aria-label=\"Order pages\""));
        assert!(response.text().contains("Orders 1–50"));
        assert!(response.text().contains("page=2"));
        let second = request
            .get(&format!("/office/orders?event={event}&page=2"))
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(second.status_code(), 200);
        assert_eq!(
            row_numbers(&second.text()),
            (1..=50)
                .rev()
                .map(|i| format!("ORDER-{i:03}"))
                .collect::<Vec<_>>()
        );
        assert!(second.text().contains("Orders 51–100"));
        assert!(second.text().contains("page=1"));
        assert!(second.text().contains("page=3"));
        let third = request
            .get(&format!("/office/orders?event={event}&page=3"))
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(row_numbers(&third.text()), vec!["ORDER-000"]);
        assert!(third.text().contains("Orders 101–101"));
        assert!(!third.text().contains(">Next</a>"));
        for query in ["buyer%2D001", "ORDER%2D001", "Guest%2D001"] {
            let result = request
                .get(&format!("/office/orders?event={event}&q={query}"))
                .add_header("cookie", &cookie)
                .await;
            assert_eq!(result.status_code(), 200);
            assert_eq!(row_numbers(&result.text()), vec!["ORDER-001"]);
            assert!(!result.text().contains(">Next</a>"));
        }
        let searched = request
            .get(&format!(
                "/office/orders?event={event}&q=example.com&page=2"
            ))
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(searched.status_code(), 200);
        assert_eq!(
            row_numbers(&searched.text()),
            (1..=50)
                .rev()
                .map(|i| format!("ORDER-{i:03}"))
                .collect::<Vec<_>>()
        );
        assert!(searched.text().contains("q=example%2Ecom"));
        assert!(searched.text().contains(">Next</a>"));
        let no_event_name = request
            .get("/office/orders?q=Guest%2D001")
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(row_numbers(&no_event_name.text()), Vec::<String>::new());
        for invalid in ["0", "-1", "bad", "999999999999999999999999999999999"] {
            let result = request
                .get(&format!("/office/orders?event={event}&page={invalid}"))
                .add_header("cookie", &cookie)
                .await;
            assert_eq!(result.status_code(), 200);
            assert_eq!(row_numbers(&result.text()), numbers);
        }
        let huge = request
            .get(&format!("/office/orders?event={event}&page=4294967295"))
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(huge.status_code(), 200);
        assert_eq!(row_numbers(&huge.text()), Vec::<String>::new());
        assert!(huge.text().contains("page=4294967294"));
        let encoded = request
            .get("/office/orders?page=2&q=%3C%22%26%2B%20%3F%2F%C3%A9&event=%3C%26%22")
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(encoded.status_code(), 200);
        assert!(encoded.text().contains(
            "href=\"/office/orders?page=1&amp;q=%3C%22%26%2B%20%3F%2F%C3%A9&amp;event=%3C%26%22\""
        ));
        assert!(!encoded.text().contains("value=\"<\"&+ ?/é\""));
        let csv = request
            .get(&format!("/office/events/{event}/export/orders.csv"))
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(csv.status_code(), 200);
        assert_eq!(csv.text().lines().count(), 102);
        assert!(csv
            .text()
            .contains("ORDER-001,buyer-001@example.com,0,,0,0,Paid,Yes,"));
        assert_eq!(request.get("/office/orders").await.status_code(), 303);
    })
    .await;
}

async fn database() -> (
    Option<tempfile::TempDir>,
    DatabaseConnection,
    OrganizerCipher,
) {
    let evidence = std::env::var_os("CRABGATE_TEST_EVIDENCE_DIR");
    let dir = if let Some(root) = &evidence {
        tempfile::Builder::new()
            .prefix("office-sqlite-")
            .tempdir_in(root)
            .unwrap()
    } else {
        tempfile::tempdir().unwrap()
    };
    let path = dir.path().join("office.db");
    let cleanup = if evidence.is_some() {
        println!("Office SQLite evidence: {}", dir.keep().display());
        None
    } else {
        Some(dir)
    };
    let db = sea_orm::Database::connect(format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap();
    loco_rs::db::migrate::<migration::OrganizerMigrator>(&db)
        .await
        .unwrap();
    let cipher =
        crabgate::crypto::MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .unwrap()
            .generate_organizer_key("office-tests")
            .unwrap()
            .1;
    (cleanup, db, cipher)
}

#[tokio::test]
async fn orders_page_limits_database_work_and_keeps_full_csv_and_search_matches() {
    let (_dir, mut db, cipher) = database().await;
    let event = fixture(&db, &cipher).await;
    let statements = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = statements.clone();
    db.set_metric_callback(move |info| captured.lock().unwrap().push(info.statement.clone()));
    let first = admin_orders::list_page(&db, &cipher, Some(&event), "  ", 1)
        .await
        .unwrap();
    assert_eq!(
        first
            .orders
            .iter()
            .map(|row| row.number.as_str())
            .collect::<Vec<_>>(),
        (51..=100)
            .rev()
            .map(|i| format!("ORDER-{i:03}"))
            .collect::<Vec<_>>()
    );
    assert!(first.has_next);
    assert!(first.orders.iter().all(|row| row.ticket_count == 1
        && row.complimentary
        && row.status == "Paid"
        && row.event_title == "Page fixture"));
    let issued = statements.lock().unwrap().clone();
    db.set_metric_callback(|_| {});
    assert!(issued.len() <= 2);
    for statement in issued {
        assert!(db.query_all_raw(statement).await.unwrap().len() <= 51);
    }
    let second = admin_orders::list_page(&db, &cipher, Some(&event), "", 2)
        .await
        .unwrap();
    assert_eq!(
        second
            .orders
            .iter()
            .map(|row| row.number.as_str())
            .collect::<Vec<_>>(),
        (1..=50)
            .rev()
            .map(|i| format!("ORDER-{i:03}"))
            .collect::<Vec<_>>()
    );
    assert!(second.has_next);
    let third = admin_orders::list_page(&db, &cipher, Some(&event), "", 3)
        .await
        .unwrap();
    assert_eq!(
        third
            .orders
            .iter()
            .map(|row| row.number.as_str())
            .collect::<Vec<_>>(),
        vec!["ORDER-000"]
    );
    assert!(!third.has_next);
    assert!(
        admin_orders::list_page(&db, &cipher, Some("unknown"), "", 1)
            .await
            .unwrap()
            .orders
            .is_empty()
    );
    assert!(admin_orders::list_page(&db, &cipher, None, "", u32::MAX)
        .await
        .unwrap()
        .orders
        .is_empty());
    for (selected, query, expected) in [
        (Some(event.as_str()), "  BUYER-001 ", vec!["ORDER-001"]),
        (None, "order-001", vec!["ORDER-001"]),
        (Some(event.as_str()), "Guest-001", vec!["ORDER-001"]),
        (None, "Guest-001", Vec::new()),
        (Some(event.as_str()), "g", Vec::new()),
    ] {
        let page = admin_orders::list_page(&db, &cipher, selected, query, 1)
            .await
            .unwrap();
        assert_eq!(
            page.orders
                .iter()
                .map(|row| row.number.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(!page.has_next);
    }
    let other = events::create(
        &db,
        &EventInput {
            title: "Other Event".to_owned(),
            date: "2027-06-16".to_owned(),
            start_time: "19:00".to_owned(),
            end_time: "21:00".to_owned(),
            timezone: "Europe/Dublin".to_owned(),
            venue: "Hall".to_owned(),
            address: "Street".to_owned(),
            description: String::new(),
            event_capacity: None,
            max_per_order: 2,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let other_type = events::add_ticket_type(
        &db,
        &other,
        &TicketTypeInput {
            name: "Other type".to_owned(),
            price: "0".to_owned(),
            capacity: 2,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    let later = Utc::now().timestamp() + 100;
    events::publish(&db, &other.id, later).await.unwrap();
    let other_order = admin_orders::complimentary(
        &db,
        &cipher,
        &other.id,
        &other_type.id,
        "other@example.com",
        "Other guest",
        later,
    )
    .await
    .unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE orders SET number='OTHER' WHERE id=?",
        [other_order.into()],
    ))
    .await
    .unwrap();
    let all_events = admin_orders::list_page(&db, &cipher, None, "", 1)
        .await
        .unwrap();
    assert_eq!(all_events.orders.len(), 50);
    assert_eq!(
        all_events
            .orders
            .iter()
            .take(3)
            .map(|row| row.number.as_str())
            .collect::<Vec<_>>(),
        vec!["OTHER", "ORDER-100", "ORDER-099"]
    );
    assert_eq!(all_events.orders[0].event_title, "Other Event");
    assert_eq!(
        admin_orders::list_page(&db, &cipher, Some(&other.id), "", 1)
            .await
            .unwrap()
            .orders
            .iter()
            .map(|row| row.number.as_str())
            .collect::<Vec<_>>(),
        vec!["OTHER"]
    );
    assert!(
        admin_orders::list_page(&db, &cipher, Some(&event), "other@example", 1)
            .await
            .unwrap()
            .orders
            .is_empty()
    );
    let full = admin_orders::list(&db, &cipher, Some(&event), "")
        .await
        .unwrap();
    assert_eq!(full.len(), 101);
    let mut expected="Order,Buyer email,Original cents,Discount code,Discount cents,Paid cents,Status,Complimentary,Created at\r\n".to_owned();
    for row in full {
        let email = if row.number == "ORDER-000" {
            "zero@example.com".to_owned()
        } else {
            format!("buyer-{}@example.com", &row.number[6..])
        };
        expected.push_str(&format!(
            "{},{email},0,,0,0,Paid,Yes,{}\r\n",
            row.number, row.created_at
        ));
    }
    assert_eq!(
        admin_orders::report(&db, &cipher, &event, admin_orders::Report::Orders)
            .await
            .unwrap(),
        expected
    );
}

#[tokio::test]
async fn bulk_complimentary_selectors_preserve_type_order_and_seat_availability() {
    let (_dir, db, cipher) = database().await;
    let event = events::create(
        &db,
        &EventInput {
            title: "Seated".to_owned(),
            date: "2027-06-15".to_owned(),
            start_time: "19:00".to_owned(),
            end_time: "21:00".to_owned(),
            timezone: "Europe/Dublin".to_owned(),
            venue: "Hall".to_owned(),
            address: "Street".to_owned(),
            description: String::new(),
            event_capacity: Some(20),
            max_per_order: 2,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let mut kinds = Vec::new();
    for name in ["Standard", "Priority"] {
        kinds.push(
            events::add_ticket_type(
                &db,
                &event,
                &TicketTypeInput {
                    name: name.to_owned(),
                    price: "0".to_owned(),
                    capacity: 10,
                    sales_open: None,
                    sales_close: None,
                },
            )
            .await
            .unwrap(),
        );
    }
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE ticket_types SET sort_order=-1 WHERE id=?",
        [kinds[1].id.clone().into()],
    ))
    .await
    .unwrap();
    let now = Utc::now().timestamp();
    crabgate::seats::add_many(
        &db,
        &event.id,
        &kinds[0].id,
        "Main",
        "A",
        &(1..=6).map(|i| i.to_string()).collect::<Vec<_>>(),
        now,
    )
    .await
    .unwrap();
    let seats = crabgate::seats::list(&db, &event.id, now).await.unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE seats SET active=0 WHERE id=?",
        [seats[4].id.clone().into()],
    ))
    .await
    .unwrap();
    events::publish(&db, &event.id, now).await.unwrap();
    admin_orders::complimentary_with_seat(
        &db,
        &cipher,
        &event.id,
        (&kinds[0].id, Some(&seats[1].id)),
        "sold@example.com",
        "Sold",
        now,
    )
    .await
    .unwrap();
    let voided = admin_orders::complimentary_with_seat(
        &db,
        &cipher,
        &event.id,
        (&kinds[0].id, Some(&seats[5].id)),
        "void@example.com",
        "Void",
        now,
    )
    .await
    .unwrap();
    admin_orders::refund(&db, &voided, None, now).await.unwrap();
    let selections = [crabgate::inventory::Selection {
        ticket_type_id: kinds[0].id.clone(),
        quantity: 1,
    }];
    let active = crabgate::inventory::reserve_with_options(
        &db,
        &event.id,
        &selections,
        None,
        &[seats[2].id.clone()],
        now,
    )
    .await
    .unwrap();
    let expired = crabgate::inventory::reserve_with_options(
        &db,
        &event.id,
        &selections,
        None,
        &[seats[3].id.clone()],
        now - crabgate::inventory::HOLD_DURATION_SECONDS - 1,
    )
    .await
    .unwrap();
    assert_eq!(expired.expires_at, now - 1);
    let empty = events::create(
        &db,
        &EventInput {
            title: "Empty draft".to_owned(),
            date: "2027-06-16".to_owned(),
            start_time: "19:00".to_owned(),
            end_time: "21:00".to_owned(),
            timezone: "Europe/Dublin".to_owned(),
            venue: "Hall".to_owned(),
            address: "Street".to_owned(),
            description: String::new(),
            event_capacity: None,
            max_per_order: 2,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let all = events::list(&db).await.unwrap();
    assert_eq!(
        all.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(),
        vec!["Seated", "Empty draft"]
    );
    let types = events::list_all_ticket_types(&db).await.unwrap();
    assert_eq!(
        types[&event.id]
            .iter()
            .map(|kind| kind.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Priority", "Standard"]
    );
    assert!(!types.contains_key(&empty.id));
    let available = crabgate::seats::available_for_events(&db, now)
        .await
        .unwrap();
    assert_eq!(
        available[&event.id]
            .iter()
            .map(|seat| seat.label.as_str())
            .collect::<Vec<_>>(),
        vec!["1", "4", "6"]
    );
    assert!(available[&event.id].iter().all(|seat| seat.active
        && seat.available
        && seat.section == "Main"
        && seat.row == "A"
        && seat.ticket_type_id == kinds[0].id));
    assert!(!available.contains_key(&empty.id));
    let at_expiry = crabgate::seats::available_for_events(&db, active.expires_at)
        .await
        .unwrap();
    assert_eq!(
        at_expiry[&event.id]
            .iter()
            .map(|seat| seat.label.as_str())
            .collect::<Vec<_>>(),
        vec!["1", "3", "4", "6"]
    );

    assert_eq!(
        crabgate::seats::list(&db, &event.id, now)
            .await
            .unwrap()
            .into_iter()
            .filter(|seat| seat.available)
            .map(|seat| seat.id)
            .collect::<Vec<_>>(),
        available[&event.id]
            .iter()
            .map(|seat| seat.id.clone())
            .collect::<Vec<_>>()
    );
}
