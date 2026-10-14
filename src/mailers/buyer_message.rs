use loco_rs::{app::AppContext, mailer::Email};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted,
    Failed,
    Uncertain,
}

pub fn prepare(email: &str, subject: &str, body: &str, reply_to: Option<&str>) -> Email {
    let escaped = body
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;");
    Email {
        from: Some(crate::mailers::sender()),
        to: email.to_owned(),
        reply_to: reply_to.map(str::to_owned),
        subject: subject.to_owned(),
        text: body.to_owned(),
        html: format!("<!doctype html><html><body><pre style=\"white-space:pre-wrap;font-family:sans-serif\">{escaped}</pre></body></html>"),
        ..Default::default()
    }
}

pub async fn send(ctx: &AppContext, email: &Email) -> Outcome {
    let Some(mailer) = &ctx.mailer else {
        return Outcome::Failed;
    };
    match tokio::time::timeout(std::time::Duration::from_secs(60), mailer.mail(email)).await {
        Ok(Ok(())) => Outcome::Accepted,
        Ok(Err(loco_rs::Error::EmailSender(_) | loco_rs::Error::ParseAddress(_))) => {
            Outcome::Failed
        }
        Ok(Err(loco_rs::Error::Smtp(error))) if error.status().is_some() => Outcome::Failed,
        _ => Outcome::Uncertain,
    }
}
