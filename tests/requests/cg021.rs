use crabgate::{
    app::App,
    booking_terms, data_transfer,
    events::{self, EventInput, TicketTypeInput},
    members::{self, Role},
    orders, settings,
};
use loco_rs::{app::AppContext, testing::prelude::*};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

async fn session(ctx: &AppContext, id: &str, email: &str, role: Role) -> String {
    members::add_member(&ctx.db, id, email, role).await.unwrap();
    let token = members::request_magic_link(&ctx.db, id, email, orders::now())
        .await
        .unwrap()
        .unwrap();
    let session = members::claim_magic_link(&ctx.db, id, &token, orders::now())
        .await
        .unwrap()
        .unwrap();
    format!("cg_session={session}")
}

async fn event(db: &DatabaseConnection, price: &str) -> (String, String) {
    let event = events::create(
        db,
        &EventInput {
            title: "Booking terms test".into(),
            date: "2027-06-15".into(),
            start_time: "19:00".into(),
            end_time: "21:00".into(),
            timezone: "Europe/Dublin".into(),
            venue: "Hall".into(),
            address: "Street".into(),
            description: String::new(),
            event_capacity: Some(20),
            max_per_order: 2,
            attendee_name_required: false,
        },
    )
    .await
    .unwrap();
    let kind = events::add_ticket_type(
        db,
        &event,
        &TicketTypeInput {
            name: "Standard".into(),
            price: price.into(),
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

fn token(html: &str) -> String {
    html.split("name=\"booking_terms_token\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned()
}

async fn hold_evidence(
    db: &DatabaseConnection,
    id: &str,
) -> (Option<String>, Option<i64>, Option<String>, Option<i64>) {
    let row = db.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT booking_terms_snapshot, booking_terms_accepted_at, buyer_email_encrypted, booking_terms_submitted_at FROM holds WHERE id = ?", [id.into()]
    )).await.unwrap().unwrap();
    (
        row.try_get("", "booking_terms_snapshot").unwrap(),
        row.try_get("", "booking_terms_accepted_at").unwrap(),
        row.try_get("", "buyer_email_encrypted").unwrap(),
        row.try_get("", "booking_terms_submitted_at").unwrap(),
    )
}

#[tokio::test]
#[serial]
async fn cg021_settings_http_validation_permissions_and_tenant_isolation() {
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let scope = registry.active_dbs().await.unwrap().remove(0);
        let admin = session(
            &ctx,
            &scope.organizer.id,
            "terms-admin@example.com",
            Role::Admin,
        )
        .await;
        let door = session(
            &ctx,
            &scope.organizer.id,
            "terms-door@example.com",
            Role::DoorStaff,
        )
        .await;
        let path = "/office/setup/booking-terms";
        let form = [("url", "https://example.com/booking"), ("required", "yes")];
        assert_eq!(request.post(path).form(&form).await.status_code(), 303);
        assert_eq!(
            request
                .post(path)
                .add_header("cookie", &door)
                .form(&form)
                .await
                .status_code(),
            403
        );
        assert_eq!(
            request
                .get(path)
                .add_header("cookie", &admin)
                .await
                .status_code(),
            405
        );
        assert!(settings::booking_terms(&scope.db)
            .await
            .unwrap()
            .url
            .is_empty());
        assert_eq!(
            request
                .post(path)
                .add_header("cookie", &admin)
                .add_header("origin", "https://evil.example")
                .form(&form)
                .await
                .status_code(),
            403
        );
        for url in [
            "",
            "http://example.com/terms",
            "javascript:alert(1)",
            "https://user:password@example.com/terms",
            "https://",
            "https://example.com/\nterms",
        ] {
            let response = request
                .post(path)
                .add_header("cookie", &admin)
                .form(&[("url", url), ("required", "yes")])
                .await;
            assert_eq!(response.status_code(), 303);
            assert_eq!(
                response.header("location"),
                "/office/setup?step=details&error=booking_terms"
            );
            assert!(settings::booking_terms(&scope.db)
                .await
                .unwrap()
                .url
                .is_empty());
        }
        let result = request
            .post(path)
            .add_header("cookie", &admin)
            .form(&form)
            .await;
        assert_eq!(result.status_code(), 303);
        let page = request
            .get("/office/setup?step=details")
            .add_header("cookie", &admin)
            .await;
        assert_eq!(page.status_code(), 200);
        assert!(page
            .text()
            .contains("value=\"https://example.com/booking\""));
        assert!(page
            .text()
            .contains("Require buyer acknowledgement at checkout"));
        let terms = settings::booking_terms(&scope.db).await.unwrap();
        assert_eq!(terms.url, "https://example.com/booking");
        assert!(terms.required);
        let escaped_url = "https://example.com/terms?x=\"onclick=\"alert(1)";
        assert_eq!(
            request
                .post(path)
                .add_header("cookie", &admin)
                .form(&[("url", escaped_url)])
                .await
                .status_code(),
            303
        );
        let (event, kind) = event(&scope.db, "0").await;
        let start = request
            .post("/checkout")
            .form(&[
                ("event_id".to_owned(), event),
                (format!("quantity_{kind}"), "1".into()),
            ])
            .await;
        assert_eq!(start.status_code(), 303);
        let page = request
            .get(start.header("location").to_str().unwrap())
            .await;
        assert_eq!(page.status_code(), 200);
        assert!(!page.text().contains("\"onclick=\""));
        assert!(page.text().contains("&quot;onclick=&quot;"));
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg021_free_guest_rejects_forged_stale_and_missing_ack_then_records_exact_evidence() {
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let scope = registry.active_dbs().await.unwrap().remove(0);
        let cipher = registry.cipher(&scope.organizer).unwrap();
        let admin = session(
            &ctx,
            &scope.organizer.id,
            "free-admin@example.com",
            Role::Admin,
        )
        .await;
        settings::set_contact(&scope.db, "", "https://example.com/privacy", "")
            .await
            .unwrap();
        settings::set_booking_terms(&scope.db, "https://example.com/terms-a", true)
            .await
            .unwrap();
        let (event, kind) = event(&scope.db, "0").await;
        let start = request
            .post("/checkout")
            .form(&[
                ("event_id".to_owned(), event.clone()),
                (format!("quantity_{kind}"), "1".into()),
            ])
            .await;
        assert_eq!(start.status_code(), 303);
        let path = start.header("location").to_str().unwrap().to_owned();
        let hold = path.strip_prefix("/checkout/").unwrap();
        let forged = request
            .post(&path)
            .form(&[
                ("email", "guest@example.com"),
                ("booking_terms_ack", "yes"),
                ("booking_terms_token", "forged"),
                ("booking_terms_url", "https://evil.example"),
            ])
            .await;
        assert_eq!(forged.status_code(), 400);
        assert!(forged.text().contains("Review the link below"));
        let evidence = hold_evidence(&scope.db, hold).await;
        assert_eq!((evidence.1, evidence.2), (None, None));
        let page = request
            .get(&path)
            .add_header("x-forwarded-host", "evil.example")
            .await;
        assert_eq!(page.status_code(), 200);
        assert!(page.text().contains("href=\"https://example.com/terms-a\""));
        assert!(page.text().contains("href=\"https://example.com/privacy\""));
        assert!(page.text().contains("value=\"yes\" required"));
        let old = token(&page.text());
        let missing = request
            .post(&path)
            .form(&[
                ("email", "guest@example.com"),
                ("booking_terms_token", old.as_str()),
            ])
            .await;
        assert_eq!(missing.status_code(), 400);
        assert!(missing.text().contains("Please acknowledge"));
        assert_eq!(hold_evidence(&scope.db, hold).await.1, None);
        settings::set_booking_terms(&scope.db, "https://example.com/terms-b", true)
            .await
            .unwrap();
        let changed = request
            .post(&path)
            .form(&[
                ("email", "guest@example.com"),
                ("booking_terms_token", old.as_str()),
                ("booking_terms_ack", "yes"),
            ])
            .await;
        assert_eq!(changed.status_code(), 400);
        assert!(changed.text().contains("Booking terms changed"));
        assert!(changed
            .text()
            .contains("href=\"https://example.com/terms-b\""));
        assert!(!changed.text().contains("value=\"yes\" checked"));
        let new = token(&changed.text());
        assert_ne!(old, new);
        assert_eq!(hold_evidence(&scope.db, hold).await.1, None);
        assert_eq!(
            request
                .post(&path)
                .form(&[
                    ("email", "guest@example.com"),
                    ("booking_terms_token", old.as_str()),
                    ("booking_terms_ack", "yes")
                ])
                .await
                .status_code(),
            400
        );
        let before = orders::now();
        let result = request
            .post(&path)
            .form(&[
                ("email", "guest@example.com"),
                ("booking_terms_token", new.as_str()),
                ("booking_terms_ack", "yes"),
            ])
            .await;
        assert_eq!(result.status_code(), 303);
        assert_eq!(result.header("location"), format!("{path}/confirmation"));
        let order_id = orders::order_id_for_hold(&scope.db, hold)
            .await
            .unwrap()
            .unwrap();
        let evidence = booking_terms::order_evidence(&scope.db, &order_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evidence.terms.url, "https://example.com/terms-b");
        assert_eq!(
            evidence.terms.wording,
            "I acknowledge the organizer's booking terms."
        );
        assert!(evidence.accepted_at >= before && evidence.accepted_at <= orders::now());
        let saved_time = evidence.accepted_at;
        settings::set_booking_terms(&scope.db, "https://example.com/terms-c", true)
            .await
            .unwrap();
        assert_eq!(
            request
                .post(&path)
                .form(&[("email", "other@example.com")])
                .await
                .status_code(),
            303
        );
        assert_eq!(
            booking_terms::order_evidence(&scope.db, &order_id)
                .await
                .unwrap()
                .unwrap()
                .accepted_at,
            saved_time
        );
        let order = orders::get_order(&scope.db, &cipher, &order_id)
            .await
            .unwrap();
        assert_eq!(order.buyer_email, "guest@example.com");
        assert_eq!(order.tickets.len(), 1);
        let detail = request
            .get(&format!("/office/orders/{order_id}"))
            .add_header("cookie", &admin)
            .await;
        assert_eq!(detail.status_code(), 200);
        assert!(detail.text().contains("https://example.com/terms-b"));
        assert!(detail.text().contains("Accepted:"));
        let issued = request
            .post("/office/orders/complimentary")
            .add_header("cookie", &admin)
            .form(&[
                ("ticket_type_id", kind.as_str()),
                ("email", "comp@example.com"),
                ("name", "Comp"),
                ("booking_terms_ack", "yes"),
            ])
            .await;
        assert_eq!(issued.status_code(), 303);
        let comp = issued
            .header("location")
            .to_str()
            .unwrap()
            .strip_prefix("/office/orders/")
            .unwrap()
            .split('?')
            .next()
            .unwrap()
            .to_owned();
        assert!(booking_terms::order_evidence(&scope.db, &comp)
            .await
            .unwrap()
            .is_none());
        let comp_page = request
            .get(&format!("/office/orders/{comp}"))
            .add_header("cookie", &admin)
            .await;
        assert_eq!(comp_page.status_code(), 200);
        assert!(comp_page.text().contains("No acknowledgement recorded."));
        assert_eq!(
            request
                .post(&format!("/office/orders/{order_id}/anonymize"))
                .add_header("cookie", &admin)
                .await
                .status_code(),
            303
        );
        assert_eq!(
            orders::get_order(&scope.db, &cipher, &order_id)
                .await
                .unwrap()
                .buyer_email,
            ""
        );
        assert_eq!(
            booking_terms::order_evidence(&scope.db, &order_id)
                .await
                .unwrap()
                .unwrap()
                .accepted_at,
            saved_time
        );
        let export = data_transfer::export(&scope.db, &cipher).await.unwrap();
        let target = registry.create("Imported", "cg021-import").await.unwrap();
        let target_cipher = registry.cipher(&target).unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), export).unwrap();
        data_transfer::import_fresh(
            file.path(),
            std::path::Path::new(&target.db_path),
            &target_cipher,
        )
        .await
        .unwrap();
        let imported = sea_orm::Database::connect(format!("sqlite://{}?mode=rw", target.db_path))
            .await
            .unwrap();
        let evidence = booking_terms::order_evidence(&imported, &order_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evidence.terms.url, "https://example.com/terms-b");
        assert_eq!(evidence.accepted_at, saved_time);
        assert!(booking_terms::order_evidence(&imported, &comp)
            .await
            .unwrap()
            .is_none());
    })
    .await;
}

