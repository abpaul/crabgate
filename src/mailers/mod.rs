pub mod event_change;
pub mod hosted;
pub mod late_payment;
pub mod member;
pub mod refund;
pub mod setup;
pub mod ticket;

pub fn logo_url(ctx: &loco_rs::app::AppContext) -> String {
    let origin =
        if crate::organizers::Edition::from_environment() == crate::organizers::Edition::Hosted {
            crate::hosted::origin(None)
        } else {
            ctx.config.server.full_url()
        };
    format!("{origin}/static/shared/crabgate-mark-email.png")
}

pub fn sender() -> String {
    std::env::var("CRABGATE_MAIL_FROM")
        .unwrap_or_else(|_| "CrabGate <hello@crabgate.com>".to_owned())
}
pub mod buyer_message;
