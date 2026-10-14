use chrono::Utc;
use crabgate::{
    admin_orders,
    app::App,
    crypto::OrganizerCipher,
    door,
    events::{self, EventInput, TicketTypeInput},
    initializers::organizers::OrganizerRuntime,
    inventory::{self, Selection},
    members::{self, Role},
    orders::{self, PaymentOutcome},
};
use loco_rs::{app::Hooks, boot::StartMode, environment::Environment};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn smtp_capture() -> (
    u16,
    tokio::sync::mpsc::Receiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            write.write_all(b"220 loopback SMTP\r\n").await.unwrap();
            let mut data = false;
            let mut message = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap() == 0 {
                    break;
                }
                let response = if data {
                    if line != ".\r\n" {
                        message.push_str(&line);
                        continue;
                    }
                    sender.send(std::mem::take(&mut message)).await.unwrap();
                    data = false;
                    b"250 accepted\r\n".as_slice()
                } else if line.starts_with("DATA") {
                    data = true;
                    b"354 continue\r\n".as_slice()
                } else if line.starts_with("QUIT") {
                    write.write_all(b"221 bye\r\n").await.unwrap();
                    break;
                } else {
                    b"250 OK\r\n".as_slice()
                };
                write.write_all(response).await.unwrap();
            }
        }
    });
    (port, receiver, task)
}

async fn delivered_token(
    receiver: &mut tokio::sync::mpsc::Receiver<String>,
    prefix: &str,
) -> String {
    let message = tokio::time::timeout(std::time::Duration::from_secs(10), receiver.recv())
        .await
        .unwrap()
        .unwrap()
        .replace("=\r\n", "");
    let tail = message.split_once(prefix).expect("delivered sign-in URL").1;
    let token: String = tail
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    assert!(!token.is_empty());
    token
}