fn callback(hold: &str, session: &str) -> (String, String) {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let body = serde_json::json!({"type":"checkout.session.completed","data":{"object":{"id":session,"payment_status":"paid","payment_intent":format!("pi_{session}"),"amount_total":1000,"currency":"eur","metadata":{"hold_id":hold}}}}).to_string();
    let timestamp = orders::now();
    let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec_cg021").unwrap();
    mac.update(format!("{timestamp}.{body}").as_bytes());
    (
        body,
        format!(
            "t={timestamp},v1={}",
            hex::encode(mac.finalize().into_bytes())
        ),
    )
}

#[tokio::test]
#[serial]
async fn cg021_submitted_no_terms_service_holds_keep_facts_on_guest_http_retry() {
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let scope = registry.active_dbs().await.unwrap().remove(0);
        let cipher = registry.cipher(&scope.organizer).unwrap();
        let (event, kind) = event(&scope.db, "0").await;
        for initial in ["disabled", "optional", "legacy-null"] {
            settings::set_booking_terms(
                &scope.db,
                if initial == "optional" {
                    "https://example.com/optional"
                } else {
                    ""
                },
                false,
            )
            .await
            .unwrap();
            let now = orders::now();
            let hold = crabgate::inventory::reserve(
                &scope.db,
                &event,
                &[crabgate::inventory::Selection {
                    ticket_type_id: kind.clone(),
                    quantity: 1,
                }],
                now,
            )
            .await
            .unwrap();
            if initial == "legacy-null" {
                scope
                    .db
                    .execute_raw(Statement::from_sql_and_values(
                        DatabaseBackend::Sqlite,
                        "UPDATE holds SET booking_terms_snapshot = NULL WHERE id = ?",
                        [hold.id.clone().into()],
                    ))
                    .await
                    .unwrap();
            }
            let details = orders::hold_details(&scope.db, &hold.id).await.unwrap();
            orders::save_details(
                &scope.db,
                &cipher,
                &details,
                "submitted@example.com",
                &[String::new()],
                false,
                now,
            )
            .await
            .unwrap();
            let before = hold_evidence(&scope.db, &hold.id).await;
            assert_eq!(before.1, None);
            assert_eq!(before.3, Some(now));
            settings::set_booking_terms(&scope.db, "https://example.com/current-required", true)
                .await
                .unwrap();
            let path = format!("/checkout/{}", hold.id);
            let page = request.get(&path).await;
            assert_eq!(page.status_code(), 200);
            assert!(!page.text().contains("https://example.com/current-required"));
            assert_eq!(hold_evidence(&scope.db, &hold.id).await, before);
            let token = token(&page.text());
            assert_eq!(token, "");
            let result = request
                .post(&path)
                .form(&[
                    ("email", "submitted@example.com"),
                    ("booking_terms_token", token.as_str()),
                    ("booking_terms_ack", "yes"),
                ])
                .await;
            assert_eq!(result.status_code(), 303);
            let id = orders::order_id_for_hold(&scope.db, &hold.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                orders::get_order(&scope.db, &cipher, &id)
                    .await
                    .unwrap()
                    .tickets
                    .len(),
                1
            );
            let after = hold_evidence(&scope.db, &hold.id).await;
            assert_eq!(
                (&after.0, after.1, after.3),
                (&before.0, before.1, before.3)
            );
            assert_eq!(
                orders::get_order(&scope.db, &cipher, &id)
                    .await
                    .unwrap()
                    .buyer_email,
                "submitted@example.com"
            );
            assert!(booking_terms::order_evidence(&scope.db, &id)
                .await
                .unwrap()
                .is_none());
        }
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg021_paid_guest_webhook_enforces_ack_and_keeps_submitted_snapshot_on_change() {
    let _mock = super::checkout_expiry::mock().await;
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let scope = registry.active_dbs().await.unwrap().remove(0);
        let cipher = registry.cipher(&scope.organizer).unwrap();
        settings::set(&scope.db, &cipher, "stripe_secret_key", "sk_test_synthetic")
            .await
            .unwrap();
        settings::set(&scope.db, &cipher, "webhook_secret", "whsec_cg021")
            .await
            .unwrap();
        settings::set_booking_terms(&scope.db, "https://example.com/paid-a", true)
            .await
            .unwrap();
        let (event, kind) = event(&scope.db, "10").await;
        for skip_ack in [true, false] {
            let start = request
                .post("/checkout")
                .form(&[
                    ("event_id".to_owned(), event.clone()),
                    (format!("quantity_{kind}"), "1".into()),
                ])
                .await;
            assert_eq!(start.status_code(), 303);
            let path = start.header("location").to_str().unwrap().to_owned();
            let hold = path.strip_prefix("/checkout/").unwrap();
            let row = scope
                .db
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT stripe_session_id FROM holds WHERE id = ?",
                    [hold.into()],
                ))
                .await
                .unwrap()
                .unwrap();
            let session: String = row.try_get("", "stripe_session_id").unwrap();
            let page = request.get(&path).await;
            assert_eq!(page.status_code(), 200);
            let token = token(&page.text());
            let missing = request
                .post(&path)
                .form(&[
                    ("email", "paid@example.com"),
                    ("booking_terms_token", token.as_str()),
                ])
                .await;
            assert_eq!(missing.status_code(), 400);
            if skip_ack {
                scope
                    .db
                    .execute_raw(Statement::from_sql_and_values(
                        DatabaseBackend::Sqlite,
                        "UPDATE holds SET buyer_email_encrypted = ? WHERE id = ?",
                        [
                            cipher
                                .encrypt("buyer_email", "paid@example.com")
                                .unwrap()
                                .into(),
                            hold.into(),
                        ],
                    ))
                    .await
                    .unwrap();
            } else {
                let submitted = request
                    .post(&path)
                    .form(&[
                        ("email", "paid@example.com"),
                        ("booking_terms_token", token.as_str()),
                        ("booking_terms_ack", "yes"),
                    ])
                    .await;
                assert_eq!(submitted.status_code(), 303);
                assert_eq!(
                    submitted.header("location"),
                    format!("https://checkout.stripe.com/{session}")
                );
                assert!(hold_evidence(&scope.db, hold).await.1.is_some());
                settings::set_booking_terms(&scope.db, "https://example.com/paid-b", true)
                    .await
                    .unwrap();
                let before = hold_evidence(&scope.db, hold).await;
                let details = orders::hold_details(&scope.db, hold).await.unwrap();
                orders::save_details(
                    &scope.db,
                    &cipher,
                    &details,
                    "paid@example.com",
                    &[String::new()],
                    false,
                    orders::now(),
                )
                .await
                .unwrap();
                let after = hold_evidence(&scope.db, hold).await;
                assert_eq!((after.0, after.1, after.3), (before.0, before.1, before.3));
                let page = request.get(&path).await;
                assert!(page.text().contains("href=\"https://example.com/paid-a\""));
            }
            let (body, signature) = callback(hold, &session);
            for _ in 0..2 {
                let response = request
                    .post("/webhooks/stripe")
                    .add_header("stripe-signature", &signature)
                    .add_header("content-type", "application/json")
                    .text(body.clone())
                    .await;
                assert_eq!(response.status_code(), 200, "{}", response.text());
            }
            let order = orders::order_id_for_hold(&scope.db, hold).await.unwrap();
            if skip_ack {
                assert_eq!(order, None);
                let row = scope
                    .db
                    .query_one_raw(Statement::from_sql_and_values(
                        DatabaseBackend::Sqlite,
                        "SELECT status FROM holds WHERE id = ?",
                        [hold.into()],
                    ))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(row.try_get::<String>("", "status").unwrap(), "Refunded");
            } else {
                let order = order.unwrap();
                let evidence = booking_terms::order_evidence(&scope.db, &order)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(evidence.terms.url, "https://example.com/paid-a");
                assert_eq!(
                    orders::get_order(&scope.db, &cipher, &order)
                        .await
                        .unwrap()
                        .tickets
                        .len(),
                    1
                );
                assert_eq!(
                    request
                        .get(&format!("{path}/confirmation"))
                        .await
                        .status_code(),
                    200
                );
            }
        }
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg021_hosted_settings_do_not_cross_tenants() {
    let prior = std::env::var("CRABGATE_EDITION").ok();
    std::env::set_var("CRABGATE_EDITION", "hosted");
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let a = registry.create("A", "cg021-a").await.unwrap();
        let b = registry.create("B", "cg021-b").await.unwrap();
        let admin = session(&ctx, &a.id, "a@example.com", Role::Admin).await;
        let form = [("url", "https://example.com/a"), ("required", "yes")];
        let path = "/office/setup/booking-terms";
        let response = request
            .post(path)
            .add_header("host", "cg021-a.crabgate.com")
            .add_header("cookie", &admin)
            .form(&form)
            .await;
        assert_eq!(response.status_code(), 303);
        let response = request
            .post(path)
            .add_header("host", "cg021-b.crabgate.com")
            .add_header("cookie", &admin)
            .form(&form)
            .await;
        assert_eq!(response.status_code(), 303);
        assert!(response
            .header("location")
            .to_str()
            .unwrap()
            .contains("sign-in"));
        let a_db = registry.resolve_organizer(&a).await.unwrap();
        let b_db = registry.resolve_organizer(&b).await.unwrap();
        assert_eq!(
            settings::booking_terms(&a_db.db).await.unwrap().url,
            "https://example.com/a"
        );
        assert_eq!(settings::booking_terms(&b_db.db).await.unwrap().url, "");
    })
    .await;
    if let Some(prior) = prior {
        std::env::set_var("CRABGATE_EDITION", prior);
    } else {
        std::env::remove_var("CRABGATE_EDITION");
    }
}

