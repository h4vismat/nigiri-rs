pub(crate) const LND_GRPC_PORT: u16 = 10_009;
pub(crate) const LND_PEER_PORT: u16 = 9_735;
pub(crate) const BITCOIN_ZMQ_BLOCK_PORT: u16 = 28_332;
pub(crate) const BITCOIN_ZMQ_TX_PORT: u16 = 28_333;
pub(crate) const TLS_CERT_PATH: &str = "/root/.lnd/tls.cert";

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use crate::{ContainerImage, RPC_PASSWORD, RPC_USER, runtime::lnd_spec};

    use super::{
        BITCOIN_ZMQ_BLOCK_PORT, BITCOIN_ZMQ_TX_PORT, LND_GRPC_PORT, LND_PEER_PORT, TLS_CERT_PATH,
    };

    // Catches a regression that publishes LND's peer/REST interfaces to the host, loses one of
    // bitcoind's private RPC/ZMQ routes, or makes the TLS certificate invalid for the mapped host.
    #[test]
    fn lnd_container_spec_has_exact_private_topology_and_host_grpc_contract() {
        let spec = lnd_spec(
            ContainerImage::lnd_default(),
            "nigiri-rs-network-abc".to_owned(),
            "nigiri-rs-lnd-alice-abc".to_owned(),
            "nigiri-rs-bitcoind-abc",
            "127.0.0.1",
        )
        .expect("the pinned LND specification is valid");

        assert_eq!(spec.service, "lnd");
        assert_eq!(spec.name, "nigiri-rs-lnd-alice-abc");
        assert_eq!(spec.network, "nigiri-rs-network-abc");
        assert_eq!(spec.entrypoint, None);
        assert_eq!(spec.exposed_ports, vec![LND_GRPC_PORT]);
        assert_eq!(LND_PEER_PORT, 9_735);
        assert_eq!(TLS_CERT_PATH, "/root/.lnd/tls.cert");
        assert_eq!(
            spec.command,
            vec![
                "--bitcoin.active".to_owned(),
                "--bitcoin.regtest".to_owned(),
                "--bitcoin.node=bitcoind".to_owned(),
                "--bitcoind.rpchost=nigiri-rs-bitcoind-abc:18443".to_owned(),
                format!("--bitcoind.rpcuser={RPC_USER}"),
                format!("--bitcoind.rpcpass={RPC_PASSWORD}"),
                format!(
                    "--bitcoind.zmqpubrawblock=tcp://nigiri-rs-bitcoind-abc:{BITCOIN_ZMQ_BLOCK_PORT}"
                ),
                format!(
                    "--bitcoind.zmqpubrawtx=tcp://nigiri-rs-bitcoind-abc:{BITCOIN_ZMQ_TX_PORT}"
                ),
                format!("--rpclisten=0.0.0.0:{LND_GRPC_PORT}"),
                format!("--listen=0.0.0.0:{LND_PEER_PORT}"),
                "--tlsextraip=127.0.0.1".to_owned(),
            ]
        );
        assert!(
            !spec
                .command
                .iter()
                .any(|argument| argument == "--noseedbackup")
        );
        let debug = format!("{spec:?}");
        assert!(!debug.contains(RPC_USER));
        assert!(!debug.contains(RPC_PASSWORD));
        assert!(!debug.contains("--bitcoind.rpcpass"));
    }

    // Catches a regression that emits an IP-only TLS flag for a remote engine hostname, which
    // would make the certificate unusable at the endpoint returned by the runtime.
    #[test]
    fn lnd_container_spec_uses_the_tls_flag_matching_the_endpoint_host_kind() {
        let spec = lnd_spec(
            ContainerImage::lnd_default(),
            "nigiri-rs-network-abc".to_owned(),
            "nigiri-rs-lnd-bob-abc".to_owned(),
            "nigiri-rs-bitcoind-abc",
            "engine.example",
        )
        .expect("a DNS runtime endpoint is valid");

        assert!("127.0.0.1".parse::<IpAddr>().is_ok());
        assert!("engine.example".parse::<IpAddr>().is_err());
        assert_eq!(
            spec.command.last().map(String::as_str),
            Some("--tlsextradomain=engine.example")
        );
    }
}
