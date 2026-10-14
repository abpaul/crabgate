use std::collections::HashMap;

use chrono::Utc;
use crabgate::{
    admin_orders::{self, Report},
    crypto::{MasterKey, OrganizerCipher},
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, Selection},
    orders::{self, PaymentOutcome},
    questions,
};
use loco_rs::db;
use migration::OrganizerMigrator;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

#[allow(clippy::too_many_arguments)]
async fn checkout(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: &str,
    ticket_type_id: &str,
    email: &str,
    names: &[&str],
    answers: &[(String, &str)],
    now: i64,
) -> String {
    checkout_selections(
        db,
        cipher,
        event_id,
        &[Selection {
            ticket_type_id: ticket_type_id.to_owned(),
            quantity: names.len() as i64,
        }],
        email,
        names,
        answers,
        now,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn checkout_selections(
    db: &DatabaseConnection,
    cipher: &OrganizerCipher,
    event_id: &str,
    selections: &[Selection],
    email: &str,
    names: &[&str],
    answers: &[(String, &str)],
    now: i64,
) -> String {
    let hold = inventory::reserve(db, event_id, selections, now)
        .await
        .unwrap();
    let asked = questions::list_for_hold(db, &hold.id).await.unwrap();
    let form = answers
        .iter()
        .map(|(field, value)| (field.clone(), (*value).to_owned()))
        .collect::<HashMap<_, _>>();
    questions::save_answers(db, cipher, &hold.id, &asked, names.len(), &form)
        .await
        .unwrap();
    let details = orders::hold_details(db, &hold.id).await.unwrap();
    let names = names
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    orders::save_details(db, cipher, &details, email, &names, false, now)
        .await
        .unwrap();
    let session = format!("test_{}", hold.id);
    orders::attach_session(db, &hold.id, &session, now)
        .await
        .unwrap();
    match orders::complete_payment(
        db,
        &session,
        &format!("pi_{}", hold.id),
        details.total_cents,
        now,
    )
    .await
    .unwrap()
    {
        PaymentOutcome::Paid(id) => id,
        _ => panic!("Order was not created"),
    }
}

fn event_input() -> EventInput {
    EventInput {
        title: "Export test".to_owned(),
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
    }
}

async fn setup() -> (
    tempfile::TempDir,
    DatabaseConnection,
    OrganizerCipher,
    String,
    String,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = sea_orm::Database::connect(format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("exports.db").display()
    ))
    .await
    .unwrap();
    db::migrate::<OrganizerMigrator>(&db).await.unwrap();
    let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .unwrap()
        .generate_organizer_key("test")
        .unwrap()
        .1;
    let event = events::create(&db, &event_input()).await.unwrap();
    let kind = events::add_ticket_type(
        &db,
        &event,
        &TicketTypeInput {
            name: "General".to_owned(),
            price: "0".to_owned(),
            capacity: 40,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    (dir, db, cipher, event.id, kind.id)
}

#[tokio::test]
async fn csv_exports_preserve_deleted_questions() {
    let (_dir, db, cipher, event_id, ticket_type_id) = setup().await;
    let invoice = questions::add(&db, &event_id, "Order", "Text", "Invoice name", &[], false)
        .await
        .unwrap();
    let mailing = questions::add(
        &db,
        &event_id,
        "Order",
        "Checkbox",
        "Join the mailing list",
        &[],
        false,
    )
    .await
    .unwrap();
    let removed = questions::add(&db, &event_id, "Order", "Text", "Old question", &[], false)
        .await
        .unwrap();
    let badge = questions::add(
        &db,
        &event_id,
        "Attendee",
        "Text",
        "Badge name, if different",
        &[],
        false,
    )
    .await
    .unwrap();
    let access = questions::add(
        &db,
        &event_id,
        "Attendee",
        "Checkbox",
        "Needs step-free access",
        &[],
        false,
    )
    .await
    .unwrap();
    events::publish(&db, &event_id, Utc::now().timestamp())
        .await
        .unwrap();
    let now = Utc::now().timestamp();
    let team = checkout(
        &db,
        &cipher,
        &event_id,
        &ticket_type_id,
        "team@example.com",
        &["Niamh", "Cian"],
        &[
            (format!("question_{invoice}"), "Acme, \"Ltd\""),
            (format!("question_{removed}"), "Gone"),
            (format!("question_{badge}_0"), "=SUM(A1)"),
            (format!("question_{access}_0"), "yes"),
        ],
        now,
    )
    .await;
    let anonymized = checkout(
        &db,
        &cipher,
        &event_id,
        &ticket_type_id,
        "solo@example.com",
        &["Orla"],
        &[
            (format!("question_{invoice}"), "Bee Co"),
            (format!("question_{mailing}"), "yes"),
            (format!("question_{badge}_0"), "Orla B"),
            (format!("question_{access}_0"), "yes"),
        ],
        now + 10,
    )
    .await;
    admin_orders::anonymize(&db, &cipher, &anonymized)
        .await
        .unwrap();
    let guest = admin_orders::complimentary(
        &db,
        &cipher,
        &event_id,
        &ticket_type_id,
        "guest@example.com",
        "Pat",
        now + 20,
    )
    .await
    .unwrap();
    questions::delete(&db, &event_id, &removed).await.unwrap();
    questions::delete(&db, &event_id, &badge).await.unwrap();
    questions::delete(&db, &event_id, &access).await.unwrap();

    let mut number = HashMap::new();
    for id in [&team, &anonymized, &guest] {
        let order = admin_orders::order(&db, &cipher, id).await.unwrap();
        number.insert(id.clone(), order.number);
    }
    let orders_csv = admin_orders::report(&db, &cipher, &event_id, Report::Orders)
        .await
        .unwrap();
    assert_eq!(
        orders_csv,
        format!(
            "Order,Buyer email,Original cents,Discount code,Discount cents,Paid cents,Status,Complimentary,Created at,Invoice name,Join the mailing list,Old question\r\n\
             {},guest@example.com,0,,0,0,Paid,Yes,{},,,\r\n\
             {},,0,,0,0,Paid,No,{},,,\r\n\
             {},team@example.com,0,,0,0,Paid,No,{},\"Acme, \"\"Ltd\"\"\",No,Gone\r\n",
            number[&guest],
            now + 20,
            number[&anonymized],
            now + 10,
            number[&team],
            now,
        )
    );

    let tickets = db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT id, order_id, code, created_at FROM tickets ORDER BY rowid".to_owned(),
        ))
        .await
        .unwrap();
    let cells = [
        "Niamh,team@example.com,Valid,,'=SUM(A1),Yes",
        "Cian,team@example.com,Valid,,,No",
        ",,Valid,,,",
        "Pat,guest@example.com,Valid,,,",
    ];
    let mut expected = tickets
        .iter()
        .zip(cells)
        .map(|(ticket, cells)| {
            let order_id: String = ticket.try_get("", "order_id").unwrap();
            let code: String = ticket.try_get("", "code").unwrap();
            (
                ticket.try_get::<i64>("", "created_at").unwrap(),
                ticket.try_get::<String>("", "id").unwrap(),
                format!("{},{code},General,,{cells}\r\n", number[&order_id]),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(expected.len(), 4);
    expected.sort();
    let tickets_csv = admin_orders::report(&db, &cipher, &event_id, Report::Tickets)
        .await
        .unwrap();
    assert_eq!(
        tickets_csv,
        format!(
            "Order,Ticket code,Ticket Type,Seat,Attendee,Buyer email,Status,Checked in at,\"Badge name, if different\",Needs step-free access\r\n{}",
            expected
                .into_iter()
                .map(|(_, _, line)| line)
                .collect::<String>()
        )
    );
}

fn csv_rows(csv: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = csv.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                cell.push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => row.push(std::mem::take(&mut cell)),
            '\r' if !quoted => {
                assert_eq!(chars.next(), Some('\n'));
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
            }
            _ => cell.push(ch),
        }
    }
    assert!(!quoted);
    assert!(cell.is_empty() && row.is_empty());
    rows
}

#[tokio::test]
async fn edited_question_does_not_reinterpret_historical_text_as_checkbox() {
    let (_dir, db, cipher, event, kind) = setup().await;
    let question = questions::add(&db, &event, "Order", "Text", "Old label", &[], false)
        .await
        .unwrap();
    let now = Utc::now().timestamp();
    events::publish(&db, &event, now).await.unwrap();
    let old = checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "old@example.com",
        &["Old"],
        &[(format!("question_{question}"), "Historical text")],
        now,
    )
    .await;
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE event_questions SET kind = 'Checkbox', label = 'New label' WHERE id = ?",
        [question.clone().into()],
    ))
    .await
    .unwrap();
    let new = checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "new@example.com",
        &["New"],
        &[],
        now + 1,
    )
    .await;
    let csv = admin_orders::report(&db, &cipher, &event, Report::Orders)
        .await
        .unwrap();
    let rows = csv_rows(&csv);
    assert_eq!(&rows[0][9..], ["New label", "Old label"]);
    let old_number = admin_orders::order(&db, &cipher, &old)
        .await
        .unwrap()
        .number;
    let new_number = admin_orders::order(&db, &cipher, &new)
        .await
        .unwrap()
        .number;
    assert_eq!(
        &rows.iter().find(|row| row[0] == old_number).unwrap()[9..],
        ["", "Historical text"]
    );
    assert_eq!(
        &rows.iter().find(|row| row[0] == new_number).unwrap()[9..],
        ["No", ""]
    );
}

