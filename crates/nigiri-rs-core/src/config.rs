use std::time::Duration;

use url::Url;

use crate::{ElectrumEndpoint, NigiriError};

pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default retention ceiling for a single transport response body.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Largest accepted value for [`NigiriConfig::max_response_bytes`].
///
/// An unbounded value read from a config file or environment variable could turn
/// one response into an out-of-memory abort. 16 MiB is far above any Bitcoin Core
/// or Elements regtest response and keeps the worst case survivable.
pub const MAX_RESPONSE_BYTES_LIMIT: usize = 16 * 1024 * 1024;

/// Complete immutable client configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NigiriConfig {
    /// Esplora HTTP base URL.
    pub esplora_url: Url,
    /// Exact node JSON-RPC endpoint URL. Bitcoin defaults to port 18443; Liquid to 18884.
    ///
    /// The path is preserved exactly so callers can target a wallet-specific Bitcoin Core RPC
    /// endpoint such as `/wallet/name`.
    pub node_rpc_url: Url,
    /// Electrum host and TCP port.
    ///
    /// Bitcoin defaults to port 50000; Liquid to 50001. A fixture replaces this with its
    /// runtime-mapped port, so read it from the client rather than assuming the default.
    pub electrum: ElectrumEndpoint,
    /// Node JSON-RPC username. The default is Nigiri's public regtest user `admin1`.
    pub node_rpc_user: String,
    /// Node JSON-RPC password. The default is Nigiri's public regtest password `123`.
    ///
    /// These values are deliberately visible in derived [`Debug`] output because
    /// Nigiri publishes them as fixed regtest defaults; they are not production secrets.
    pub node_rpc_password: String,
    /// Maximum duration of one HTTP request and response operation.
    pub timeout: Duration,
    /// Maximum bytes retained from one node RPC or Esplora response body.
    ///
    /// Anything past this limit is rejected rather than buffered, so raise it
    /// deliberately for methods whose results are large (`listunspent`,
    /// `listtransactions`, `getblock <hash> 2`). Defaults to
    /// [`DEFAULT_MAX_RESPONSE_BYTES`].
    ///
    /// Values above [`MAX_RESPONSE_BYTES_LIMIT`] are rejected. Low megabytes cover
    /// every Bitcoin Core and Elements response in a regtest environment.
    ///
    /// ```
    /// use nigiri_rs_core::{Bitcoin, DEFAULT_MAX_RESPONSE_BYTES, NigiriClient, NigiriConfig};
    ///
    /// # fn main() -> Result<(), nigiri_rs_core::NigiriError> {
    /// let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
    ///     esplora_url: "http://localhost:30000".parse().unwrap(),
    ///     timeout: std::time::Duration::from_secs(30),
    ///     max_response_bytes: 4 * DEFAULT_MAX_RESPONSE_BYTES,
    ///     ..Default::default()
    /// })?;
    /// # let _ = client;
    /// # Ok(())
    /// # }
    /// ```
    pub max_response_bytes: usize,
}

impl NigiriConfig {
    pub(crate) fn bitcoin() -> Self {
        Self::defaults("http://localhost:30000", "http://localhost:18443/", 50_000)
    }

    pub(crate) fn liquid() -> Self {
        Self::defaults("http://localhost:30001", "http://localhost:18884/", 50_001)
    }

