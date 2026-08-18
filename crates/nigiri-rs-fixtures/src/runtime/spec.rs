use crate::{ContainerImage, FixtureChain, FixtureError, node::merge_node_args};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ContainerSpec {
    pub(crate) service: &'static str,
    pub(crate) image: ContainerImage,
    pub(crate) entrypoint: Option<String>,
    pub(crate) name: String,
    pub(crate) network: String,
    pub(crate) command: Vec<String>,
    pub(crate) exposed_ports: Vec<u16>,
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