#[tokio::test]
async fn duplicate_labels_and_snapshot_versions_keep_separate_values_and_scopes() {
    let (_dir, db, cipher, event, kind) = setup().await;
    let q = questions::add(&db, &event, "Order", "Text", "Shared", &[], false)
        .await
        .unwrap();
    let other = questions::add(&db, &event, "Order", "Text", "Shared", &[], false)
        .await
        .unwrap();
    let now = Utc::now().timestamp();
    events::publish(&db, &event, now).await.unwrap();
    let old = checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "old@example.com",
        &["Old"],
        &[
            (format!("question_{q}"), "Old text"),
            (format!("question_{other}"), "Different ID"),
        ],
        now,
    )
    .await;
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE event_questions SET kind = 'Checkbox' WHERE id = ?",
        [q.clone().into()],
    ))
    .await
    .unwrap();
    let checked = checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "checked@example.com",
        &["Checked"],
        &[(format!("question_{q}"), "yes")],
        now + 1,
    )
    .await;
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE event_questions SET scope = 'Attendee', kind = 'Choice', options_json = '[\"A\",\"B\"]' WHERE id = ?", [q.clone().into()])).await.unwrap();
    let attendees = checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "attendees@example.com",
        &["First", "Second"],
        &[
            (format!("question_{q}_0"), "A"),
            (format!("question_{q}_1"), "B"),
        ],
        now + 2,
    )
    .await;
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE event_questions SET options_json = '[\"A\",\"C\"]', required = 1 WHERE id = ?",
        [q.clone().into()],
    ))
    .await
    .unwrap();
    let latest = checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "latest@example.com",
        &["Latest"],
        &[(format!("question_{q}_0"), "C")],
        now + 3,
    )
    .await;
    questions::delete(&db, &event, &other).await.unwrap();
    let csv = admin_orders::report(&db, &cipher, &event, Report::Orders)
        .await
        .unwrap();
    assert_eq!(
        csv,
        admin_orders::report(&db, &cipher, &event, Report::Orders)
            .await
            .unwrap()
    );
    let rows = csv_rows(&csv);
    let headers = &rows[0][9..];
    assert_eq!(headers.len(), 3);
    assert_eq!(
        headers
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    assert!(headers[0].starts_with(&format!("Shared [{q}; ")));
    assert!(headers[1].starts_with(&format!("Shared [{other}; ")));
    assert!(headers[2].starts_with(&format!("Shared [{q}; ")));
    for (id, expected) in [
        (&old, ["Old text", "Different ID", ""]),
        (&checked, ["", "", "Yes"]),
        (&attendees, ["", "", ""]),
        (&latest, ["", "", ""]),
    ] {
        let number = admin_orders::order(&db, &cipher, id).await.unwrap().number;
        assert_eq!(
            &rows.iter().find(|row| row[0] == number).unwrap()[9..],
            expected
        );
    }
    let tickets = csv_rows(
        &admin_orders::report(&db, &cipher, &event, Report::Tickets)
            .await
            .unwrap(),
    );
    assert_eq!(tickets[0].len(), 10);
    assert_ne!(tickets[0][8], tickets[0][9]);
    for (name, expected) in [
        ("Old", ["", ""]),
        ("Checked", ["", ""]),
        ("First", ["", "A"]),
        ("Second", ["", "B"]),
        ("Latest", ["C", ""]),
    ] {
        assert_eq!(
            &tickets.iter().find(|row| row[4] == name).unwrap()[8..],
            expected
        );
    }
}

#[tokio::test]
async fn deleted_unchecked_boxes_are_no_only_for_orders_actually_asked() {
    let (_dir, db, cipher, event, kind) = setup().await;
    let now = Utc::now().timestamp();
    events::publish(&db, &event, now).await.unwrap();
    let earlier = checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "early@example.com",
        &["Early"],
        &[],
        now,
    )
    .await;
    let order_box = questions::add(&db, &event, "Order", "Checkbox", "Order box", &[], false)
        .await
        .unwrap();
    let attendee_box = questions::add(
        &db,
        &event,
        "Attendee",
        "Checkbox",
        "Attendee box",
        &[],
        false,
    )
    .await
    .unwrap();
    let unchecked = checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "unchecked@example.com",
        &["One", "Two"],
        &[],
        now + 1,
    )
    .await;
    let anonymous = checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "private@example.com",
        &["Private"],
        &[],
        now + 2,
    )
    .await;
    admin_orders::anonymize(&db, &cipher, &anonymous)
        .await
        .unwrap();
    let comp = admin_orders::complimentary(
        &db,
        &cipher,
        &event,
        &kind,
        "comp@example.com",
        "Guest",
        now + 3,
    )
    .await
    .unwrap();
    questions::delete(&db, &event, &order_box).await.unwrap();
    questions::delete(&db, &event, &attendee_box).await.unwrap();
    let abandoned = questions::add(
        &db,
        &event,
        "Order",
        "Text",
        "Abandoned checkout",
        &[],
        false,
    )
    .await
    .unwrap();
    inventory::reserve(
        &db,
        &event,
        &[Selection {
            ticket_type_id: kind.clone(),
            quantity: 1,
        }],
        now + 4,
    )
    .await
    .unwrap();
    questions::delete(&db, &event, &abandoned).await.unwrap();
    let csv = admin_orders::report(&db, &cipher, &event, Report::Orders)
        .await
        .unwrap();
    let rows = csv_rows(&csv);
    assert_eq!(&rows[0][9..], ["Order box"]);
    for (id, expected) in [
        (&earlier, ""),
        (&unchecked, "No"),
        (&anonymous, ""),
        (&comp, ""),
    ] {
        let number = admin_orders::order(&db, &cipher, id).await.unwrap().number;
        assert_eq!(
            rows.iter().find(|row| row[0] == number).unwrap()[9],
            expected
        );
    }
    let rows = csv_rows(
        &admin_orders::report(&db, &cipher, &event, Report::Tickets)
            .await
            .unwrap(),
    );
    assert_eq!(&rows[0][8..], ["Attendee box"]);
    for (name, expected) in [
        ("Early", ""),
        ("One", "No"),
        ("Two", "No"),
        ("", ""),
        ("Guest", ""),
    ] {
        assert_eq!(rows.iter().find(|row| row[4] == name).unwrap()[8], expected);
    }
    assert!(!csv.contains("private@example.com"));
    assert!(questions::order_answers(&db, &cipher, &anonymous)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn historical_headers_and_values_escape_csv_and_block_formulas() {
    let (_dir, db, cipher, event, kind) = setup().await;
    let now = Utc::now().timestamp();
    let labels = [
        "=Formula",
        "+Label",
        "-Label",
        "@Label",
        "Line, \"quoted\"\nnext",
    ];
    let values = [
        "=SUM(A1)",
        "+1",
        "-1",
        "@SUM(A1)",
        "First, \"quoted\"\nsecond\rthird",
    ];
    let mut ids = Vec::new();
    for label in labels {
        ids.push(
            questions::add(&db, &event, "Order", "Text", label, &[], false)
                .await
                .unwrap(),
        );
    }
    events::publish(&db, &event, now).await.unwrap();
    let answers = ids
        .iter()
        .zip(values)
        .map(|(id, value)| (format!("question_{id}"), value))
        .collect::<Vec<_>>();
    checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "csv@example.com",
        &["CSV"],
        &answers,
        now,
    )
    .await;
    for id in ids {
        questions::delete(&db, &event, &id).await.unwrap();
    }
    let csv = admin_orders::report(&db, &cipher, &event, Report::Orders)
        .await
        .unwrap();
    let rows = csv_rows(&csv);
    assert_eq!(
        &rows[0][9..],
        [
            "'=Formula",
            "'+Label",
            "'-Label",
            "'@Label",
            "Line, \"quoted\"\nnext"
        ]
    );
    assert_eq!(
        &rows[1][9..],
        [
            "'=SUM(A1)",
            "'+1",
            "'-1",
            "'@SUM(A1)",
            "First, \"quoted\"\nsecond\rthird"
        ]
    );
    assert!(csv.contains("\"Line, \"\"quoted\"\"\nnext\""));
    assert!(csv.contains("\"First, \"\"quoted\"\"\nsecond\rthird\""));
}

