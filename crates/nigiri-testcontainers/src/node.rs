use std::time::Duration;

use nigiri_rs::{NigiriClient, NigiriConfig, NigiriError};
use testcontainers::{
    ContainerAsync, ContainerRequest, GenericImage, ImageExt, core::IntoContainerPort,
    runners::AsyncRunner,
};
use url::Url;
use uuid::Uuid;

use crate::{
    ContainerImage, FixtureError, RPC_PASSWORD, RPC_USER,
    chain::FixtureChain,
    deadline::Deadline,
    diagnostics::{MAX_SOURCE_BYTES, redacted_head, redacted_source, redacted_tail},
    endpoint::mapped_http_url,
    owned_start::{attach_container_log, classify_start_error, mapped_port, run_owned_start},
    readiness::RETRY_DELAY,
};

pub(crate) struct StartedNode {
    pub(crate) container: ContainerAsync<GenericImage>,
    pub(crate) client_config: NigiriConfig,
}

pub(crate) fn request<C: FixtureChain>(
    image: &ContainerImage,
    network_name: &str,
    container_name: &str,
) -> Result<ContainerRequest<GenericImage>, FixtureError> {
    image.validate()?;

    Ok(
        GenericImage::new(image.name().to_owned(), image.testcontainers_tag())
            .with_exposed_port(C::NODE_RPC_PORT.tcp())
            .with_network(network_name)
            .with_container_name(container_name)
            .with_cmd(C::node_cmd()),
    )
}

pub(crate) async fn start_node<C: FixtureChain>(
    image: &ContainerImage,
    network_name: &str,
    container_name: &str,
    deadline: &Deadline,
) -> Result<StartedNode, FixtureError> {
    let container_request = request::<C>(image, network_name, container_name)?;
    let container = run_owned_start(
        C::NODE_SERVICE,
        image,
        deadline,
        container_name,
        "starting node container",
        container_request.start(),
    )
    .await?;

    let host = deadline
        .run(C::NODE_SERVICE, "resolving node host", container.get_host())
        .await?
        .map_err(|error| classify_start_error(C::NODE_SERVICE, image, error))?
        .to_string();
    let rpc_port = mapped_port(
        C::NODE_SERVICE,
        &container,
        C::NODE_RPC_PORT,
        "resolving node RPC mapped port",
        deadline,
    )
    .await?;

    let root_url = mapped_http_url(&host, rpc_port)?;
    let root_config = fixture_rpc_config(
        root_url.clone(),
        deadline.remaining_or_expired(C::NODE_SERVICE, "configuring the root node RPC client")?,
    );
    let root = fixture_client::<C>(root_config)?;
    if let Err(not_ready) = wait_for_root_rpc::<C>(&root, deadline).await {
        // A node that never answered is the likeliest startup failure, and its own log is the
        // only thing that explains why, so the timeout carries a bounded tail of it.
        return Err(attach_container_log(C::NODE_SERVICE, not_ready, &container).await);
    }

    let wallet_name = format!("nigiri-rs-{}", Uuid::new_v4().simple());
    let wallet_creation = deadline
        .run(
            C::NODE_SERVICE,
            "creating node wallet",
            root.rpc("createwallet", (&wallet_name,)),
        )
        .await?;
    let _: serde_json::Value =
        wallet_creation.map_err(|source| bootstrap_error(C::CHAIN_NAME, "createwallet", source))?;

    let wallet_url = wallet_rpc_url(&root_url, &wallet_name)?;
    // This client outlives startup, so it gets the whole startup budget as its request timeout
    // rather than whatever is left of it: every startup RPC below is bounded by the shared
    // deadline anyway, and a caller must not inherit a timeout that depends on how slow startup
    // happened to be.
    let client_config = fixture_rpc_config(wallet_url, deadline.budget());
    let client = fixture_client::<C>(client_config.clone())?;

    C::fund_wallet(&client, deadline).await?;

    Ok(StartedNode {
        container,
        client_config,
    })
}

/// Builds a fixture RPC client, keeping the rejected configuration out of the error chain.
///
/// `FixtureError::Client` forwards a `NigiriError` and its whole raw cause chain, and a rejected
/// fixture configuration carries the fixture credentials, so the rejection is reported as bounded,
/// redacted configuration detail instead.
pub(crate) fn fixture_client<C: FixtureChain>(
    config: NigiriConfig,
) -> Result<NigiriClient<C>, FixtureError> {
    NigiriClient::<C>::with_config(config).map_err(|source| FixtureError::InvalidConfiguration {
        detail: redacted_head(
            &format!("fixture RPC client configuration was rejected: {source}"),
            MAX_SOURCE_BYTES,
        ),
    })
}

