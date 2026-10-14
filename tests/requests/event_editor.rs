use axum::body::Bytes;
use crabgate::{
    app::App,
    events::{self, EventInput},
    members::{self, Role},
};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;

fn multipart(bytes: &[u8], mime: &str) -> Bytes {
    let mut body = format!("--cover-boundary\r\nContent-Disposition: form-data; name=\"cover\"; filename=\"cover.jpg\"\r\nContent-Type: {mime}\r\n\r\n").into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n--cover-boundary--\r\n");
    Bytes::from(body)
}

#[tokio::test]
#[serial]
async fn oversized_real_multipart_upload_reports_its_cause() {
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
                title: "Upload Test".to_owned(),
                date: "2027-06-15".to_owned(),
                start_time: "19:00".to_owned(),
                end_time: "21:00".to_owned(),
                timezone: "Europe/Dublin".to_owned(),
                venue: "Hall".to_owned(),
                address: "Road".to_owned(),
                description: String::new(),
                event_capacity: None,
                max_per_order: 10,
                attendee_name_required: false,
            },
        )
        .await
        .unwrap();
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
        let currency = request
            .post("/office/setup/currency")
            .add_header("cookie", &cookie)
            .form(&[("currency", "GBP")])
            .await;
        assert_eq!(currency.status_code(), 303);
        let setup = request
            .get("/office/setup?step=details")
            .add_header("cookie", &cookie)
            .await;
        assert!(setup
            .text()
            .contains("<option value=\"GBP\" selected>GBP (Pound sterling)</option>"));
        members::add_member(&ctx.db, &id, "door@example.com", Role::DoorStaff)
            .await
            .unwrap();
        let now = chrono::Utc::now().timestamp();
        let door_link = members::request_magic_link(&ctx.db, &id, "door@example.com", now)
            .await
            .unwrap()
            .unwrap();
        let door_session = members::claim_magic_link(&ctx.db, &id, &door_link, now + 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            request
                .post(&format!("/office/events/{}/cover", event.id))
                .add_header("cookie", format!("cg_session={door_session}"))
                .content_type("multipart/form-data; boundary=cover-boundary")
                .bytes(multipart(
                    include_bytes!("../fixtures/cover.png"),
                    "image/png"
                ))
                .await
                .status_code(),
            403
        );
        let invalid_event = request
            .post(&format!("/office/events/{}", event.id))
            .add_header("cookie", &cookie)
            .form(&[
                ("title", "X"),
                ("date", "2027-06-15"),
                ("start_time", "19:00"),
                ("end_time", "21:00"),
                ("timezone", "Europe/Dublin"),
                ("venue", "Changed Hall"),
                ("address", "Changed Road"),
                ("description", "Keep this text"),
                ("event_capacity", ""),
                ("max_per_order", "10"),
            ])
            .await;
        assert_eq!(invalid_event.status_code(), 400);
        assert!(invalid_event.text().contains("Enter an event name"));
        assert!(invalid_event.text().contains("value=\"Changed Hall\""));
        assert!(invalid_event.text().contains("Keep this text"));
        let invalid_max = request
            .post(&format!("/office/events/{}", event.id))
            .add_header("cookie", &cookie)
            .form(&[
                ("title", "Preview Concert"),
                ("date", "2027-06-15"),
                ("start_time", "19:00"),
                ("end_time", "21:00"),
                ("timezone", "Europe/Dublin"),
                ("venue", "Hall"),
                ("address", "Road"),
                ("description", ""),
                ("max_per_order", "not-a-number"),
            ])
            .await;
        assert_eq!(invalid_max.status_code(), 400);
        assert!(invalid_max
            .text()
            .contains("Maximum per order must be 1 to 100"));
        assert!(invalid_max.text().contains("value=\"not-a-number\""));
        let zone_change = request
            .post(&format!("/office/events/{}", event.id))
            .add_header("cookie", &cookie)
            .form(&[
                ("title", "Upload Test"),
                ("date", "2027-06-15"),
                ("start_time", "19:00"),
                ("end_time", "21:00"),
                ("timezone", "Europe/Bucharest"),
                ("venue", "Hall"),
                ("address", "Road"),
                ("description", ""),
                ("max_per_order", "10"),
            ])
            .await;
        assert_eq!(zone_change.status_code(), 400);
        assert!(zone_change.text().contains("Review the times and confirm"));
        assert!(zone_change
            .text()
            .contains("value=\"Europe/Bucharest\" selected"));
        assert_eq!(
            events::get(&db, &event.id).await.unwrap().unwrap().timezone,
            "Europe/Dublin"
        );
        let invalid_type = request
            .post(&format!("/office/events/{}/ticket-types", event.id))
            .add_header("cookie", &cookie)
            .form(&[
                ("name", "Preserved Type"),
                ("price", "wrong"),
                ("capacity", "12"),
                ("sales_open", ""),
                ("sales_close", ""),
            ])
            .await;
        assert_eq!(invalid_type.status_code(), 400);
        assert!(invalid_type.text().contains("Enter a valid price"));
        assert!(invalid_type.text().contains("value=\"Preserved Type\""));
        let invalid_capacity = request
            .post(&format!("/office/events/{}/ticket-types", event.id))
            .add_header("cookie", &cookie)
            .form(&[
                ("name", "Capacity Type"),
                ("price", "10"),
                ("capacity", "not-a-number"),
            ])
            .await;
        assert_eq!(invalid_capacity.status_code(), 400);
        assert!(invalid_capacity
            .text()
            .contains("Enter a valid ticket type capacity"));
        assert!(invalid_capacity.text().contains("value=\"not-a-number\""));
        let ticket_type = events::add_ticket_type(
            &db,
            &event,
            &crabgate::events::TicketTypeInput {
                name: "Seated".to_owned(),
                price: "10".to_owned(),
                capacity: 12,
                sales_open: None,
                sales_close: None,
            },
        )
        .await
        .unwrap();
        let seat_response = request
            .post(&format!("/office/events/{}/seats", event.id))
            .add_header("cookie", &cookie)
            .form(&[
                ("ticket_type_id", ticket_type.id.as_str()),
                ("section", "Main"),
                ("row", "A"),
                ("labels", "1, 2"),
            ])
            .await;
        assert_eq!(seat_response.status_code(), 303);
        let seat_page = request
            .get(&format!("/office/events/{}", event.id))
            .add_header("cookie", &cookie)
            .await;
        assert!(seat_page.text().contains("Main, Row A, Seat 1"));
        let bad_seats = request
            .post(&format!("/office/events/{}/seats", event.id))
            .add_header("cookie", &cookie)
            .form(&[
                ("ticket_type_id", ticket_type.id.as_str()),
                ("section", "Main"),
                ("row", "B"),
                ("labels", "3, 3"),
            ])
            .await;
        assert_eq!(bad_seats.status_code(), 400);
        assert!(bad_seats.text().contains("distinct seat labels"));
        assert!(bad_seats.text().contains("Main"));
        let question = request
            .post(&format!("/office/events/{}/questions", event.id))
            .add_header("cookie", &cookie)
            .form(&[
                ("scope", "Order"),
                ("kind", "Choice"),
                ("label", "Who <you>?"),
                ("options", "One\nTwo"),
                ("required", "true"),
            ])
            .await;
        assert_eq!(question.status_code(), 303);
        assert_eq!(
            request
                .post(&format!("/office/events/{}/questions", event.id))
                .add_header("cookie", format!("cg_session={door_session}"))
                .form(&[
                    ("scope", "Order"),
                    ("kind", "Text"),
                    ("label", "Blocked"),
                    ("options", ""),
                ])
                .await
                .status_code(),
            403
        );
        let question_page = request
            .get(&format!("/office/events/{}", event.id))
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(question_page.status_code(), 200);
        assert!(question_page.text().contains("Who &lt;you&gt;?"));
        assert!(!question_page.text().contains("Who <you>?"));
        let discount = request
            .post(&format!("/office/events/{}/discounts", event.id))
            .add_header("cookie", &cookie)
            .form(&[
                ("code", "autumn20"),
                ("kind", "Percent"),
                ("value", "20"),
                ("starts", ""),
                ("ends", ""),
                ("max_redemptions", "5"),
            ])
            .await;
        assert_eq!(discount.status_code(), 303);
        let discount_page = request
            .get(&format!("/office/events/{}", event.id))
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(discount_page.status_code(), 200);
        assert!(discount_page.text().contains("AUTUMN20, 20%"));
        for (mime, image) in [
            (
                "image/jpeg",
                include_bytes!("../fixtures/cover.jpg").as_slice(),
            ),
            (
                "image/png",
                include_bytes!("../fixtures/cover.png").as_slice(),
            ),
            (
                "image/webp",
                include_bytes!("../fixtures/cover.webp").as_slice(),
            ),
            (
                "image/png",
                include_bytes!("../fixtures/cover-large.png").as_slice(),
            ),
        ] {
            let response = request
                .post(&format!("/office/events/{}/cover", event.id))
                .add_header("cookie", &cookie)
                .content_type("multipart/form-data; boundary=cover-boundary")
                .bytes(multipart(image, mime))
                .await;
            assert_eq!(response.status_code(), 303, "{mime}: {}", response.text());
            let cover = request
                .get(&format!("/office/events/{}/cover", event.id))
                .add_header("cookie", &cookie)
                .await;
            assert_eq!(cover.status_code(), 200);
            assert_eq!(cover.header("content-type"), mime);
        }
        for (mime, image, reason) in [
            ("image/gif", b"GIF89a".as_slice(), "Unsupported format"),
            ("image/png", b"not a PNG".as_slice(), "Unreadable image"),
            (
                "image/jpeg",
                b"\xff\xd8\xffbad".as_slice(),
                "Unreadable image",
            ),
        ] {
            let response = request
                .post(&format!("/office/events/{}/cover", event.id))
                .add_header("cookie", &cookie)
                .content_type("multipart/form-data; boundary=cover-boundary")
                .bytes(multipart(image, mime))
                .await;
            assert_eq!(response.status_code(), 400);
            assert!(response.text().contains(reason));
            assert!(response.text().contains("id=\"cover\""));
        }
        let malformed = request
            .post(&format!("/office/events/{}/cover", event.id))
            .add_header("cookie", &cookie)
            .content_type("multipart/form-data; boundary=cover-boundary")
            .bytes(Bytes::from_static(b"--cover-boundary\r\nnot valid\r\n"))
            .await;
        assert_eq!(malformed.status_code(), 400);
        assert!(malformed.text().contains("Choose an image"));
        let medium = request
            .post(&format!("/office/events/{}/cover", event.id))
            .add_header("cookie", &cookie)
            .content_type("multipart/form-data; boundary=cover-boundary")
            .bytes(multipart(&vec![b'x'; 3_100_000], "image/jpeg"))
            .await;
        assert_eq!(medium.status_code(), 400);
        assert!(medium.text().contains("over 3 MB"));
        let response = request
            .post(&format!("/office/events/{}/cover", event.id))
            .add_header("cookie", &cookie)
            .content_type("multipart/form-data; boundary=cover-boundary")
            .bytes(multipart(&vec![b'x'; 3_600_000], "image/jpeg"))
            .await;
        assert_eq!(response.status_code(), 413);
        assert!(response.text().contains("over 3 MB"));
        assert!(response.text().contains("id=\"cover\""));
        let retained = request
            .get(&format!("/office/events/{}/cover", event.id))
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(
            retained.as_bytes().as_ref(),
            include_bytes!("../fixtures/cover-large.png")
        );
    })
    .await;
}