#[tokio::test]
async fn paid_attendee_answers_follow_ticket_insertion_ordinals_and_checkins() {
    use crabgate::door::{self, CheckinResult};
    let (_dir, db, cipher, event, kind) = setup().await;
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE ticket_types SET price_cents = 1200 WHERE id = ?",
        [kind.clone().into()],
    ))
    .await
    .unwrap();
    let event_model = events::get(&db, &event).await.unwrap().unwrap();
    let second_kind = events::add_ticket_type(
        &db,
        &event_model,
        &TicketTypeInput {
            name: "Second Type".to_owned(),
            price: "5".to_owned(),
            capacity: 10,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    let q = questions::add(
        &db,
        &event,
        "Attendee",
        "Text",
        "Historical badge",
        &[],
        false,
    )
    .await
    .unwrap();
    let now = Utc::now().timestamp();
    events::publish(&db, &event, now).await.unwrap();
    let order = checkout_selections(
        &db,
        &cipher,
        &event,
        &[
            Selection {
                ticket_type_id: second_kind.id,
                quantity: 1,
            },
            Selection {
                ticket_type_id: kind,
                quantity: 2,
            },
        ],
        "paid@example.com",
        &["First", "Second", "Third"],
        &[
            (format!("question_{q}_0"), "Badge 1"),
            (format!("question_{q}_1"), "Badge 2"),
            (format!("question_{q}_2"), "Badge 3"),
        ],
        now,
    )
    .await;
    assert_eq!(
        admin_orders::order(&db, &cipher, &order)
            .await
            .unwrap()
            .total_cents,
        2900
    );
    let tickets = db
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id, code FROM tickets WHERE order_id = ? ORDER BY rowid",
            [order.clone().into()],
        ))
        .await
        .unwrap();
    for (ticket, id) in tickets.iter().zip(["ticket-z", "ticket-a", "ticket-m"]) {
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE tickets SET id = ? WHERE id = ?",
            [
                id.into(),
                ticket.try_get::<String>("", "id").unwrap().into(),
            ],
        ))
        .await
        .unwrap();
    }
    let first_code: String = tickets[0].try_get("", "code").unwrap();
    let third_code: String = tickets[2].try_get("", "code").unwrap();
    assert!(matches!(
        door::check_in(
            &db,
            &cipher,
            &event,
            &first_code,
            "Member",
            "admin",
            now + 1
        )
        .await
        .unwrap(),
        CheckinResult::CheckedIn(_)
    ));
    assert!(matches!(
        door::check_in(
            &db,
            &cipher,
            &event,
            &third_code,
            "Member",
            "admin",
            now + 2
        )
        .await
        .unwrap(),
        CheckinResult::CheckedIn(_)
    ));
    questions::delete(&db, &event, &q).await.unwrap();
    let rows = csv_rows(
        &admin_orders::report(&db, &cipher, &event, Report::Tickets)
            .await
            .unwrap(),
    );
    assert_eq!(&rows[0][8..], ["Historical badge"]);
    assert_eq!(
        &rows.iter().map(|row| row[4].as_str()).collect::<Vec<_>>()[1..],
        ["Second", "Third", "First"]
    );
    for (name, badge, kind, checked) in [
        ("First", "Badge 1", "General", (now + 1).to_string()),
        ("Second", "Badge 2", "General", String::new()),
        ("Third", "Badge 3", "Second Type", (now + 2).to_string()),
    ] {
        let row = rows.iter().find(|row| row[4] == name).unwrap();
        assert_eq!(row[8], badge);
        assert_eq!(row[2], kind);
        assert_eq!(row[7], checked);
    }
    let checkins = csv_rows(
        &admin_orders::report(&db, &cipher, &event, Report::Checkins)
            .await
            .unwrap(),
    );
    assert_eq!(checkins.len(), 3);
    assert_eq!(
        &checkins[1],
        &[
            first_code,
            String::new(),
            "First".to_owned(),
            (now + 1).to_string(),
            "Member".to_owned(),
            "admin".to_owned(),
            String::new(),
            String::new()
        ]
    );
    assert_eq!(checkins[2][0], third_code);
    assert_eq!(checkins[2][2], "Third");
    admin_orders::anonymize(&db, &cipher, &order).await.unwrap();
    for kind in [Report::Orders, Report::Tickets, Report::Checkins] {
        let csv = admin_orders::report(&db, &cipher, &event, kind)
            .await
            .unwrap();
        for pii in [
            "paid@example.com",
            "First",
            "Second,",
            "Third",
            "Badge 1",
            "Badge 2",
            "Badge 3",
        ] {
            assert!(!csv.contains(pii));
        }
    }
}

#[tokio::test]
#[serial_test::serial]
async fn checkout_http_export_retains_deleted_answers_and_enforces_permissions() {
    use crabgate::{
        app::App,
        members::{self, Role},
        operator,
    };
    use loco_rs::testing::prelude::*;
    request::<App, _, _>(|request, ctx| async move {
        let row = ctx.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT id, db_path, data_key FROM organizers WHERE subdomain = 'local'")).await.unwrap().unwrap();
        let organizer: String = row.try_get("", "id").unwrap();
        let path: String = row.try_get("", "db_path").unwrap();
        let wrapped: String = row.try_get("", "data_key").unwrap();
        let db = sea_orm::Database::connect(format!("sqlite://{path}?mode=rw")).await.unwrap();
        let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap()
            .open_organizer_key(&organizer, &wrapped).unwrap();
        let event = events::create(&db, &event_input()).await.unwrap();
        let kind = events::add_ticket_type(&db, &event, &TicketTypeInput {
            name: "General".to_owned(), price: "0".to_owned(), capacity: 10, sales_open: None, sales_close: None,
        }).await.unwrap();
        let q = questions::add(&db, &event.id, "Order", "Text", "Deleted HTTP question", &[], false).await.unwrap();
        let attendee = questions::add(&db, &event.id, "Attendee", "Text", "Deleted attendee", &[], false).await.unwrap();
        let now = Utc::now().timestamp();
        events::publish(&db, &event.id, now).await.unwrap();
        let hold = inventory::reserve(&db, &event.id, &[Selection { ticket_type_id: kind.id, quantity: 1 }], now).await.unwrap();
        let submitted = request.post(&format!("/checkout/{}", hold.id)).form(&[
            ("email".to_owned(), "http@example.com".to_owned()), ("name_0".to_owned(), "HTTP Attendee".to_owned()),
            (format!("question_{q}"), "HTTP historical response".to_owned()),
            (format!("question_{attendee}_0"), "HTTP badge".to_owned()),
        ]).await;
        assert_eq!(submitted.status_code(), 303);
        assert_eq!(submitted.header("location").to_str().unwrap(), format!("/checkout/{}/confirmation", hold.id));
        questions::delete(&db, &event.id, &q).await.unwrap();
        questions::delete(&db, &event.id, &attendee).await.unwrap();
        let member_id = members::add_member(&ctx.db, &organizer, "admin@example.com", Role::Admin).await.unwrap();
        let session = members::create_session_for_email(&ctx.db, &organizer, "admin@example.com", now).await.unwrap().unwrap();
        let admin_cookie = format!("cg_session={session}");
        members::add_member(&ctx.db, &organizer, "door@example.com", Role::DoorStaff).await.unwrap();
        let token = members::request_magic_link(&ctx.db, &organizer, "door@example.com", now).await.unwrap().unwrap();
        let door_session = members::claim_magic_link(&ctx.db, &organizer, &token, now).await.unwrap().unwrap();
        ctx.db.execute_raw(Statement::from_string(DatabaseBackend::Sqlite,
            "INSERT INTO operators (id, email) VALUES ('export-support', 'support@example.com')")).await.unwrap();
        let token = operator::request_link(&ctx.db, "support@example.com", now).await.unwrap().unwrap();
        let operator_session = operator::claim_link(&ctx.db, &token, now).await.unwrap().unwrap();
        operator::grant(&ctx.db, &organizer, &member_id, 1, now).await.unwrap();
        for filename in ["orders.csv", "tickets.csv", "checkins.csv"] {
            let path = format!("/office/events/{}/export/{filename}", event.id);
            assert_eq!(request.get(&path).await.status_code(), 303);
            assert_eq!(request.get(&path).add_header("cookie", format!("cg_session={door_session}")).await.status_code(), 403);
            assert_eq!(request.get(&path).add_header("cookie", format!("cg_operator_session={operator_session}")).await.status_code(), 403);
            let res = request.get(&path).add_header("cookie", &admin_cookie).await;
            assert_eq!(res.status_code(), 200);
            assert_eq!(res.header("content-type").to_str().unwrap(), "text/csv; charset=utf-8");
            if filename == "orders.csv" { assert_eq!(&csv_rows(&res.text())[1][9..], ["HTTP historical response"]); }
            if filename == "tickets.csv" { assert_eq!(&csv_rows(&res.text())[1][8..], ["HTTP badge"]); }
        }
        let order = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT id FROM orders WHERE hold_id = ?", [hold.id.into()])).await.unwrap().unwrap();
        let id: String = order.try_get("", "id").unwrap();
        let anonymized = request.post(&format!("/office/orders/{id}/anonymize")).add_header("cookie", &admin_cookie).await;
        assert_eq!(anonymized.status_code(), 303);
        for filename in ["orders.csv", "tickets.csv"] {
            let res = request.get(&format!("/office/events/{}/export/{filename}", event.id)).add_header("cookie", &admin_cookie).await;
            assert_eq!(res.status_code(), 200);
            for pii in ["http@example.com", "HTTP Attendee", "HTTP historical response", "HTTP badge"] { assert!(!res.text().contains(pii)); }
        }
        assert!(questions::order_answers(&db, &cipher, &id).await.unwrap().is_empty());
    }).await;
}