#[tokio::test]
#[serial]
async fn cg021_optional_disabled_and_toggle_changes_preserve_historical_orders() {
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let scope = registry.active_dbs().await.unwrap().remove(0);
        let (event, kind) = event(&scope.db, "0").await;
        let start = request
            .post("/checkout")
            .form(&[
                ("event_id".to_owned(), event.clone()),
                (format!("quantity_{kind}"), "1".into()),
            ])
            .await;
        assert_eq!(start.status_code(), 303);
        let path = start.header("location").to_str().unwrap().to_owned();
        assert_eq!(
            request
                .post(&path)
                .form(&[("email", "legacy@example.com")])
                .await
                .status_code(),
            303
        );
        let legacy = orders::order_id_for_hold(&scope.db, path.strip_prefix("/checkout/").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(booking_terms::order_evidence(&scope.db, &legacy)
            .await
            .unwrap()
            .is_none());
        settings::set_booking_terms(&scope.db, "https://example.com/optional", false)
            .await
            .unwrap();
        for acknowledge in [false, true] {
            let start = request
                .post("/checkout")
                .form(&[
                    ("event_id".to_owned(), event.clone()),
                    (format!("quantity_{kind}"), "1".into()),
                ])
                .await;
            let path = start.header("location").to_str().unwrap().to_owned();
            let page = request.get(&path).await;
            let token = token(&page.text());
            assert!(page.text().contains("(Optional)"));
            assert!(!page.text().contains("value=\"yes\" required"));
            let mut form = vec![
                ("email", "optional@example.com"),
                ("booking_terms_token", token.as_str()),
            ];
            if acknowledge {
                form.push(("booking_terms_ack", "yes"));
            }
            assert_eq!(request.post(&path).form(&form).await.status_code(), 303);
            let order =
                orders::order_id_for_hold(&scope.db, path.strip_prefix("/checkout/").unwrap())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(
                booking_terms::order_evidence(&scope.db, &order)
                    .await
                    .unwrap()
                    .is_some(),
                acknowledge
            );
        }
        let start = request
            .post("/checkout")
            .form(&[
                ("event_id".to_owned(), event.clone()),
                (format!("quantity_{kind}"), "1".into()),
            ])
            .await;
        let path = start.header("location").to_str().unwrap().to_owned();
        let page = request.get(&path).await;
        let old = token(&page.text());
        settings::set_booking_terms(&scope.db, "https://example.com/optional", true)
            .await
            .unwrap();
        let changed = request
            .post(&path)
            .form(&[
                ("email", "toggle@example.com"),
                ("booking_terms_token", old.as_str()),
                ("booking_terms_ack", "yes"),
            ])
            .await;
        assert_eq!(changed.status_code(), 400);
        assert!(changed.text().contains("Booking terms changed"));
        assert!(booking_terms::order_evidence(&scope.db, &legacy)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            request.post(&format!("{path}/cancel")).await.status_code(),
            303
        );
        let token = token(&changed.text());
        assert_eq!(
            request
                .post(&path)
                .form(&[
                    ("email", "toggle@example.com"),
                    ("booking_terms_token", token.as_str()),
                    ("booking_terms_ack", "yes")
                ])
                .await
                .status_code(),
            410
        );
        assert!(
            orders::order_id_for_hold(&scope.db, path.strip_prefix("/checkout/").unwrap())
                .await
                .unwrap()
                .is_none()
        );
    })
    .await;
}

