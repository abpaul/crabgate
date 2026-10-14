use crabgate::app::App;
use crabgate::{
    admin_orders,
    crypto::MasterKey,
    discounts,
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, Selection},
    members::{self, Role},
    orders::{self, PaymentOutcome},
    questions, seats,
};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;

async fn hold_status(db: &sea_orm::DatabaseConnection, hold_id: &str) -> String {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT status FROM holds WHERE id = ?",
            [hold_id.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    row.try_get("", "status").unwrap()
}

#[tokio::test]
#[serial]
async fn can_get_home_page() {
    request::<App, _, _>(|request, _ctx| async move {
        let res = request.get("/").await;
        assert_eq!(res.status_code(), 200);
        assert!(res.text().contains("Pick an event."));
        let missing = request.get("/unknown-page").await;
        assert_eq!(missing.status_code(), 404);
        assert!(missing.text().contains("Page not found."));
        assert_eq!(request.get("/static/not-here.css").await.status_code(), 404);
    })
    .await;
}

#[tokio::test]
#[serial]
async fn assigned_seat_shop_and_checkout_keep_the_selected_seat() {
    request::<App, _, _>(|request, ctx| async move {
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
        let db = sea_orm::Database::connect(format!("sqlite://{path}?mode=rw"))
            .await
            .unwrap();
        crabgate::settings::set_contact(
            &db,
            "https://example.com/imprint",
            "https://example.com/privacy",
            "hello@example.com",
        )
        .await
        .unwrap();
        let event = events::create(
            &db,
            &EventInput {
                title: "Assigned seat shop".to_owned(),
                date: "2027-06-15".to_owned(),
                start_time: "18:00".to_owned(),
                end_time: "20:00".to_owned(),
                timezone: "Europe/Dublin".to_owned(),
                venue: "Hall".to_owned(),
                address: "1 Street".to_owned(),
                description: String::new(),
                event_capacity: Some(1),
                max_per_order: 1,
                attendee_name_required: false,
            },
        )
        .await
        .unwrap();
        let kind = events::add_ticket_type(
            &db,
            &event,
            &TicketTypeInput {
                name: "Standard".to_owned(),
                price: "0".to_owned(),
                capacity: 1,
                sales_open: None,
                sales_close: None,
            },
        )
        .await
        .unwrap();
        let now = chrono::Utc::now().timestamp();
        seats::add_many(
            &db,
            &event.id,
            &kind.id,
            "Main",
            "A",
            &["1".to_owned()],
            now,
        )
        .await
        .unwrap();
        let seat = seats::list(&db, &event.id, now).await.unwrap().remove(0);
        events::publish(&db, &event.id, now).await.unwrap();
        let shop = request.get(&format!("/events/{}", event.id)).await;
        assert_eq!(shop.status_code(), 200);
        assert!(shop.text().contains("Main, Row A, Seat 1"));
        assert!(shop.text().contains("href=\"https://example.com/privacy\""));
        assert!(shop.text().contains(&format!("name=\"seat_{}\"", seat.id)));
        let wrong = request
            .post("/checkout")
            .form(&[("event_id", event.id.as_str()), ("quantity_ignored", "1")])
            .await;
        assert_eq!(wrong.status_code(), 400);
        let selected = request
            .post("/checkout")
            .form(&[
                ("event_id".to_owned(), event.id.clone()),
                (format!("seat_{}", seat.id), "1".to_owned()),
            ])
            .await;
        assert_eq!(selected.status_code(), 303);
        let location = selected.header("location").to_str().unwrap().to_owned();
        let checkout = request.get(&location).await;
        assert_eq!(checkout.status_code(), 200);
        assert!(checkout.text().contains("Main, Row A, Seat 1"));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn free_hold_cancel_requires_post_and_releases_capacity() {
    request::<App, _, _>(|request, ctx| async move {
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
        let db = sea_orm::Database::connect(format!("sqlite://{path}?mode=rw"))
            .await
            .unwrap();
        let event = events::create(
            &db,
            &EventInput {
                title: "Free Hold Test".to_owned(),
                date: "2027-06-15".to_owned(),
                start_time: "18:00".to_owned(),
                end_time: "20:00".to_owned(),
                timezone: "Europe/Dublin".to_owned(),
                venue: "Test Hall".to_owned(),
                address: "1 Test Street".to_owned(),
                description: String::new(),
                event_capacity: Some(1),
                max_per_order: 10,
                attendee_name_required: false,
            },
        )
        .await
        .unwrap();
        let ticket_type = events::add_ticket_type(
            &db,
            &event,
            &TicketTypeInput {
                name: "General".to_owned(),
                price: "0".to_owned(),
                capacity: 1,
                sales_open: None,
                sales_close: None,
            },
        )
        .await
        .unwrap();
        events::publish(&db, &event.id, chrono::Utc::now().timestamp())
            .await
            .unwrap();
        let question_id =
            questions::add(&db, &event.id, "Order", "Text", "Favourite song", &[], true)
                .await
                .unwrap();
        let form = [
            ("event_id".to_owned(), event.id),
            (format!("quantity_{}", ticket_type.id), "1".to_owned()),
        ];
        let reserved = request.post("/checkout").form(&form).await;
        assert_eq!(reserved.status_code(), 303);
        let location = reserved.header("location").to_str().unwrap().to_owned();
        let hold_id = location.trim_start_matches("/checkout/");
        assert_eq!(hold_status(&db, hold_id).await, "Open");
        assert_eq!(
            request
                .get(&format!("{location}/cancel"))
                .await
                .status_code(),
            405
        );
        assert_eq!(hold_status(&db, hold_id).await, "Open");
        assert_eq!(
            request
                .post(&format!("{location}/cancel"))
                .await
                .status_code(),
            303
        );
        assert_eq!(hold_status(&db, hold_id).await, "Expired");
        let reserved_again = request.post("/checkout").form(&form).await;
        assert_eq!(reserved_again.status_code(), 303);
        let second_location = reserved_again
            .header("location")
            .to_str()
            .unwrap()
            .to_owned();
        let second_hold = second_location.trim_start_matches("/checkout/");
        let answer_key = format!("question_{question_id}");
        let paid = request
            .post(&second_location)
            .form(&[
                ("email", "buyer@example.com"),
                ("name_0", ""),
                (answer_key.as_str(), "Sea shanty"),
            ])
            .await;
        assert_eq!(paid.status_code(), 303, "{}", paid.text());
        let order_id = orders::order_id_for_hold(&db, second_hold)
            .await
            .unwrap()
            .unwrap();
        let encrypted = db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT answer_encrypted FROM hold_answers WHERE hold_id = ?",
                [second_hold.into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<String>("", "answer_encrypted")
            .unwrap();
        assert!(!encrypted.contains("Sea shanty"));
        assert!(orders::order_id_for_hold(&db, second_hold)
            .await
            .unwrap()
            .is_some());
        assert!(!order_id.is_empty());
    })
    .await;
}

#[tokio::test]
#[serial]
async fn event_list_shows_paid_tickets_and_net_revenue() {
    request::<App, _, _>(|request, ctx| async move {
        let organizer = ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT id, db_path, data_key FROM organizers WHERE subdomain = 'local'".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        let id: String = organizer.try_get("", "id").unwrap();
        let path: String = organizer.try_get("", "db_path").unwrap();
        let wrapped: String = organizer.try_get("", "data_key").unwrap();
        let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
            .unwrap()
            .open_organizer_key(&id, &wrapped)
            .unwrap();
        let db = sea_orm::Database::connect(format!("sqlite://{path}?mode=rw"))
            .await
            .unwrap();
        let event = events::create(
            &db,
            &EventInput {
                title: "Harbour Concert".to_owned(),
                date: "2027-06-15".to_owned(),
                start_time: "19:00".to_owned(),
                end_time: "21:00".to_owned(),
                timezone: "Europe/Dublin".to_owned(),
                venue: "Harbour Hall".to_owned(),
                address: "1 Main Street".to_owned(),
                description: String::new(),
                event_capacity: None,
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
                name: "Standard".to_owned(),
                price: "15.50".to_owned(),
                capacity: 3,
                sales_open: None,
                sales_close: None,
            },
        )
        .await
        .unwrap();
        events::add_ticket_type(
            &db,
            &event,
            &TicketTypeInput {
                name: "Premium".to_owned(),
                price: "25".to_owned(),
                capacity: 2,
                sales_open: None,
                sales_close: None,
            },
        )
        .await
        .unwrap();
        let now = chrono::Utc::now().timestamp();
        events::publish(&db, &event.id, now).await.unwrap();
        let shop = request.get("/").await;
        assert_eq!(shop.status_code(), 200);
        assert!(shop.text().contains("From €15.50"));
        let question_id = questions::add(
            &db,
            &event.id,
            "Order",
            "Choice",
            "Choose a meal",
            &["Vegetarian".to_owned(), "Standard".to_owned()],
            true,
        )
        .await
        .unwrap();
        let hold = inventory::reserve(
            &db,
            &event.id,
            &[Selection {
                ticket_type_id: kind.id.clone(),
                quantity: 1,
            }],
            now,
        )
        .await
        .unwrap();
        let checkout = request.get(&format!("/checkout/{}", hold.id)).await;
        assert_eq!(checkout.status_code(), 200);
        assert!(checkout.text().contains("Choose a meal"));
        assert!(checkout.text().contains("Vegetarian"));
        let incomplete = request
            .post(&format!("/checkout/{}", hold.id))
            .form(&[("email", "buyer@example.com"), ("name_0", "")])
            .await;
        assert_eq!(incomplete.status_code(), 400);
        assert!(incomplete
            .text()
            .contains("Please answer every required question."));
        assert!(incomplete.text().contains("value=\"buyer@example.com\""));
        assert!(incomplete.text().contains("Choose a meal"));
        assert_eq!(
            questions::field_name(&questions::list(&db, &event.id).await.unwrap()[0], 0),
            format!("question_{question_id}")
        );
        let details = orders::hold_details(&db, &hold.id).await.unwrap();
        orders::save_details(
            &db,
            &cipher,
            &details,
            "buyer@example.com",
            &[String::new()],
            false,
            now,
        )
        .await
        .unwrap();
        questions::save_answers(
            &db,
            &cipher,
            &hold.id,
            &questions::list_for_hold(&db, &hold.id).await.unwrap(),
            1,
            &[(format!("question_{question_id}"), "Vegetarian".to_owned())]
                .into_iter()
                .collect(),
        )
        .await
        .unwrap();
        orders::attach_session(&db, &hold.id, "cs_event_list", now)
            .await
            .unwrap();
        let order_id =
            match orders::complete_payment(&db, "cs_event_list", "pi_event_list", 1550, now + 1)
                .await
                .unwrap()
            {
                PaymentOutcome::Paid(id) => id,
                _ => panic!("payment did not create an Order"),
            };
        discounts::create(
            &db,
            discounts::NewDiscount {
                event_id: &event.id,
                code: "ALLFREE",
                kind: "Percent",
                value: 100,
                starts_at: None,
                ends_at: None,
                max_redemptions: None,
            },
            now,
        )
        .await
        .unwrap();
        let discount_hold = request
            .post("/checkout")
            .form(&[
                ("event_id".to_owned(), event.id.clone()),
                (format!("quantity_{}", kind.id), "1".to_owned()),
                ("discount_code".to_owned(), "allfree".to_owned()),
            ])
            .await;
        assert_eq!(discount_hold.status_code(), 303);
        let checkout_path = discount_hold
            .header("location")
            .to_str()
            .unwrap()
            .to_owned();
        let discounted_checkout = request.get(&checkout_path).await;
        assert_eq!(discounted_checkout.status_code(), 200);
        assert!(discounted_checkout
            .text()
            .contains("Code <strong>ALLFREE</strong> saves €15.50"));
        assert!(discounted_checkout.text().contains("Get tickets"));
        members::add_member(&ctx.db, &id, "admin@example.com", Role::Admin)
            .await
            .unwrap();
        let session = members::create_session_for_email(&ctx.db, &id, "admin@example.com", now)
            .await
            .unwrap()
            .unwrap();
        let cookie = format!("cg_session={session}");
        let listed = request
            .get("/office/events")
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(listed.status_code(), 200);
        assert!(listed.text().contains("1 of 5"));
        assert!(listed.text().contains("€15.50"));
        assert_eq!(
            admin_orders::refund(&db, &order_id, None, now + 2)
                .await
                .unwrap_err()
                .to_string(),
            "Stripe is not configured"
        );
        assert_eq!(
            admin_orders::work_progress(&db, &order_id)
                .await
                .unwrap()
                .refund,
            "Failed"
        );
        assert_eq!(
            admin_orders::order(&db, &cipher, &order_id)
                .await
                .unwrap()
                .status,
            "Paid"
        );
        admin_orders::mark_refunded(&db, &order_id, now + 2)
            .await
            .unwrap();
        let refunded = request
            .get("/office/events")
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(refunded.status_code(), 200);
        assert!(refunded.text().contains("0 of 5"));
        assert!(refunded.text().contains("€0"));
    })
    .await;
}

async fn cg012_shop_db(ctx: &loco_rs::app::AppContext) -> sea_orm::DatabaseConnection {
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

fn cg012_input() -> EventInput {
    EventInput {
        title: "Sales window check".into(),
        date: "2027-06-15".into(),
        start_time: "19:00".into(),
        end_time: "21:00".into(),
        timezone: "Europe/Dublin".into(),
        venue: "Hall".into(),
        address: "Street".into(),
        description: String::new(),
        event_capacity: None,
        max_per_order: 5,
        attendee_name_required: false,
    }
}

#[tokio::test]
#[serial]
async fn cg012_stock_labels_use_available_inventory_and_keep_order_limits() {
    request::<App, _, _>(|request, ctx| async move {
        let db = cg012_shop_db(&ctx).await;
        for (capacity, event_cap, sold, held, expected) in [
            (100, None, 0, 0, None),
            (6, None, 0, 0, Some("6 left")),
            (100, Some(6), 1, 1, Some("4 left")),
            (8, None, 1, 1, Some("6 left")),
            (7, None, 0, 0, None),
        ] {
            let mut input = cg012_input();
            input.event_capacity = event_cap;
            let event = events::create(&db, &input).await.unwrap();
            let kind = events::add_ticket_type(
                &db,
                &event,
                &TicketTypeInput {
                    name: "Standard".into(),
                    price: "0".into(),
                    capacity,
                    sales_open: None,
                    sales_close: None,
                },
            )
            .await
            .unwrap();
            let now = chrono::Utc::now().timestamp();
            events::publish(&db, &event.id, now).await.unwrap();
            if sold > 0 {
                let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
                    .unwrap()
                    .generate_organizer_key("stock-test")
                    .unwrap()
                    .1;
                admin_orders::complimentary(
                    &db,
                    &cipher,
                    &event.id,
                    &kind.id,
                    "buyer@example.com",
                    "",
                    now,
                )
                .await
                .unwrap();
            }
            if held > 0 {
                inventory::reserve(
                    &db,
                    &event.id,
                    &[Selection {
                        ticket_type_id: kind.id.clone(),
                        quantity: held,
                    }],
                    now,
                )
                .await
                .unwrap();
            }
            let page = request.get(&format!("/events/{}", event.id)).await;
            assert_eq!(page.status_code(), 200);
            let html = page.text();
            if let Some(label) = expected {
                assert!(html.contains(label), "missing {label}");
            } else {
                assert!(!html.contains(" left</span>"), "false low stock label");
            }
            assert!(html.contains(if expected == Some("4 left") {
                "data-max=\"4\""
            } else {
                "data-max=\"5\""
            }));
            let forged = request
                .post("/checkout")
                .form(&[
                    ("event_id".to_owned(), event.id.clone()),
                    (format!("quantity_{}", kind.id), "6".to_owned()),
                ])
                .await;
            assert_eq!(forged.status_code(), 400);
        }
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg012_unavailable_pages_hide_purchase_and_reject_forged_checkout() {
    request::<App, _, _>(|request, ctx| async move {
        let db = cg012_shop_db(&ctx).await;
        for (case, label) in [
            ("cancelled", "This event is cancelled."),
            ("past", "This event has ended."),
            ("started", "Ticket sales have ended."),
            ("soon", "Coming soon"),
            ("sold", "Sold out"),
            ("closed", "Sales ended"),
            ("empty", "Tickets aren&#39;t on sale yet."),
        ] {
            let mut input = cg012_input();
            if case == "past" {
                input.date = "2025-06-15".into();
            }
            let event = events::create(&db, &input).await.unwrap();
            let now = chrono::Utc::now().timestamp();
            let kind = if case != "empty" {
                Some(
                    events::add_ticket_type(
                        &db,
                        &event,
                        &TicketTypeInput {
                            name: "Standard".into(),
                            price: "0".into(),
                            capacity: if case == "sold" { 0 } else { 100 },
                            sales_open: (case == "soon").then(|| "2027-06-01T10:00".into()),
                            sales_close: (case == "closed").then(|| "2025-06-01T10:00".into()),
                        },
                    )
                    .await
                    .unwrap(),
                )
            } else {
                None
            };
            db.execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE events SET status = ? WHERE id = ?",
                [
                    if case == "cancelled" {
                        "Cancelled"
                    } else {
                        "Published"
                    }
                    .into(),
                    event.id.clone().into(),
                ],
            ))
            .await
            .unwrap();
            if case == "started" {
                db.execute_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "UPDATE events SET starts_at = ?, ends_at = ? WHERE id = ?",
                    [
                        (now - 60).into(),
                        (now + 3600).into(),
                        event.id.clone().into(),
                    ],
                ))
                .await
                .unwrap();
            }
            let page = request.get(&format!("/events/{}", event.id)).await;
            assert_eq!(page.status_code(), 200);
            let html = page.text();
            assert!(html.contains(label), "{case}: missing {label}");
            assert!(
                !html.contains(">Continue</button>"),
                "{case}: offers checkout"
            );
            assert!(
                !html.contains("id=\"discount-code\""),
                "{case}: offers discount"
            );
            assert!(!html.contains("data-total"), "{case}: offers total");
            assert!(!html.contains("data-quantity"), "{case}: offers quantity");
            if case == "cancelled" || case == "past" || case == "started" {
                assert!(!html.contains("https://schema.org/InStock"));
            }
            if let Some(kind) = kind {
                let forged = request
                    .post("/checkout")
                    .form(&[
                        ("event_id".to_owned(), event.id.clone()),
                        (format!("quantity_{}", kind.id), "1".to_owned()),
                    ])
                    .await;
                assert_eq!(forged.status_code(), 400, "{case}: forged checkout");
            }
            let row = db
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT COUNT(*) AS count FROM holds WHERE event_id = ?",
                    [event.id.clone().into()],
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.try_get::<i64>("", "count").unwrap(), 0);
        }
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg012_event_edit_route_preserves_values_on_invalid_sale_window() {
    request::<App, _, _>(|request, ctx| async move {
        let db = cg012_shop_db(&ctx).await;
        let organizer = ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT id FROM organizers WHERE subdomain = 'local'".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        let id: String = organizer.try_get("", "id").unwrap();
        members::add_member(&ctx.db, &id, "admin@example.com", Role::Admin)
            .await
            .unwrap();
        let session = members::create_session_for_email(
            &ctx.db,
            &id,
            "admin@example.com",
            chrono::Utc::now().timestamp(),
        )
        .await
        .unwrap()
        .unwrap();
        let cookie = format!("cg_session={session}");
        let event = events::create(&db, &cg012_input()).await.unwrap();
        let kind = events::add_ticket_type(
            &db,
            &event,
            &TicketTypeInput {
                name: "Early".into(),
                price: "0".into(),
                capacity: 100,
                sales_open: Some("2027-06-10T10:00".into()),
                sales_close: Some("2027-06-15T18:00".into()),
            },
        )
        .await
        .unwrap();
        let path = format!("/office/events/{}", event.id);
        for (start, status, persisted) in [
            ("17:00", 400, 1813082400_i64),
            ("18:00", 303, 1813078800),
            ("20:00", 303, 1813086000),
        ] {
            let response = request
                .post(&path)
                .add_header("cookie", &cookie)
                .form(&[
                    ("title", "Sales window check"),
                    ("date", "2027-06-15"),
                    ("start_time", start),
                    ("end_time", "21:00"),
                    ("timezone", "Europe/Dublin"),
                    ("venue", "Hall"),
                    ("address", "Street"),
                    ("description", ""),
                    ("event_capacity", ""),
                    ("max_per_order", "5"),
                ])
                .await;
            assert_eq!(response.status_code(), status);
            if status == 400 {
                assert!(response
                    .text()
                    .contains("Adjust the saved sales window for Early"));
                assert!(response.text().contains("value=\"17:00\""));
                assert!(response.text().contains("value=\"2027-06-15T18:00\""));
            }
            assert_eq!(
                events::get(&db, &event.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .starts_at,
                persisted
            );
            let saved = events::list_ticket_types(&db, &event.id)
                .await
                .unwrap()
                .remove(0);
            assert_eq!(saved.sales_open, Some(1812618000));
            assert_eq!(saved.sales_close, Some(1813078800));
        }
        let response = request
            .post(&format!("{path}/ticket-types/{}", kind.id))
            .add_header("cookie", &cookie)
            .form(&[
                ("name", "Early"),
                ("price", "0"),
                ("capacity", "100"),
                ("sales_open", ""),
                ("sales_close", ""),
            ])
            .await;
        assert_eq!(response.status_code(), 303);
        let response = request
            .post(&path)
            .add_header("cookie", &cookie)
            .form(&[
                ("title", "Sales window check"),
                ("date", "2027-03-28"),
                ("start_time", "02:30"),
                ("end_time", "03:30"),
                ("timezone", "Europe/London"),
                ("confirm_timezone_change", "true"),
                ("venue", "Hall"),
                ("address", "Street"),
                ("description", ""),
                ("event_capacity", ""),
                ("max_per_order", "5"),
            ])
            .await;
        assert_eq!(response.status_code(), 303);
        let saved = events::get(&db, &event.id).await.unwrap().unwrap();
        assert_eq!(saved.starts_at, 1806197400);
        assert_eq!(saved.ends_at, 1806201000);
        assert_eq!(saved.timezone, "Europe/London");
        assert_eq!(
            events::list_ticket_types(&db, &event.id).await.unwrap()[0].sales_close,
            None
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg012_assigned_inventory_labels_and_unavailable_seats_match_checkout() {
    request::<App, _, _>(|request, ctx| async move {
        let db = cg012_shop_db(&ctx).await;
        for active in [true, false] {
            let mut input = cg012_input();
            input.max_per_order = 1;
            let event = events::create(&db, &input).await.unwrap();
            let kind = events::add_ticket_type(
                &db,
                &event,
                &TicketTypeInput {
                    name: "Seated".into(),
                    price: "0".into(),
                    capacity: 10,
                    sales_open: None,
                    sales_close: None,
                },
            )
            .await
            .unwrap();
            let now = chrono::Utc::now().timestamp();
            seats::add_many(
                &db,
                &event.id,
                &kind.id,
                "Main",
                "A",
                &["1".into(), "2".into()],
                now,
            )
            .await
            .unwrap();
            let available = seats::list(&db, &event.id, now).await.unwrap();
            if !active {
                for seat in &available {
                    seats::deactivate(&db, &event.id, &seat.id, now)
                        .await
                        .unwrap();
                }
            }
            events::publish(&db, &event.id, now).await.unwrap();
            let page = request.get(&format!("/events/{}", event.id)).await;
            assert_eq!(page.status_code(), 200);
            if active {
                assert!(page.text().contains("2 left"));
                assert!(page.text().contains("data-max=\"1\""));
                assert!(page
                    .text()
                    .contains(&format!("name=\"seat_{}\"", available[0].id)));
            } else {
                assert!(page.text().contains("Sold out"));
                assert!(!page.text().contains(">Continue</button>"));
                let forged = request
                    .post("/checkout")
                    .form(&[
                        ("event_id".to_owned(), event.id.clone()),
                        (format!("seat_{}", available[0].id), "1".into()),
                    ])
                    .await;
                assert_eq!(forged.status_code(), 400);
            }
        }
    })
    .await;
}
