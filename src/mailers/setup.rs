use loco_rs::prelude::*;
use serde_json::json;

static TEST_EMAIL: Dir<'_> = include_dir!("src/mailers/setup/test");

pub struct SetupMailer;
impl Mailer for SetupMailer {}
impl SetupMailer {
    pub async fn send_test(ctx: &AppContext, email: &str, organizer_name: &str) -> Result<()> {
        let args = mailer::Args {
            from: Some(crate::mailers::sender()),
            to: email.to_owned(),
            locals: json!({"logo_url": crate::mailers::logo_url(ctx), "organizer_name": organizer_name}),
            ..Default::default()
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            Self::mail_template_now(ctx, &TEST_EMAIL, args),
        )
        .await
        .map_err(|_| Error::string("Setup mail acceptance is uncertain after timeout; another attempt may deliver a duplicate."))?
    }
}
