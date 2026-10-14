use loco_rs::prelude::*;
use serde_json::json;

static REFUND_EMAIL: Dir<'_> = include_dir!("src/mailers/late_payment/refund");

pub struct LatePaymentMailer;

impl Mailer for LatePaymentMailer {}

impl LatePaymentMailer {
    pub async fn send_refund(
        ctx: &AppContext,
        email: &str,
        organizer_name: &str,
        reply_to: Option<&str>,
    ) -> Result<()> {
        let args = mailer::Args {
            from: Some(crate::mailers::sender()),
            to: email.to_owned(),
            reply_to: reply_to.map(str::to_owned),
            locals: json!({"logo_url": crate::mailers::logo_url(ctx), "organizer_name": organizer_name}),
            ..Default::default()
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            Self::mail_template_now(ctx, &REFUND_EMAIL, args),
        )
        .await
        .map_err(|_| Error::string("Late-payment refund mail acceptance is uncertain after timeout; another attempt may deliver a duplicate."))?
    }
}
