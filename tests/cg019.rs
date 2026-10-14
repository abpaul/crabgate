use crabgate::{
    admin_orders,
    app::App,
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, Selection},
    members::{self, Role},
    operator, orders, questions,
};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;

#[tokio::test]
#[serial]
async fn attendee_roster_public_flow() {
    request::<App, _, _>(|request, ctx| async move {
        let runtime = ctx.shared_store.get::<std::sync::Arc<crabgate::initializers::organizers::OrganizerRuntime>>().unwrap();
        let registry = runtime.registry();
        let scope = registry.resolve("localhost").await.unwrap();
        let cipher = registry.cipher(&scope.organizer).unwrap();
        let now = crabgate::orders::now();
        let member = members::add_member(&ctx.db, &scope.organizer.id, "roster-admin@example.com", Role::Admin).await.unwrap();
        let token = members::request_magic_link(&ctx.db, &scope.organizer.id, "roster-admin@example.com", now).await.unwrap().unwrap();
        let session = members::claim_magic_link(&ctx.db, &scope.organizer.id, &token, now).await.unwrap().unwrap();
        let cookie = format!("cg_session={session}");
        assert_eq!(request.get("/office/attendees").await.status_code(), 303);
        let empty = request.get("/office/attendees").add_header("cookie", &cookie).await;
        assert_eq!(empty.status_code(), 200, "{}", empty.text());
        assert!(empty.text().contains("No attendees yet"));
        let event = events::create(&scope.db, &EventInput { title: "Roster workshop".into(), date: "2027-06-15".into(), start_time: "19:00".into(), end_time: "21:00".into(), timezone: "Europe/Dublin".into(), venue: "Hall".into(), address: "Main Street".into(), description: String::new(), event_capacity: Some(10), max_per_order: 4, attendee_name_required: false }).await.unwrap();
        let kind = events::add_ticket_type(&scope.db, &event, &TicketTypeInput { name: "Workshop place".into(), price: "0".into(), capacity: 10, sales_open: None, sales_close: None }).await.unwrap();
        events::publish(&scope.db, &event.id, now).await.unwrap();
        let first = admin_orders::complimentary(&scope.db, &cipher, &event.id, &kind.id, "buyer@example.com", "Niamh <Guest>", now).await.unwrap();
        let second = admin_orders::complimentary(&scope.db, &cipher, &event.id, &kind.id, "other@example.com", "Cian", now).await.unwrap();
        let order = admin_orders::order(&scope.db, &cipher, &first).await.unwrap();
        let ticket = &order.tickets[0];
        let checked = request.post(&format!("/door/{}/check", event.id)).add_header("cookie", &cookie).form(&serde_json::json!({"code": ticket.code})).await;
        assert_eq!(checked.status_code(), 200);
        assert!(checked.text().contains("Checked in"));
        assert!(checked.text().contains("result-ok"));
        for query in ["niamh", "BUYER@", ticket.code.as_str(), order.number.as_str()] {
            let response = request.get(&format!("/office/attendees?q={query}")).add_header("cookie", &cookie).await;
            assert_eq!(response.status_code(), 200);
            assert!(response.text().contains("Niamh &lt;Guest&gt;"), "{query}");
            assert!(!response.text().contains(">Cian<"), "{query}");
            assert!(response.text().contains("Checked in"));
            assert!(response.text().contains(&format!("/office/orders/{first}#ticket-{}", ticket.id)));
        }
        let unmatched = request.get("/office/attendees?q=not-present").add_header("cookie", &cookie).await;
        assert!(unmatched.text().contains("No attendees match"));
        let wrong_event = request.get("/office/attendees?event=missing").add_header("cookie", &cookie).await;
        assert_eq!(wrong_event.status_code(), 404);
        assert!(wrong_event.text().contains("Event unavailable"));
        let roster = request.get(&format!("/office/attendees?event={}", event.id)).add_header("cookie", &cookie).await;
        assert!(roster.text().contains(">Cian<"));
        assert!(roster.text().contains("Buyer email"));
        assert!(roster.text().contains("/office/orders#complimentary"));
        assert!(roster.text().contains(&format!("/office/events/{}/export/tickets.csv", event.id)));
        let exported = request.get(&format!("/office/events/{}/export/tickets.csv", event.id)).add_header("cookie", &cookie).await;
        assert_eq!(exported.status_code(), 200);
        assert!(exported.text().contains("Niamh <Guest>"));
        let comp_page = request.get("/office/orders").add_header("cookie", &cookie).await;
        assert_eq!(comp_page.status_code(), 200);
        assert!(comp_page.text().contains("id=\"complimentary\""));
        let issued = request.post("/office/orders/complimentary").add_header("cookie", &cookie).form(&serde_json::json!({"ticket_type_id": kind.id, "email": "http-buyer@example.com", "name": "HTTP Guest"})).await;
        assert_eq!(issued.status_code(), 303);
        let issued_location = issued.header("location").to_str().unwrap().to_owned();
        assert!(issued_location.starts_with("/office/orders/"));
        let issued_detail = request.get(&issued_location).add_header("cookie", &cookie).await;
        assert_eq!(issued_detail.status_code(), 200);
        assert!(issued_detail.text().contains("HTTP Guest"));
        assert!(issued_detail.text().contains("Ticket valid"));
        let found = request.get("/office/attendees?q=HTTP").add_header("cookie", &cookie).await;
        assert_eq!(found.status_code(), 200);
        assert!(found.text().contains("HTTP Guest"));
        let first_undo = request.post(&format!("/door/{}/undo/{}", event.id, ticket.id)).add_header("cookie", &cookie).await;
        assert_eq!(first_undo.status_code(), 303);
        let rechecked = request.post(&format!("/door/{}/check", event.id)).add_header("cookie", &cookie).form(&serde_json::json!({"code": ticket.code})).await;
        assert_eq!(rechecked.status_code(), 200);
        assert!(rechecked.text().contains("result-ok"));
        let refunded = request.post(&format!("/office/orders/{first}/refund")).add_header("cookie", &cookie).await;
        assert_eq!(refunded.status_code(), 303);
        let still_checked = request.get(&format!("/office/attendees?q={}", ticket.code)).add_header("cookie", &cookie).await;
        assert!(still_checked.text().contains("Admission invalid"));
        assert!(still_checked.text().contains("Checked in:"));
        let undone = request.post(&format!("/door/{}/undo/{}", event.id, ticket.id)).add_header("cookie", &cookie).await;
        assert_eq!(undone.status_code(), 303);
        let refused = request.post(&format!("/door/{}/check", event.id)).add_header("cookie", &cookie).form(&serde_json::json!({"code": ticket.code})).await;
        assert_eq!(refused.status_code(), 200);
        assert!(refused.text().contains("Void Ticket"));
        assert!(refused.text().contains("result-bad"));
        let detail = request.get(&format!("/office/orders/{first}")).add_header("cookie", &cookie).await;
        assert_eq!(detail.status_code(), 200);
        assert!(detail.text().contains("Admission invalid"));
        assert!(detail.text().contains("Check-in undone"));
        assert!(detail.text().contains("UTC"));
        assert_eq!(detail.text().matches("Checked in:").count(), 2);
        assert_eq!(detail.text().matches("Check-in undone:").count(), 2);
        let roster = request.get(&format!("/office/attendees?q={}", ticket.code)).add_header("cookie", &cookie).await;
        assert!(roster.text().contains("Admission invalid"));
        assert!(roster.text().contains("Check-in undone"));
        assert!(roster.text().contains("Refunded"));
        assert_eq!(admin_orders::order(&scope.db, &cipher, &first).await.unwrap().tickets[0].id, ticket.id);
        assert_eq!(admin_orders::order(&scope.db, &cipher, &second).await.unwrap().status, "Paid");
        scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE orders SET status = 'RefundPending' WHERE id = ?", [second.clone().into()])).await.unwrap();
        let pending = request.get("/office/attendees?q=Cian").add_header("cookie", &cookie).await;
        assert_eq!(pending.status_code(), 200);
        assert!(pending.text().contains("RefundPending"));
        assert!(pending.text().contains("Ticket valid"));
        assert!(!pending.text().contains("Admission invalid"));
        scope.db.execute_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite, "UPDATE orders SET status = 'Paid' WHERE id = ?", [second.clone().into()])).await.unwrap();
        ctx.db.execute_unprepared("INSERT INTO operators (id, email) VALUES ('roster-support', 'roster-support@example.com')").await.unwrap();
        let token = operator::request_link(&ctx.db, "roster-support@example.com", now).await.unwrap().unwrap();
        let support_session = operator::claim_link(&ctx.db, &token, now).await.unwrap().unwrap();
        let support_cookie = format!("cg_operator_session={support_session}");
        assert_eq!(request.get("/office/attendees").add_header("cookie", &support_cookie).await.status_code(), 403);
        operator::grant(&ctx.db, &scope.organizer.id, &member, 1, now).await.unwrap();
        let support = request.get(&format!("/office/attendees?event={}", event.id)).add_header("cookie", &support_cookie).await;
        assert_eq!(support.status_code(), 200);
        assert!(support.text().contains("Niamh &lt;Guest&gt;"));
        assert!(!support.text().contains("export/tickets.csv"));
        assert!(!support.text().contains("/office/orders#complimentary"));
        assert_eq!(request.get(&format!("/office/events/{}/export/tickets.csv", event.id)).add_header("cookie", &support_cookie).await.status_code(), 403);
        assert_eq!(request.post("/office/orders/complimentary").add_header("cookie", &support_cookie).await.status_code(), 403);
        let question = questions::add(&scope.db, &event.id, "Attendee", "Text", "Original meal choice", &[], false).await.unwrap();
        let order_question = questions::add(&scope.db, &event.id, "Order", "Text", "Original group note", &[], false).await.unwrap();
        let hold = inventory::reserve(&scope.db, &event.id, &[Selection { ticket_type_id: kind.id.clone(), quantity: 2 }], now).await.unwrap();
        let asked = questions::list_for_hold(&scope.db, &hold.id).await.unwrap();
        let form = std::collections::HashMap::from([(format!("question_{question}_0"), "Vegetarian".to_owned()), (format!("question_{question}_1"), "No preference".to_owned()), (format!("question_{order_question}"), "Group note retained".to_owned())]);
        questions::save_answers(&scope.db, &cipher, &hold.id, &asked, 2, &form).await.unwrap();
        let hold_details = orders::hold_details(&scope.db, &hold.id).await.unwrap();
        orders::save_details(&scope.db, &cipher, &hold_details, "answer-buyer@example.com", &["Answer One".into(), "Answer Two".into()], false, now).await.unwrap();
        orders::attach_session(&scope.db, &hold.id, "synthetic-free-checkout", now).await.unwrap();
        let orders::PaymentOutcome::Paid(answer_order_id) = orders::complete_payment(&scope.db, "synthetic-free-checkout", "synthetic-free-intent", 0, now).await.unwrap() else { panic!("free checkout did not create an order") };
        questions::delete(&scope.db, &event.id, &question).await.unwrap();
        questions::delete(&scope.db, &event.id, &order_question).await.unwrap();
        let answer_order = admin_orders::order(&scope.db, &cipher, &answer_order_id).await.unwrap();
        let detail = request.get(&format!("/office/orders/{answer_order_id}")).add_header("cookie", &cookie).await;
        assert_eq!(detail.status_code(), 200);
        assert!(detail.text().contains("Original meal choice"));
        assert!(detail.text().contains("Original group note"));
        assert!(detail.text().contains("Group note retained"));
        for (name, answer) in [("Answer One", "Vegetarian"), ("Answer Two", "No preference")] {
            let identity = answer_order.tickets.iter().find(|ticket| ticket.attendee_name.as_deref() == Some(name)).unwrap();
            let html = detail.text();
            let section = html.split(&format!("id=\"ticket-{}\"", identity.id)).nth(1).unwrap().split("</section>").next().unwrap();
            assert!(section.contains(name));
            assert!(section.contains(answer));
            assert!(section.contains(&identity.code));
        }
        scope.db.execute_unprepared("ALTER TABLE tickets RENAME TO cg019_unavailable_tickets").await.unwrap();
        let unavailable = request.get("/office/attendees").add_header("cookie", &cookie).await;
        assert_eq!(unavailable.status_code(), 500);
        assert!(unavailable.text().contains("Attendees could not be loaded"));
        assert!(!unavailable.text().contains("cg019_unavailable_tickets"));
        scope.db.execute_unprepared("ALTER TABLE cg019_unavailable_tickets RENAME TO tickets").await.unwrap();
    }).await;
}

