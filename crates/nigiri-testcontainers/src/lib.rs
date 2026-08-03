//! Ephemeral Bitcoin regtest fixtures backed by Testcontainers.

mod bitcoind;
mod deadline;
mod diagnostics;
mod electrs;
mod electrum;
mod endpoint;
mod error;
mod image;
mod readiness;

pub use endpoint::ElectrumEndpoint;
pub use error::FixtureError;
pub use image::ContainerImage;

/// The fixture's regtest RPC credentials.
///
/// Declared once for the whole crate: the service requests, the client configuration, and the
/// redaction patterns all derive from these, so they cannot drift apart.
pub(crate) const RPC_USER: &str = "admin1";
pub(crate) const RPC_PASSWORD: &str = "123";

#[cfg(test)]
const COMPATIBILITY_GATE_VERSION: &str = "nigiri-v0.5.17";

#[cfg(test)]
mod tests {
    #[test]
    fn compatibility_gate_version_is_recorded() {
        assert_eq!(super::COMPATIBILITY_GATE_VERSION, "nigiri-v0.5.17");
    }
}
