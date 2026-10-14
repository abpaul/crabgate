mod discovery;
mod door;
mod event_editor;
mod members;
mod organizers;
mod page;
mod series;
mod stripe_webhook_setup;

mod cancellation;

mod checkout_expiry;
mod ticket_resend;

mod cg063;

fn organizer_registry(
    ctx: &loco_rs::app::AppContext,
) -> std::sync::Arc<crabgate::organizers::OrganizerRegistry> {
    ctx.shared_store
        .get::<std::sync::Arc<crabgate::initializers::organizers::OrganizerRuntime>>()
        .unwrap()
        .registry()
}

mod cg018;

mod cg021;

mod cg024;

mod cg025;

mod cg026;

fn evidence_path(name: &str) -> std::path::PathBuf {
    let root = std::env::var_os("CRABGATE_TEST_EVIDENCE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let directory = tempfile::Builder::new()
        .prefix("crabgate-request-")
        .tempdir_in(root)
        .unwrap()
        .keep();
    println!("Request evidence: {}", directory.display());
    directory.join(name)
}

#[test]
fn repeated_evidence_names_preserve_both_results() {
    let first = evidence_path("response.html");
    let second = evidence_path("response.html");
    assert_ne!(first, second);
    std::fs::write(&first, "first response").unwrap();
    std::fs::write(&second, "second response").unwrap();
    assert_eq!(std::fs::read_to_string(first).unwrap(), "first response");
    assert_eq!(std::fs::read_to_string(second).unwrap(), "second response");
}