struct EditionGuard(Option<String>);
impl Drop for EditionGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(edition) => std::env::set_var("CRABGATE_EDITION", edition),
            None => std::env::remove_var("CRABGATE_EDITION"),
        }
    }
}

#[tokio::test]
#[serial]
async fn attendee_roster_tenant_and_role_boundaries() {
    let _edition = EditionGuard(std::env::var("CRABGATE_EDITION").ok());
    std::env::set_var("CRABGATE_EDITION", "hosted");
    request::<App, _, _>(|request, ctx| async move {
        let runtime = ctx
            .shared_store
            .get::<std::sync::Arc<crabgate::initializers::organizers::OrganizerRuntime>>()
            .unwrap();
        let registry = runtime.registry();
        let owner = registry
            .create("Roster owner", "roster-owner")
            .await
            .unwrap();
        let neighbor = registry
            .create("Roster neighbor", "roster-neighbor")
            .await
            .unwrap();
        let scope = registry.resolve("roster-owner.crabgate.com").await.unwrap();
        let cipher = registry.cipher(&owner).unwrap();
        let now = orders::now();
        let event = events::create(
            &scope.db,
            &EventInput {
                title: "Private tenant event".into(),
                date: "2027-06-15".into(),
                start_time: "19:00".into(),
                end_time: "21:00".into(),
                timezone: "Europe/Dublin".into(),
                venue: "Hall".into(),
                address: "Main Street".into(),
                description: String::new(),
                event_capacity: Some(2),
                max_per_order: 2,
                attendee_name_required: false,
            },
        )
        .await
        .unwrap();
        let kind = events::add_ticket_type(
            &scope.db,
            &event,
            &TicketTypeInput {
                name: "Private ticket".into(),
                price: "0".into(),
                capacity: 2,
                sales_open: None,
                sales_close: None,
            },
        )
        .await
        .unwrap();
        events::publish(&scope.db, &event.id, now).await.unwrap();
        let order_id = admin_orders::complimentary(
            &scope.db,
            &cipher,
            &event.id,
            &kind.id,
            "private-buyer@example.com",
            "Private Attendee",
            now,
        )
        .await
        .unwrap();
        for (organizer, host) in [
            (&owner, "roster-owner.crabgate.com"),
            (&neighbor, "roster-neighbor.crabgate.com"),
        ] {
            for role in [Role::Admin, Role::DoorStaff] {
                let email = format!("{:?}-{}@example.com", role, organizer.subdomain);
                members::add_member(&ctx.db, &organizer.id, &email, role)
                    .await
                    .unwrap();
                let token = members::request_magic_link(&ctx.db, &organizer.id, &email, now)
                    .await
                    .unwrap()
                    .unwrap();
                let session = members::claim_magic_link(&ctx.db, &organizer.id, &token, now)
                    .await
                    .unwrap()
                    .unwrap();
                let cookie = format!("cg_session={session}");
                let roster = request
                    .get("/office/attendees")
                    .add_header("host", host)
                    .add_header("cookie", &cookie)
                    .await;
                if role == Role::DoorStaff {
                    assert_eq!(roster.status_code(), 403);
                    assert!(!roster.text().contains("private-buyer@example.com"));
                } else if organizer.id == owner.id {
                    assert_eq!(roster.status_code(), 200);
                    assert!(roster.text().contains("Private Attendee"));
                    let other = request
                        .get("/office/attendees")
                        .add_header("host", "roster-neighbor.crabgate.com")
                        .add_header("cookie", &cookie)
                        .await;
                    assert_eq!(other.status_code(), 303);
                    assert!(!other.text().contains("Private Attendee"));
                } else {
                    assert_eq!(roster.status_code(), 200);
                    assert!(roster.text().contains("No attendees yet"));
                    assert!(!roster.text().contains("Private Attendee"));
                    assert!(!roster.text().contains("private-buyer@example.com"));
                    assert_eq!(
                        request
                            .get(&format!("/office/orders/{order_id}"))
                            .add_header("host", host)
                            .add_header("cookie", &cookie)
                            .await
                            .status_code(),
                        404
                    );
                    assert_eq!(
                        request
                            .get(&format!("/office/attendees?event={}", event.id))
                            .add_header("host", host)
                            .add_header("cookie", &cookie)
                            .await
                            .status_code(),
                        404
                    );
                    assert_eq!(
                        request
                            .get(&format!("/office/events/{}/export/tickets.csv", event.id))
                            .add_header("host", host)
                            .add_header("cookie", &cookie)
                            .await
                            .status_code(),
                        404
                    );
                }
            }
        }
    })
    .await;
}
