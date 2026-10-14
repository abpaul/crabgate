use crabgate::links;
use serial_test::serial;

#[test]
#[serial]
fn order_link_expires_and_is_bound_to_organizer_and_order() {
    let previous = std::env::var("CRABGATE_MASTER_KEY").ok();
    std::env::set_var(
        "CRABGATE_MASTER_KEY",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    );
    let signature = links::sign_order("organizer-1", "order-1", 2_000).unwrap();
    assert!(links::verify_order(
        "organizer-1",
        "order-1",
        2_000,
        &signature,
        1_999
    ));
    assert!(!links::verify_order(
        "organizer-1",
        "order-1",
        2_000,
        &signature,
        2_000
    ));
    assert!(!links::verify_order(
        "organizer-2",
        "order-1",
        2_000,
        &signature,
        1_999
    ));
    assert!(!links::verify_order(
        "organizer-1",
        "order-2",
        2_000,
        &signature,
        1_999
    ));
    if let Some(value) = previous {
        std::env::set_var("CRABGATE_MASTER_KEY", value);
    } else {
        std::env::remove_var("CRABGATE_MASTER_KEY");
    }
}
