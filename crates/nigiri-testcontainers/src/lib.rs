//! Ephemeral Bitcoin regtest fixtures backed by Testcontainers.

mod endpoint;
mod error;
mod image;

pub use endpoint::ElectrumEndpoint;
pub use error::FixtureError;
pub use image::ContainerImage;

#[cfg(test)]
const COMPATIBILITY_GATE_VERSION: &str = "nigiri-v0.5.17";

#[cfg(test)]
mod tests {
    #[test]
    fn compatibility_gate_version_is_recorded() {
        assert_eq!(super::COMPATIBILITY_GATE_VERSION, "nigiri-v0.5.17");
    }
}
