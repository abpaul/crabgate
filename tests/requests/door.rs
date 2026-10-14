use chrono::Utc;
use crabgate::{
    app::App,
    crypto::MasterKey,
    door,
    events::{self, EventInput},
    members::{self, Role},
};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;

#[tokio::test]
#[serial]
async fn door_link_can_open_only_its_event_and_cannot_undo() {
    request::<App, _, _>(|request, ctx| async move {
        let row = ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT id, db_path, data_key FROM organizers WHERE subdomain = 'local'".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        let organizer_id: String = row.try_get("", "id").unwrap();
        let path: String = row.try_get("", "db_path").unwrap();
        let wrapped: String = row.try_get("", "data_key").unwrap();
        let db = sea_orm::Database::connect(format!("sqlite://{path}?mode=rwc"))
            .await
            .unwrap();
        let cipher = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
            .unwrap()
            .open_organizer_key(&organizer_id, &wrapped)
            .unwrap();
        let event = events::create(
            &db,
            &EventInput {
                title: "Door Test".to_owned(),
                date: "2027-06-15".to_owned(),
                start_time: "19:00".to_owned(),
                end_time: "21:00".to_owned(),
                timezone: "Europe/Dublin".to_owned(),
                venue: "Hall".to_owned(),
                address: "1 Main Street".to_owned(),
                description: String::new(),
                event_capacity: None,
                max_per_order: 10,
                attendee_name_required: false,
            },
        )
        .await
        .unwrap();
        db.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE events SET status = 'Published' WHERE id = ?",
            [event.id.clone().into()],
        ))
        .await
        .unwrap();
        let link = door::create_link(
            &db,
            &cipher,
            &event.id,
            "Front door",
            Utc::now().timestamp(),
        )
        .await
        .unwrap();
        assert_eq!(
            request
                .get(&format!("/door/{}", event.id))
                .await
                .status_code(),
            403
        );
        let scan = request
            .get(&format!("/door/{}?link={}", event.id, link.token))
            .await;
        assert_eq!(scan.status_code(), 200);
        assert!(scan.text().contains("Hold the code inside the frame"));
        assert_eq!(
            request
                .post(&format!(
                    "/door/{}/undo/anything?link={}",
                    event.id, link.token
                ))
                .await
                .status_code(),
            403
        );
        members::add_member(&ctx.db, &organizer_id, "door@example.com", Role::DoorStaff)
            .await
            .unwrap();
        let now = Utc::now().timestamp();
        let token = members::request_magic_link(&ctx.db, &organizer_id, "door@example.com", now)
            .await
            .unwrap()
            .unwrap();
        let session = members::claim_magic_link(&ctx.db, &organizer_id, &token, now + 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            request
                .get(&format!("/door/{}", event.id))
                .add_header("cookie", format!("cg_session={session}"))
                .await
                .status_code(),
            200
        );
        assert_eq!(
            request
                .get("/office/events")
                .add_header("cookie", format!("cg_session={session}"))
                .await
                .status_code(),
            403
        );
        door::revoke_link(&db, &link.id, now + 2).await.unwrap();
        assert_eq!(
            request
                .get(&format!("/door/{}?link={}", event.id, link.token))
                .await
                .status_code(),
            403
        );
    })
    .await;
}
