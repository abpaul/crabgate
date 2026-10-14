use crabgate::{
    admin_orders,
    app::App,
    events::{self, EventInput, TicketTypeInput},
    inventory::{self, Selection},
    links,
    orders::{self, PaymentOutcome},
};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;
use std::sync::Arc;

#[tokio::test]
#[serial]
async fn cg022_public_order_print_and_signed_admission_states() {
    request::<App, _, _>(|request, ctx| async move {
        let registry = ctx
            .shared_store
            .get::<Arc<crabgate::initializers::organizers::OrganizerRuntime>>()
            .unwrap()
            .registry();
        let scope = registry.active_dbs().await.unwrap().remove(0);
        let db = &scope.db;
        let cipher = registry.cipher(&scope.organizer).unwrap();
        let now = orders::now();
        let event = events::create(
            db,
            &EventInput {
                title: "Print night & friends".into(),
                date: "2027-06-15".into(),
                start_time: "19:00".into(),
                end_time: "21:00".into(),
                timezone: "Europe/Dublin".into(),
                venue: "Town Hall".into(),
                address: "123 Main Street".into(),
                description: String::new(),
                event_capacity: Some(30),
                max_per_order: 10,
                attendee_name_required: true,
            },
        )
        .await
        .unwrap();
        let mut selections = Vec::new();
        for name in ["Standard", "Balcony"] {
            let kind = events::add_ticket_type(
                db,
                &event,
                &TicketTypeInput {
                    name: name.into(),
                    price: "0".into(),
                    capacity: 15,
                    sales_open: None,
                    sales_close: None,
                },
            )
            .await
            .unwrap();
            selections.push(Selection {
                ticket_type_id: kind.id,
                quantity: 1,
            });
        }
        events::publish(db, &event.id, now).await.unwrap();
        let hold = inventory::reserve(db, &event.id, &selections, now)
            .await
            .unwrap();
        let details = orders::hold_details(db, &hold.id).await.unwrap();
        let long_name = "Alexandra".repeat(22);
        orders::save_details(
            db,
            &cipher,
            &details,
            "print@example.com",
            &["Sam & Lee".into(), long_name.clone()],
            true,
            now,
        )
        .await
        .unwrap();
        orders::attach_session(db, &hold.id, "cs_cg022", now)
            .await
            .unwrap();
        let PaymentOutcome::Paid(id) =
            orders::complete_payment(db, "cs_cg022", "pi_cg022", 0, now + 1)
                .await
                .unwrap()
        else {
            panic!("expected paid order")
        };
        let order = orders::get_order(db, &cipher, &id).await.unwrap();
        assert_eq!(order.tickets.len(), 2);
        let expiry = now + 3600;
        let sig = links::sign_order(&scope.organizer.id, &id, expiry).unwrap();
        let path = format!("/orders/{id}?expires_at={expiry}&sig={sig}");
        let response = request.get(&path).await;
        assert_eq!(response.status_code(), 200);
        let html = response.text();
        assert_eq!(html.matches("<article class=\"stub\"").count(), 2);
        assert_eq!(html.matches("alt=\"QR code for ticket").count(), 2);
        assert!(html.contains("Sam &amp; Lee"));
        assert!(html.contains(&long_name));
        for text in ["Standard", "Balcony", "Town Hall", "19:00", &order.number] {
            assert!(html.contains(text), "missing {text}");
        }
        for ticket in &order.tickets {
            let article = html
                .split("<article class=\"stub\"")
                .find(|part| part.contains(&ticket.code))
                .unwrap()
                .split("</article>")
                .next()
                .unwrap();
            assert!(article.contains(
                ticket
                    .attendee_name
                    .as_deref()
                    .unwrap()
                    .replace('&', "&amp;")
                    .as_str()
            ));
            assert_eq!(article.matches("alt=\"QR code for ticket").count(), 1);
            for text in [
                "Print night &amp; friends",
                "Tue 15 June 2027",
                "Town Hall",
                "123 Main Street",
                &order.number,
            ] {
                assert!(
                    article.contains(text),
                    "ticket {} missing {text}",
                    ticket.code
                );
            }
        }
        let query = html
            .split("/qr/")
            .nth(1)
            .unwrap()
            .split('?')
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .replace("&amp;", "&");
        let qr = format!("/orders/{id}/qr/{}?{query}", order.tickets[0].id);
        let image = request.get(&qr).await;
        assert_eq!(image.status_code(), 200);
        assert_eq!(image.header("content-type"), "image/png");
        assert_eq!(image.header("cache-control"), "no-store");
        assert_eq!(
            request
                .get(&format!("/orders/{id}/qr/missing?{query}"))
                .await
                .status_code(),
            404
        );
        assert_eq!(
            request
                .get(&format!("/orders/{id}?expires_at={expiry}&sig=00"))
                .await
                .status_code(),
            403
        );
        let expired = now - 1;
        let expired_sig = links::sign_order(&scope.organizer.id, &id, expired).unwrap();
        assert_eq!(
            request
                .get(&format!(
                    "/orders/{id}?expires_at={expired}&sig={expired_sig}"
                ))
                .await
                .status_code(),
            403
        );
        assert_eq!(
            request
                .get(&format!(
                    "/orders/{id}/qr/{}?expires_at={expired}&sig={expired_sig}",
                    order.tickets[0].id
                ))
                .await
                .status_code(),
            403
        );
        let wrong_sig = links::sign_order("another-organizer", &id, expiry).unwrap();
        assert_eq!(
            request
                .get(&format!("/orders/{id}?expires_at={expiry}&sig={wrong_sig}"))
                .await
                .status_code(),
            403
        );
        for status in ["RefundPending", "Refunded"] {
            db.execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "UPDATE orders SET status = ? WHERE id = ?",
                [status.into(), id.clone().into()],
            ))
            .await
            .unwrap();
            let invalid = request.get(&path).await.text();
            assert_eq!(invalid.matches("alt=\"QR code for ticket").count(), 0);
            assert_eq!(
                invalid
                    .matches("<strong>Order inactive</strong>")
                    .count(),
                2
            );
            assert_eq!(request.get(&qr).await.status_code(), 410);
        }
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE orders SET status = 'Paid' WHERE id = ?",
            [id.clone().into()],
        ))
        .await
        .unwrap();
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET status = 'Cancelled' WHERE id = ?",
            [event.id.clone().into()],
        ))
        .await
        .unwrap();
        let invalid = request.get(&path).await.text();
        assert_eq!(invalid.matches("alt=\"QR code for ticket").count(), 0);
        assert_eq!(
            invalid
                .matches("<strong>Event cancelled</strong>")
                .count(),
            2
        );
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET status = 'Published' WHERE id = ?",
            [event.id.clone().into()],
        ))
        .await
        .unwrap();
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE tickets SET status = 'Void' WHERE id = ?",
            [order.tickets[0].id.clone().into()],
        ))
        .await
        .unwrap();
        let void = request.get(&path).await.text();
        assert_eq!(void.matches("alt=\"QR code for ticket").count(), 1);
        assert!(void.contains("<strong>Void</strong>"));
        assert_eq!(request.get(&qr).await.status_code(), 410);
        admin_orders::refund(db, &id, None, now + 2).await.unwrap();
        let refunded = request.get(&path).await.text();
        assert_eq!(refunded.matches("alt=\"QR code for ticket").count(), 0);
        assert!(refunded.contains("These tickets cannot be used for entry."));
        assert_eq!(refunded.matches("Not valid for entry.").count(), 2);
        assert_eq!(refunded.matches("<p class=\"print-only\">No money was paid for this order. No money will be returned.</p>").count(), 2);
        assert_eq!(request.get(&qr).await.status_code(), 410);
        admin_orders::cancel_event(db, &event.id, now + 3)
            .await
            .unwrap();
        let cancelled = request.get(&path).await.text();
        assert_eq!(cancelled.matches("alt=\"QR code for ticket").count(), 0);
        assert!(cancelled.contains("Event cancelled"));
        assert!(
            html.contains("id=\"print-order\""),
            "public order has no print action"
        );
        assert!(html.contains("Print tickets"));
        assert!(html.contains("/static/shared/order-print.js"));
        assert!(html.contains("@media print"));
        assert!(html.contains("123 Main Street"));
        let script = request.get("/static/shared/order-print.js").await;
        assert_eq!(script.status_code(), 200);
        assert!(script.text().contains("window.print()"));
    })
    .await;
}
