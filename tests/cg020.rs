use crabgate::{
    admin_orders,
    app::App,
    buyer_messages,
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, Selection},
    members::{self, Role},
    operator, orders,
};
use loco_rs::{app::AppContext, testing::prelude::*};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

async fn session(ctx: &AppContext, organizer_id: &str, email: &str, role: Role) -> String {
    let now = orders::now();
    members::add_member(&ctx.db, organizer_id, email, role)
        .await
        .unwrap();
    let token = members::request_magic_link(&ctx.db, organizer_id, email, now)
        .await
        .unwrap()
        .unwrap();
    let session = members::claim_magic_link(&ctx.db, organizer_id, &token, now)
        .await
        .unwrap()
        .unwrap();
    format!("cg_session={session}")
}

async fn smtp(
    ctx: &AppContext,
    mode: Arc<AtomicUsize>,
    data: Arc<AtomicUsize>,
) -> (AppContext, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let job = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mode = mode.clone();
            let data = data.clone();
            connections.spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                if write
                    .write_all(b"220 localhost test SMTP\r\n")
                    .await
                    .is_err()
                {
                    return;
                }
                while let Ok(Some(line)) = lines.next_line().await {
                    let response = if line.starts_with("EHLO") {
                        "250 localhost\r\n"
                    } else if line.starts_with("MAIL FROM") && mode.load(Ordering::SeqCst) == 1 {
                        "550 rejected\r\n"
                    } else if line == "DATA" {
                        if write.write_all(b"354 send data\r\n").await.is_err() {
                            return;
                        }
                        while let Ok(Some(body)) = lines.next_line().await {
                            if body == "." {
                                break;
                            }
                        }
                        data.fetch_add(1, Ordering::SeqCst);
                        if mode.load(Ordering::SeqCst) == 2 {
                            return;
                        }
                        while mode.load(Ordering::SeqCst) == 3 {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        "250 accepted\r\n"
                    } else if line == "QUIT" {
                        let _ = write.write_all(b"221 bye\r\n").await;
                        return;
                    } else {
                        "250 ok\r\n"
                    };
                    if write.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    let mut config = ctx.config.mailer.clone().unwrap().smtp.unwrap();
    config.host = "127.0.0.1".into();
    config.port = port;
    config.secure = false;
    config.auth = None;
    let mut context = ctx.clone();
    context.mailer = Some(loco_rs::mailer::EmailSender::smtp(&config).unwrap());
    (context, job)
}

async fn states(db: &sea_orm::DatabaseConnection, id: &str) -> Vec<String> {
    db.query_all_raw(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT state FROM buyer_message_recipients WHERE message_id = ? ORDER BY state",
        [id.into()],
    ))
    .await
    .unwrap()
    .into_iter()
    .map(|row| row.try_get("", "state").unwrap())
    .collect()
}

#[tokio::test]
#[serial]
async fn buyer_messages_http_and_smtp_flow() {
    request::<App, _, _>(|request, ctx| async move {
    let runtime = ctx.shared_store.get::<Arc<crabgate::initializers::organizers::OrganizerRuntime>>().unwrap();
    let registry = runtime.registry(); let scope = registry.resolve("localhost").await.unwrap();
    let cipher = registry.cipher(&scope.organizer).unwrap(); let now = orders::now();
    let cookie = session(&ctx, &scope.organizer.id, "cg020-admin@example.com", Role::Admin).await;
    let door_cookie = session(&ctx, &scope.organizer.id, "cg020-door@example.com", Role::DoorStaff).await;
    let event = events::create(&scope.db, &EventInput { title: "Arrival information".into(), date: "2027-06-15".into(), start_time: "19:00".into(), end_time: "21:00".into(), timezone: "Europe/Dublin".into(), venue: "Hall".into(), address: "Main Street".into(), description: String::new(), event_capacity: Some(30), max_per_order: 4, attendee_name_required: false }).await.unwrap();
    let kind = events::add_ticket_type(&scope.db, &event, &TicketTypeInput { name: "Free place".into(), price: "0".into(), capacity: 30, sales_open: None, sales_close: None }).await.unwrap();
    events::publish(&scope.db, &event.id, now).await.unwrap();
    let path = format!("/office/events/{}/messages", event.id);
    assert_eq!(request.get(&path).await.status_code(), 303);
    let empty = request.get(&path).add_header("cookie", &cookie).await;
    assert_eq!(empty.status_code(), 200, "{}", empty.text()); assert!(empty.text().contains("No eligible Buyers")); assert!(empty.text().contains("0 eligible Buyer email addresses now"));
    assert_eq!(request.get(&path).add_header("cookie", &door_cookie).await.status_code(), 403);
    assert_eq!(request.get("/office/events/missing/messages").add_header("cookie", &cookie).await.status_code(), 404);
    let first = admin_orders::complimentary(&scope.db, &cipher, &event.id, &kind.id, "Buyer@Example.com", "One", now).await.unwrap();
    admin_orders::complimentary(&scope.db, &cipher, &event.id, &kind.id, "buyer@example.com", "Two", now).await.unwrap();
    let hold = inventory::reserve(&scope.db, &event.id, &[Selection{ ticket_type_id: kind.id.clone(), quantity: 2 }], now).await.unwrap();
    let details=orders::hold_details(&scope.db, &hold.id).await.unwrap();
    orders::save_details(&scope.db, &cipher, &details, "free@example.com", &["Free one".into(), "Free two".into()], false, now).await.unwrap();
    orders::attach_session(&scope.db, &hold.id, "cg020-free-session", now).await.unwrap();
    let free_id=match orders::complete_payment(&scope.db, "cg020-free-session", "", 0, now).await.unwrap() { orders::PaymentOutcome::Paid(id)=>id, _=>panic!("expected a free Order") };
    scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE orders SET status = 'RefundPending' WHERE id = ?", [free_id.clone().into()])).await.unwrap();
    scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE tickets SET status = 'Void' WHERE id = (SELECT id FROM tickets WHERE order_id = ? LIMIT 1)", [free_id.into()])).await.unwrap();
    let refunded = admin_orders::complimentary(&scope.db, &cipher, &event.id, &kind.id, "refund@example.com", "Refund", now).await.unwrap();
    admin_orders::mark_refunded(&scope.db, &refunded, now).await.unwrap();
    let void = admin_orders::complimentary(&scope.db, &cipher, &event.id, &kind.id, "void@example.com", "Void", now).await.unwrap();
    scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE tickets SET status = 'Void' WHERE order_id = ?", [void.into()])).await.unwrap();
    let page = request.get(&path).add_header("cookie", &cookie).await;
    assert!(page.text().contains("2 eligible Buyer email addresses now")); assert!(!page.text().contains("buyer@example.com"));
    let invalid = request.post(&format!("{path}/preview")).add_header("cookie", &cookie).form(&serde_json::json!({"subject":"Oops\nBcc: other@example.com", "body":"Hello"})).await;
    assert_eq!(invalid.status_code(), 422); assert!(invalid.text().contains("without line breaks"));
    let preview = request.post(&format!("{path}/preview")).add_header("cookie", &cookie).form(&serde_json::json!({"subject":"Arrival <notes>", "body":"Use <script>alert(1)</script> & bring ID."})).await;
    assert_eq!(preview.status_code(), 303);
    let drafts = buyer_messages::messages(&scope.db, &cipher, &event.id).await.unwrap();
    assert_eq!(drafts.len(), 1); assert_eq!(drafts[0].recipient_count, 2); let id = drafts[0].id.clone(); let mut send_snapshot = drafts[0].snapshot.clone();
    let page = request.get(&path).add_header("cookie", &cookie).await;
    assert!(page.text().contains("Arrival &lt;notes&gt;")); assert!(page.text().contains("&lt;script&gt;")); assert!(!page.text().contains("<script>alert")); assert!(page.text().contains("intend to send it to 2"));
    let send = format!("{path}/{id}/send"); let retry = format!("{path}/{id}/retry");
    assert_eq!(request.get(&send).add_header("cookie", &cookie).await.status_code(), 405);
    assert_eq!(request.post(&send).add_header("cookie", &cookie).form(&serde_json::json!({})).await.status_code(), 422);
    assert_eq!(request.post(&send).add_header("cookie", &door_cookie).form(&serde_json::json!({"confirm_send":"yes", "snapshot": &send_snapshot})).await.status_code(), 403);
    assert_eq!(request.post(&format!("/office/events/missing/messages/{id}/send")).add_header("cookie", &cookie).form(&serde_json::json!({"confirm_send":"yes", "snapshot": &send_snapshot})).await.status_code(), 404);
    let changed = admin_orders::complimentary(&scope.db, &cipher, &event.id, &kind.id, "new@example.com", "New", now).await.unwrap();
    assert_eq!(request.post(&send).add_header("cookie", &cookie).form(&serde_json::json!({"confirm_send":"yes", "snapshot": &send_snapshot})).await.status_code(), 409);
    assert_eq!(states(&scope.db, &id).await, Vec::<String>::new());
    assert_eq!(buyer_messages::messages(&scope.db, &cipher, &event.id).await.unwrap()[0].recipient_count, 3); send_snapshot=buyer_messages::messages(&scope.db, &cipher, &event.id).await.unwrap()[0].snapshot.clone();
    assert_eq!(request.post(&send).add_header("cookie", &cookie).form(&serde_json::json!({"confirm_send":"yes", "snapshot": &send_snapshot})).await.status_code(), 303);
    assert_eq!(request.post(&send).add_header("cookie", &cookie).form(&serde_json::json!({"confirm_send":"yes", "snapshot": &send_snapshot})).await.status_code(), 303);
    assert_eq!(states(&scope.db, &id).await, vec!["Pending", "Pending", "Pending"]);
    admin_orders::mark_refunded(&scope.db, &changed, now).await.unwrap();
    let mode = Arc::new(AtomicUsize::new(1)); let data = Arc::new(AtomicUsize::new(0));
    let (mail_ctx, smtp_job) = smtp(&ctx, mode.clone(), data.clone()).await;
    buyer_messages::deliver_pending(&mail_ctx, &registry, &scope.organizer, &scope.db).await.unwrap();
    assert_eq!(states(&scope.db, &id).await, vec!["Failed", "Failed", "Skipped"]); assert_eq!(data.load(Ordering::SeqCst), 0);
    buyer_messages::deliver_pending(&mail_ctx, &registry, &scope.organizer, &scope.db).await.unwrap();
    assert_eq!(states(&scope.db, &id).await, vec!["Failed", "Failed", "Skipped"]);
    let page = request.get(&path).add_header("cookie", &cookie).await;
    assert!(page.text().contains("Retry 2 failed messages"));
    assert_eq!(request.post(&retry).add_header("cookie", &cookie).form(&serde_json::json!({})).await.status_code(), 303);
    mode.store(3, Ordering::SeqCst);
    let runner = { let db=scope.db.clone(); let registry=registry.clone(); let organizer=scope.organizer.clone(); let context=mail_ctx.clone(); tokio::spawn(async move { buyer_messages::deliver_pending(&context, &registry, &organizer, &db).await.unwrap(); }) };
    tokio::time::timeout(Duration::from_secs(10), async {while data.load(Ordering::SeqCst) == 0 {tokio::time::sleep(Duration::from_millis(10)).await;}}).await.unwrap();
    assert_eq!(states(&scope.db, &id).await, vec!["Pending", "Sending", "Skipped"]);
    let concurrent = { let db=scope.db.clone(); let registry=registry.clone(); let organizer=scope.organizer.clone(); let context=mail_ctx.clone(); tokio::spawn(async move { buyer_messages::deliver_pending(&context, &registry, &organizer, &db).await.unwrap(); }) };
    tokio::time::timeout(Duration::from_secs(10), async {while data.load(Ordering::SeqCst) < 2 {tokio::time::sleep(Duration::from_millis(10)).await;}}).await.unwrap();
    assert_eq!(states(&scope.db, &id).await, vec!["Sending", "Sending", "Skipped"]);
    mode.store(0, Ordering::SeqCst); runner.await.unwrap(); concurrent.await.unwrap();
    assert_eq!(data.load(Ordering::SeqCst), 2); assert_eq!(states(&scope.db, &id).await, vec!["Accepted", "Accepted", "Skipped"]);
    let reopened=sea_orm::Database::connect(format!("sqlite://{}?mode=rw",scope.organizer.db_path)).await.unwrap();
    assert_eq!(states(&reopened,&id).await, vec!["Accepted", "Accepted", "Skipped"]); reopened.close().await.unwrap();
    assert_eq!(request.post(&retry).add_header("cookie", &cookie).form(&serde_json::json!({"retry_uncertain":"yes"})).await.status_code(), 303);
    buyer_messages::deliver_pending(&mail_ctx, &registry, &scope.organizer, &scope.db).await.unwrap(); assert_eq!(data.load(Ordering::SeqCst), 2);
    let uncertain = buyer_messages::preview(&scope.db, &cipher, &event.id, "Lost acknowledgement", "Doors open at six", now).await.unwrap();
    queue_preview(&scope.db, &cipher, &event.id, &uncertain, now).await.unwrap(); mode.store(2, Ordering::SeqCst);
    buyer_messages::deliver_pending(&mail_ctx, &registry, &scope.organizer, &scope.db).await.unwrap();
    assert_eq!(states(&scope.db, &uncertain).await, vec!["Uncertain", "Uncertain"]); assert_eq!(data.load(Ordering::SeqCst), 4);
    buyer_messages::deliver_pending(&mail_ctx, &registry, &scope.organizer, &scope.db).await.unwrap(); assert_eq!(data.load(Ordering::SeqCst), 4);
    let uncertain_retry = format!("{path}/{uncertain}/retry");
    request.post(&uncertain_retry).add_header("cookie", &cookie).form(&serde_json::json!({})).await;
    assert_eq!(states(&scope.db, &uncertain).await, vec!["Uncertain", "Uncertain"]);
    assert!(request.get(&path).add_header("cookie", &cookie).await.text().contains("can send duplicates"));
    assert_eq!(request.post(&uncertain_retry).add_header("cookie", &cookie).form(&serde_json::json!({"retry_uncertain":"yes"})).await.status_code(), 303);
    mode.store(0, Ordering::SeqCst); buyer_messages::deliver_pending(&mail_ctx, &registry, &scope.organizer, &scope.db).await.unwrap();
    assert_eq!(states(&scope.db, &uncertain).await, vec!["Accepted", "Accepted"]); assert_eq!(data.load(Ordering::SeqCst), 6);
    let mail = crabgate::mailers::buyer_message::prepare("buyer@example.com", "Arrival", "<script>&\"'", None);
    assert_eq!(mail.text, "<script>&\"'"); assert!(mail.html.contains("&lt;script&gt;&amp;&quot;&#39;")); assert!(!mail.html.contains("<script>")); assert_eq!(mail.cc, None); assert_eq!(mail.bcc, None);
    let stored = scope.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "SELECT content_encrypted FROM buyer_messages WHERE id = ?", [id.clone().into()])).await.unwrap().unwrap().try_get::<String>("", "content_encrypted").unwrap();
    assert!(!stored.contains("buyer@example.com")); assert!(!stored.contains("Arrival"));
    assert!(cipher.decrypt("buyer_message_email", &stored).is_err());
    let interrupted = buyer_messages::preview(&scope.db, &cipher, &event.id, "Interrupted", "A note", now).await.unwrap(); queue_preview(&scope.db, &cipher, &event.id, &interrupted, now).await.unwrap();
    scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE buyer_message_recipients SET state = 'Sending', claim_token = 'old', claimed_at = ? WHERE message_id = ?", [(now-301).into(), interrupted.clone().into()])).await.unwrap();
    buyer_messages::recover(&scope.db, now).await.unwrap(); assert_eq!(states(&scope.db, &interrupted).await, vec!["Uncertain", "Uncertain"]);
    let refreshed = request.get(&path).add_header("cookie", &cookie).await; assert_eq!(refreshed.status_code(), 200); assert!(refreshed.text().contains("Accepted by SMTP")); assert!(refreshed.text().contains("does not confirm inbox delivery"));
    ctx.db.execute_unprepared("INSERT INTO operators (id, email) VALUES ('cg020-support', 'cg020-support@example.com')").await.unwrap();
    let token = operator::request_link(&ctx.db, "cg020-support@example.com", now).await.unwrap().unwrap(); let support = operator::claim_link(&ctx.db, &token, now).await.unwrap().unwrap();
    let member_id=members::add_member(&ctx.db, &scope.organizer.id, "cg020-admin@example.com", Role::Admin).await.unwrap();
    operator::grant(&ctx.db, &scope.organizer.id, &member_id, 1, now).await.unwrap(); let support_cookie=format!("cg_operator_session={support}");
    let support_page=request.get(&path).add_header("cookie", &support_cookie).await; assert_eq!(support_page.status_code(), 200); assert!(!support_page.text().contains("name=\"confirm_send\""));
    for write in [send.as_str(), retry.as_str(), &format!("{path}/preview")] { assert_eq!(request.post(write).add_header("cookie", &support_cookie).form(&serde_json::json!({"confirm_send":"yes", "subject":"Test", "body":"Test"})).await.status_code(), 403); }
    assert_eq!(request.get(&retry).add_header("cookie", &support_cookie).await.status_code(), 405);
    let current = buyer_messages::recipients(&*scope.db, &cipher, &event.id).await.unwrap(); assert_eq!(current.into_iter().collect::<Vec<_>>(), vec!["buyer@example.com", "free@example.com"]);
    assert_eq!(admin_orders::order(&scope.db, &cipher, &first).await.unwrap().status, "Paid");
    scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE events SET status = 'Cancelled' WHERE id = ?", [event.id.clone().into()])).await.unwrap();
    assert_eq!(buyer_messages::recipients(&*scope.db, &cipher, &event.id).await.unwrap().len(), 0);
    let cancelled=buyer_messages::preview(&scope.db, &cipher, &event.id, "No send", "No send", now).await.unwrap(); assert!(matches!(queue_preview(&scope.db, &cipher, &event.id, &cancelled, now).await, Err(buyer_messages::MessageError::Empty)));
    smtp_job.abort(); let _=smtp_job.await; drop(scope); runtime.shutdown().await.unwrap();
 }).await;
}