#[tokio::test]
async fn answer_only_snapshots_remain_exported_and_other_events_stay_out() {
    let (_dir, db, cipher, event, kind) = setup().await;
    let q = questions::add(
        &db,
        &event,
        "Order",
        "Text",
        "Answer-only snapshot",
        &[],
        false,
    )
    .await
    .unwrap();
    let now = Utc::now().timestamp();
    events::publish(&db, &event, now).await.unwrap();
    let order = checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "own@example.com",
        &["Own"],
        &[(format!("question_{q}"), "Preserved answer")],
        now,
    )
    .await;
    questions::delete(&db, &event, &q).await.unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "DELETE FROM hold_questions WHERE hold_id = (SELECT hold_id FROM orders WHERE id = ?)",
        [order.clone().into()],
    ))
    .await
    .unwrap();
    let mut input = event_input();
    input.title = "Other event".to_owned();
    let other = events::create(&db, &input).await.unwrap();
    let other_kind = events::add_ticket_type(
        &db,
        &other,
        &TicketTypeInput {
            name: "Other".to_owned(),
            price: "0".to_owned(),
            capacity: 10,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    let other_q = questions::add(
        &db,
        &other.id,
        "Order",
        "Text",
        "Other historical question",
        &[],
        false,
    )
    .await
    .unwrap();
    events::publish(&db, &other.id, now).await.unwrap();
    checkout(
        &db,
        &cipher,
        &other.id,
        &other_kind.id,
        "other@example.com",
        &["Other"],
        &[(format!("question_{other_q}"), "Other private answer")],
        now,
    )
    .await;
    questions::delete(&db, &other.id, &other_q).await.unwrap();
    let csv = admin_orders::report(&db, &cipher, &event, Report::Orders)
        .await
        .unwrap();
    let rows = csv_rows(&csv);
    assert_eq!(rows.len(), 2);
    assert_eq!(&rows[0][9..], ["Answer-only snapshot"]);
    assert_eq!(&rows[1][9..], ["Preserved answer"]);
    assert!(!csv.contains("Other"));
    assert!(!csv.contains("other@example.com"));
    admin_orders::anonymize(&db, &cipher, &order).await.unwrap();
    let csv = admin_orders::report(&db, &cipher, &event, Report::Orders)
        .await
        .unwrap();
    assert!(!csv.contains("own@example.com"));
    assert!(!csv.contains("Preserved answer"));
    assert!(!csv.contains("Answer-only snapshot"));
}

#[tokio::test]
async fn generated_headers_avoid_raw_labels_and_collision_chains_in_both_reports() {
    for (scope, offset, label) in [
        ("Order", 9, "X"),
        ("Attendee", 8, "X"),
        ("Order", 9, "=X"),
        ("Attendee", 8, "=X"),
    ] {
        let (_dir, db, cipher, event, kind) = setup().await;
        let first = questions::add(&db, &event, scope, "Text", label, &[], false)
            .await
            .unwrap();
        let second = questions::add(&db, &event, scope, "Text", label, &[], false)
            .await
            .unwrap();
        let report = || {
            if scope == "Order" {
                Report::Orders
            } else {
                Report::Tickets
            }
        };
        let initial = csv_rows(
            &admin_orders::report(&db, &cipher, &event, report())
                .await
                .unwrap(),
        );
        let generated = initial[0][offset].clone();
        let second_header = initial[0][offset + 1].clone();
        let copied = questions::add(&db, &event, scope, "Text", &generated, &[], false)
            .await
            .unwrap();
        let chained_label = format!("{generated} [2]");
        let chained = questions::add(&db, &event, scope, "Text", &chained_label, &[], false)
            .await
            .unwrap();
        let ordinary = questions::add(
            &db,
            &event,
            scope,
            "Text",
            "Ordinary, \"quoted\"\nlabel",
            &[],
            false,
        )
        .await
        .unwrap();
        let fixed = questions::add(&db, &event, scope, "Text", "Order", &[], false)
            .await
            .unwrap();
        let field = |id: &str| {
            if scope == "Order" {
                format!("question_{id}")
            } else {
                format!("question_{id}_0")
            }
        };
        let now = Utc::now().timestamp();
        events::publish(&db, &event, now).await.unwrap();
        checkout(
            &db,
            &cipher,
            &event,
            &kind,
            "collision@example.com",
            &["Attendee"],
            &[
                (field(&first), "First response"),
                (field(&second), "Second response"),
                (field(&copied), "Copied response"),
                (field(&chained), "Chained response"),
                (field(&ordinary), "Ordinary response"),
                (field(&fixed), "Fixed response"),
            ],
            now,
        )
        .await;
        let csv = admin_orders::report(&db, &cipher, &event, report())
            .await
            .unwrap();
        assert_eq!(
            csv,
            admin_orders::report(&db, &cipher, &event, report())
                .await
                .unwrap()
        );
        let rows = csv_rows(&csv);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][offset], format!("{generated} [3]"));
        assert_eq!(rows[0][offset + 1], second_header);
        assert_eq!(rows[0][offset + 2], generated);
        assert_eq!(rows[0][offset + 3], chained_label);
        assert_eq!(rows[0][offset + 4], "Ordinary, \"quoted\"\nlabel");
        assert!(rows[0][offset + 5].starts_with(&format!("Order [{fixed}; ")));
        assert_eq!(
            rows[0]
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            rows[0].len()
        );
        assert_eq!(
            &rows[1][offset..],
            [
                "First response",
                "Second response",
                "Copied response",
                "Chained response",
                "Ordinary response",
                "Fixed response"
            ]
        );
    }
}

#[tokio::test]
async fn protected_headers_remain_unique_and_formula_safe_in_both_reports() {
    for (scope, offset) in [("Order", 9), ("Attendee", 8)] {
        let (_dir, db, cipher, event, kind) = setup().await;
        let mut ids = Vec::new();
        for label in [
            "=X",
            "'=X",
            "+Y",
            "'+Y",
            "@Z,\"quoted\"",
            "'@Z,\"quoted\"",
            "Unique",
        ] {
            ids.push(
                questions::add(&db, &event, scope, "Text", label, &[], false)
                    .await
                    .unwrap(),
            );
        }
        let answers = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let field = if scope == "Order" {
                    format!("question_{id}")
                } else {
                    format!("question_{id}_0")
                };
                (
                    field,
                    [
                        "=value", "Literal", "+value", "Space", "@value", "Quoted", "Ordinary",
                    ][i],
                )
            })
            .collect::<Vec<_>>();
        let now = Utc::now().timestamp();
        events::publish(&db, &event, now).await.unwrap();
        checkout(
            &db,
            &cipher,
            &event,
            &kind,
            "formula@example.com",
            &["Attendee"],
            &answers,
            now,
        )
        .await;
        let report = || {
            if scope == "Order" {
                Report::Orders
            } else {
                Report::Tickets
            }
        };
        let csv = admin_orders::report(&db, &cipher, &event, report())
            .await
            .unwrap();
        assert_eq!(
            csv,
            admin_orders::report(&db, &cipher, &event, report())
                .await
                .unwrap()
        );
        let rows = csv_rows(&csv);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0]
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            rows[0].len()
        );
        for (i, prefix) in [
            "'=X",
            "'=X",
            "'+Y",
            "'+Y",
            "'@Z,\"quoted\"",
            "'@Z,\"quoted\"",
        ]
        .iter()
        .enumerate()
        {
            assert!(rows[0][offset + i].starts_with(&format!("{prefix} [{}; ", ids[i])));
        }
        assert_eq!(rows[0][offset + 6], "Unique");
        assert_eq!(
            &rows[1][offset..],
            ["'=value", "Literal", "'+value", "Space", "'@value", "Quoted", "Ordinary"]
        );
    }
}

#[tokio::test]
async fn checkbox_required_versions_keep_no_yes_and_ordinals() {
    let (_dir, db, cipher, event, kind) = setup().await;
    let q = questions::add(&db, &event, "Attendee", "Checkbox", "Consent", &[], false)
        .await
        .unwrap();
    let now = Utc::now().timestamp();
    events::publish(&db, &event, now).await.unwrap();
    checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "old@example.com",
        &["Old Yes", "Old No"],
        &[(format!("question_{q}_0"), "yes")],
        now,
    )
    .await;
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE event_questions SET required = 1 WHERE id = ?",
        [q.clone().into()],
    ))
    .await
    .unwrap();
    checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "new@example.com",
        &["New Yes", "New Second"],
        &[
            (format!("question_{q}_0"), "yes"),
            (format!("question_{q}_1"), "yes"),
        ],
        now + 1,
    )
    .await;
    questions::delete(&db, &event, &q).await.unwrap();
    let rows = csv_rows(
        &admin_orders::report(&db, &cipher, &event, Report::Tickets)
            .await
            .unwrap(),
    );
    assert_eq!(rows[0].len(), 10);
    for (name, expected) in [
        ("Old Yes", ["Yes", ""]),
        ("Old No", ["No", ""]),
        ("New Yes", ["", "Yes"]),
        ("New Second", ["", "Yes"]),
    ] {
        assert_eq!(
            &rows.iter().find(|row| row[4] == name).unwrap()[8..],
            expected
        );
    }
}

