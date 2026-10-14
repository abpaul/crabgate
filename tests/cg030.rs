use crabgate::{
    app::App,
    initializers::organizers::OrganizerRuntime,
    members::{self, Role},
};
use loco_rs::{app::Hooks, boot::StartMode, environment::Environment};
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

fn session_cookie(
    response: &reqwest::Response,
    name: &str,
    secure: bool,
    domain: Option<&str>,
) -> String {
    let raw = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap();
    let attributes: Vec<_> = raw.split(';').map(str::trim).collect();
    assert!(attributes[0].starts_with(&format!("{name}=")));
    assert!(attributes.contains(&"HttpOnly"));
    assert!(attributes.contains(&"SameSite=Lax"));
    assert!(attributes.contains(&"Path=/"));
    assert_eq!(attributes.contains(&"Secure"), secure);
    assert_eq!(
        attributes.iter().find_map(|a| a.strip_prefix("Domain=")),
        domain
    );
    attributes[0].to_owned()
}

#[tokio::test]
#[serial_test::serial]
async fn hosted_public_sign_in_scopes_sessions_and_revokes_access() {
    for environment in [Environment::Test, Environment::Production] {
        let root = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
            .unwrap_or_else(std::env::temp_dir);
        let storage = tempfile::Builder::new()
            .prefix("cg030-http-")
            .tempdir_in(root)
            .unwrap()
            .keep();
        println!(
            "synthetic HTTP evidence: {} ({environment:?})",
            storage.display()
        );
        let variables = [
            ("CRABGATE_EDITION", "hosted".to_owned()),
            ("SHOP_BASE_DOMAIN", "cg030.example.test".to_owned()),
            ("SHOP_SCHEME", "https".to_owned()),
            (
                "CRABGATE_OPERATOR_EMAILS",
                "operator@example.test".to_owned(),
            ),
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
        let mut config = App::load_config(&Environment::Test).await.unwrap();
        config.database.uri = format!("sqlite://{}?mode=rwc", storage.join("control.db").display());
        let mailer = config.mailer.as_mut().unwrap();
        mailer.stub = false;
        let smtp_config = mailer.smtp.as_mut().unwrap();
        smtp_config.host = "127.0.0.1".into();
        smtp_config.port = port;
        smtp_config.secure = false;
        smtp_config.auth = None;
        let boot = App::boot(StartMode::ServerOnly, &environment, config)
            .await
            .unwrap();
        let ctx = boot.app_context;
        let runtime = ctx.shared_store.get::<Arc<OrganizerRuntime>>().unwrap();
        let registry = runtime.registry();
        let a = registry.create("CG030 Alpha", "alpha").await.unwrap();
        registry.create("CG030 Beta", "beta").await.unwrap();
        members::add_member(&ctx.db, &a.id, "admin@example.test", Role::Admin)
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server =
            tokio::spawn(async move { axum::serve(listener, boot.router.unwrap()).await.unwrap() });
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap();
        let base = "cg030.example.test";
        let alpha = "alpha.cg030.example.test";
        let beta = "beta.cg030.example.test";
        let get =
            |host: &str, path: &str| client.get(format!("{origin}{path}")).header("host", host);
        let post = |host: &str, path: &str| {
            client
                .post(format!("{origin}{path}"))
                .header("host", host)
                .header("origin", format!("https://{host}"))
        };
        assert_eq!(get(base, "/").send().await.unwrap().status().as_u16(), 200);
        assert_eq!(
            get(base, "/sign-in")
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            200
        );
        let entry = get(base, "/sign-in?shop=alpha").send().await.unwrap();
        assert_eq!(entry.status().as_u16(), 303);
        assert_eq!(
            entry.headers()["location"],
            "https://alpha.cg030.example.test/office/sign-in"
        );
        let page = get(alpha, "/office/sign-in").send().await.unwrap();
        assert_eq!(page.status().as_u16(), 200);
        assert!(page
            .text()
            .await
            .unwrap()
            .contains("Email me a sign-in link"));
        assert_eq!(
            get(alpha, "/office/events")
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            303
        );
        let requested = post(alpha, "/office/sign-in")
            .form(&[("email", "admin@example.test")])
            .send()
            .await
            .unwrap();
        assert_eq!(requested.status().as_u16(), 303);
        assert_eq!(requested.headers()["location"], "/office/sign-in?sent=1");
        let token = delivered_token(
            &mut receiver,
            "https://alpha.cg030.example.test/office/magic-link/",
        )
        .await;
        let path = format!("/office/magic-link/{token}");
        assert_eq!(
            get(beta, &path).send().await.unwrap().status().as_u16(),
            401
        );
        let claim = get(alpha, &path).send().await.unwrap();
        assert_eq!(claim.status().as_u16(), 303);
        assert_eq!(claim.headers()["location"], "/office/events");
        let cookie = session_cookie(
            &claim,
            "cg_session",
            environment == Environment::Production,
            None,
        );
        assert_eq!(
            get(alpha, &path).send().await.unwrap().status().as_u16(),
            401
        );
        let office = get(alpha, "/office/events")
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(office.status().as_u16(), 200);
        assert!(office.text().await.unwrap().contains("Signed in as Admin"));
        let other = get(beta, "/office/events")
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(other.status().as_u16(), 303);
        assert_eq!(other.headers()["location"], "/office/sign-in");
        assert_eq!(
            get("missing.cg030.example.test", "/office/events")
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            404
        );
        assert_eq!(
            get(base, "/operator/sign-in")
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            200
        );
        assert_eq!(
            post(base, "/operator/sign-in")
                .form(&[("email", "operator@example.test")])
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            303
        );
        let token = delivered_token(
            &mut receiver,
            "https://cg030.example.test/operator/magic-link/",
        )
        .await;
        let claim = get(base, &format!("/operator/magic-link/{token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(claim.status().as_u16(), 303);
        assert_eq!(
            get(base, &format!("/operator/magic-link/{token}"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            401
        );
        let operator_cookie = session_cookie(
            &claim,
            "cg_operator_session",
            environment == Environment::Production,
            Some(base),
        );
        assert_eq!(
            get(base, "/operator")
                .header("cookie", &operator_cookie)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            200
        );
        assert_eq!(
            get(alpha, "/office/events")
                .header("cookie", &operator_cookie)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            403
        );
        assert_eq!(
            get(beta, "/office/events")
                .header("cookie", &operator_cookie)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            403
        );
        let grant = post(alpha, "/office/access-grants")
            .header("cookie", &cookie)
            .form(&[("hours", "24")])
            .send()
            .await
            .unwrap();
        assert_eq!(grant.status().as_u16(), 303);
        assert_eq!(grant.headers()["location"], "/office/access-grants");
        let granted_office = get(alpha, "/office/events")
            .header("cookie", &operator_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(granted_office.status().as_u16(), 200);
        assert!(granted_office.text().await.unwrap().contains("CG030 Alpha"));
        assert_eq!(
            get(beta, "/office/events")
                .header("cookie", &operator_cookie)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            403
        );
        let grants = get(alpha, "/office/access-grants")
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(grants.status().as_u16(), 200);
        let grants_page = grants.text().await.unwrap();
        assert!(grants_page.contains("Grant temporary read access to CG030 Alpha."));
        let revoke_path = grants_page
            .split_once("action=\"/office/access-grants/")
            .expect("public grant revocation form")
            .1
            .split_once('"')
            .unwrap()
            .0;
        assert!(revoke_path.ends_with("/revoke"));
        let revoked = post(alpha, &format!("/office/access-grants/{revoke_path}"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(revoked.status().as_u16(), 303);
        assert_eq!(revoked.headers()["location"], "/office/access-grants");
        assert_eq!(
            get(alpha, "/office/events")
                .header("cookie", &operator_cookie)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            403
        );
        let member_logout = post(alpha, "/office/sign-out")
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(member_logout.status().as_u16(), 303);
        let deletion: Vec<_> = member_logout.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .map(str::trim)
            .collect();
        assert_eq!(deletion[0], "cg_session=");
        assert!(deletion.contains(&"Path=/"));
        assert!(deletion.contains(&"Max-Age=0"));
        assert!(!deletion
            .iter()
            .any(|attribute| attribute.starts_with("Domain=")));
        assert_eq!(
            get(alpha, "/office/events")
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            303
        );
        let logout = post(base, "/operator/sign-out")
            .header("cookie", &operator_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(logout.status().as_u16(), 303);
        let deletion = logout.headers()["set-cookie"].to_str().unwrap();
        assert!(deletion.contains("Domain=cg030.example.test"));
        assert!(deletion.contains("Path=/"));
        assert!(deletion.contains("Max-Age=0"));
        assert_eq!(
            get(base, "/operator")
                .header("cookie", &operator_cookie)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            303
        );
        println!("{environment:?}: normal HTTP sign-in, cookie attributes, cross-tenant denial, tenant-scoped operator grant, grant revocation and logout passed; TLS and browser cookie delivery untested");
        server.abort();
        let _ = server.await;
        smtp.abort();
        let _ = smtp.await;
        runtime.shutdown().await.unwrap();
        for (key, value) in prior {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial]
async fn hosted_member_invitation_limit_preserves_membership_without_sending_email() {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    let root = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir);
    let storage = tempfile::Builder::new()
        .prefix("cg033-invite-http-")
        .tempdir_in(root)
        .unwrap()
        .keep();
    println!("synthetic invitation HTTP evidence: {}", storage.display());
    let variables = [
        ("CRABGATE_EDITION", "hosted".to_owned()),
        ("SHOP_BASE_DOMAIN", "cg033.example.test".to_owned()),
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
    let mut config = App::load_config(&Environment::Test).await.unwrap();
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
    let alpha = registry.create("CG033 Alpha", "alpha").await.unwrap();
    let beta = registry.create("CG033 Beta", "beta").await.unwrap();
    members::add_member(&ctx.db, &alpha.id, "admin@example.test", Role::Admin)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server =
        tokio::spawn(async move { axum::serve(listener, boot.router.unwrap()).await.unwrap() });
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();
    let get = |host: &str, path: &str| client.get(format!("{origin}{path}")).header("host", host);
    let post = |host: &str, path: &str| {
        client
            .post(format!("{origin}{path}"))
            .header("host", host)
            .header("origin", format!("http://{host}"))
    };
    let host = "alpha.cg033.example.test";
    let other = "beta.cg033.example.test";
    let invited = "staff@example.test";
    let path = "/office/door/members";
    assert_eq!(
        post(host, "/office/sign-in")
            .form(&[("email", "admin@example.test")])
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        303
    );
    let token = delivered_token(
        &mut receiver,
        "http://alpha.cg033.example.test/office/magic-link/",
    )
    .await;
    let claim = get(host, &format!("/office/magic-link/{token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(claim.status().as_u16(), 303);
    let admin = session_cookie(&claim, "cg_session", false, None);
    let page = get(host, "/office/door")
        .header("cookie", &admin)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status().as_u16(), 200);
    assert!(page
        .text()
        .await
        .unwrap()
        .contains("action=\"/office/door/members\""));
    for (target, cookie, expected) in [(host, "", 303), (other, admin.as_str(), 303)] {
        assert_eq!(
            post(target, path)
                .header("cookie", cookie)
                .form(&[("email", invited), ("role", "Door Staff")])
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            expected
        );
    }
    let last_admin = post(host, path)
        .header("cookie", &admin)
        .form(&[("email", " ADMIN@EXAMPLE.TEST "), ("role", "Door Staff")])
        .send()
        .await
        .unwrap();
    assert_eq!(last_admin.status().as_u16(), 400);
    assert!(last_admin.text().await.unwrap().contains("last Admin"));
    let mut tokens = Vec::new();
    for _ in 0..3 {
        let response = post(host, path)
            .header("cookie", &admin)
            .form(&[("email", invited), ("role", "Door Staff")])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 303);
        assert_eq!(response.headers()["location"], "/office/door");
        tokens.push(
            delivered_token(
                &mut receiver,
                "http://alpha.cg033.example.test/office/magic-link/",
            )
            .await,
        );
    }
    assert_ne!(tokens[0], tokens[1]);
    assert_ne!(tokens[1], tokens[2]);
    assert_ne!(tokens[0], tokens[2]);
    let claim = get(host, &format!("/office/magic-link/{}", tokens[2]))
        .send()
        .await
        .unwrap();
    assert_eq!(claim.status().as_u16(), 303);
    assert_eq!(claim.headers()["location"], "/door");
    let staff = session_cookie(&claim, "cg_session", false, None);
    let blocked_message = "Member saved, but the invitation email was not sent. Too many tries. Please wait a little and send the invite again.";
    for role in ["Admin", "Door Staff"] {
        let response = post(host, path)
            .header("cookie", &admin)
            .form(&[("email", " STAFF@EXAMPLE.TEST "), ("role", role)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 429);
        assert_eq!(response.text().await.unwrap(), blocked_message);
        let members = members::list_members(&ctx.db, &alpha.id).await.unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(
            members
                .iter()
                .find(|member| member.email == invited)
                .unwrap()
                .role
                .as_str(),
            role
        );
    }
    assert_eq!(
        get(host, "/door")
            .header("cookie", &staff)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );
    assert_eq!(
        post(host, path)
            .header("cookie", &staff)
            .form(&[("email", "unauthorized@example.test"), ("role", "Admin")])
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        403
    );
    assert_eq!(
        get(host, "/office/data/export")
            .header("cookie", &staff)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        403
    );
    assert_eq!(
        get(other, "/door")
            .header("cookie", &staff)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        303
    );
    let id = members::list_members(&ctx.db, &alpha.id)
        .await
        .unwrap()
        .into_iter()
        .find(|member| member.email == invited)
        .unwrap()
        .id;
    assert_eq!(
        post(host, &format!("{path}/{id}/remove"))
            .header("cookie", &admin)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        303
    );
    let recreated = post(host, path)
        .header("cookie", &admin)
        .form(&[("email", invited), ("role", "Door Staff")])
        .send()
        .await
        .unwrap();
    assert_eq!(recreated.status().as_u16(), 429);
    assert_eq!(recreated.text().await.unwrap(), blocked_message);
    let members = members::list_members(&ctx.db, &alpha.id).await.unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(
        members
            .iter()
            .filter(|member| member.role == Role::Admin)
            .count(),
        1
    );
    assert_eq!(
        members
            .iter()
            .find(|member| member.email == invited)
            .unwrap()
            .role,
        Role::DoorStaff
    );
    assert!(members::list_members(&ctx.db, &beta.id)
        .await
        .unwrap()
        .is_empty());
    let links: i64 = ctx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS total FROM magic_links".to_owned(),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "total")
        .unwrap();
    assert_eq!(links, 4);
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    println!("Public HTTP: three invites including last allowed delivered unique links; three blocked invites returned429, saved roles/recreated membership, issued no extra email/link; last Admin, staff and tenant boundaries passed");
    server.abort();
    let _ = server.await;
    smtp.abort();
    let _ = smtp.await;
    runtime.shutdown().await.unwrap();
    for (key, value) in prior {
        if let Some(value) = value {
            std::env::set_var(key, value);
        } else {
            std::env::remove_var(key);
        }
    }
}