struct EditionGuard(Option<String>);
impl Drop for EditionGuard {
    fn drop(&mut self) {
        match &self.0 {
            Some(value) => std::env::set_var("CRABGATE_EDITION", value),
            None => std::env::remove_var("CRABGATE_EDITION"),
        }
    }
}

#[tokio::test]
#[serial]
async fn buyer_messages_tenant_isolation_and_current_email() {
    let _edition = EditionGuard(std::env::var("CRABGATE_EDITION").ok());
    std::env::set_var("CRABGATE_EDITION", "hosted");
    request::<App, _, _>(|request, ctx| async move {
        let runtime = ctx.shared_store.get::<Arc<crabgate::initializers::organizers::OrganizerRuntime>>().unwrap();
        let registry = runtime.registry(); let one=registry.create("Message One", "cg020-one").await.unwrap(); let two=registry.create("Message Two", "cg020-two").await.unwrap();
        let scope=registry.resolve_organizer(&one).await.unwrap(); let other=registry.resolve_organizer(&two).await.unwrap();
        let cipher=registry.cipher(&one).unwrap(); let other_cipher=registry.cipher(&two).unwrap(); let now=orders::now();
        let cookie=session(&ctx, &one.id, "cg020-one-admin@example.com", Role::Admin).await; let other_cookie=session(&ctx, &two.id, "cg020-two-admin@example.com", Role::Admin).await;
        let event=events::create(&scope.db, &EventInput { title:"Tenant One instructions".into(), date:"2027-06-15".into(), start_time:"19:00".into(), end_time:"21:00".into(), timezone:"Europe/Dublin".into(), venue:"Hall".into(), address:"Street".into(), description:String::new(), event_capacity:Some(10), max_per_order:4, attendee_name_required:false }).await.unwrap();
        let kind=events::add_ticket_type(&scope.db,&event,&TicketTypeInput{name:"Place".into(),price:"0".into(),capacity:10,sales_open:None,sales_close:None}).await.unwrap(); events::publish(&scope.db,&event.id,now).await.unwrap();
        let order=admin_orders::complimentary(&scope.db,&cipher,&event.id,&kind.id,"original@example.com","Guest",now).await.unwrap();
        let path=format!("/office/events/{}/messages",event.id); let host="cg020-one.crabgate.com"; let other_host="cg020-two.crabgate.com";
        assert_eq!(request.get(&path).add_header("host",host).add_header("cookie",&cookie).await.status_code(),200);
        assert_eq!(request.get(&path).add_header("host",other_host).add_header("cookie",&cookie).await.status_code(),303);
        assert_eq!(request.get(&path).add_header("host",other_host).add_header("cookie",&other_cookie).await.status_code(),404);
        let preview=request.post(&format!("{path}/preview")).add_header("host",host).add_header("origin",&format!("https://{host}")).add_header("cookie",&cookie).form(&serde_json::json!({"subject":"Current email", "body":"Arrival at six"})).await;
        assert_eq!(preview.status_code(),303); let saved=buyer_messages::messages(&scope.db,&cipher,&event.id).await.unwrap(); let id=saved[0].id.clone(); let mut send_snapshot=saved[0].snapshot.clone();
        let send=format!("{path}/{id}/send");
        assert_eq!(request.post(&send).add_header("host",host).add_header("origin","https://evil.example").add_header("cookie",&cookie).form(&serde_json::json!({"confirm_send":"yes", "snapshot": &send_snapshot})).await.status_code(),403);
        for write in [send.clone(),format!("{path}/{id}/retry"),format!("{path}/preview")] {
            assert_eq!(request.post(&write).add_header("host",other_host).add_header("origin",&format!("https://{other_host}")).add_header("cookie",&other_cookie).form(&serde_json::json!({"confirm_send":"yes", "subject":"Other", "body":"Other"})).await.status_code(),404);
        }
        let updated=cipher.encrypt("buyer_email","current@example.com").unwrap(); scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"UPDATE orders SET buyer_email_encrypted = ? WHERE id = ?",[updated.into(),order.clone().into()])).await.unwrap();
        let changed=request.post(&send).add_header("host",host).add_header("origin",&format!("https://{host}")).add_header("cookie",&cookie).form(&serde_json::json!({"confirm_send":"yes", "snapshot": &send_snapshot})).await;
        assert_eq!(changed.status_code(),409); assert!(changed.text().contains("Recipients changed")); assert!(states(&scope.db,&id).await.is_empty());
        send_snapshot=buyer_messages::messages(&scope.db,&cipher,&event.id).await.unwrap()[0].snapshot.clone(); let send_again=request.post(&send).add_header("host",host).add_header("origin",&format!("https://{host}")).add_header("cookie",&cookie).form(&serde_json::json!({"confirm_send":"yes", "snapshot": &send_snapshot})).await; assert_eq!(send_again.status_code(),303);
        buyer_messages::deliver_pending(&ctx,&registry,&one,&scope.db).await.unwrap(); assert_eq!(states(&scope.db,&id).await,vec!["Accepted"]);
        let deliveries=ctx.mailer.as_ref().unwrap().deliveries(); assert_eq!(deliveries.count,1); assert!(deliveries.messages[0].contains("To: current@example.com")); assert!(!deliveries.messages[0].contains("original@example.com"));
        let another=buyer_messages::preview(&scope.db,&cipher,&event.id,"Changed after queue","A note",now).await.unwrap(); queue_preview(&scope.db,&cipher,&event.id,&another,now).await.unwrap();
        let erased=cipher.encrypt("buyer_email","").unwrap(); scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"UPDATE orders SET buyer_email_encrypted = ? WHERE id = ?",[erased.into(),order.into()])).await.unwrap();
        buyer_messages::deliver_pending(&ctx,&registry,&one,&scope.db).await.unwrap(); assert_eq!(states(&scope.db,&another).await,vec!["Skipped"]); assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count,1);
        let stored=scope.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,"SELECT content_encrypted FROM buyer_messages WHERE id = ?",[id.into()])).await.unwrap().unwrap().try_get::<String>("","content_encrypted").unwrap(); assert!(other_cipher.decrypt("buyer_message",&stored).is_err());
        for email in ["user@example.com", "user+tag@example.com", "u.ser@example.com"] { admin_orders::complimentary(&scope.db,&cipher,&event.id,&kind.id,email,"Guest",now).await.unwrap(); }
        assert_eq!(buyer_messages::recipients(&*scope.db,&cipher,&event.id).await.unwrap().into_iter().collect::<Vec<_>>(),vec!["u.ser@example.com", "user+tag@example.com", "user@example.com"]);
        let count=other.db.query_one_raw(Statement::from_string(DatabaseBackend::Sqlite,"SELECT COUNT(*) AS count FROM buyer_messages")).await.unwrap().unwrap().try_get::<i64>("","count").unwrap(); assert_eq!(count,0);
        drop(scope); drop(other); runtime.shutdown().await.unwrap();
    }).await;
}