#[tokio::test]
async fn exports_bound_question_reads_and_preserve_rows_across_batches() {
    use std::sync::{Arc, Mutex};
    let (_dir, mut db, cipher, event, kind) = setup().await;
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE ticket_types SET capacity=2000 WHERE id=?",
        [kind.clone().into()],
    ))
    .await
    .unwrap();
    let now = Utc::now().timestamp();
    events::publish(&db, &event, now).await.unwrap();
    let before = admin_orders::complimentary(
        &db,
        &cipher,
        &event,
        &kind,
        "before@example.com",
        "Before",
        now,
    )
    .await
    .unwrap();
    let reference = questions::add(&db, &event, "Order", "Text", "Reference", &[], false)
        .await
        .unwrap();
    let badge = questions::add(&db, &event, "Attendee", "Text", "Badge", &[], false)
        .await
        .unwrap();
    let consent = questions::add(
        &db,
        &event,
        "Attendee",
        "Checkbox",
        "Past consent",
        &[],
        false,
    )
    .await
    .unwrap();
    let seed = checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "batch@example.com",
        &["One", "Two", "Three"],
        &[
            (format!("question_{reference}"), "=Ref"),
            (format!("question_{badge}_0"), "First"),
            (format!("question_{badge}_1"), "Second"),
            (format!("question_{badge}_2"), "Third"),
            (format!("question_{consent}_0"), "yes"),
        ],
        now + 1,
    )
    .await;
    for (id, number) in [(&before, "BEFORE"), (&seed, "SEED")] {
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE orders SET number=? WHERE id=?",
            [number.into(), id.clone().into()],
        ))
        .await
        .unwrap();
        let tickets = db
            .query_all_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT id FROM tickets WHERE order_id=? ORDER BY rowid",
                [id.clone().into()],
            ))
            .await
            .unwrap();
        for (ordinal, ticket) in tickets.iter().enumerate() {
            db.execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE tickets SET code=?, created_at=? WHERE id=?",
                [
                    format!("{number}-{}", ordinal + 1).into(),
                    ((now + i64::from(number == "SEED")) * 10 + ordinal as i64).into(),
                    ticket.try_get::<String>("", "id").unwrap().into(),
                ],
            ))
            .await
            .unwrap();
        }
    }
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<500) INSERT INTO holds (id,event_id,status,expires_at,buyer_email_encrypted,created_at) SELECT 'batch-hold-'||n.i,h.event_id,h.status,h.expires_at,h.buyer_email_encrypted,h.created_at+n.i FROM n JOIN orders o ON o.id=? JOIN holds h ON h.id=o.hold_id",
        [seed.clone().into()])).await.unwrap();
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<500) INSERT INTO orders (id,number,event_id,hold_id,stripe_session_id,buyer_email_encrypted,total_cents,status,created_at) SELECT 'batch-order-'||n.i,printf('B%03d',n.i),o.event_id,'batch-hold-'||n.i,'batch-session-'||n.i,o.buyer_email_encrypted,o.total_cents,o.status,o.created_at+n.i FROM n JOIN orders o ON o.id=?",
        [seed.clone().into()])).await.unwrap();
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO tickets (id,order_id,event_id,ticket_type_id,code,attendee_name_encrypted,status,created_at) SELECT o.id||'-'||t.code,o.id,t.event_id,t.ticket_type_id,o.number||'-'||t.code,t.attendee_name_encrypted,t.status,o.created_at*10+(SELECT COUNT(*) FROM tickets earlier WHERE earlier.order_id=t.order_id AND earlier.rowid<t.rowid) FROM orders o JOIN tickets t ON t.order_id=? WHERE o.id LIKE 'batch-order-%' ORDER BY o.created_at,t.rowid",
        [seed.clone().into()])).await.unwrap();
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO hold_questions SELECT o.hold_id,q.question_id,q.scope,q.kind,q.label,q.options_json,q.required,q.sort_order FROM orders o JOIN orders seed ON seed.id=? JOIN hold_questions q ON q.hold_id=seed.hold_id WHERE o.id LIKE 'batch-order-%'",
        [seed.clone().into()])).await.unwrap();
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO hold_answers SELECT o.hold_id,a.question_id,a.ordinal,a.question_snapshot_json,a.answer_encrypted FROM orders o JOIN orders seed ON seed.id=? JOIN hold_answers a ON a.hold_id=seed.hold_id WHERE o.id LIKE 'batch-order-%'",
        [seed.into()])).await.unwrap();
    for question in [reference, badge, consent] {
        questions::delete(&db, &event, &question).await.unwrap();
    }
    admin_orders::anonymize(&db, &cipher, "batch-order-250")
        .await
        .unwrap();
    let reads = Arc::new(Mutex::new(Vec::new()));
    let captured = reads.clone();
    db.set_metric_callback(move |info| {
        assert!(!info.failed);
        captured.lock().unwrap().push(
            info.statement
                .values
                .as_ref()
                .map_or(0, |values| values.0.len()),
        );
    });
    let csv = admin_orders::report(&db, &cipher, &event, Report::Orders)
        .await
        .unwrap();
    let mut expected = "Order,Buyer email,Original cents,Discount code,Discount cents,Paid cents,Status,Complimentary,Created at,Reference\r\n".to_owned();
    for i in (1..=500).rev() {
        let (email, answer) = if i == 250 {
            ("", "")
        } else {
            ("batch@example.com", "'=Ref")
        };
        expected.push_str(&format!(
            "B{i:03},{email},0,,0,0,Paid,No,{},{answer}\r\n",
            now + 1 + i
        ));
    }
    expected.push_str(&format!("SEED,batch@example.com,0,,0,0,Paid,No,{},'=Ref\r\nBEFORE,before@example.com,0,,0,0,Paid,Yes,{now},\r\n", now + 1));
    assert_eq!(csv, expected);
    assert!(
        reads.lock().unwrap().len() <= 20,
        "{} reads exceed the bounded export budget",
        reads.lock().unwrap().len()
    );
    assert!(reads.lock().unwrap().iter().all(|bound| *bound <= 500));
    let expected_orders = expected;
    reads.lock().unwrap().clear();
    let csv = admin_orders::report(&db, &cipher, &event, Report::Tickets)
        .await
        .unwrap();
    let mut expected = "Order,Ticket code,Ticket Type,Seat,Attendee,Buyer email,Status,Checked in at,Badge,Past consent\r\nBEFORE,BEFORE-1,General,,Before,before@example.com,Valid,,,\r\n".to_owned();
    for i in 0..=500 {
        let number = if i == 0 {
            "SEED".to_owned()
        } else {
            format!("B{i:03}")
        };
        for (ordinal, name, answer, consent) in [
            (1, "One", "First", "Yes"),
            (2, "Two", "Second", "No"),
            (3, "Three", "Third", "No"),
        ] {
            let code = if i == 0 {
                format!("SEED-{ordinal}")
            } else {
                format!("{number}-SEED-{ordinal}")
            };
            let (name, email, answer, consent) = if i == 250 {
                ("", "", "", "")
            } else {
                (name, "batch@example.com", answer, consent)
            };
            expected.push_str(&format!(
                "{number},{code},General,,{name},{email},Valid,,{answer},{consent}\r\n"
            ));
        }
    }
    assert_eq!(csv, expected);
    assert!(
        reads.lock().unwrap().len() <= 20,
        "{} reads exceed the bounded export budget",
        reads.lock().unwrap().len()
    );
    assert!(reads.lock().unwrap().iter().all(|bound| *bound <= 500));
    let selection = [Selection {
        ticket_type_id: kind.clone(),
        quantity: 1,
    }];
    let (orders_csv, tickets_csv, reservation) = tokio::join!(
        admin_orders::report(&db, &cipher, &event, Report::Orders),
        admin_orders::report(&db, &cipher, &event, Report::Tickets),
        inventory::reserve(&db, &event, &selection, now + 1),
    );
    assert_eq!(orders_csv.unwrap(), expected_orders);
    assert_eq!(tickets_csv.unwrap(), expected);
    reservation.unwrap();
    assert_eq!(
        inventory::type_counts(&db, &kind, now + 1).await.unwrap(),
        (1504, 1)
    );
}

