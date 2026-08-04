//! What a chain contributes to a fixture. Everything else is shared.

use std::future::Future;

use nigiri_rs_core::{NigiriClient, NigiriNetwork};

use crate::{ContainerImage, FixtureError, deadline::Deadline};

mod bitcoin;
mod liquid;

/// A chain a fixture can start, and the six things that differ between chains.
///
/// Sealed: the container lifecycle, diagnostics, and teardown guarantees this crate makes
/// depend on internals a downstream implementation could not uphold.
pub trait FixtureChain: NigiriNetwork + Sized + private::Sealed + 'static {
    /// Names this chain's node in diagnostics, e.g. `"bitcoind"`.
    const NODE_SERVICE: &'static str;
    /// Names this chain in caller-facing error text, e.g. `"Bitcoin"`.
    const CHAIN_NAME: &'static str;
    const NODE_RPC_PORT: u16;
    const ELECTRS_HTTP_PORT: u16;
    const ELECTRS_ELECTRUM_PORT: u16;
    /// Prefix for this chain's node container, completed with the fixture's UUID scope.
    const NODE_NAME_PREFIX: &'static str;

    fn node_image_default() -> ContainerImage;
    fn electrs_image_default() -> ContainerImage;
    fn node_cmd() -> Vec<String>;
    fn electrs_cmd(node_container: &str) -> Vec<String>;

    /// Leaves the wallet spendable and the tip above zero.
    ///
    /// Everything before this point — container, root RPC, `createwallet`, the wallet-scoped
    /// client — is shared. What funds a wallet is not: Bitcoin mines a coinbase, Liquid has no
    /// block subsidy and must reach the genesis outputs instead.
    ///
    /// `Deadline` is crate-private even though this trait is `pub`: the trait is sealed, so no
    /// downstream crate can implement or call this, only the fixture start path within this crate.
    #[allow(
        private_interfaces,
        reason = "sealed trait; Deadline never crosses the crate boundary"
    )]
    fn fund_wallet<'a>(
        client: &'a NigiriClient<Self>,
        deadline: &'a Deadline,
    ) -> impl Future<Output = Result<(), FixtureError>> + Send + 'a;
}

pub(crate) mod private {
    pub trait Sealed {}

    impl Sealed for nigiri_rs_core::Bitcoin {}
    impl Sealed for nigiri_rs_core::Liquid {}
}

#[cfg(test)]
mod tests {
    use nigiri_rs_core::Bitcoin;

    use super::FixtureChain;
    use crate::ContainerImage;

    // Catches a regression that changes the Bitcoin topology contract every other module reads
    // through the trait: its service name, ports, container prefix, or pinned images.
    #[test]
    fn bitcoin_declares_the_regtest_topology() {
        assert_eq!(Bitcoin::NODE_SERVICE, "bitcoind");
        assert_eq!(Bitcoin::CHAIN_NAME, "Bitcoin");
        assert_eq!(Bitcoin::NODE_RPC_PORT, 18_443);
        assert_eq!(Bitcoin::ELECTRS_HTTP_PORT, 30_000);
        assert_eq!(Bitcoin::ELECTRS_ELECTRUM_PORT, 50_000);
        assert_eq!(Bitcoin::NODE_NAME_PREFIX, "nigiri-rs-bitcoind");
        assert_eq!(
            Bitcoin::node_image_default(),
            ContainerImage::bitcoind_default()
        );
        assert_eq!(
            Bitcoin::electrs_image_default(),
            ContainerImage::electrs_default()
        );
    }

    // Catches a regression that drops the credentials the fixture client will authenticate with,
    // or points the indexer at the wrong node port. The exact argument vector is deliberately not
    // asserted: a future configuration surface may produce it differently.
    #[test]
    fn bitcoin_commands_carry_credentials_and_the_node_endpoint() {
        let node = Bitcoin::node_cmd();
        assert!(node.iter().any(|argument| argument == "-rpcuser=admin1"));
        assert!(node.iter().any(|argument| argument == "-rpcpassword=123"));

        let electrs = Bitcoin::electrs_cmd("nigiri-rs-bitcoind-abc");
        assert!(electrs.iter().any(|argument| argument == "admin1:123"));
        assert!(
            electrs
                .iter()
                .any(|argument| argument == "nigiri-rs-bitcoind-abc:18443")
        );
    }

    // Catches a regression that changes the Liquid topology contract, and one that reintroduces a
    // parameter this Elements build rejects. `-con_dyna_deploy_start`,
    // `-con_nminerconfirmationwindow`, `-con_nrulechangeactivationthreshold`,
    // `-con_taproot_signal_start`, and `-pchmessagestart` appear in Nigiri's elements.conf but are
    // rejected outright on argv and are silently ignored in the conf; porting them makes the node
    // refuse to start. Verified 2026-08-03.
    #[test]
    fn liquid_declares_its_topology_and_omits_the_parameters_elements_rejects() {
        use nigiri_rs_core::Liquid;

        assert_eq!(Liquid::NODE_SERVICE, "elements");
        assert_eq!(Liquid::CHAIN_NAME, "Liquid");
        assert_eq!(Liquid::NODE_RPC_PORT, 18_884);
        assert_eq!(Liquid::ELECTRS_HTTP_PORT, 30_001);
        assert_eq!(Liquid::ELECTRS_ELECTRUM_PORT, 50_001);
        assert_eq!(Liquid::NODE_NAME_PREFIX, "nigiri-rs-elements");
        assert_eq!(
            Liquid::node_image_default(),
            ContainerImage::elements_default()
        );
        assert_eq!(
            Liquid::electrs_image_default(),
            ContainerImage::electrs_liquid_default()
        );

        let node = Liquid::node_cmd();
        assert!(node.iter().any(|argument| argument == "-rpcuser=admin1"));
        assert!(node.iter().any(|argument| argument == "-rpcpassword=123"));

        let electrs = Liquid::electrs_cmd("nigiri-rs-elements-abc");
        assert!(electrs.iter().any(|argument| argument == "admin1:123"));
        assert!(
            electrs
                .iter()
                .any(|argument| argument == "nigiri-rs-elements-abc:18884")
        );

        for rejected in [
            "-con_dyna_deploy_start",
            "-con_nminerconfirmationwindow",
            "-con_nrulechangeactivationthreshold",
            "-con_taproot_signal_start",
            "-pchmessagestart",
        ] {
            assert!(
                !node.iter().any(|argument| argument.starts_with(rejected)),
                "{rejected} is rejected by elementsd and must not be passed"
            );
        }
        // Without this the wallet cannot reach the genesis outputs and has no funds at all:
        // Liquid has no block subsidy, so mining does not fund anything.
        assert!(
            node.iter()
                .any(|argument| argument == "-con_connect_genesis_outputs=1")
        );
    }
}