/// The node RPC half of a fixture client's configuration.
///
/// `esplora_url` is a self-pointing placeholder: only the node half is known here, and
/// `FixtureBuilder::start` replaces it with the Esplora base URL Electrs publishes.
fn fixture_rpc_config(node_rpc_url: Url, timeout: Duration) -> NigiriConfig {
    NigiriConfig {
        esplora_url: node_rpc_url.clone(),
        node_rpc_url,
        node_rpc_user: RPC_USER.to_owned(),
        node_rpc_password: RPC_PASSWORD.to_owned(),
        timeout,
        ..Default::default()
    }
}

async fn wait_for_root_rpc<C: FixtureChain>(
    root: &NigiriClient<C>,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let mut last_observation = "waiting for root getblockchaininfo RPC".to_owned();

    loop {
        match deadline
            .run(
                C::NODE_SERVICE,
                &last_observation,
                root.rpc::<serde_json::Value, _>("getblockchaininfo", ()),
            )
            .await
        {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(error)) => {
                last_observation = redacted_tail(&format!("root RPC: {error}"));
                deadline
                    .run(
                        C::NODE_SERVICE,
                        &last_observation,
                        tokio::time::sleep(RETRY_DELAY),
                    )
                    .await?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn wallet_rpc_url(root_url: &Url, wallet_name: &str) -> Result<Url, FixtureError> {
    let mut wallet_url = root_url.clone();
    wallet_url
        .path_segments_mut()
        .map_err(|_| FixtureError::InvalidConfiguration {
            detail: "node RPC URL cannot hold a wallet path".to_owned(),
        })?
        .extend(["wallet", wallet_name]);
    Ok(wallet_url)
}

pub(crate) fn bootstrap_error(
    chain: &'static str,
    operation: &'static str,
    source: NigiriError,
) -> FixtureError {
    FixtureError::Bootstrap {
        chain,
        operation,
        diagnostics: redacted_tail(&source.to_string()),
        source: redacted_source(source),
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io, time::Duration};

    use nigiri_rs::NigiriError;
    use testcontainers::{
        Image,
        core::{
            IntoContainerPort,
            error::{ClientError, TestcontainersError},
        },
    };
    use url::Url;

    use super::{bootstrap_error, fixture_client, fixture_rpc_config, request, wallet_rpc_url};
    use crate::{
        ContainerImage, FixtureError,
        chain::FixtureChain,
        diagnostics::{MAX_DIAGNOSTIC_BYTES, MAX_SOURCE_BYTES},
        owned_start::{classify_start_error, port_discovery_error},
    };

    // Catches a regression that exposes the wrong node port or drops the topology a fixture's
    // containers are scoped to.
    #[test]
    fn request_exposes_the_chains_rpc_port_on_the_fixture_topology() {
        use nigiri_rs::Bitcoin;

        let request = super::request::<Bitcoin>(
            &ContainerImage::bitcoind_default(),
            "nigiri-test-fixture",
            "nigiri-bitcoind-fixture",
        )
        .expect("the pinned Bitcoind image is valid");

        assert_eq!(request.expose_ports(), &[18_443.tcp()]);
        assert_eq!(request.network().as_deref(), Some("nigiri-test-fixture"));
        assert_eq!(
            request.container_name().as_deref(),
            Some("nigiri-bitcoind-fixture")
        );
        // Guards against a regression that passes `image.tag()` instead of
        // `image.testcontainers_tag()`: both compile and every other assertion here would still
        // pass, but the container would pull a floating `latest` instead of the pinned
        // `tag@digest`, silently unpinning the image the crate's docs promise is pinned.
        assert_eq!(
            request.image().name(),
            ContainerImage::bitcoind_default().name()
        );
        assert_eq!(
            request.image().tag(),
            ContainerImage::bitcoind_default().testcontainers_tag()
        );
    }

    // Catches a regression that defers invalid image validation until Docker request startup.
    #[test]
    fn request_rejects_invalid_images_before_constructing_a_request() {
        use nigiri_rs::Bitcoin;

        let error = match request::<Bitcoin>(
            &ContainerImage::new("", "v1"),
            "nigiri-test-fixture",
            "nigiri-bitcoind-fixture",
        ) {
            Err(error) => error,
            Ok(_) => panic!("an image without a name must be rejected"),
        };

        assert!(matches!(error, FixtureError::InvalidConfiguration { .. }));
    }

    // Catches a regression that lets the wallet endpoint acquire a trailing slash or loses the
    // exact wallet name path segment needed by Bitcoin Core.
    #[test]
    fn wallet_rpc_url_is_exactly_wallet_name_without_a_trailing_slash() {
        let root = Url::parse("http://127.0.0.1:18443/").expect("a static root URL is valid");

        let wallet = wallet_rpc_url(&root, "nigiri-rs-123")
            .expect("a hierarchical node RPC URL can contain a wallet path");

        assert_eq!(
            wallet.as_str(),
            "http://127.0.0.1:18443/wallet/nigiri-rs-123"
        );
    }

    // Catches a regression that reports a rejected fixture client configuration through
    // `FixtureError::Client`, whose transparent source chain would carry the raw configuration.
    #[test]
    fn a_rejected_client_configuration_is_reported_without_a_raw_source() {
        use nigiri_rs::Bitcoin;

        let url = Url::parse("http://127.0.0.1:18443/").expect("a static root URL is valid");

        let error = fixture_client::<Bitcoin>(fixture_rpc_config(url, Duration::ZERO))
            .expect_err("a zero request timeout must be rejected");

        let FixtureError::InvalidConfiguration { detail } = error else {
            panic!("a rejected fixture configuration must not become a transparent client error");
        };
        assert!(detail.len() <= MAX_SOURCE_BYTES);
        assert!(!detail.contains("admin1"));
        assert!(
            detail.starts_with("fixture RPC client configuration was rejected:"),
            "{detail}"
        );
    }

    // Catches a regression that boxes a raw error as a fixture source, letting error-chain
    // formatters expose fixture credentials or an unbounded body that bounded diagnostics hide.
    #[test]
    fn fixture_error_sources_are_bounded_and_redacted_across_the_whole_chain() {
        use nigiri_rs::Bitcoin;

        let secret_body = format!(
            "{} -rpcuser=admin1 -rpcpassword=123 admin1:123",
            "node-error-".repeat(4_000),
        );
        let image = ContainerImage::bitcoind_default();
        let errors = [
            bootstrap_error(
                "Bitcoin",
                "createwallet",
                NigiriError::InvalidResponse {
                    operation: "bootstrap RPC".into(),
                    detail: secret_body.clone(),
                },
            ),
            classify_start_error(
                Bitcoin::NODE_SERVICE,
                &image,
                TestcontainersError::other(io::Error::other(secret_body.clone())),
            ),
            classify_start_error(
                Bitcoin::NODE_SERVICE,
                &image,
                TestcontainersError::Client(ClientError::InvalidDockerHost(secret_body.clone())),
            ),
            port_discovery_error(
                Bitcoin::NODE_SERVICE,
                Bitcoin::NODE_RPC_PORT,
                TestcontainersError::other(io::Error::other(secret_body.clone())),
                "mapped port unavailable",
            ),
        ];

        for error in errors {
            let mut cause = Error::source(&error);
            let mut depth = 0_usize;

            while let Some(source) = cause {
                depth += 1;
                for rendered in [source.to_string(), format!("{source:?}")] {
                    assert!(rendered.len() <= MAX_SOURCE_BYTES, "{rendered:.64}");
                    assert!(!rendered.contains("admin1:123"));
                    assert!(!rendered.contains("-rpcuser=admin1"));
                    assert!(!rendered.contains("-rpcpassword=123"));
                }
                cause = source.source();
            }

            assert_eq!(
                depth, 1,
                "a fixture source must not expose a raw cause chain"
            );
            assert!(!format!("{error:?}").contains("admin1:123"));
        }
    }

    // Catches a regression that exposes wallet bootstrap failures as generic client errors or
    // leaks a large credential-bearing RPC error into the fixture display.
    #[test]
    fn bootstrap_errors_keep_the_operation_source_and_redacted_bounded_diagnostics() {
        for operation in ["createwallet", "getnewaddress", "generatetoaddress"] {
            let source = NigiriError::InvalidResponse {
                operation: "bootstrap RPC".into(),
                detail: format!("{} admin1:123", "node-error-".repeat(2_000)),
            };
            let error = bootstrap_error("Bitcoin", operation, source);

            assert!(error.to_string().starts_with(&format!(
                "Bitcoin wallet bootstrap failed during {operation}:"
            )));
            assert!(
                Error::source(&error).map(ToString::to_string).is_some_and(
                    |source| source.starts_with("invalid response during bootstrap RPC")
                )
            );
            let FixtureError::Bootstrap {
                operation: actual_operation,
                diagnostics,
                ..
            } = error
            else {
                panic!("a wallet RPC error must be classified as bootstrap failure");
            };
            assert_eq!(actual_operation, operation);
            assert!(diagnostics.len() <= MAX_DIAGNOSTIC_BYTES);
            assert!(!diagnostics.contains("admin1:123"));
        }
    }
}