#[tokio::test]
async fn historical_headers_normalize_json_whitespace_and_keep_negative_order_ordinals() {
    let (_dir, db, cipher, event, kind) = setup().await;
    let question = questions::add(
        &db,
        &event,
        "Order",
        "Choice",
        "Plan",
        &["A".into(), "B".into()],
        false,
    )
    .await
    .unwrap();
    let now = Utc::now().timestamp();
    events::publish(&db, &event, now).await.unwrap();
    let mut orders = Vec::new();
    for (i, label, number, email, name, answer) in [
        (0, "Plan", "FIRST", "first@example.com", "First", "A"),
        (1, "Version", "SECOND", "second@example.com", "Second", "B"),
        (2, "Plan", "THIRD", "third@example.com", "Third", "A"),
    ] {
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE event_questions SET label=? WHERE id=?",
            [label.into(), question.clone().into()],
        ))
        .await
        .unwrap();
        let order = checkout(
            &db,
            &cipher,
            &event,
            &kind,
            email,
            &[name],
            &[(format!("question_{question}"), answer)],
            now + i,
        )
        .await;
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE orders SET number=? WHERE id=?",
            [number.into(), order.clone().into()],
        ))
        .await
        .unwrap();
        orders.push(order);
    }
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE hold_answers SET ordinal=-7 WHERE hold_id=(SELECT hold_id FROM orders WHERE id=?)",
        [orders[0].clone().into()],
    ))
    .await
    .unwrap();
    let snapshot: String = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "SELECT a.question_snapshot_json FROM hold_answers a JOIN orders o ON o.hold_id=a.hold_id WHERE o.id=?",[orders[2].clone().into()])).await.unwrap().unwrap().try_get("","question_snapshot_json").unwrap();
    let question_value: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE hold_answers SET question_snapshot_json=? WHERE hold_id=(SELECT hold_id FROM orders WHERE id=?)",[serde_json::to_string_pretty(&question_value).unwrap().into(),orders[2].clone().into()])).await.unwrap();
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE hold_questions SET options_json='[ \"A\", \"B\" ]' WHERE hold_id=(SELECT hold_id FROM orders WHERE id=?)",[orders[2].clone().into()])).await.unwrap();
    questions::delete(&db, &event, &question).await.unwrap();
    let csv = admin_orders::report(&db, &cipher, &event, Report::Orders)
        .await
        .unwrap();
    assert_eq!(csv,format!("Order,Buyer email,Original cents,Discount code,Discount cents,Paid cents,Status,Complimentary,Created at,Plan,Version\r\nTHIRD,third@example.com,0,,0,0,Paid,No,{},A,\r\nSECOND,second@example.com,0,,0,0,Paid,No,{},,B\r\nFIRST,first@example.com,0,,0,0,Paid,No,{now},A,\r\n",now+2,now+1));
}

#[tokio::test]
async fn csv_validates_all_header_metadata_and_only_exported_answer_ciphertext() {
    use crabgate::admin_orders::AdminOrderError;
    use crabgate::questions::QuestionError;
    let (_dir, db, cipher, event, kind) = setup().await;
    let reference = questions::add(&db, &event, "Order", "Text", "Reference", &[], false)
        .await
        .unwrap();
    let badge = questions::add(&db, &event, "Attendee", "Text", "Badge", &[], false)
        .await
        .unwrap();
    let now = Utc::now().timestamp();
    events::publish(&db, &event, now).await.unwrap();
    let order = checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "scope@example.com",
        &["One"],
        &[
            (format!("question_{reference}"), "Order value"),
            (format!("question_{badge}_0"), "Ticket value"),
        ],
        now,
    )
    .await;
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE orders SET number='ONE' WHERE id=?",
        [order.clone().into()],
    ))
    .await
    .unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE tickets SET code='ONE-1' WHERE order_id=?",
        [order.clone().into()],
    ))
    .await
    .unwrap();
    let hold: String = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT hold_id FROM orders WHERE id=?",
            [order.clone().into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "hold_id")
        .unwrap();
    let mut saved = HashMap::new();
    for row in db.query_all_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"SELECT question_id,question_snapshot_json,answer_encrypted FROM hold_answers WHERE hold_id=?",[hold.clone().into()])).await.unwrap() {
        saved.insert(row.try_get::<String>("","question_id").unwrap(),(row.try_get::<String>("","question_snapshot_json").unwrap(),row.try_get::<String>("","answer_encrypted").unwrap()));
    }
    let expected_orders=format!("Order,Buyer email,Original cents,Discount code,Discount cents,Paid cents,Status,Complimentary,Created at,Reference\r\nONE,scope@example.com,0,,0,0,Paid,No,{now},Order value\r\n");
    let expected_tickets="Order,Ticket code,Ticket Type,Seat,Attendee,Buyer email,Status,Checked in at,Badge\r\nONE,ONE-1,General,,One,scope@example.com,Valid,,Ticket value\r\n";
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"UPDATE hold_answers SET answer_encrypted='invalid ciphertext' WHERE hold_id=? AND question_id=?",[hold.clone().into(),reference.clone().into()])).await.unwrap();
    assert_eq!(
        admin_orders::report(&db, &cipher, &event, Report::Tickets)
            .await
            .unwrap(),
        expected_tickets
    );
    assert!(matches!(
        admin_orders::report(&db, &cipher, &event, Report::Orders).await,
        Err(AdminOrderError::Question(QuestionError::Crypto(_)))
    ));
    assert!(matches!(
        questions::order_answers(&db, &cipher, &order).await,
        Err(QuestionError::Crypto(_))
    ));
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE hold_answers SET answer_encrypted=? WHERE hold_id=? AND question_id=?",
        [
            saved[&reference].1.clone().into(),
            hold.clone().into(),
            reference.clone().into(),
        ],
    ))
    .await
    .unwrap();
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"UPDATE hold_answers SET answer_encrypted='invalid ciphertext' WHERE hold_id=? AND question_id=?",[hold.clone().into(),badge.clone().into()])).await.unwrap();
    assert_eq!(
        admin_orders::report(&db, &cipher, &event, Report::Orders)
            .await
            .unwrap(),
        expected_orders
    );
    assert!(matches!(
        admin_orders::report(&db, &cipher, &event, Report::Tickets).await,
        Err(AdminOrderError::Question(QuestionError::Crypto(_)))
    ));
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE hold_answers SET answer_encrypted=? WHERE hold_id=? AND question_id=?",
        [
            saved[&badge].1.clone().into(),
            hold.clone().into(),
            badge.clone().into(),
        ],
    ))
    .await
    .unwrap();
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"INSERT INTO hold_answers(hold_id,question_id,ordinal,question_snapshot_json,answer_encrypted) VALUES(?,?,1,?,'invalid ciphertext')",[hold.clone().into(),badge.clone().into(),saved[&badge].0.clone().into()])).await.unwrap();
    assert_eq!(
        admin_orders::report(&db, &cipher, &event, Report::Tickets)
            .await
            .unwrap(),
        expected_tickets
    );
    assert!(matches!(
        questions::order_answers(&db, &cipher, &order).await,
        Err(QuestionError::Crypto(_))
    ));
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"UPDATE hold_answers SET question_snapshot_json='{' WHERE hold_id=? AND question_id=? AND ordinal<0",[hold.clone().into(),reference.clone().into()])).await.unwrap();
    assert!(matches!(
        admin_orders::report(&db, &cipher, &event, Report::Tickets).await,
        Err(AdminOrderError::Question(QuestionError::Json(_)))
    ));
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"UPDATE hold_answers SET question_snapshot_json=? WHERE hold_id=? AND question_id=? AND ordinal<0",[saved[&reference].0.clone().into(),hold.clone().into(),reference.clone().into()])).await.unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE hold_questions SET options_json='[' WHERE hold_id=? AND question_id=?",
        [hold.clone().into(), reference.clone().into()],
    ))
    .await
    .unwrap();
    assert!(matches!(
        admin_orders::report(&db, &cipher, &event, Report::Tickets).await,
        Err(AdminOrderError::Question(QuestionError::Json(_)))
    ));
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE hold_questions SET options_json='[]' WHERE hold_id=? AND question_id=?",
        [hold.into(), reference.into()],
    ))
    .await
    .unwrap();
    let other = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .unwrap()
        .generate_organizer_key("other")
        .unwrap()
        .1;
    for report in [Report::Orders, Report::Tickets] {
        assert!(matches!(
            admin_orders::report(&db, &other, &event, report).await,
            Err(AdminOrderError::Crypto(_))
                | Err(AdminOrderError::Question(QuestionError::Crypto(_)))
        ));
    }
}

