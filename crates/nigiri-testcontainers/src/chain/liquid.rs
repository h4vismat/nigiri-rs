use nigiri_rs::{Liquid, NigiriClient};

use crate::{
    ContainerImage, FixtureError, RPC_PASSWORD, RPC_USER, chain::FixtureChain, deadline::Deadline,
    node::bootstrap_error,
};

/// The Elements peer-to-peer port. Never mapped: a fixture is a single node with no peers, but
/// Elements still binds it.
const P2P_PORT: u16 = 18_886;

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

    /// Reproduces Nigiri's `liquidregtest` chain without its configuration file.
    ///
    /// Verified 2026-08-03: this vector produces the same genesis hash, `current_params_root`,
    /// `current_signblock_hex`, and `current_fedpeg_script` as the node Nigiri runs from a
    /// bind-mounted `elements.conf`. The five parameters in that conf which this omits are
    /// rejected on argv and silently ignored in the conf; they do nothing either way.
    ///
    /// This is Nigiri's set, not a minimal one. Seven of these are provably droppable; see the
    /// design document before removing any, and gate every removal on the genesis test plus a
    /// real Electrs sync.
    fn node_cmd() -> Vec<String> {
        vec![
            "-chain=liquidregtest".to_owned(),
            "-server=1".to_owned(),
            "-listen=1".to_owned(),
            "-txindex=1".to_owned(),
            format!("-rpcbind=0.0.0.0:{}", Self::NODE_RPC_PORT),
            "-rpcallowip=0.0.0.0/0".to_owned(),
            format!("-rpcport={}", Self::NODE_RPC_PORT),
            format!("-rpcuser={RPC_USER}"),
            format!("-rpcpassword={RPC_PASSWORD}"),
            format!("-port={P2P_PORT}"),
            "-blockfilterindex=1".to_owned(),
            "-peerblockfilters=1".to_owned(),
            "-checkblockindex=0".to_owned(),
            // Peg-in validation would require a Bitcoin mainchain node this fixture does not run,
            // which is also why `-mainchainrpc*` is omitted: it is only read when this is 1.
            "-validatepegin=0".to_owned(),
            format!("-initialfreecoins={INITIAL_FREE_COINS}"),
            "-fallbackfee=0.000001".to_owned(),
            "-con_dyna_deploy_signal=1".to_owned(),
            // The wallet's only funds. Liquid has no block subsidy, so without connecting the
            // genesis outputs to the UTXO set the wallet stays empty no matter how much is mined.
            "-con_connect_genesis_outputs=1".to_owned(),
            "-anyonecanspendaremine=1".to_owned(),
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
