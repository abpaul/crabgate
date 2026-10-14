use loco_rs::prelude::*;
use serde_json::json;

static MAGIC_LINK: Dir<'_> = include_dir!("src/mailers/member/magic_link");

pub struct MemberMailer;

impl Mailer for MemberMailer {}

impl MemberMailer {
    pub async fn send_magic_link(ctx: &AppContext, email: &str, url: &str) -> Result<()> {
        let args = mailer::Args {
            from: Some(crate::mailers::sender()),
            to: email.to_owned(),
            locals: json!({"logo_url": crate::mailers::logo_url(ctx),  "url": url }),
            ..Default::default()
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            Self::mail_template_now(ctx, &MAGIC_LINK, args),
        )
        .await
        .map_err(|_| Error::string("Member link acceptance is uncertain after timeout; another attempt may deliver a duplicate."))?
    }
}
