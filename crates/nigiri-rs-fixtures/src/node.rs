use std::time::Duration;

use nigiri_rs_core::{NigiriClient, NigiriConfig, NigiriError};
use url::Url;
use uuid::Uuid;

use crate::{
    ContainerImage, ElectrumEndpoint, FixtureError, RPC_PASSWORD, RPC_USER,
    chain::FixtureChain,
    deadline::Deadline,
    diagnostics::{MAX_SOURCE_BYTES, redacted_head, redacted_source, redacted_tail},
    endpoint::mapped_http_url,
    readiness::RETRY_DELAY,
    runtime::{ContainerEngine, RunningContainer, Startup, node_spec, runtime_error},
};

pub(crate) struct StartedNode {
    pub(crate) container: RunningContainer,
    pub(crate) client_config: NigiriConfig,
}

/// Extends the chain's own arguments with a composite's, letting the composite win a conflict.
///
/// A composite cannot simply append. `Liquid::node_cmd` sets `-validatepegin=0` and a peg pair
/// needs `-validatepegin=1`; a vector carrying both says two contradictory things and leaves the
/// node's behaviour resting on which occurrence Elements happens to prefer. So an argument the
/// composite sets removes the chain's own, and everything else is appended in order.
///
/// A key is the text before the first `=`, or the whole token when there is none. Matching is
/// exact, so `-validatepegin` cannot shadow `-validatepeginfoo`.
pub(crate) fn merge_node_args(base: Vec<String>, extra: &[String]) -> Vec<String> {
    let overridden: Vec<&str> = extra
        .iter()
        .map(|argument| argument_key(argument))
        .collect();

    let mut merged: Vec<String> = base
        .into_iter()
        .filter(|argument| !overridden.contains(&argument_key(argument)))
        .collect();
    merged.extend_from_slice(extra);
    merged
}

/// The part of a node argument that says which setting it sets.
fn argument_key(argument: &str) -> &str {
    argument.split_once('=').map_or(argument, |(key, _)| key)
}

