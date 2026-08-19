use nigiri_rs_lnd::{LND_PROTO_COMMIT, LND_PROTO_VERSION};

#[test]
fn generated_protocol_baseline_is_the_pinned_lnd_release() {
    assert_eq!(LND_PROTO_VERSION, "v0.21.1-beta");
    assert_eq!(LND_PROTO_COMMIT, "2b87887");
}
