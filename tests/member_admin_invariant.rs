use crabgate::{
    app::App,
    members::{self, Role},
};
use loco_rs::testing::prelude::*;
use sea_orm::{ConnectOptions, ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};
use serial_test::serial;

#[tokio::test]
#[serial]
async fn inviting_the_only_admin_as_door_staff_keeps_admin_access() {
    request::<App, _, _>(|request, ctx| async move {
        let organizer_id: String = ctx
            .db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT id FROM organizers WHERE subdomain = 'local'",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "id")
            .unwrap();
        let id = members::add_member(&ctx.db, &organizer_id, "admin@example.com", Role::Admin)
            .await
            .unwrap();
        let now = crabgate::orders::now();
        let session =
            members::create_session_for_email(&ctx.db, &organizer_id, "admin@example.com", now)
                .await
                .unwrap()
                .unwrap();
        let cookie = format!("cg_session={session}");
        let invitation = request
            .post("/office/door/members")
            .add_header("cookie", &cookie)
            .form(&[("email", " ADMIN@EXAMPLE.COM "), ("role", "Door Staff")])
            .await;
        let admins = members::list_members(&ctx.db, &organizer_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|member| member.role == Role::Admin)
            .count();
        println!(
            "Sole-Admin Door Staff invitation: HTTP {}, remaining Admins: {admins}",
            invitation.status_code()
        );
        assert_eq!(admins, 1);
        assert_eq!(invitation.status_code(), 400);
        assert!(invitation.text().contains("last Admin"));
        assert_eq!(
            members::find_session(&ctx.db, &organizer_id, &session, now)
                .await
                .unwrap()
                .unwrap()
                .role,
            Role::Admin
        );
        assert_eq!(
            request
                .get("/office/events")
                .add_header("cookie", &cookie)
                .await
                .status_code(),
            200
        );
        assert_eq!(
            request
                .post(&format!("/office/door/members/{id}/remove"))
                .add_header("cookie", &cookie)
                .await
                .status_code(),
            400
        );
    })
    .await;
}

async fn control_database() -> (tempfile::TempDir, sea_orm::DatabaseConnection) {
    let dir = tempfile::tempdir().unwrap();
    let mut options = ConnectOptions::new(format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("control.db").display()
    ));
    options.min_connections(2).max_connections(2);
    let control = sea_orm::Database::connect(options).await.unwrap();
    let first = control.begin().await.unwrap();
    let second = tokio::time::timeout(std::time::Duration::from_secs(2), control.begin())
        .await
        .expect("a second connection must be available while the first transaction is held")
        .unwrap();
    second.rollback().await.unwrap();
    first.rollback().await.unwrap();
    loco_rs::db::migrate::<migration::Migrator>(&control)
        .await
        .unwrap();
    control.execute_raw(Statement::from_string(DatabaseBackend::Sqlite,
        "INSERT INTO organizers (id, name, subdomain, db_path) VALUES ('arts', 'Arts', 'arts', 'unused-arts.db'), ('other', 'Other', 'other', 'unused-other.db')"
    )).await.unwrap();
    (dir, control)
}

async fn admin_count(db: &sea_orm::DatabaseConnection, organizer: &str) -> usize {
    members::list_members(db, organizer)
        .await
        .unwrap()
        .into_iter()
        .filter(|member| member.role == Role::Admin)
        .count()
}

#[tokio::test]
async fn bootstrap_case_normalization_and_multi_admin_changes_keep_a_local_admin() {
    let (_dir, db) = control_database().await;
    let first = members::add_member(&db, "arts", " ADMIN@EXAMPLE.COM ", Role::Admin)
        .await
        .unwrap();
    assert_eq!(admin_count(&db, "arts").await, 1);
    assert_eq!(
        members::add_member(&db, "arts", "admin@example.com", Role::Admin)
            .await
            .unwrap(),
        first
    );
    members::add_member(&db, "other", "other@example.com", Role::Admin)
        .await
        .unwrap();
    assert!(matches!(
        members::add_member(&db, "arts", "Admin@Example.com", Role::DoorStaff).await,
        Err(members::MemberError::LastAdmin)
    ));
    assert!(!members::remove_member(&db, "arts", &first).await.unwrap());
    assert_eq!(members::list_members(&db, "arts").await.unwrap().len(), 1);
    let second = members::add_member(&db, "arts", "second@example.com", Role::Admin)
        .await
        .unwrap();
    assert_eq!(
        members::add_member(&db, "arts", " ADMIN@EXAMPLE.COM ", Role::DoorStaff)
            .await
            .unwrap(),
        first
    );
    assert_eq!(admin_count(&db, "arts").await, 1);
    assert!(members::remove_member(&db, "arts", &first).await.unwrap());
    assert!(!members::remove_member(&db, "arts", &second).await.unwrap());
    members::add_member(&db, "arts", "third@example.com", Role::Admin)
        .await
        .unwrap();
    assert!(members::remove_member(&db, "arts", &second).await.unwrap());
    assert_eq!(admin_count(&db, "arts").await, 1);
    assert!(!members::remove_member(&db, "arts", "unknown")
        .await
        .unwrap());
    assert_eq!(admin_count(&db, "other").await, 1);
}

#[tokio::test]
async fn simultaneous_demotions_of_two_admins_allow_only_one() {
    let (_dir, db) = control_database().await;
    members::add_member(&db, "arts", "first@example.com", Role::Admin)
        .await
        .unwrap();
    members::add_member(&db, "arts", "second@example.com", Role::Admin)
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        members::add_member(&db, "arts", "first@example.com", Role::DoorStaff),
        members::add_member(&db, "arts", "second@example.com", Role::DoorStaff),
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let rejected = first.err().or_else(|| second.err()).unwrap();
    assert!(matches!(rejected, members::MemberError::LastAdmin));
    assert_eq!(admin_count(&db, "arts").await, 1);
    assert_eq!(members::list_members(&db, "arts").await.unwrap().len(), 2);
}

#[tokio::test]
async fn simultaneous_removals_of_two_admins_allow_only_one() {
    let (_dir, db) = control_database().await;
    let first = members::add_member(&db, "arts", "first@example.com", Role::Admin)
        .await
        .unwrap();
    let second = members::add_member(&db, "arts", "second@example.com", Role::Admin)
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        members::remove_member(&db, "arts", &first),
        members::remove_member(&db, "arts", &second)
    );
    assert_eq!(
        usize::from(first.unwrap()) + usize::from(second.unwrap()),
        1
    );
    assert_eq!(admin_count(&db, "arts").await, 1);
    assert_eq!(members::list_members(&db, "arts").await.unwrap().len(), 1);
}

#[tokio::test]
async fn simultaneous_removal_and_demotion_keep_one_admin() {
    let (_dir, db) = control_database().await;
    let first = members::add_member(&db, "arts", "first@example.com", Role::Admin)
        .await
        .unwrap();
    members::add_member(&db, "arts", "second@example.com", Role::Admin)
        .await
        .unwrap();
    let (removed, demoted) = tokio::join!(
        members::remove_member(&db, "arts", &first),
        members::add_member(&db, "arts", "second@example.com", Role::DoorStaff)
    );
    assert_eq!(
        usize::from(removed.unwrap()) + usize::from(demoted.is_ok()),
        1
    );
    if let Err(error) = demoted {
        assert!(matches!(error, members::MemberError::LastAdmin));
    }
    assert_eq!(admin_count(&db, "arts").await, 1);
}