pub(crate) async fn start_node<C: FixtureChain, E: ContainerEngine>(
    startup: &mut Startup<E>,
    image: &ContainerImage,
    network_name: &str,
    container_name: &str,
    extra_args: &[String],
    deadline: &Deadline,
) -> Result<StartedNode, FixtureError> {
    let spec = node_spec::<C>(
        image.clone(),
        network_name.to_owned(),
        container_name.to_owned(),
        extra_args.to_vec(),
    )?;
    let container = match deadline
        .run(
            C::NODE_SERVICE,
            "starting node container",
            startup.start_container(spec),
        )
        .await?
    {
        Ok(container) => container,
        Err(error) => {
            let error = runtime_error(C::NODE_SERVICE, error);
            return Err(crate::runtime::attach_container_log(
                startup,
                C::NODE_SERVICE,
                container_name,
                error,
            )
            .await);
        }
    };

    let host = container.host.clone();
    let rpc_port = *container.ports.get(&C::NODE_RPC_PORT).ok_or_else(|| {
        FixtureError::InvalidConfiguration {
            detail: format!(
                "container runtime omitted the mapped {} port for {}",
                C::NODE_RPC_PORT,
                C::NODE_SERVICE
            ),
        }
    })?;

    let root_url = mapped_http_url(&host, rpc_port)?;
    let root_config = fixture_rpc_config::<C>(
        root_url.clone(),
        deadline.remaining_or_expired(C::NODE_SERVICE, "configuring the root node RPC client")?,
    );
    let root = fixture_client::<C>(root_config)?;
    if let Err(not_ready) = wait_for_root_rpc::<C>(&root, deadline).await {
        // A node that never answered is the likeliest startup failure, and its own log is the
        // only thing that explains why, so the timeout carries a bounded tail of it.
        return Err(crate::runtime::attach_container_log(
            startup,
            C::NODE_SERVICE,
            &container.id,
            not_ready,
        )
        .await);
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
    let client_config = fixture_rpc_config::<C>(wallet_url, deadline.budget());
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
/// `FixtureBuilder::start` replaces it with the Esplora base URL Electrs publishes. `electrum` is
/// likewise a placeholder, but it must still be `C`'s own fixed port rather than whatever
/// `NigiriConfig::default()` supplies: `Default` always returns the Bitcoin configuration
/// regardless of `C`, so leaving this to `..Default::default()` would silently give a Liquid
/// fixture Bitcoin's port (50000) until `FixtureBuilder::start` overwrites it. `start` always does
/// overwrite it, but a config built here must not be wrong for whatever brief window it exists
/// before that happens, and a future chain-dependent field would inherit the same silent mistake.
fn fixture_rpc_config<C: FixtureChain>(node_rpc_url: Url, timeout: Duration) -> NigiriConfig {
    NigiriConfig {
        esplora_url: node_rpc_url.clone(),
        node_rpc_url,
        node_rpc_user: RPC_USER.to_owned(),
        node_rpc_password: RPC_PASSWORD.to_owned(),
        timeout,
        electrum: ElectrumEndpoint::new("localhost", C::ELECTRS_ELECTRUM_PORT)
            .expect("localhost with a chain's fixed Electrum port is always valid"),
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
    use std::{error::Error, time::Duration};

    use nigiri_rs_core::NigiriError;
    use url::Url;

    use super::{bootstrap_error, fixture_client, fixture_rpc_config, wallet_rpc_url};
    use crate::{
        FixtureError,
        chain::FixtureChain,
        diagnostics::{MAX_DIAGNOSTIC_BYTES, MAX_SOURCE_BYTES},
    };

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
        use nigiri_rs_core::Bitcoin;

        let url = Url::parse("http://127.0.0.1:18443/").expect("a static root URL is valid");

        let error = fixture_client::<Bitcoin>(fixture_rpc_config::<Bitcoin>(url, Duration::ZERO))
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

    // Catches a regression that appends a composite's argument beside the chain's conflicting one
    // instead of replacing it. `Liquid::node_cmd` sets `-validatepegin=0` and a peg pair needs
    // `1`; passing both leaves the node's behaviour resting on which occurrence Elements happens
    // to prefer.
    #[test]
    fn merge_replaces_a_conflicting_chain_argument_in_place() {
        let merged = super::merge_node_args(
            vec![
                "-chain=liquidregtest".to_owned(),
                "-validatepegin=0".to_owned(),
                "-printtoconsole=1".to_owned(),
            ],
            &["-validatepegin=1".to_owned()],
        );

        assert_eq!(
            merged,
            vec![
                "-chain=liquidregtest".to_owned(),
                "-printtoconsole=1".to_owned(),
                "-validatepegin=1".to_owned(),
            ],
            "the chain's own setting must be dropped, not duplicated"
        );
        assert_eq!(
            merged
                .iter()
                .filter(|argument| argument.starts_with("-validatepegin"))
                .count(),
            1
        );
    }

    // Catches a regression that drops a composite's non-conflicting arguments, or reorders either
    // side. LightningStack's ZMQ publishers are additions the chain says nothing about.
    #[test]
    fn merge_appends_what_the_chain_does_not_set_and_keeps_both_orders() {
        let merged = super::merge_node_args(
            vec!["-chain=regtest".to_owned(), "-txindex=1".to_owned()],
            &[
                "-zmqpubrawblock=tcp://0.0.0.0:28332".to_owned(),
                "-zmqpubrawtx=tcp://0.0.0.0:28333".to_owned(),
            ],
        );

        assert_eq!(
            merged,
            vec![
                "-chain=regtest".to_owned(),
                "-txindex=1".to_owned(),
                "-zmqpubrawblock=tcp://0.0.0.0:28332".to_owned(),
                "-zmqpubrawtx=tcp://0.0.0.0:28333".to_owned(),
            ]
        );
    }

    // Catches a regression that matches argument keys by prefix, which would let `-validatepegin`
    // silently delete an unrelated `-validatepeginfoo`, and one that mishandles a valueless flag.
    #[test]
    fn merge_matches_argument_keys_exactly() {
        let merged = super::merge_node_args(
            vec![
                "-validatepeginfoo=1".to_owned(),
                "-server".to_owned(),
                "-txindex=1".to_owned(),
            ],
            &["-validatepegin=1".to_owned(), "-server".to_owned()],
        );

        assert_eq!(
            merged,
            vec![
                "-validatepeginfoo=1".to_owned(),
                "-txindex=1".to_owned(),
                "-validatepegin=1".to_owned(),
                "-server".to_owned(),
            ],
            "only an exact key match may be replaced"
        );
    }

    // Catches a regression in the path every existing call site takes: no extras must leave the
    // chain's vector untouched, not merely equal to it by accident.
    #[test]
    fn merge_without_extras_is_the_chains_own_vector() {
        use nigiri_rs_core::Liquid;

        assert_eq!(
            super::merge_node_args(Liquid::node_cmd(), &[]),
            Liquid::node_cmd()
        );
    }
}
