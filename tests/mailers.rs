use crabgate::{
    app::App,
    mailers::{
        event_change::EventChangeMailer, hosted::HostedMailer, late_payment::LatePaymentMailer,
        member::MemberMailer, refund::RefundMailer, setup::SetupMailer, ticket::TicketMailer,
    },
};
use loco_rs::testing::prelude::*;
use serde_json::json;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn every_email_renders_and_escapes_supplied_html() {
    request::<App, _, _>(|_, mut ctx| async move {
        let unsafe_name = "<script>alert(1)</script>";
        let url = "https://example.com/sign-in?sig=abc&next=1";
        MemberMailer::send_magic_link(&ctx, "test@example.com", url).await.unwrap();
        HostedMailer::verify(&ctx, "test@example.com", url).await.unwrap();
        HostedMailer::warning(&ctx, "test@example.com", unsafe_name, 7).await.unwrap();
        SetupMailer::send_test(&ctx, "test@example.com", unsafe_name).await.unwrap();
        LatePaymentMailer::send_refund(&ctx, "test@example.com", unsafe_name, None).await.unwrap();
        RefundMailer::send(&ctx, "test@example.com", unsafe_name, "CG-123", unsafe_name, false, None).await.unwrap();
        RefundMailer::send(&ctx, "test@example.com", unsafe_name, "CG-123", unsafe_name, true, None).await.unwrap();
        TicketMailer::send_confirmation(&ctx, "test@example.com", unsafe_name, "CG-123", url,
            json!([{"ticket_type": unsafe_name, "attendee_name": unsafe_name, "code": "CG-ABC", "qr_url": url}]), Some("hello@example.com")).await.unwrap();
        EventChangeMailer::send(&ctx, "test@example.com", unsafe_name, crabgate::mailers::event_change::Details {
            event_title: unsafe_name, order_number: "CG-123", old_when: "Yesterday", new_when: "Tomorrow",
            old_place: unsafe_name, new_place: "New Hall",
        }, None).await.unwrap();
        let deliveries = ctx.mailer.as_ref().unwrap().deliveries();
        assert_eq!(deliveries.count, 9);
        for message in &deliveries.messages {
            let html = message.split("Content-Type: text/html").nth(1).unwrap();
            assert!(html.contains("background:#F4EFE4"));
            assert!(html.contains("#FFC933"));
            assert!(!html.contains("<script>alert(1)</script>"));
        }
        assert!(deliveries.messages.iter().any(|message| message.contains("CG-ABC")));
        assert!(deliveries.messages.iter().any(|message| message.contains("Reply-To: hello@example.com")));
        assert!(deliveries.messages.iter().any(|message| message.contains("&amp;next")));
        if std::env::var_os("CRABGATE_SEND_MAILPIT").is_some() {
            let smtp = ctx.config.mailer.as_ref().unwrap().smtp.as_ref().unwrap();
            ctx.mailer = Some(loco_rs::mailer::EmailSender::smtp(smtp).unwrap());
            MemberMailer::send_magic_link(&ctx, "preview@example.com", url).await.unwrap();
            HostedMailer::verify(&ctx, "preview@example.com", url).await.unwrap();
            HostedMailer::warning(&ctx, "preview@example.com", unsafe_name, 7).await.unwrap();
            SetupMailer::send_test(&ctx, "preview@example.com", unsafe_name).await.unwrap();
            LatePaymentMailer::send_refund(&ctx, "preview@example.com", unsafe_name, None).await.unwrap();
            RefundMailer::send(&ctx, "preview@example.com", unsafe_name, "CG-123", unsafe_name, false, None).await.unwrap();
            RefundMailer::send(&ctx, "preview@example.com", unsafe_name, "CG-123", unsafe_name, true, None).await.unwrap();
            TicketMailer::send_confirmation(&ctx, "preview@example.com", unsafe_name, "CG-123", url,
                json!([{"ticket_type": "General", "attendee_name": "Alex", "code": "CG-ABC", "qr_url": url}]), None).await.unwrap();
            EventChangeMailer::send(&ctx, "preview@example.com", unsafe_name, crabgate::mailers::event_change::Details {
                event_title: "Updated Event", order_number: "CG-123", old_when: "Yesterday", new_when: "Tomorrow",
                old_place: "Old Hall", new_place: "New Hall",
            }, None).await.unwrap();
        }
    }).await;
}
