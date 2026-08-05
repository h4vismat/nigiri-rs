use testcontainers::{
    ContainerAsync, ContainerRequest, GenericImage, ImageExt, core::IntoContainerPort,
    runners::AsyncRunner,
};
use url::Url;

use crate::{
    ContainerImage, ElectrumEndpoint, FixtureError,
    chain::FixtureChain,
    deadline::Deadline,
    endpoint::mapped_http_url,
    owned_start::{classify_start_error, mapped_port, run_owned_start},
};

pub(crate) const SERVICE: &str = "electrs";

/// A running Electrs and the two endpoints a fixture serves from it.
pub(crate) struct StartedElectrs {
    pub(crate) container: ContainerAsync<GenericImage>,
    pub(crate) esplora_url: Url,
    pub(crate) electrum_endpoint: ElectrumEndpoint,
}

/// Starts Electrs against an already-running node and resolves both of its mapped ports.
///
/// Electrs is reached only through mapped ports, never the fixed container ports, so concurrent
/// fixtures cannot collide on the host.
pub(crate) async fn start_electrs<C: FixtureChain>(
    image: &ContainerImage,
    network_name: &str,
    container_name: &str,
    node_name: &str,
    deadline: &Deadline,
) -> Result<StartedElectrs, FixtureError> {
    let container_request = request::<C>(image, network_name, container_name, node_name)?;
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
        C::ELECTRS_HTTP_PORT,
        "resolving the Electrs Esplora mapped port",
        deadline,
    )
    .await?;
    let electrum_port = mapped_port(
        SERVICE,
        &container,
        C::ELECTRS_ELECTRUM_PORT,
        "resolving the Electrs Electrum mapped port",
        deadline,
    )
    .await?;

    Ok(StartedElectrs {
        container,
        esplora_url: mapped_http_url(&host, esplora_port)?,
        electrum_endpoint: ElectrumEndpoint::new(host, electrum_port).map_err(|_| {
            FixtureError::InvalidConfiguration {
                detail: "container runtime returned an invalid mapped Electrum endpoint".to_owned(),
            }
        })?,
    })
}

pub(crate) fn request<C: FixtureChain>(
    image: &ContainerImage,
    network_name: &str,
    container_name: &str,
    node_name: &str,
) -> Result<ContainerRequest<GenericImage>, FixtureError> {
    image.validate()?;

    // No entrypoint unless the image descriptor carries one. Every Electrs image entrypoints its own
    // binary, at a path that differs between them (`/bin/electrs` on Mempool's, `/build/electrs` on
    // Nigiri's), so hard-coding one here breaks every other image — which is exactly what an earlier
    // hard-coded `/build/electrs` did to the Mempool images.
    let mut generic = GenericImage::new(image.name().to_owned(), image.testcontainers_tag());
    if let Some(entrypoint) = image.entrypoint() {
        generic = generic.with_entrypoint(entrypoint);
    }

    Ok(generic
        .with_exposed_port(C::ELECTRS_HTTP_PORT.tcp())
        .with_exposed_port(C::ELECTRS_ELECTRUM_PORT.tcp())
        .with_network(network_name)
        .with_container_name(container_name)
        .with_cmd(C::electrs_cmd(node_name)))
}

#[cfg(test)]
mod tests {
    use testcontainers::{Image, core::IntoContainerPort};

    use super::request;
    use crate::{ContainerImage, FixtureError};

    // Catches a regression that exposes the wrong indexer ports or drops the fixture topology.
    // The argument vector itself is the chain's business and is not asserted here or in
    // `chain::tests`; it is exercised end-to-end by the Docker-gated fixture suites, which start a
    // real indexer against a real node — a dropped or mistyped flag surfaces there as a fixture
    // that never reaches readiness.
    #[test]
    fn request_exposes_the_chains_indexer_ports() {
        use nigiri_rs_core::Bitcoin;

        let request = super::request::<Bitcoin>(
            &ContainerImage::electrs_default(),
            "nigiri-test-fixture",
            "nigiri-electrs-fixture",
            "nigiri-bitcoind-fixture",
        )
        .expect("the pinned Electrs image is valid");

        // Guards against a regression that reintroduces a hard-coded entrypoint: the binary lives
        // at a different path in each Electrs image, so pinning one path here breaks every other
        // image, including any a caller supplies through the builder. An image that genuinely needs
        // one carries it on its own descriptor, which `node::tests` covers for Elements.
        assert_eq!(request.entrypoint(), None);
        assert_eq!(request.expose_ports(), &[30_000.tcp(), 50_000.tcp()]);
        assert_eq!(request.network().as_deref(), Some("nigiri-test-fixture"));
        // Guards against a regression that passes `image.tag()` instead of
        // `image.testcontainers_tag()`: both compile and every other assertion here would still
        // pass, but the container would pull a floating `latest` instead of the pinned
        // `tag@digest`, silently unpinning the image the crate's docs promise is pinned.
        assert_eq!(
            request.image().name(),
            ContainerImage::electrs_default().name()
        );
        assert_eq!(
            request.image().tag(),
            ContainerImage::electrs_default().testcontainers_tag()
        );
    }

    // Catches a regression that defers invalid image validation until Docker request startup.
    #[test]
    fn request_rejects_invalid_images_before_constructing_a_request() {
        use nigiri_rs_core::Bitcoin;

        let error = match request::<Bitcoin>(
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
