use testcontainers::{
    ContainerAsync, ContainerRequest, GenericImage, ImageExt, core::IntoContainerPort,
    runners::AsyncRunner,
};
use url::Url;

use crate::{
    ContainerImage, ElectrumEndpoint, FixtureError, RPC_PASSWORD, RPC_USER,
    deadline::Deadline,
    endpoint::mapped_http_url,
    owned_start::{classify_start_error, mapped_port, run_owned_start},
};

pub(crate) const SERVICE: &str = "electrs";
const HTTP_PORT: u16 = 30_000;
const ELECTRUM_PORT: u16 = 50_000;

/// A running Electrs and the two endpoints a fixture serves from it.
pub(crate) struct StartedElectrs {
    pub(crate) container: ContainerAsync<GenericImage>,
    pub(crate) esplora_url: Url,
    pub(crate) electrum_endpoint: ElectrumEndpoint,
}

/// Starts Electrs against an already-running Bitcoind and resolves both of its mapped ports.
///
/// Electrs is reached only through mapped ports, never the fixed container ports, so concurrent
/// fixtures cannot collide on the host.
pub(crate) async fn start_electrs(
    image: &ContainerImage,
    network_name: &str,
    container_name: &str,
    bitcoin_name: &str,
    deadline: &Deadline,
) -> Result<StartedElectrs, FixtureError> {
    let container_request = request(image, network_name, container_name, bitcoin_name)?;
    let container = run_owned_start(
        SERVICE,
        image,
        deadline,
        container_name,
        "starting Electrs container",
        container_request.start(),
    )
    .await?;

    let host = deadline
        .run(SERVICE, "resolving the Electrs host", container.get_host())
        .await?
        .map_err(|error| classify_start_error(SERVICE, image, error))?
        .to_string();
    let esplora_port = mapped_port(
        SERVICE,
        &container,
        HTTP_PORT,
        "resolving the Electrs Esplora mapped port",
        deadline,
    )
    .await?;
    let electrum_port = mapped_port(
        SERVICE,
        &container,
        ELECTRUM_PORT,
        "resolving the Electrs Electrum mapped port",
        deadline,
    )
    .await?;

    Ok(StartedElectrs {
        container,
        esplora_url: mapped_http_url(&host, esplora_port)?,
        electrum_endpoint: ElectrumEndpoint::new(host, electrum_port)?,
    })
}

pub(crate) fn request(
    image: &ContainerImage,
    network_name: &str,
    container_name: &str,
    bitcoin_name: &str,
) -> Result<ContainerRequest<GenericImage>, FixtureError> {
    image.validate()?;

    Ok(
        GenericImage::new(image.name().to_owned(), image.testcontainers_tag())
            .with_entrypoint("/build/electrs")
            .with_exposed_port(HTTP_PORT.tcp())
            .with_exposed_port(ELECTRUM_PORT.tcp())
            .with_network(network_name)
            .with_container_name(container_name)
            .with_cmd([
                "-vvvv".to_owned(),
                "--network".to_owned(),
                "regtest".to_owned(),
                "--daemon-dir".to_owned(),
                "/tmp/bitcoin".to_owned(),
                "--db-dir".to_owned(),
                "/tmp/electrs".to_owned(),
                "--daemon-rpc-addr".to_owned(),
                format!("{bitcoin_name}:{}", crate::bitcoind::RPC_PORT),
                "--cookie".to_owned(),
                format!("{RPC_USER}:{RPC_PASSWORD}"),
                "--http-addr".to_owned(),
                "0.0.0.0:30000".to_owned(),
                "--electrum-rpc-addr".to_owned(),
                "0.0.0.0:50000".to_owned(),
                "--cors".to_owned(),
                "*".to_owned(),
                "--jsonrpc-import".to_owned(),
            ]),
    )
}

#[cfg(test)]
mod tests {
    use testcontainers::{ContainerRequest, GenericImage, Image, core::IntoContainerPort};

    use super::request;
    use crate::{ContainerImage, FixtureError};

    fn command(request: &ContainerRequest<GenericImage>) -> Vec<String> {
        request
            .cmd()
            .map(|argument| argument.into_owned())
            .collect()
    }

    // Catches a request regression that changes Electrs's pinned image, topology, entrypoint,
    // ports, daemon endpoint, credentials, or index data paths.
    #[test]
    fn request_preserves_the_exact_regtest_indexer_contract() {
        let request = request(
            &ContainerImage::electrs_default(),
            "nigiri-test-fixture",
            "nigiri-electrs-fixture",
            "nigiri-bitcoind-fixture",
        )
        .expect("the pinned Electrs image is valid");

        assert_eq!(request.image().name(), "ghcr.io/vulpemventures/electrs");
        assert_eq!(
            request.image().tag(),
            "latest@sha256:999a2218f423c0fb167ee53b282aa7929a9d4abba38ef16f67f407acd00589d4"
        );
        assert_eq!(request.entrypoint(), Some("/build/electrs"));
        assert_eq!(request.expose_ports(), &[30_000.tcp(), 50_000.tcp()]);
        assert_eq!(request.network().as_deref(), Some("nigiri-test-fixture"));
        assert_eq!(
            request.container_name().as_deref(),
            Some("nigiri-electrs-fixture")
        );
        assert_eq!(
            command(&request),
            [
                "-vvvv",
                "--network",
                "regtest",
                "--daemon-dir",
                "/tmp/bitcoin",
                "--db-dir",
                "/tmp/electrs",
                "--daemon-rpc-addr",
                "nigiri-bitcoind-fixture:18443",
                "--cookie",
                "admin1:123",
                "--http-addr",
                "0.0.0.0:30000",
                "--electrum-rpc-addr",
                "0.0.0.0:50000",
                "--cors",
                "*",
                "--jsonrpc-import",
            ]
        );
    }

    // Catches a regression that defers invalid image validation until Docker request startup.
    #[test]
    fn request_rejects_invalid_images_before_constructing_a_request() {
        let error = match request(
            &ContainerImage::new("registry.example/electrs", ""),
            "nigiri-test-fixture",
            "nigiri-electrs-fixture",
            "nigiri-bitcoind-fixture",
        ) {
            Err(error) => error,
            Ok(_) => panic!("an image without a tag must be rejected"),
        };

        assert!(matches!(error, FixtureError::InvalidConfiguration { .. }));
    }
}
