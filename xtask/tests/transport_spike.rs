use std::time::Duration;

#[tokio::test]
async fn both_loopback_lifecycle_modes_discover_the_frozen_surface() {
    let (stateless, legacy) = tokio::time::timeout(
        Duration::from_secs(10),
        xtask::transport_spike::probe_loopback_transports(),
    )
    .await
    .expect("loopback transport spike timed out")
    .expect("loopback transport spike failed");

    assert_eq!((stateless, legacy), (6, 6));
}