async fn correction_event(db: &sea_orm::DatabaseConnection) -> (String, String) {
    let event = events::create(
        db,
        &EventInput {
            title: "Correction instructions".into(),
            date: "2027-06-15".into(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Hall".into(),
            address: "Street".into(),
            description: String::new(),
            event_capacity: Some(20),
            max_per_order: 4,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let kind = events::add_ticket_type(
        db,
        &event,
        &TicketTypeInput {
            name: "Place".into(),
            price: "0".into(),
            capacity: 20,
            sales_open: None,
            sales_close: None,
        },
    )
    .await
    .unwrap();
    events::publish(db, &event.id, orders::now()).await.unwrap();
    (event.id, kind.id)
}

fn confirmation(html: &str, id: &str) -> serde_json::Value {
    let article = html.split(&format!("id=\"message-{id}\"")).nth(1).unwrap();
    let snapshot = article
        .split("name=\"snapshot\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert_eq!(snapshot.len(), 64);
    serde_json::json!({"confirm_send": "yes", "snapshot": snapshot})
}

async fn queue_preview(
    db: &sea_orm::DatabaseConnection,
    cipher: &crabgate::crypto::OrganizerCipher,
    event: &str,
    id: &str,
    now: i64,
) -> Result<(), buyer_messages::MessageError> {
    let message = buyer_messages::messages(db, cipher, event)
        .await
        .unwrap()
        .into_iter()
        .find(|message| message.id == id)
        .unwrap();
    buyer_messages::queue(db, cipher, event, id, &message.snapshot, now).await
}

async fn address_copies(
    db: &sea_orm::DatabaseConnection,
    cipher: &crabgate::crypto::OrganizerCipher,
    id: &str,
) -> (serde_json::Value, Vec<String>) {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT content_encrypted FROM buyer_messages WHERE id = ?",
            [id.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    let content = cipher
        .decrypt(
            "buyer_message",
            &row.try_get::<String>("", "content_encrypted").unwrap(),
        )
        .unwrap();
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT email_encrypted FROM buyer_message_recipients WHERE message_id = ? ORDER BY id",
            [id.into()],
        ))
        .await
        .unwrap();
    let emails = rows
        .into_iter()
        .map(|row| {
            cipher
                .decrypt(
                    "buyer_message_email",
                    &row.try_get::<String>("", "email_encrypted").unwrap(),
                )
                .unwrap()
        })
        .collect();
    (serde_json::from_str(&content).unwrap(), emails)
}

#[tokio::test]
async fn buyer_message_transfer_roundtrip_preserves_literal_content_and_tickets() {
    let dir = tempfile::tempdir().unwrap().keep();
    let old_path = dir.join("old.db");
    let new_path = dir.join("new.db");
    let export = dir.join("export.db");
    let old_db = sea_orm::Database::connect(format!("sqlite://{}?mode=rwc", old_path.display()))
        .await
        .unwrap();
    loco_rs::db::migrate::<migration::OrganizerMigrator>(&old_db)
        .await
        .unwrap();
    let master =
        crabgate::crypto::MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .unwrap();
    let old = master.generate_organizer_key("old").unwrap().1;
    let new = master.generate_organizer_key("new").unwrap().1;
    let (event, kind) = correction_event(&old_db).await;
    let order_id = admin_orders::complimentary(
        &old_db,
        &old,
        &event,
        &kind,
        "transfer@example.com",
        "Literal Guest",
        orders::now(),
    )
    .await
    .unwrap();
    let original = admin_orders::order(&old_db, &old, &order_id).await.unwrap();
    let id = buyer_messages::preview(
        &old_db,
        &old,
        &event,
        "Arrival <six>",
        "Come at six.\nBring <ID> & ticket.",
        orders::now(),
    )
    .await
    .unwrap();
    queue_preview(&old_db, &old, &event, &id, orders::now())
        .await
        .unwrap();
    std::fs::write(
        &export,
        crabgate::data_transfer::export(&old_db, &old)
            .await
            .unwrap(),
    )
    .unwrap();
    let target = sea_orm::Database::connect(format!("sqlite://{}?mode=rwc", new_path.display()))
        .await
        .unwrap();
    loco_rs::db::migrate::<migration::OrganizerMigrator>(&target)
        .await
        .unwrap();
    target.close().await.unwrap();
    crabgate::data_transfer::import_fresh(&export, &new_path, &new)
        .await
        .unwrap();
    let imported = sea_orm::Database::connect(format!("sqlite://{}?mode=rw", new_path.display()))
        .await
        .unwrap();
    let history = buyer_messages::messages(&imported, &new, &event)
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].subject, "Arrival <six>");
    assert_eq!(history[0].body, "Come at six.\nBring <ID> & ticket.");
    assert_eq!(history[0].state, "Queued");
    assert_eq!(history[0].pending, 1);
    let (content, recipients) = address_copies(&imported, &new, &id).await;
    assert_eq!(
        content["recipients"],
        serde_json::json!(["transfer@example.com"])
    );
    assert_eq!(recipients, vec!["transfer@example.com"]);
    let order = admin_orders::order(&imported, &new, &order_id)
        .await
        .unwrap();
    assert_eq!(order.number, original.number);
    assert_eq!(order.buyer_email, "transfer@example.com");
    assert_eq!(order.status, "Paid");
    assert_eq!(order.tickets.len(), 1);
    assert_eq!(order.tickets[0].code, original.tickets[0].code);
    assert_eq!(order.tickets[0].status, "Valid");
    assert_eq!(
        order.tickets[0].attendee_name.as_deref(),
        Some("Literal Guest")
    );
    imported.close().await.unwrap();
    old_db.close().await.unwrap();
    println!("CG020 transfer database evidence: {}", dir.display());
}

#[tokio::test]
#[serial]
async fn old_send_forms_require_fresh_snapshot_even_with_same_count() {
    request::<App, _, _>(|request, ctx| async move {
        let runtime = ctx
            .shared_store
            .get::<Arc<crabgate::initializers::organizers::OrganizerRuntime>>()
            .unwrap();
        let registry = runtime.registry();
        let scope = registry.resolve("localhost").await.unwrap();
        let cipher = registry.cipher(&scope.organizer).unwrap();
        let cookie = session(
            &ctx,
            &scope.organizer.id,
            "correction-admin@example.com",
            Role::Admin,
        )
        .await;
        let (event, kind) = correction_event(&scope.db).await;
        let first = admin_orders::complimentary(
            &scope.db,
            &cipher,
            &event,
            &kind,
            "first@example.com",
            "First",
            orders::now(),
        )
        .await
        .unwrap();
        let path = format!("/office/events/{event}/messages");
        assert_eq!(
            request
                .post(&format!("{path}/preview"))
                .add_header("cookie", &cookie)
                .form(&serde_json::json!({"subject":"Arrival", "body":"Come at six"}))
                .await
                .status_code(),
            303
        );
        let id = buyer_messages::messages(&scope.db, &cipher, &event)
            .await
            .unwrap()[0]
            .id
            .clone();
        let send = format!("{path}/{id}/send");
        let tab_one = request.get(&path).add_header("cookie", &cookie).await;
        let tab_two = request.get(&path).add_header("cookie", &cookie).await;
        let old_one = confirmation(&tab_one.text(), &id);
        let old_two = confirmation(&tab_two.text(), &id);
        assert_eq!(old_one, old_two);
        admin_orders::complimentary(
            &scope.db,
            &cipher,
            &event,
            &kind,
            "second@example.com",
            "Second",
            orders::now(),
        )
        .await
        .unwrap();
        for form in [&old_one, &old_one, &old_two] {
            assert_eq!(
                request
                    .post(&send)
                    .add_header("cookie", &cookie)
                    .form(form)
                    .await
                    .status_code(),
                409
            );
            assert_eq!(states(&scope.db, &id).await, Vec::<String>::new());
        }
        let fresh_page = request.get(&path).add_header("cookie", &cookie).await;
        assert!(fresh_page
            .text()
            .contains("intend to send it to 2 Buyer email addresses"));
        let before_email_change = confirmation(&fresh_page.text(), &id);
        assert_ne!(before_email_change, old_one);
        let encrypted = cipher
            .encrypt("buyer_email", "updated@example.com")
            .unwrap();
        scope
            .db
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE orders SET buyer_email_encrypted = ? WHERE id = ?",
                [encrypted.into(), first.into()],
            ))
            .await
            .unwrap();
        for form in [&before_email_change, &before_email_change, &old_two] {
            assert_eq!(
                request
                    .post(&send)
                    .add_header("cookie", &cookie)
                    .form(form)
                    .await
                    .status_code(),
                409
            );
            assert_eq!(states(&scope.db, &id).await, Vec::<String>::new());
        }
        assert_eq!(
            request
                .post(&send)
                .add_header("cookie", &cookie)
                .form(&serde_json::json!({"confirm_send":"yes"}))
                .await
                .status_code(),
            409
        );
        let final_page = request.get(&path).add_header("cookie", &cookie).await;
        let fresh = confirmation(&final_page.text(), &id);
        assert_ne!(fresh, before_email_change);
        assert_eq!(
            request
                .post(&send)
                .add_header("cookie", &cookie)
                .form(&fresh)
                .await
                .status_code(),
            303
        );
        assert_eq!(
            request
                .post(&send)
                .add_header("cookie", &cookie)
                .form(&old_two)
                .await
                .status_code(),
            303
        );
        assert_eq!(states(&scope.db, &id).await, vec!["Pending", "Pending"]);
        buyer_messages::deliver_pending(&ctx, &registry, &scope.organizer, &scope.db)
            .await
            .unwrap();
        let deliveries = ctx.mailer.as_ref().unwrap().deliveries();
        assert_eq!(deliveries.count, 2);
        assert!(deliveries
            .messages
            .iter()
            .any(|mail| mail.contains("To: updated@example.com")));
        assert!(deliveries
            .messages
            .iter()
            .any(|mail| mail.contains("To: second@example.com")));
        assert!(deliveries
            .messages
            .iter()
            .all(|mail| !mail.contains("first@example.com")));
        drop(scope);
        runtime.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
#[serial]
async fn http_anonymization_erases_generated_copies_and_preserves_other_orders() {
    request::<App, _, _>(|request, ctx| async move {
        let runtime = ctx.shared_store.get::<Arc<crabgate::initializers::organizers::OrganizerRuntime>>().unwrap(); let registry = runtime.registry(); let scope = registry.resolve("localhost").await.unwrap(); let cipher = registry.cipher(&scope.organizer).unwrap();
        let cookie = session(&ctx, &scope.organizer.id, "privacy-admin@example.com", Role::Admin).await;
        let (event, kind) = correction_event(&scope.db).await; let (other_event, other_kind) = correction_event(&scope.db).await;
        let sole = admin_orders::complimentary(&scope.db, &cipher, &event, &kind, "sole@example.com", "Sole Guest", orders::now()).await.unwrap();
        let other = admin_orders::complimentary(&scope.db, &cipher, &other_event, &other_kind, "sole@example.com", "Other Guest", orders::now()).await.unwrap();
        let pending = buyer_messages::preview(&scope.db, &cipher, &event, "Pending", "Author wrote sole@example.com", orders::now()).await.unwrap(); queue_preview(&scope.db, &cipher, &event, &pending, orders::now()).await.unwrap();
        let preview = buyer_messages::preview(&scope.db, &cipher, &event, "Preview", "Come at six", orders::now()).await.unwrap();
        let other_message = buyer_messages::preview(&scope.db, &cipher, &other_event, "Other Event", "Come at seven", orders::now()).await.unwrap(); queue_preview(&scope.db, &cipher, &other_event, &other_message, orders::now()).await.unwrap();
        let accepted = buyer_messages::preview(&scope.db, &cipher, &event, "Accepted", "Come at six", orders::now()).await.unwrap(); queue_preview(&scope.db, &cipher, &event, &accepted, orders::now()).await.unwrap();
        scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE buyer_message_recipients SET state = 'Accepted', attempts = 2 WHERE message_id = ?", [accepted.clone().into()])).await.unwrap();
        let failed = buyer_messages::preview(&scope.db, &cipher, &event, "Failed", "Come at six", orders::now()).await.unwrap(); queue_preview(&scope.db, &cipher, &event, &failed, orders::now()).await.unwrap();
        scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE buyer_message_recipients SET state = 'Failed', attempts = 1 WHERE message_id = ?", [failed.clone().into()])).await.unwrap();
        let intact: String = scope.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "SELECT content_encrypted FROM buyer_messages WHERE id = ?", [preview.clone().into()])).await.unwrap().unwrap().try_get("", "content_encrypted").unwrap();
        scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE buyer_messages SET content_encrypted = 'invalid' WHERE id = ?", [preview.clone().into()])).await.unwrap();
        assert_eq!(request.post(&format!("/office/orders/{sole}/anonymize")).add_header("cookie", &cookie).await.status_code(), 500);
        let unchanged = admin_orders::order(&scope.db, &cipher, &sole).await.unwrap(); assert_eq!(unchanged.buyer_email, "sole@example.com"); assert_eq!(unchanged.tickets[0].attendee_name.as_deref(), Some("Sole Guest")); assert_eq!(states(&scope.db, &pending).await, vec!["Pending"]); assert_eq!(address_copies(&scope.db, &cipher, &pending).await.1, vec!["sole@example.com"]);
        scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE buyer_messages SET content_encrypted = ? WHERE id = ?", [intact.into(), preview.clone().into()])).await.unwrap();
        let new_email = cipher.encrypt("buyer_email", "new-sole@example.com").unwrap(); scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE orders SET buyer_email_encrypted = ? WHERE id = ?", [new_email.into(), sole.clone().into()])).await.unwrap();
        assert_eq!(request.post(&format!("/office/orders/{sole}/anonymize")).add_header("cookie", &cookie).await.status_code(), 303);
        let erased = admin_orders::order(&scope.db, &cipher, &sole).await.unwrap(); assert_eq!(erased.buyer_email, ""); assert_eq!(erased.status, "Paid"); assert_eq!(erased.tickets[0].status, "Valid"); assert_eq!(erased.tickets[0].attendee_name, None);
        for id in [&pending, &accepted, &failed] { let (content, emails) = address_copies(&scope.db, &cipher, id).await; assert_eq!(content["recipients"], serde_json::json!([])); assert_eq!(emails, vec![""]); }
        let (content, _) = address_copies(&scope.db, &cipher, &pending).await; assert_eq!(content["body"], "Author wrote sole@example.com");
        assert_eq!(address_copies(&scope.db, &cipher, &preview).await.0["recipients"], serde_json::json!([]));
        let history = buyer_messages::messages(&scope.db, &cipher, &event).await.unwrap(); assert_eq!(history.iter().find(|m| m.id == preview).unwrap().recipient_count, 0); assert_eq!(history.iter().find(|m| m.id == accepted).unwrap().accepted, 1); assert_eq!(history.iter().find(|m| m.id == accepted).unwrap().recipient_count, 1);
        assert_eq!(states(&scope.db, &pending).await, vec!["Skipped"]); assert_eq!(states(&scope.db, &failed).await, vec!["Skipped"]);
        let attempts: i64 = scope.db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "SELECT attempts FROM buyer_message_recipients WHERE message_id = ?", [accepted.clone().into()])).await.unwrap().unwrap().try_get("", "attempts").unwrap(); assert_eq!(attempts, 2);
        buyer_messages::retry(&scope.db, &event, &failed, true, orders::now()).await.unwrap();
        buyer_messages::deliver_pending(&ctx, &registry, &scope.organizer, &scope.db).await.unwrap();
        let deliveries = ctx.mailer.as_ref().unwrap().deliveries(); assert_eq!(deliveries.count, 1); assert!(deliveries.messages[0].contains("To: sole@example.com")); assert!(deliveries.messages[0].contains("Other Event"));
        assert_eq!(admin_orders::order(&scope.db, &cipher, &other).await.unwrap().buyer_email, "sole@example.com"); assert_eq!(address_copies(&scope.db, &cipher, &other_message).await.1, vec!["sole@example.com"]);
        let shared_one = admin_orders::complimentary(&scope.db, &cipher, &event, &kind, "Shared@Example.com", "One", orders::now()).await.unwrap(); let shared_two = admin_orders::complimentary(&scope.db, &cipher, &event, &kind, "shared@example.com", "Two", orders::now()).await.unwrap();
        let shared = buyer_messages::preview(&scope.db, &cipher, &event, "Shared", "Come at six", orders::now()).await.unwrap(); queue_preview(&scope.db, &cipher, &event, &shared, orders::now()).await.unwrap();
        assert_eq!(request.post(&format!("/office/orders/{shared_one}/anonymize")).add_header("cookie", &cookie).await.status_code(), 303);
        assert_eq!(address_copies(&scope.db, &cipher, &shared).await.0["recipients"], serde_json::json!(["shared@example.com"])); assert_eq!(address_copies(&scope.db, &cipher, &shared).await.1, vec!["shared@example.com"]);
        buyer_messages::deliver_pending(&ctx, &registry, &scope.organizer, &scope.db).await.unwrap(); assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 2);
        assert_eq!(request.post(&format!("/office/orders/{shared_two}/anonymize")).add_header("cookie", &cookie).await.status_code(), 303);
        assert_eq!(address_copies(&scope.db, &cipher, &shared).await.0["recipients"], serde_json::json!([])); assert_eq!(address_copies(&scope.db, &cipher, &shared).await.1, vec![""]); assert_eq!(states(&scope.db, &shared).await, vec!["Accepted"]);
        buyer_messages::retry(&scope.db, &event, &shared, true, orders::now()).await.unwrap(); buyer_messages::deliver_pending(&ctx, &registry, &scope.organizer, &scope.db).await.unwrap(); assert_eq!(ctx.mailer.as_ref().unwrap().deliveries().count, 2);
        drop(scope); runtime.shutdown().await.unwrap();
    }).await;
}

