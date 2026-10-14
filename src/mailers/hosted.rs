use loco_rs::prelude::*;
use serde_json::json;

static VERIFY: Dir<'_> = include_dir!("src/mailers/hosted/verify");
static WARNING: Dir<'_> = include_dir!("src/mailers/hosted/warning");
static SIGNUP_NOTICE: Dir<'_> = include_dir!("src/mailers/hosted/signup_notice");

pub struct HostedMailer;

impl Mailer for HostedMailer {}

impl HostedMailer {
    pub async fn verify(ctx: &AppContext, email: &str, url: &str) -> Result<()> {
        Self::mail_template_now(
            ctx,
            &VERIFY,
            mailer::Args {
                from: Some(crate::mailers::sender()),
                to: email.to_owned(),
                locals: json!({"logo_url": crate::mailers::logo_url(ctx), "url": url}),
                ..Default::default()
            },
        )
        .await
    }

    pub async fn signup_notice(
        ctx: &AppContext,
        name: &str,
        email: &str,
        shop_url: &str,
        next_event: &str,
    ) -> Result<()> {
        let to = std::env::var("CRABGATE_SIGNUP_NOTICE_EMAIL")
            .unwrap_or_else(|_| "hello@crabgate.com".to_owned());
        Self::mail_template(
            ctx,
            &SIGNUP_NOTICE,
            mailer::Args {
                from: Some(crate::mailers::sender()),
                to,
                reply_to: Some(email.to_owned()),
                locals: json!({"logo_url": crate::mailers::logo_url(ctx), "name": name, "email": email, "shop_url": shop_url, "next_event": next_event}),
                ..Default::default()
            },
        )
        .await
    }

    pub async fn warning(ctx: &AppContext, email: &str, name: &str, days: i64) -> Result<()> {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            Self::mail_template_now(
                ctx,
                &WARNING,
                mailer::Args {
                    from: Some(crate::mailers::sender()),
                    to: email.to_owned(),
                    locals: json!({"logo_url": crate::mailers::logo_url(ctx), "name": name, "days": days}),
                    ..Default::default()
                },
            ),
        )
        .await
        .map_err(|_| Error::string("Lifecycle warning acceptance is uncertain after timeout; another attempt may deliver a duplicate."))?
    }
}