#[tokio::test]
async fn interleaved_hundred_ticket_orders_do_not_reload_all_order_answers_per_chunk() {
    use std::sync::{Arc, Mutex};
    let (_dir, mut db, cipher, event, kind) = setup().await;
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE events SET max_per_order=100 WHERE id=?",
        [event.clone().into()],
    ))
    .await
    .unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE ticket_types SET capacity=20000 WHERE id=?",
        [kind.clone().into()],
    ))
    .await
    .unwrap();
    let badge = questions::add(&db, &event, "Attendee", "Text", "Badge", &[], false)
        .await
        .unwrap();
    let now = Utc::now().timestamp();
    events::publish(&db, &event, now).await.unwrap();
    let names = (0..100)
        .map(|i| format!("Guest-{i:03}"))
        .collect::<Vec<_>>();
    let answers = (0..100)
        .map(|i| (format!("question_{badge}_{i}"), format!("Badge-{i:03}")))
        .collect::<Vec<_>>();
    let name_refs = names.iter().map(String::as_str).collect::<Vec<_>>();
    let answer_refs = answers
        .iter()
        .map(|(field, answer)| (field.clone(), answer.as_str()))
        .collect::<Vec<_>>();
    let seed = checkout(
        &db,
        &cipher,
        &event,
        &kind,
        "many@example.com",
        &name_refs,
        &answer_refs,
        now,
    )
    .await;
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE orders SET number='M000' WHERE id=?",
        [seed.clone().into()],
    ))
    .await
    .unwrap();
    let tickets = db
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT id FROM tickets WHERE order_id=? ORDER BY rowid",
            [seed.clone().into()],
        ))
        .await
        .unwrap();
    for (ordinal, ticket) in tickets.iter().enumerate() {
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE tickets SET id=?,code=?,created_at=? WHERE id=?",
            [
                format!("many-{ordinal:03}-000").into(),
                format!("M000-T{ordinal:03}").into(),
                now.into(),
                ticket.try_get::<String>("", "id").unwrap().into(),
            ],
        ))
        .await
        .unwrap();
    }
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<99) INSERT INTO holds(id,event_id,status,expires_at,buyer_email_encrypted,created_at) SELECT 'many-hold-'||i,h.event_id,h.status,h.expires_at,h.buyer_email_encrypted,h.created_at FROM n JOIN orders o ON o.id=? JOIN holds h ON h.id=o.hold_id",[seed.clone().into()])).await.unwrap();
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<99) INSERT INTO orders(id,number,event_id,hold_id,stripe_session_id,buyer_email_encrypted,total_cents,status,created_at) SELECT 'many-order-'||i,printf('M%03d',i),o.event_id,'many-hold-'||i,'many-session-'||i,o.buyer_email_encrypted,o.total_cents,o.status,o.created_at FROM n JOIN orders o ON o.id=?",[seed.clone().into()])).await.unwrap();
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"INSERT INTO hold_items SELECT o.hold_id,i.ticket_type_id,i.quantity,i.price_cents FROM orders o JOIN orders seed ON seed.id=? JOIN hold_items i ON i.hold_id=seed.hold_id WHERE o.id LIKE 'many-order-%'",[seed.clone().into()])).await.unwrap();
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"INSERT INTO tickets(id,order_id,event_id,ticket_type_id,code,attendee_name_encrypted,status,created_at) SELECT substr(t.id,1,9)||substr(o.number,2),o.id,t.event_id,t.ticket_type_id,o.number||'-'||substr(t.code,6),t.attendee_name_encrypted,t.status,t.created_at FROM orders o JOIN tickets t ON t.order_id=? WHERE o.id LIKE 'many-order-%' ORDER BY o.number,t.rowid",[seed.clone().into()])).await.unwrap();
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"INSERT INTO hold_questions SELECT o.hold_id,q.question_id,q.scope,q.kind,q.label,q.options_json,q.required,q.sort_order FROM orders o JOIN orders seed ON seed.id=? JOIN hold_questions q ON q.hold_id=seed.hold_id WHERE o.id LIKE 'many-order-%'",[seed.clone().into()])).await.unwrap();
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"INSERT INTO hold_answers SELECT o.hold_id,a.question_id,a.ordinal,a.question_snapshot_json,a.answer_encrypted FROM orders o JOIN orders seed ON seed.id=? JOIN hold_answers a ON a.hold_id=seed.hold_id WHERE o.id LIKE 'many-order-%'",[seed.into()])).await.unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let recorded = captured.clone();
    db.set_metric_callback(move |info| {
        assert!(!info.failed);
        recorded.lock().unwrap().push(info.statement.clone());
    });
    let csv = admin_orders::report(&db, &cipher, &event, Report::Tickets)
        .await
        .unwrap();
    let mut expected =
        "Order,Ticket code,Ticket Type,Seat,Attendee,Buyer email,Status,Checked in at,Badge\r\n"
            .to_owned();
    for ordinal in 0..100 {
        for order in 0..100 {
            expected.push_str(&format!("M{order:03},M{order:03}-T{ordinal:03},General,,Guest-{ordinal:03},many@example.com,Valid,,Badge-{ordinal:03}\r\n"));
        }
    }
    assert_eq!(csv, expected);
    assert_eq!(
        inventory::type_counts(&db, &kind, now).await.unwrap(),
        (10000, 0)
    );
    db.set_metric_callback(|_| {});
    let statements = captured.lock().unwrap().clone();
    assert!(statements
        .iter()
        .all(|s| s.values.as_ref().map_or(0, |v| v.0.len()) <= 500));
    let mut answer_rows = 0;
    for statement in statements
        .into_iter()
        .filter(|s| s.sql.contains("a.answer_encrypted"))
    {
        let rows = db.query_all_raw(statement).await.unwrap();
        assert!(
            rows.len() <= 1000,
            "{} Answer models exceed the per-chunk budget",
            rows.len()
        );
        answer_rows += rows.len();
    }
    assert!(
        (10000..=12000).contains(&answer_rows),
        "{answer_rows} Answer models exceed the export budget"
    );
}

#[tokio::test]
async fn historical_question_id_chunks_preserve_every_header_and_exported_value() {
    use std::sync::{Arc, Mutex};
    let (_dir, mut db, cipher, event, kind) = setup().await;
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE ticket_types SET capacity=400 WHERE id=?",
        [kind.clone().into()],
    ))
    .await
    .unwrap();
    let now = Utc::now().timestamp();
    events::publish(&db, &event, now).await.unwrap();
    for i in 0..260 {
        let question = questions::add(
            &db,
            &event,
            "Attendee",
            "Text",
            &format!("Field-{i:03}"),
            &[],
            false,
        )
        .await
        .unwrap();
        let order = checkout(
            &db,
            &cipher,
            &event,
            &kind,
            &format!("idchunk-{i:03}@example.com"),
            &["Guest"],
            &[(format!("question_{question}_0"), &format!("Value-{i:03}"))],
            now + i,
        )
        .await;
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE orders SET number=? WHERE id=?",
            [format!("O{i:03}").into(), order.clone().into()],
        ))
        .await
        .unwrap();
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE tickets SET code=?,created_at=? WHERE order_id=?",
            [format!("T{i:03}").into(), (now + i).into(), order.into()],
        ))
        .await
        .unwrap();
        questions::delete(&db, &event, &question).await.unwrap();
    }
    let captured = Arc::new(Mutex::new(Vec::new()));
    let recorded = captured.clone();
    db.set_metric_callback(move |info| {
        assert!(!info.failed);
        recorded.lock().unwrap().push(info.statement.clone());
    });
    let csv = admin_orders::report(&db, &cipher, &event, Report::Tickets)
        .await
        .unwrap();
    let mut expected = format!(
        "Order,Ticket code,Ticket Type,Seat,Attendee,Buyer email,Status,Checked in at,{}\r\n",
        (0..260)
            .map(|i| format!("Field-{i:03}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    for i in 0..260 {
        let mut values = vec![String::new(); 260];
        values[i] = format!("Value-{i:03}");
        expected.push_str(&format!(
            "O{i:03},T{i:03},General,,Guest,idchunk-{i:03}@example.com,Valid,,{}\r\n",
            values.join(",")
        ));
    }
    assert_eq!(csv, expected);
    db.set_metric_callback(|_| {});
    let statements = captured.lock().unwrap().clone();
    assert!(statements.len() <= 20);
    assert!(statements
        .iter()
        .all(|s| s.values.as_ref().map_or(0, |v| v.0.len()) <= 500));
    let mut answers = 0;
    for statement in statements
        .into_iter()
        .filter(|s| s.sql.contains("a.answer_encrypted"))
    {
        answers += db.query_all_raw(statement).await.unwrap().len();
    }
    assert_eq!(answers, 260);
}

#[tokio::test]
async fn answer_only_history_keeps_values_when_snapshot_and_storage_ids_differ() {
    let (_dir, db, cipher, event, kind) = setup().await;
    let badge = questions::add(&db, &event, "Attendee", "Text", "Badge", &[], false)
        .await
        .unwrap();
    let now = Utc::now().timestamp();
    events::publish(&db, &event, now).await.unwrap();
    for (offset, number, name, email, value, storage_id) in [
        (
            0,
            "ONE",
            "One",
            "one@example.com",
            "First legacy",
            "first-storage-id",
        ),
        (
            1,
            "TWO",
            "Two",
            "two@example.com",
            "Second legacy",
            "second-storage-id",
        ),
    ] {
        let order = checkout(
            &db,
            &cipher,
            &event,
            &kind,
            email,
            &[name],
            &[(format!("question_{badge}_0"), value)],
            now + offset,
        )
        .await;
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE orders SET number=? WHERE id=?",
            [number.into(), order.clone().into()],
        ))
        .await
        .unwrap();
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE tickets SET code=?,created_at=? WHERE order_id=?",
            [
                format!("{number}-1").into(),
                (now + offset).into(),
                order.clone().into(),
            ],
        ))
        .await
        .unwrap();
        let row=db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"SELECT a.question_snapshot_json,o.hold_id FROM hold_answers a JOIN orders o ON o.hold_id=a.hold_id WHERE o.id=?",[order.into()])).await.unwrap().unwrap();
        let hold: String = row.try_get("", "hold_id").unwrap();
        let mut snapshot: questions::Question =
            serde_json::from_str(&row.try_get::<String>("", "question_snapshot_json").unwrap())
                .unwrap();
        snapshot.id = "historical-snapshot-id".to_owned();
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE hold_answers SET question_id=?,question_snapshot_json=? WHERE hold_id=?",
            [
                storage_id.into(),
                serde_json::to_string(&snapshot).unwrap().into(),
                hold.clone().into(),
            ],
        ))
        .await
        .unwrap();
        if number == "ONE" {
            for index in 0..251 {
                db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                    "INSERT INTO hold_answers (hold_id,question_id,ordinal,question_snapshot_json,answer_encrypted) SELECT hold_id,?,ordinal,question_snapshot_json,? FROM hold_answers WHERE hold_id=? AND question_id=?",
                    [format!("alias-{index:03}").into(), cipher.encrypt("question_answer", "Later duplicate").unwrap().into(), hold.clone().into(), storage_id.into()]
                )).await.unwrap();
            }
        }
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "DELETE FROM hold_questions WHERE hold_id=?",
            [hold.into()],
        ))
        .await
        .unwrap();
    }
    questions::delete(&db, &event, &badge).await.unwrap();
    assert_eq!(admin_orders::report(&db,&cipher,&event,Report::Tickets).await.unwrap(),"Order,Ticket code,Ticket Type,Seat,Attendee,Buyer email,Status,Checked in at,Badge\r\nONE,ONE-1,General,,One,one@example.com,Valid,,First legacy\r\nTWO,TWO-1,General,,Two,two@example.com,Valid,,Second legacy\r\n");
}

