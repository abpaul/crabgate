use loco_rs::prelude::*;
use serde_json::json;

static CHANGE: Dir<'_> = include_dir!("src/mailers/event_change/notice");

pub struct EventChangeMailer;
impl Mailer for EventChangeMailer {}

pub struct Details<'a> {
    pub event_title: &'a str,
    pub order_number: &'a str,
    pub old_when: &'a str,
    pub new_when: &'a str,
    pub old_place: &'a str,
    pub new_place: &'a str,
}

impl EventChangeMailer {
    pub async fn send(
        ctx: &AppContext,
        email: &str,
        organizer_name: &str,
        details: Details<'_>,
        reply_to: Option<&str>,
    ) -> Result<()> {
        Self::mail_template_now(
            ctx,
            &CHANGE,
            mailer::Args {
                from: Some(crate::mailers::sender()),
                to: email.to_owned(),
                reply_to: reply_to.map(str::to_owned),
                locals: json!({"logo_url": crate::mailers::logo_url(ctx), 
                    "organizer_name": organizer_name, "event_title": details.event_title,
                    "order_number": details.order_number, "old_when": details.old_when, "new_when": details.new_when,
                    "old_place": details.old_place, "new_place": details.new_place
                }),
                ..Default::default()
            },
        )
        .await
    }
}
