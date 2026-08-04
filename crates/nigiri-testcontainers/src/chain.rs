//! What a chain contributes to a fixture. Everything else is shared.

use std::future::Future;

use nigiri_rs::{NigiriClient, NigiriNetwork};

use crate::{ContainerImage, FixtureError, deadline::Deadline};

mod bitcoin;

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

    impl Sealed for nigiri_rs::Bitcoin {}
    impl Sealed for nigiri_rs::Liquid {}
}

#[cfg(test)]
mod tests {
    use nigiri_rs::Bitcoin;

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
}
