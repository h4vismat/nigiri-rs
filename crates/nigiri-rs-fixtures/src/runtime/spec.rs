use std::{fmt, net::IpAddr};

use nigiri_rs_core::Bitcoin;

use crate::{
    ContainerImage, FixtureChain, FixtureError, RPC_PASSWORD, RPC_USER,
    lnd::{BITCOIN_ZMQ_BLOCK_PORT, BITCOIN_ZMQ_TX_PORT, LND_GRPC_PORT, LND_PEER_PORT},
    node::merge_node_args,
};

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ContainerSpec {
    pub(crate) service: &'static str,
    pub(crate) image: ContainerImage,
    pub(crate) entrypoint: Option<String>,
    pub(crate) name: String,
    pub(crate) network: String,
    pub(crate) command: Vec<String>,
    pub(crate) exposed_ports: Vec<u16>,
}

// Commands can carry RPC credentials. Keeping the entire vector out of Debug prevents both the
// current bitcoind/LND passwords and future secret-bearing arguments from reaching diagnostics.
impl fmt::Debug for ContainerSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContainerSpec")
            .field("service", &self.service)
            .field("image", &self.image)
            .field("entrypoint", &self.entrypoint)
            .field("name", &self.name)
            .field("network", &self.network)
            .field("exposed_ports", &self.exposed_ports)
            .finish_non_exhaustive()
    }
}

pub(crate) fn node_spec<C: FixtureChain>(
    image: ContainerImage,
    network: String,
    name: String,
    extra_args: Vec<String>,
) -> Result<ContainerSpec, FixtureError> {
    image.validate()?;
    let entrypoint = image.entrypoint().map(str::to_owned);

    Ok(ContainerSpec {
        service: C::NODE_SERVICE,
        image,
        entrypoint,
        name,
        network,
        command: merge_node_args(C::node_cmd(), &extra_args),
        exposed_ports: vec![C::NODE_RPC_PORT],
    })
}

pub(crate) fn electrs_spec<C: FixtureChain>(
    image: ContainerImage,
    network: String,
    name: String,
    node_name: &str,
) -> Result<ContainerSpec, FixtureError> {
    image.validate()?;
    let entrypoint = image.entrypoint().map(str::to_owned);

    Ok(ContainerSpec {
        service: "electrs",
        image,
        entrypoint,
        name,
        network,
        command: C::electrs_cmd(node_name),
        exposed_ports: vec![C::ELECTRS_HTTP_PORT, C::ELECTRS_ELECTRUM_PORT],
    })
}

#[allow(
    dead_code,
    reason = "Task 6 specification is consumed by the Task 7 LndPair startup"
)]
pub(crate) fn lnd_spec(
    image: ContainerImage,
    network: String,
    name: String,
    bitcoind_name: &str,
    endpoint_host: &str,
) -> Result<ContainerSpec, FixtureError> {
    image.validate()?;
    let entrypoint = image.entrypoint().map(str::to_owned);
    let rpc_port = <Bitcoin as FixtureChain>::NODE_RPC_PORT;
    let tls_extra = if endpoint_host.parse::<IpAddr>().is_ok() {
        format!("--tlsextraip={endpoint_host}")
    } else {
        format!("--tlsextradomain={endpoint_host}")
    };

    Ok(ContainerSpec {
        service: "lnd",
        image,
        entrypoint,
        name,
        network,
        command: vec![
            "--bitcoin.active".to_owned(),
            "--bitcoin.regtest".to_owned(),
            "--bitcoin.node=bitcoind".to_owned(),
            format!("--bitcoind.rpchost={bitcoind_name}:{rpc_port}"),
            format!("--bitcoind.rpcuser={RPC_USER}"),
            format!("--bitcoind.rpcpass={RPC_PASSWORD}"),
            format!("--bitcoind.zmqpubrawblock=tcp://{bitcoind_name}:{BITCOIN_ZMQ_BLOCK_PORT}"),
            format!("--bitcoind.zmqpubrawtx=tcp://{bitcoind_name}:{BITCOIN_ZMQ_TX_PORT}"),
            format!("--rpclisten=0.0.0.0:{LND_GRPC_PORT}"),
            format!("--listen=0.0.0.0:{LND_PEER_PORT}"),
            tls_extra,
        ],
        // Docker networking does not require a published port for peers on the fixture network.
        // Only the host-facing gRPC API receives a random loopback mapping.
        exposed_ports: vec![LND_GRPC_PORT],
    })
}

#[cfg(test)]
mod tests {
    use nigiri_rs_core::{Bitcoin, Liquid};

    use super::{electrs_spec, node_spec};
    use crate::ContainerImage;

    #[test]
    fn node_spec_preserves_the_complete_runtime_contract() {
        let spec = node_spec::<Liquid>(
            ContainerImage::elements_default(),
            "fixture-network".to_owned(),
            "elements-node".to_owned(),
            vec!["-validatepegin=1".to_owned()],
        )
        .expect("the pinned Elements specification is valid");

        assert_eq!(spec.service, "elements");
        assert_eq!(spec.image.name(), "blockstream/elementsd");
        assert_eq!(spec.image.tag(), "23.3.3");
        assert_eq!(
            spec.image.digest(),
            Some("sha256:1abe3ae514662492279c9ba8adc94fea46a0fa60efdd62f4eb93d3e803adff37")
        );
        assert_eq!(spec.entrypoint.as_deref(), Some("elementsd"));
        assert_eq!(spec.name, "elements-node");
        assert_eq!(spec.network, "fixture-network");
        assert_eq!(spec.exposed_ports, vec![18_884]);
        assert!(spec.command.contains(&"-validatepegin=1".to_owned()));
        assert!(!spec.command.contains(&"-validatepegin=0".to_owned()));
    }

    #[test]
    fn electrs_spec_exposes_both_protocols_and_points_at_its_node() {
        let spec = electrs_spec::<Bitcoin>(
            ContainerImage::electrs_default(),
            "fixture-network".to_owned(),
            "bitcoin-electrs".to_owned(),
            "bitcoin-node",
        )
        .expect("the pinned Electrs specification is valid");

        assert_eq!(spec.service, "electrs");
        assert_eq!(spec.name, "bitcoin-electrs");
        assert_eq!(spec.network, "fixture-network");
        assert_eq!(spec.exposed_ports, vec![30_000, 50_000]);
        assert!(
            spec.command
                .windows(2)
                .any(|pair| pair == ["--daemon-rpc-addr", "bitcoin-node:18443"])
        );
    }
}
