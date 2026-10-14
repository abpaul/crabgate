use chrono::Utc;
use crabgate::{
    app::App,
    members::{self, MemberError, Role},
};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;

async fn local_organizer_id(ctx: &loco_rs::app::AppContext) -> String {
    ctx.db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT id FROM organizers WHERE subdomain = 'local'",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "id")
        .unwrap()
}

#[tokio::test]
#[serial]
async fn admin_magic_link_is_single_use_and_grants_back_office_access() {
    request::<App, _, _>(|request, ctx| async move {
        let organizer_id = local_organizer_id(&ctx).await;
        members::add_member(&ctx.db, &organizer_id, "admin@example.com", Role::Admin)
            .await
            .unwrap();
        let now = Utc::now().timestamp();
        let token = members::request_magic_link(&ctx.db, &organizer_id, "admin@example.com", now)
            .await
            .unwrap()
            .unwrap();
        let session = members::claim_magic_link(&ctx.db, &organizer_id, &token, now + 1)
            .await
            .unwrap()
            .unwrap();
        assert!(
            members::claim_magic_link(&ctx.db, &organizer_id, &token, now + 2)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            members::find_session(&ctx.db, &organizer_id, &session, now + 2)
                .await
                .unwrap()
                .unwrap()
                .role,
            Role::Admin
        );
        assert_eq!(request.get("/office/sign-in").await.status_code(), 200);
        let entry = request.get("/sign-in").await;
        assert_eq!(entry.status_code(), 303);
        assert_eq!(
            entry.header("location").to_str().unwrap(),
            "/office/sign-in"
        );
        let events = request
            .get("/office/events")
            .add_header("cookie", format!("cg_session={session}"))
            .await;
        assert_eq!(events.status_code(), 200);
        assert!(events.text().contains("Signed in as Admin"));
        members::add_member(&ctx.db, &organizer_id, "door@example.com", Role::DoorStaff)
            .await
            .unwrap();
        let door_link =
            members::request_magic_link(&ctx.db, &organizer_id, "door@example.com", now)
                .await
                .unwrap()
                .unwrap();
        let door_session = members::claim_magic_link(&ctx.db, &organizer_id, &door_link, now + 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            request
                .get("/office/events")
                .add_header("cookie", format!("cg_session={door_session}"))
                .await
                .status_code(),
            403
        );
        assert_eq!(request.get("/api/auth/register").await.status_code(), 404);
        assert_eq!(
            request
                .post("/office/sign-out")
                .add_header("cookie", format!("cg_session={session}"))
                .await
                .status_code(),
            303
        );
        assert!(
            members::find_session(&ctx.db, &organizer_id, &session, now + 2)
                .await
                .unwrap()
                .is_none()
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cross_origin_posts_are_rejected() {
    request::<App, _, _>(|request, _| async move {
        assert_eq!(
            request
                .post("/office/sign-in")
                .add_header("host", "localhost")
                .add_header("origin", "https://outside.example")
                .await
                .status_code(),
            403
        );
        assert_eq!(
            request
                .post("/office/sign-in")
                .add_header("host", "localhost")
                .add_header("sec-fetch-site", "cross-site")
                .add_header("origin", "null")
                .await
                .status_code(),
            403
        );
        assert_eq!(
            request
                .post("/office/sign-in")
                .add_header("host", "local.crabgate.com")
                .add_header("sec-fetch-site", "same-site")
                .await
                .status_code(),
            403
        );
        assert_eq!(
            request
                .post("/office/sign-in")
                .form(&[("email", "unknown@example.com")])
                .add_header("host", "localhost")
                .add_header("sec-fetch-site", "same-origin")
                .add_header("origin", "null")
                .await
                .status_code(),
            303
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn magic_links_expire_and_requests_are_limited_to_three_per_hour() {
    request::<App, _, _>(|_request, ctx| async move {
        let organizer_id = local_organizer_id(&ctx).await;
        members::add_member(&ctx.db, &organizer_id, "door@example.com", Role::DoorStaff)
            .await
            .unwrap();
        let token = members::request_magic_link(&ctx.db, &organizer_id, "door@example.com", 1000)
            .await
            .unwrap()
            .unwrap();
        assert!(
            members::claim_magic_link(&ctx.db, &organizer_id, &token, 1901)
                .await
                .unwrap()
                .is_none()
        );
        for _ in 0..2 {
            assert!(
                members::request_magic_link(&ctx.db, &organizer_id, "door@example.com", 1001)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        assert!(matches!(
            members::request_magic_link(&ctx.db, &organizer_id, "door@example.com", 1002).await,
            Err(MemberError::RateLimited)
        ));
        assert!(
            members::request_magic_link(&ctx.db, &organizer_id, "door@example.com", 3600)
                .await
                .unwrap()
                .is_some()
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn self_hosted_admin_can_confirm_master_key_backup() {
    request::<App, _, _>(|request, ctx| async move {
        let organizer_id = local_organizer_id(&ctx).await;
        members::add_member(&ctx.db, &organizer_id, "key-admin@example.com", Role::Admin)
            .await
            .unwrap();
        let now = Utc::now().timestamp();
        let link =
            members::request_magic_link(&ctx.db, &organizer_id, "key-admin@example.com", now)
                .await
                .unwrap()
                .unwrap();
        let session = members::claim_magic_link(&ctx.db, &organizer_id, &link, now + 1)
            .await
            .unwrap()
            .unwrap();
        let cookie = format!("cg_session={session}");
        let status = request
            .get("/office/setup/master-key-status")
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(status.status_code(), 200);
        assert_eq!(
            status.json::<serde_json::Value>(),
            serde_json::json!({"show": true})
        );
        let setup = request
            .get("/office/setup")
            .add_header("cookie", &cookie)
            .await;
        assert!(setup.text().contains("Save your master key</h2>"));
        assert!(setup.text().contains("Check ticket emails"));
        assert_eq!(
            request
                .post("/office/setup/master-key")
                .add_header("cookie", &cookie)
                .await
                .status_code(),
            303
        );
        let status = request
            .get("/office/setup/master-key-status")
            .add_header("cookie", &cookie)
            .await;
        assert_eq!(
            status.json::<serde_json::Value>(),
            serde_json::json!({"show": false})
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn magic_links_use_url_safe_tokens() {
    request::<App, _, _>(|request, ctx| async move {
        let organizer_id = local_organizer_id(&ctx).await;
        let now = Utc::now().timestamp();
        for index in 0..12 {
            let email = format!("url-{index}@example.com");
            members::add_member(&ctx.db, &organizer_id, &email, Role::Admin)
                .await
                .unwrap();
            let token = members::request_magic_link(&ctx.db, &organizer_id, &email, now)
                .await
                .unwrap()
                .unwrap();
            assert!(token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'));
            let response = request.get(&format!("/office/magic-link/{token}")).await;
            assert_eq!(response.status_code(), 303);
        }
    })
    .await;
}
