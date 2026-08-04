use nigiri_rs::{Liquid, NigiriClient};

use crate::{
    ContainerImage, FixtureError, RPC_PASSWORD, RPC_USER, chain::FixtureChain, deadline::Deadline,
    node::bootstrap_error,
};

/// The OP_TRUE coins created in the genesis block, in satoshi. Reproduces Nigiri's value so the
/// chain this fixture builds is bit-identical to the one Nigiri runs.
const INITIAL_FREE_COINS: u64 = 2_100_000_000_000_000;

impl FixtureChain for Liquid {
    const NODE_SERVICE: &'static str = "elements";
    const CHAIN_NAME: &'static str = "Liquid";
    const NODE_RPC_PORT: u16 = 18_884;
    const ELECTRS_HTTP_PORT: u16 = 30_001;
    const ELECTRS_ELECTRUM_PORT: u16 = 50_001;
    const NODE_NAME_PREFIX: &'static str = "nigiri-rs-elements";

    fn node_image_default() -> ContainerImage {
        ContainerImage::elements_default()
    }

    fn electrs_image_default() -> ContainerImage {
        ContainerImage::electrs_liquid_default()
    }

    /// Reproduces Nigiri's `liquidregtest` chain without its configuration file, minimized to what
    /// this fixture actually needs.
    ///
    /// Verified 2026-08-03: this vector produces the same genesis hash, `current_params_root`,
    /// `current_signblock_hex`, and `current_fedpeg_script` as the node Nigiri runs from a
    /// bind-mounted `elements.conf`. The five parameters in that conf which this omits are
    /// rejected on argv and silently ignored in the conf; they do nothing either way.
    ///
    /// Also verified 2026-08-03: seven flags Nigiri sets could be dropped without changing the
    /// genesis hash or breaking the Docker-gated suite (Electrs sync included) — `-listen=1` and
    /// `-port=…` (a fixture is a single node with no peers), `-blockfilterindex=1`,
    /// `-peerblockfilters=1`, and `-checkblockindex=0` (Electrs does not need them either),
    /// `-anyonecanspendaremine=1` (already the default on custom chains), and
    /// `-con_dyna_deploy_signal=1` (affects deployment signaling in later blocks, not present at
    /// genesis or in anything the ported tests exercise). None of the survivors below are
    /// speculative: each is load-bearing for something this fixture or its test suite does.
    ///
    /// - `-txindex=1`: the ported tests call `getrawtransaction` on arbitrary txids.
    /// - `-fallbackfee=0.000001`: regtest has no fee-estimation history, so
    ///   `fundrawtransaction` (used to build the wallet transaction in the ported tests) has
    ///   nothing to estimate from without it.
    /// - `-validatepegin=0`: peg-in validation would require a Bitcoin mainchain node this
    ///   fixture does not run, which is also why `-mainchainrpc*` is omitted below: it is only
    ///   read when this is 1.
    /// - `-initialfreecoins=…` and `-con_connect_genesis_outputs=1`: the wallet's only funds.
    ///   Liquid has no block subsidy, so without connecting the genesis outputs to the UTXO set
    ///   the wallet stays empty no matter how much is mined.
    ///
    /// `-rpcport` is also given explicitly alongside `-rpcbind`, unlike Bitcoin, which pins its
    /// RPC port through `-rpcbind` alone: Elements' chain-section config model resolves the RPC
    /// port separately from the bind address, and this flag was part of the empirically verified
    /// minimal set above.
    fn node_cmd() -> Vec<String> {
        vec![
            "-chain=liquidregtest".to_owned(),
            "-server=1".to_owned(),
            "-txindex=1".to_owned(),
            format!("-rpcbind=0.0.0.0:{}", Self::NODE_RPC_PORT),
            "-rpcallowip=0.0.0.0/0".to_owned(),
            format!("-rpcport={}", Self::NODE_RPC_PORT),
            format!("-rpcuser={RPC_USER}"),
            format!("-rpcpassword={RPC_PASSWORD}"),
            "-validatepegin=0".to_owned(),
            format!("-initialfreecoins={INITIAL_FREE_COINS}"),
            "-fallbackfee=0.000001".to_owned(),
            "-con_connect_genesis_outputs=1".to_owned(),
            "-printtoconsole=1".to_owned(),
        ]
    }

    fn electrs_cmd(node_container: &str) -> Vec<String> {
        vec![
            "-vvvv".to_owned(),
            "--parent-network".to_owned(),
            "regtest".to_owned(),
            "--network".to_owned(),
            "liquidregtest".to_owned(),
            // Inert, exactly as on Bitcoin: `--jsonrpc-import` makes Electrs read blocks over
            // JSON-RPC rather than from the daemon directory, so this path is never opened.
            // Verified 2026-08-03 against a path that does not exist in the image.
            "--daemon-dir".to_owned(),
            "/tmp/liquid".to_owned(),
            "--db-dir".to_owned(),
            "/tmp/electrs".to_owned(),
            "--daemon-rpc-addr".to_owned(),
            format!("{node_container}:{}", Self::NODE_RPC_PORT),
            "--cookie".to_owned(),
            format!("{RPC_USER}:{RPC_PASSWORD}"),
            "--http-addr".to_owned(),
            format!("0.0.0.0:{}", Self::ELECTRS_HTTP_PORT),
            "--electrum-rpc-addr".to_owned(),
            format!("0.0.0.0:{}", Self::ELECTRS_ELECTRUM_PORT),
            "--cors".to_owned(),
            "*".to_owned(),
            "--jsonrpc-import".to_owned(),
        ]
    }

    /// Funds the wallet from the genesis outputs, then mines one block.
    ///
    /// Nothing here mines for money. Liquid's coins exist at height 0 and are spendable as soon as
    /// the wallet has rescanned; the single block exists only because callers reasonably expect a
    /// nonzero tip. This is why Liquid never touches the initial-mining permit Bitcoin needs.
    #[allow(
        private_interfaces,
        reason = "sealed trait; Deadline never crosses the crate boundary"
    )]
    async fn fund_wallet(
        client: &NigiriClient<Liquid>,
        deadline: &Deadline,
    ) -> Result<(), FixtureError> {
        deadline
            .run(
                Self::NODE_SERVICE,
                "rescanning for the genesis outputs",
                client.rpc::<serde_json::Value, _>("rescanblockchain", (0_u64,)),
            )
            .await?
            .map_err(|source| bootstrap_error(Self::CHAIN_NAME, "rescanblockchain", source))?;

        let address = deadline
            .run(
                Self::NODE_SERVICE,
                "creating initial mining address",
                client.new_address(),
            )
            .await?
            .map_err(|source| bootstrap_error(Self::CHAIN_NAME, "getnewaddress", source))?
            .to_string();

        deadline
            .run(
                Self::NODE_SERVICE,
                "mining the initial block",
                client.generate_to_address(1, address.as_str()),
            )
            .await?
            .map_err(|source| bootstrap_error(Self::CHAIN_NAME, "generatetoaddress", source))?;

        Ok(())
    }
}
