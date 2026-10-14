#![allow(elided_lifetimes_in_paths)]
#![allow(clippy::wildcard_imports)]
pub use sea_orm_migration::prelude::*;
mod m20260930_000001_control;
mod m20260930_000002_organizer;
mod m20260930_000003_events;
mod m20260930_000004_checkout;
mod m20260930_000005_session_url;
mod m20260930_000006_door;
mod m20260930_000007_admin;
mod m20260930_000008_notices;
mod m20260930_000009_refund_events;
mod m20260930_000010_hosted;
mod m20260930_000011_order_resends;
mod m20260930_000012_onboarding;
mod m20260930_000013_event_changes;
mod m20260930_000014_billing_attempts;
mod m20260930_000015_questions;
mod m20260930_000016_discounts;
mod m20260930_000017_seats;
mod m20260930_000018_setup_requests;
mod m20260930_000019_series;

mod m20261007_000020_order_refund_work;

mod m20261007_000021_hosted_cleanup;

mod m20261007_000022_series_exceptions;

mod m20261008_000023_buyer_messages;

mod m20261008_000024_booking_terms;

mod m20261009_000025_lifecycle_recipients;
mod m20261009_000026_lifecycle_claims;
mod m20261009_000027_hold_refund_email_claims;
mod m20261009_000028_signup_rate_limits;
mod m20261009_000029_ticket_email_retries;
mod m20261011_000030_read_indexes;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260930_000001_control::Migration),
            Box::new(m20260930_000010_hosted::Migration),
            Box::new(m20260930_000012_onboarding::Migration),
            Box::new(m20260930_000014_billing_attempts::Migration),
            Box::new(m20260930_000018_setup_requests::Migration),
            Box::new(m20261007_000021_hosted_cleanup::Migration),
            Box::new(m20261009_000025_lifecycle_recipients::Migration),
            Box::new(m20261009_000026_lifecycle_claims::Migration),
            Box::new(m20261009_000028_signup_rate_limits::Migration),
        ]
    }
}

pub struct OrganizerMigrator;

#[async_trait::async_trait]
impl MigratorTrait for OrganizerMigrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260930_000002_organizer::Migration),
            Box::new(m20260930_000003_events::Migration),
            Box::new(m20260930_000004_checkout::Migration),
            Box::new(m20260930_000005_session_url::Migration),
            Box::new(m20260930_000006_door::Migration),
            Box::new(m20260930_000007_admin::Migration),
            Box::new(m20260930_000008_notices::Migration),
            Box::new(m20260930_000009_refund_events::Migration),
            Box::new(m20260930_000011_order_resends::Migration),
            Box::new(m20260930_000013_event_changes::Migration),
            Box::new(m20260930_000015_questions::Migration),
            Box::new(m20260930_000016_discounts::Migration),
            Box::new(m20260930_000017_seats::Migration),
            Box::new(m20260930_000019_series::Migration),
            Box::new(m20261007_000020_order_refund_work::Migration),
            Box::new(m20261007_000022_series_exceptions::Migration),
            Box::new(m20261008_000023_buyer_messages::Migration),
            Box::new(m20261008_000024_booking_terms::Migration),
            Box::new(m20261009_000027_hold_refund_email_claims::Migration),
            Box::new(m20261009_000029_ticket_email_retries::Migration),
            Box::new(m20261011_000030_read_indexes::Migration),
        ]
    }
}