#[tokio::test]
#[serial]
async fn cg021_invalid_details_questions_and_terms_roll_back_together() {
    request::<App, _, _>(|request, ctx| async move {
        let registry = super::organizer_registry(&ctx);
        let scope = registry.active_dbs().await.unwrap().remove(0);
        let cipher = registry.cipher(&scope.organizer).unwrap();
        settings::set_booking_terms(&scope.db, "https://example.com/questions", true)
            .await
            .unwrap();
        let (event, kind) = event(&scope.db, "0").await;
        let question = crabgate::questions::add(
            &scope.db,
            &event,
            "Order",
            "Text",
            "Meal preference",
            &[],
            true,
        )
        .await
        .unwrap();
        let field = format!("question_{question}");
        let start = request
            .post("/checkout")
            .form(&[
                ("event_id".to_owned(), event),
                (format!("quantity_{kind}"), "1".into()),
            ])
            .await;
        assert_eq!(start.status_code(), 303);
        let path = start.header("location").to_str().unwrap().to_owned();
        let hold = path.strip_prefix("/checkout/").unwrap();
        let page = request.get(&path).await;
        let token = token(&page.text());
        for (email, answer, ack, message) in [
            (
                "guest@example.com",
                "",
                "yes",
                "Please answer every required question.",
            ),
            ("bad", "Vegetarian", "yes", "That email address"),
            ("guest@example.com", "Vegetarian", "", "Please acknowledge"),
        ] {
            let result = request
                .post(&path)
                .form(&[
                    ("email", email),
                    (field.as_str(), answer),
                    ("booking_terms_token", token.as_str()),
                    ("booking_terms_ack", ack),
                ])
                .await;
            assert_eq!(result.status_code(), 400);
            assert!(result.text().contains(message), "{}", result.text());
            let evidence = hold_evidence(&scope.db, hold).await;
            assert_eq!((evidence.1, evidence.2), (None, None));
            let row = scope
                .db
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT COUNT(*) AS count FROM hold_answers WHERE hold_id = ?",
                    [hold.into()],
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.try_get::<i64>("", "count").unwrap(), 0);
        }
        let result = request
            .post(&path)
            .form(&[
                ("email", "guest@example.com"),
                (field.as_str(), "Vegetarian"),
                ("booking_terms_token", token.as_str()),
                ("booking_terms_ack", "yes"),
            ])
            .await;
        assert_eq!(result.status_code(), 303);
        let order = orders::order_id_for_hold(&scope.db, hold)
            .await
            .unwrap()
            .unwrap();
        let answers = crabgate::questions::order_answers(&scope.db, &cipher, &order)
            .await
            .unwrap();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].answer, "Vegetarian");
        assert!(booking_terms::order_evidence(&scope.db, &order)
            .await
            .unwrap()
            .is_some());
    })
    .await;
}