#[tokio::test]
#[serial]
async fn anonymization_during_smtp_scrubs_storage_without_claiming_retraction() {
    request::<App, _, _>(|request, ctx| async move {
        let runtime = ctx
            .shared_store
            .get::<Arc<crabgate::initializers::organizers::OrganizerRuntime>>()
            .unwrap();
        let registry = runtime.registry();
        let scope = registry.resolve("localhost").await.unwrap();
        let cipher = registry.cipher(&scope.organizer).unwrap();
        let cookie = session(
            &ctx,
            &scope.organizer.id,
            "race-admin@example.com",
            Role::Admin,
        )
        .await;
        let (event, kind) = correction_event(&scope.db).await;
        let order = admin_orders::complimentary(
            &scope.db,
            &cipher,
            &event,
            &kind,
            "inflight@example.com",
            "Guest",
            orders::now(),
        )
        .await
        .unwrap();
        let id = buyer_messages::preview(
            &scope.db,
            &cipher,
            &event,
            "Arrival",
            "Come at six",
            orders::now(),
        )
        .await
        .unwrap();
        queue_preview(&scope.db, &cipher, &event, &id, orders::now())
            .await
            .unwrap();
        let mode = Arc::new(AtomicUsize::new(3));
        let data = Arc::new(AtomicUsize::new(0));
        let (mail_ctx, smtp_job) = smtp(&ctx, mode.clone(), data.clone()).await;
        let runner = {
            let db = scope.db.clone();
            let registry = registry.clone();
            let organizer = scope.organizer.clone();
            let context = mail_ctx.clone();
            tokio::spawn(async move {
                buyer_messages::deliver_pending(&context, &registry, &organizer, &db)
                    .await
                    .unwrap();
            })
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            while data.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(states(&scope.db, &id).await, vec!["Sending"]);
        assert_eq!(
            request
                .post(&format!("/office/orders/{order}/anonymize"))
                .add_header("cookie", &cookie)
                .await
                .status_code(),
            303
        );
        assert_eq!(
            address_copies(&scope.db, &cipher, &id).await.0["recipients"],
            serde_json::json!([])
        );
        assert_eq!(address_copies(&scope.db, &cipher, &id).await.1, vec![""]);
        assert_eq!(states(&scope.db, &id).await, vec!["Sending"]);
        mode.store(0, Ordering::SeqCst);
        runner.await.unwrap();
        assert_eq!(states(&scope.db, &id).await, vec!["Accepted"]);
        assert_eq!(data.load(Ordering::SeqCst), 1);
        buyer_messages::retry(&scope.db, &event, &id, true, orders::now())
            .await
            .unwrap();
        buyer_messages::deliver_pending(&mail_ctx, &registry, &scope.organizer, &scope.db)
            .await
            .unwrap();
        assert_eq!(data.load(Ordering::SeqCst), 1);
        assert_eq!(address_copies(&scope.db, &cipher, &id).await.1, vec![""]);
        smtp_job.abort();
        let _ = smtp_job.await;
        drop(scope);
        runtime.shutdown().await.unwrap();
    })
    .await;
}
