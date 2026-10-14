use loco_rs::prelude::*;
use serde_json::{json, Value};

static TICKET_EMAIL: Dir<'_> = include_dir!("src/mailers/ticket/confirmation");

pub struct TicketMailer;

impl Mailer for TicketMailer {}

impl TicketMailer {
    pub async fn send_confirmation(
        ctx: &AppContext,
        email: &str,
        organizer_name: &str,
        order_number: &str,
        order_url: &str,
        tickets: Value,
        reply_to: Option<&str>,
    ) -> Result<()> {
        let args = mailer::Args {
            from: Some(crate::mailers::sender()),
            to: email.to_owned(),
            reply_to: reply_to.map(str::to_owned),
            locals: json!({"logo_url": crate::mailers::logo_url(ctx), "organizer_name": organizer_name, "order_number": order_number, "order_url": order_url, "tickets": tickets}),
            ..Default::default()
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            Self::mail_template_now(ctx, &TICKET_EMAIL, args),
        )
        .await
        .map_err(|_| Error::string("Ticket mail acceptance is uncertain after timeout; another attempt may deliver a duplicate."))?
    }
}