    fn defaults(esplora_url: &str, node_rpc_url: &str, electrum_port: u16) -> Self {
        Self {
            esplora_url: Url::parse(esplora_url).expect("static Nigiri URL is valid"),
            node_rpc_url: Url::parse(node_rpc_url).expect("static Nigiri URL is valid"),
            electrum: ElectrumEndpoint::new("localhost", electrum_port)
                .expect("static Nigiri Electrum endpoint is valid"),
            node_rpc_user: "admin1".to_owned(),
            node_rpc_password: "123".to_owned(),
            timeout: DEFAULT_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }

    pub(crate) fn validate_and_normalize(mut self) -> Result<Self, NigiriError> {
        validate_endpoint_url(&self.esplora_url)?;
        validate_endpoint_url(&self.node_rpc_url)?;
        normalize_base_url(&mut self.esplora_url);

        if self.timeout.is_zero() {
            return Err(invalid_configuration("timeout must be greater than zero"));
        }
        if self.max_response_bytes == 0 {
            return Err(invalid_configuration(
                "max_response_bytes must be greater than zero",
            ));
        }
        if self.max_response_bytes > MAX_RESPONSE_BYTES_LIMIT {
            return Err(invalid_configuration(
                "max_response_bytes must not exceed MAX_RESPONSE_BYTES_LIMIT",
            ));
        }
        Ok(self)
    }
}

impl Default for NigiriConfig {
    /// Returns the Bitcoin service defaults.
    ///
    /// A custom [`crate::NigiriClient<crate::Liquid>`] must override both
    /// service URLs, including [`NigiriConfig::node_rpc_url`].
    fn default() -> Self {
        Self::bitcoin()
    }
}

fn validate_endpoint_url(url: &Url) -> Result<(), NigiriError> {
    if !matches!(url.scheme(), "http" | "https") || url.cannot_be_a_base() {
        return Err(invalid_configuration(
            "endpoint URLs must use HTTP or HTTPS and support relative paths",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid_configuration(
            "endpoint URLs must not contain a query or fragment",
        ));
    }
    Ok(())
}

fn normalize_base_url(url: &mut Url) {
    if !url.path().ends_with('/') {
        let normalized = format!("{}/", url.path());
        url.set_path(&normalized);
    }
}

fn invalid_configuration(detail: &'static str) -> NigiriError {
    NigiriError::InvalidRequest {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{DEFAULT_MAX_RESPONSE_BYTES, DEFAULT_TIMEOUT, NigiriConfig};

    #[test]
    fn verified_network_defaults_include_esplora_and_node_endpoints() {
        let bitcoin = NigiriConfig::bitcoin();
        let liquid = NigiriConfig::liquid();

        assert_eq!(bitcoin.esplora_url.as_str(), "http://localhost:30000/");
        assert_eq!(liquid.esplora_url.as_str(), "http://localhost:30001/");
        assert_eq!(bitcoin.node_rpc_url.as_str(), "http://localhost:18443/");
        assert_eq!(liquid.node_rpc_url.as_str(), "http://localhost:18884/");
        assert_eq!(bitcoin.node_rpc_user, "admin1");
        assert_eq!(liquid.node_rpc_user, "admin1");
        assert_eq!(bitcoin.node_rpc_password, "123");
        assert_eq!(liquid.node_rpc_password, "123");
        assert_eq!(liquid.timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn custom_esplora_base_url_normalizes_without_changing_node_rpc_endpoint() {
        let config = NigiriConfig {
            esplora_url: "http://fixture.invalid/esplora".parse().unwrap(),
            node_rpc_url: "https://fixture.invalid/rpc".parse().unwrap(),
            timeout: Duration::from_secs(9),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            ..Default::default()
        }
        .validate_and_normalize()
        .unwrap();

        assert_eq!(
            config.esplora_url.as_str(),
            "http://fixture.invalid/esplora/"
        );
        assert_eq!(config.node_rpc_url.as_str(), "https://fixture.invalid/rpc");
        assert_eq!(config.timeout, Duration::from_secs(9));
    }

    #[test]
    fn custom_wallet_node_rpc_endpoint_preserves_path_without_trailing_slash() {
        let config = NigiriConfig {
            node_rpc_url: "http://fixture.invalid/wallet/name".parse().unwrap(),
            ..Default::default()
        }
        .validate_and_normalize()
        .unwrap();

        assert_eq!(
            config.node_rpc_url.as_str(),
            "http://fixture.invalid/wallet/name"
        );
    }

    #[test]
    fn endpoint_urls_reject_queries_and_fragments() {
        for config in [
            NigiriConfig {
                esplora_url: "http://fixture.invalid/esplora?cursor=1".parse().unwrap(),
                ..Default::default()
            },
            NigiriConfig {
                node_rpc_url: "http://fixture.invalid/rpc#fragment".parse().unwrap(),
                ..Default::default()
            },
        ] {
            assert!(config.validate_and_normalize().is_err());
        }
    }

    #[test]
    fn non_http_node_rpc_url_is_rejected() {
        let config = NigiriConfig {
            node_rpc_url: "ftp://fixture.invalid/rpc".parse().unwrap(),
            ..Default::default()
        };

        assert!(config.validate_and_normalize().is_err());
    }

    // Catches a regression that points a client at the wrong Electrum port, or drops the endpoint
    // from a default constructor. Nigiri publishes these as fixed regtest ports: 50000 for Bitcoin
    // and 50001 for Liquid, the same values the fixture uses as container ports.
    #[test]
    fn defaults_carry_the_documented_electrum_ports() {
        let bitcoin = NigiriConfig::bitcoin();
        assert_eq!(bitcoin.electrum.host(), "localhost");
        assert_eq!(bitcoin.electrum.port(), 50_000);

        let liquid = NigiriConfig::liquid();
        assert_eq!(liquid.electrum.host(), "localhost");
        assert_eq!(liquid.electrum.port(), 50_001);

        // Default is the Bitcoin configuration.
        assert_eq!(NigiriConfig::default().electrum.port(), 50_000);
    }
}