#[tokio::test]
async fn historical_header_first_occurrences_keep_ties_aliases_and_narrow_sorts() {
    use sea_orm::TransactionTrait;
    use std::sync::{Arc, Mutex};
    let (dir, mut db, cipher, event, kind) = setup().await;
    let _dir = if std::env::var_os("CRABGATE_TEST_EVIDENCE_DIR").is_some() {
        println!("Export header fixture: {}", dir.keep().display());
        None
    } else {
        Some(dir)
    };
    for (scope, kind, label, options, id) in [
        (
            "Order",
            "Choice",
            "Plan",
            vec!["A".into(), "B".into()],
            "q-Z",
        ),
        ("Order", "Text", "Name", vec![], "q-é"),
        ("Attendee", "Text", "Badge", vec![], "q-a"),
    ] {
        let created = questions::add(&db, &event, scope, kind, label, &options, false)
            .await
            .unwrap();
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE event_questions SET id=?,sort_order=0 WHERE id=?",
            [id.into(), created.into()],
        ))
        .await
        .unwrap();
    }
    let now = Utc::now().timestamp();
    events::publish(&db, &event, now).await.unwrap();
    let mut holds = Vec::new();
    for (number, id, ticket, email, name, plan) in [
        ("ONE", "Z-order", "ticket-Z", "one@example.com", "One", "A"),
        ("TWO", "é-order", "ticket-é", "two@example.com", "Two", "B"),
    ] {
        let order = checkout(
            &db,
            &cipher,
            &event,
            &kind,
            email,
            &[name],
            &[
                ("question_q-Z".into(), plan),
                ("question_q-é".into(), name),
                ("question_q-a_0".into(), name),
            ],
            now,
        )
        .await;
        let hold: String = db
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT hold_id FROM orders WHERE id=?",
                [order.clone().into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "hold_id")
            .unwrap();
        let transaction = db.begin().await.unwrap();
        transaction
            .execute_unprepared("PRAGMA defer_foreign_keys=ON")
            .await
            .unwrap();
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE orders SET id=?,number=? WHERE id=?",
                [id.into(), number.into(), order.clone().into()],
            ))
            .await
            .unwrap();
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE tickets SET order_id=?,id=?,code=? WHERE order_id=?",
                [
                    id.into(),
                    ticket.into(),
                    format!("{number}-1").into(),
                    order.into(),
                ],
            ))
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        for (question, label) in [
            ("q-Z", format!("Plan {number}")),
            ("q-é", format!("Name {number}")),
        ] {
            db.execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE hold_questions SET label=?,sort_order=0 WHERE hold_id=? AND question_id=?",
                [label.into(), hold.clone().into(), question.into()],
            ))
            .await
            .unwrap();
        }
        holds.push(hold);
    }
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE hold_questions SET options_json='[ \"A\", \"B\" ]' WHERE hold_id=? AND question_id='q-Z'", [holds[1].clone().into()]
    )).await.unwrap();
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE hold_questions SET options_json=' [] ' WHERE hold_id=? AND question_id='q-a'",
        [holds[1].clone().into()],
    ))
    .await
    .unwrap();
    for question in ["q-Z", "q-é", "q-a"] {
        questions::delete(&db, &event, question).await.unwrap();
    }
    questions::add(&db, &event, "Order", "Text", "Current", &[], false)
        .await
        .unwrap();
    let legacy = questions::Question {
        id: "legacy-snapshot".to_owned(),
        scope: "Order".to_owned(),
        kind: "Text".to_owned(),
        label: "Legacy".to_owned(),
        options: vec![],
        required: false,
    };
    let snapshot = serde_json::to_string(&legacy).unwrap();
    let pretty = serde_json::to_string_pretty(&legacy).unwrap();
    for (hold, ordinal, json, answer) in [
        (&holds[0], i64::MIN, &snapshot, "Old One"),
        (&holds[0], i64::MAX, &snapshot, "Unused maximum"),
        (&holds[0], -7, &pretty, "Later duplicate"),
        (&holds[1], -1, &snapshot, "Old Two"),
    ] {
        db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "INSERT INTO hold_answers (hold_id,question_id,ordinal,question_snapshot_json,answer_encrypted) VALUES (?,?,?,?,?)", [hold.clone().into(),"legacy-storage".into(),ordinal.into(),json.clone().into(),cipher.encrypt("question_answer",answer).unwrap().into()]
        )).await.unwrap();
    }
    let captured = Arc::new(Mutex::new(Vec::new()));
    let recorded = captured.clone();
    db.set_metric_callback(move |info| {
        assert!(!info.failed);
        recorded.lock().unwrap().push(info.statement.clone());
    });
    let expected_orders = format!("Order,Buyer email,Original cents,Discount code,Discount cents,Paid cents,Status,Complimentary,Created at,Current,Plan ONE,Name ONE,Plan TWO,Name TWO,Legacy,Plan,Name\r\nTWO,two@example.com,0,,0,0,Paid,No,{now},,,,,,Old Two,B,Two\r\nONE,one@example.com,0,,0,0,Paid,No,{now},,,,,,Old One,A,One\r\n");
    assert_eq!(
        admin_orders::report(&db, &cipher, &event, Report::Orders)
            .await
            .unwrap(),
        expected_orders
    );
    assert_eq!(admin_orders::report(&db,&cipher,&event,Report::Tickets).await.unwrap(), "Order,Ticket code,Ticket Type,Seat,Attendee,Buyer email,Status,Checked in at,Badge\r\nONE,ONE-1,General,,One,one@example.com,Valid,,One\r\nTWO,TWO-1,General,,Two,two@example.com,Valid,,Two\r\n");
    db.set_metric_callback(|_| {});
    let statements = captured.lock().unwrap().clone();
    let headers = statements
        .into_iter()
        .filter(|s| s.sql.contains("ROW_NUMBER()"));
    let mut count = 0;
    for statement in headers {
        let explanation = db
            .query_all_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                format!("EXPLAIN {}", statement.sql),
                statement.values.unwrap().0,
            ))
            .await
            .unwrap();
        let widths = explanation
            .iter()
            .filter(|row| row.try_get::<String>("", "opcode").unwrap() == "SorterOpen")
            .map(|row| row.try_get::<i64>("", "p2").unwrap())
            .collect::<Vec<_>>();
        assert!(!widths.is_empty());
        assert!(
            widths.iter().all(|width| *width <= 16),
            "Historical metadata sort records are too wide: {widths:?}"
        );
        count += 1;
    }
    assert_eq!(count, 4);
    db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE hold_answers SET question_snapshot_json='invalid JSON' WHERE hold_id=? AND question_id='legacy-storage' AND ordinal=-7", [holds[0].clone().into()]
    )).await.unwrap();
    for report in [Report::Orders, Report::Tickets] {
        assert!(matches!(
            admin_orders::report(&db, &cipher, &event, report).await,
            Err(admin_orders::AdminOrderError::Question(
                questions::QuestionError::Json(_)
            ))
        ));
    }
}