async fn event_with_ticket(
    db: &sea_orm::DatabaseConnection,
    cipher: &OrganizerCipher,
    title: &str,
    name: &str,
) -> (String, String, String) {
    let now = Utc::now();
    let date = (now + chrono::Duration::days(2)).date_naive().to_string();
    let now = now.timestamp();
    let event = events::create(
        db,
        &EventInput {
            title: title.to_owned(),
            date,
            start_time: "19:00".to_owned(),
            end_time: "21:00".to_owned(),
            timezone: "Europe/Dublin".to_owned(),
            venue: "Hall".to_owned(),
            address: "1 Main Street".to_owned(),
            description: String::new(),
            event_capacity: Some(10),
            max_per_order: 10,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let ticket_type = events::add_ticket_type(
        db,
        &event,
        &TicketTypeInput {
            name: "Standard".to_owned(),
            price: "0".to_owned(),
            capacity: 10,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(db, &event.id, now).await.unwrap();
    let hold = inventory::reserve(
        db,
        &event.id,
        &[Selection {
            ticket_type_id: ticket_type.id,
            quantity: 1,
        }],
        now,
    )
    .await
    .unwrap();
    let details = orders::hold_details(db, &hold.id).await.unwrap();
    orders::save_details(
        db,
        cipher,
        &details,
        "buyer@example.com",
        &[name.to_owned()],
        false,
        now,
    )
    .await
    .unwrap();
    let session_id = format!("free_{}", hold.id);
    orders::attach_session(db, &hold.id, &session_id, now)
        .await
        .unwrap();
    let order_id = match orders::complete_payment(db, &session_id, "", 0, now)
        .await
        .unwrap()
    {
        PaymentOutcome::Paid(id) => id,
        _ => panic!("Order not paid"),
    };
    let order = orders::get_order(db, cipher, &order_id).await.unwrap();
    (
        event.id,
        order.tickets[0].id.clone(),
        order.tickets[0].code.clone(),
    )
}

async fn sign_in(
    client: &reqwest::Client,
    origin: &str,
    email: &str,
    receiver: &mut tokio::sync::mpsc::Receiver<String>,
) -> String {
    let response = client
        .post(format!("{origin}/office/sign-in"))
        .form(&[("email", email)])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 303);
    let token = delivered_token(receiver, &format!("{origin}/office/magic-link/")).await;
    let response = client
        .get(format!("{origin}/office/magic-link/{token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 303);
    assert_eq!(response.headers()["location"], "/door");
    response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

async fn page(response: reqwest::Response, title: &str) -> String {
    assert_eq!(response.status().as_u16(), 200);
    let body = response.text().await.unwrap();
    assert!(body.contains(title), "Expected {title}: {body}");
    body
}

#[tokio::test]
#[serial_test::serial]
async fn public_door_forms_share_online_state_across_staff_sessions() {
    let root = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir);
    let storage = tempfile::Builder::new()
        .prefix("cg031-http-")
        .tempdir_in(root)
        .unwrap()
        .keep();
    println!("synthetic HTTP evidence: {}", storage.display());
    let variables = [
        ("CRABGATE_EDITION", "self-hosted".to_owned()),
        ("SHOP_SCHEME", "http".to_owned()),
        (
            "CRABGATE_ORGANIZER_DATABASE_DIR",
            storage.join("tenants").display().to_string(),
        ),
    ];
    let prior: Vec<_> = variables
        .iter()
        .map(|(key, _)| (*key, std::env::var_os(key)))
        .collect();
    for (key, value) in variables {
        std::env::set_var(key, value);
    }
    let (port, mut receiver, smtp) = smtp_capture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let mut config = App::load_config(&Environment::Test).await.unwrap();
    config.server.host = "http://127.0.0.1".to_owned();
    config.server.port = listener.local_addr().unwrap().port().into();
    config.database.uri = format!("sqlite://{}?mode=rwc", storage.join("control.db").display());
    let mailer = config.mailer.as_mut().unwrap();
    mailer.stub = false;
    let smtp_config = mailer.smtp.as_mut().unwrap();
    smtp_config.host = "127.0.0.1".into();
    smtp_config.port = port;
    smtp_config.secure = false;
    smtp_config.auth = None;
    let boot = App::boot(StartMode::ServerOnly, &Environment::Test, config)
        .await
        .unwrap();
    let ctx = boot.app_context;
    let runtime = ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
    let registry = runtime.registry();
    let scope = registry.resolve("localhost").await.unwrap();
    let cipher = registry.cipher(&scope.organizer).unwrap();
    for email in ["front@example.test", "side@example.test"] {
        members::add_member(&ctx.db, &scope.organizer.id, email, Role::DoorStaff)
            .await
            .unwrap();
    }
    let (event, ticket, code) =
        event_with_ticket(&scope.db, &cipher, "CG031 Main", "First Attendee").await;
    let (_, _, other_code) =
        event_with_ticket(&scope.db, &cipher, "CG031 Other", "Other Attendee").await;
    let order_id: String = scope
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT order_id FROM tickets WHERE id = ?",
            [ticket.clone().into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "order_id")
        .unwrap();
    let server =
        tokio::spawn(async move { axum::serve(listener, boot.router.unwrap()).await.unwrap() });
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();
    let path = format!("/door/{event}");
    assert_eq!(
        client
            .get(format!("{origin}{path}"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        403
    );
    let first = sign_in(&client, &origin, "front@example.test", &mut receiver).await;
    let second = sign_in(&client, &origin, "side@example.test", &mut receiver).await;
    assert_ne!(first, second);
    let get = |path: &str, cookie: &str| {
        client
            .get(format!("{origin}{path}"))
            .header("cookie", cookie)
    };
    let post = |path: &str, cookie: &str| {
        client
            .post(format!("{origin}{path}"))
            .header("cookie", cookie)
            .header("origin", &origin)
    };
    for cookie in [&first, &second] {
        page(get("/door", cookie).send().await.unwrap(), "CG031 Main").await;
        let scan = page(
            get(&path, cookie).send().await.unwrap(),
            "No camera? Type the code",
        )
        .await;
        assert!(scan.contains("playsinline"));
        assert!(scan.contains("/static/shared/jsQR.js"));
        assert!(scan.contains("/static/shared/door-camera.js"));
        assert_eq!(
            get("/office/events", cookie)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            403
        );
    }
    for asset in ["jsQR.js", "door-camera.js"] {
        let response = client
            .get(format!("{origin}/static/shared/{asset}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert!(!response.bytes().await.unwrap().is_empty());
    }
    let check = format!("{path}/check");
    page(
        post(&check, &first)
            .form(&[("code", "does-not-exist")])
            .send()
            .await
            .unwrap(),
        "Unknown code",
    )
    .await;
    page(
        post(&check, &first)
            .form(&[("code", &other_code)])
            .send()
            .await
            .unwrap(),
        "Wrong Event",
    )
    .await;
    let normalized = format!("  {}  ", code.to_lowercase());
    let (a, b) = tokio::join!(
        post(&check, &first).form(&[("code", &normalized)]).send(),
        post(&check, &second).form(&[("code", &code)]).send(),
    );
    let a = page(a.unwrap(), "First Attendee").await;
    let b = page(b.unwrap(), "First Attendee").await;
    let titles: Vec<_> = [&a, &b]
        .iter()
        .map(|body| {
            if body.contains("<p class=\"big\">Checked in</p>") {
                "checked-in"
            } else if body.contains("<p class=\"big\">Already checked in</p>") {
                "already"
            } else {
                panic!("Missing admission outcome: {body}")
            }
        })
        .collect();
    assert_eq!(titles.iter().filter(|&&t| t == "checked-in").count(), 1);
    assert_eq!(titles.iter().filter(|&&t| t == "already").count(), 1);
    for cookie in [&first, &second] {
        page(
            get(&path, cookie).send().await.unwrap(),
            "1 of 1 tickets checked in",
        )
        .await;
    }
    page(
        post(&check, &second)
            .form(&[("code", &code)])
            .send()
            .await
            .unwrap(),
        "Already checked in",
    )
    .await;
    let count: i64 = scope
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS n FROM checkins WHERE ticket_id = ? AND undone_at IS NULL",
            [ticket.clone().into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "n")
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(
        post(&format!("{path}/undo/{ticket}"), &second)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        303
    );
    admin_orders::refund(&scope.db, &order_id, None, Utc::now().timestamp())
        .await
        .unwrap();
    page(
        post(&check, &first)
            .form(&[("code", &code)])
            .send()
            .await
            .unwrap(),
        "Void Ticket",
    )
    .await;
    page(
        get(&format!("{path}/lookup?q=First"), &second)
            .send()
            .await
            .unwrap(),
        "Cannot admit",
    )
    .await;
    assert_eq!(door::count(&scope.db, &event).await.unwrap(), (0, 0));
    println!("PUBLIC HTTP PASS: 2 staff sessions; valid/duplicate/refunded/wrong-event/unknown/manual form; exactly 1 concurrent admission; shared counts; undo; lookup; static assets; unauthorized access");
    server.abort();
    smtp.abort();
    let _ = server.await;
    let _ = smtp.await;
    drop(scope);
    runtime.shutdown().await.unwrap();
    for (key, value) in prior {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}
