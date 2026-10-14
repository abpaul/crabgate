use loco_rs::prelude::*;
use serde_json::json;

static REFUND_EMAIL: Dir<'_> = include_dir!("src/mailers/refund/notice");
static CANCEL_EMAIL: Dir<'_> = include_dir!("src/mailers/refund/cancel");

pub struct RefundMailer;
impl Mailer for RefundMailer {}

impl RefundMailer {
    pub async fn send(
        ctx: &AppContext,
        email: &str,
        organizer_name: &str,
        order_number: &str,
        event_title: &str,
        cancelled: bool,
        reply_to: Option<&str>,
    ) -> Result<()> {
        let template = if cancelled {
            &CANCEL_EMAIL
        } else {
            &REFUND_EMAIL
        };
        Self::mail_template_now(ctx, template, mailer::Args {
            from: Some(crate::mailers::sender()),
            to: email.to_owned(),
            reply_to: reply_to.map(str::to_owned),
            locals: json!({"logo_url": crate::mailers::logo_url(ctx), "organizer_name": organizer_name, "order_number": order_number, "event_title": event_title}),
            ..Default::default()
        }).await
    }
}
