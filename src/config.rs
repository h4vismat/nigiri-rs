use std::{path::PathBuf, time::Duration};

use url::Url;

use crate::NigiriError;

pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default retention ceiling for a single transport response body.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Largest accepted value for [`NigiriConfig::max_response_bytes`].
///
/// An unbounded value read from a config file or environment variable could turn
/// one response into an out-of-memory abort. 16 MiB is far above any Bitcoin Core
/// or Elements regtest response and keeps the worst case survivable.
pub const MAX_RESPONSE_BYTES_LIMIT: usize = 16 * 1024 * 1024;

/// Complete immutable configuration for a Nigiri client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NigiriConfig {
    /// Chopsticks HTTP endpoint.
    pub chopsticks_url: Url,
    /// Esplora HTTP endpoint.
    pub esplora_url: Url,
    /// Node JSON-RPC endpoint. Bitcoin defaults to port 18443; Liquid to 18884.
    pub node_rpc_url: Url,
    /// Node JSON-RPC username. The default is Nigiri's public regtest user `admin1`.
    pub node_rpc_user: String,
    /// Node JSON-RPC password. The default is Nigiri's public regtest password `123`.
    ///
    /// These values are deliberately visible in derived [`Debug`] output because
    /// Nigiri publishes them as fixed regtest defaults; they are not production secrets.
    pub node_rpc_password: String,
    /// Legacy Nigiri executable path retained for configuration compatibility until Phase 3.
    pub executable: PathBuf,
    /// Maximum duration of one HTTP request and response operation.
    pub timeout: Duration,
    /// Maximum bytes retained from one node RPC, Chopsticks, or Esplora response body.
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
    /// use nigiri_rs::{Bitcoin, DEFAULT_MAX_RESPONSE_BYTES, NigiriClient, NigiriConfig};
    ///
    /// # fn main() -> Result<(), nigiri_rs::NigiriError> {
    /// let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
    ///     chopsticks_url: "http://localhost:3000".parse().unwrap(),
    ///     esplora_url: "http://localhost:30000".parse().unwrap(),
    ///     executable: "nigiri".into(),
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
        Self::defaults(
            "http://localhost:3000",
            "http://localhost:30000",
            "http://localhost:18443/",
        )
    }

    pub(crate) fn liquid() -> Self {
        Self::defaults(
            "http://localhost:3001",
            "http://localhost:30001",
            "http://localhost:18884/",
        )
    }

    fn defaults(chopsticks_url: &str, esplora_url: &str, node_rpc_url: &str) -> Self {
        Self {
            chopsticks_url: Url::parse(chopsticks_url).expect("static Nigiri URL is valid"),
            esplora_url: Url::parse(esplora_url).expect("static Nigiri URL is valid"),
            node_rpc_url: Url::parse(node_rpc_url).expect("static Nigiri URL is valid"),
            node_rpc_user: "admin1".to_owned(),
            node_rpc_password: "123".to_owned(),
            executable: PathBuf::from("nigiri"),
            timeout: DEFAULT_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }

    pub(crate) fn validate_and_normalize(mut self) -> Result<Self, NigiriError> {
        normalize_url(&mut self.chopsticks_url)?;
        normalize_url(&mut self.esplora_url)?;
        normalize_url(&mut self.node_rpc_url)?;

        if self.executable.as_os_str().is_empty() {
            return Err(invalid_configuration("executable path must not be empty"));
        }
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
    fn default() -> Self {
        Self::bitcoin()
    }
}

fn normalize_url(url: &mut Url) -> Result<(), NigiriError> {
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
    if !url.path().ends_with('/') {
        let normalized = format!("{}/", url.path());
        url.set_path(&normalized);
    }
    Ok(())
}

fn invalid_configuration(detail: &'static str) -> NigiriError {
    NigiriError::InvalidRequest {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use super::{DEFAULT_MAX_RESPONSE_BYTES, DEFAULT_TIMEOUT, NigiriConfig};

    #[test]
    fn verified_network_defaults_include_both_service_endpoints() {
        let bitcoin = NigiriConfig::bitcoin();
        let liquid = NigiriConfig::liquid();

        assert_eq!(bitcoin.chopsticks_url.as_str(), "http://localhost:3000/");
        assert_eq!(bitcoin.esplora_url.as_str(), "http://localhost:30000/");
        assert_eq!(liquid.chopsticks_url.as_str(), "http://localhost:3001/");
        assert_eq!(liquid.esplora_url.as_str(), "http://localhost:30001/");
        assert_eq!(bitcoin.node_rpc_url.as_str(), "http://localhost:18443/");
        assert_eq!(liquid.node_rpc_url.as_str(), "http://localhost:18884/");
        assert_eq!(bitcoin.node_rpc_user, "admin1");
        assert_eq!(liquid.node_rpc_user, "admin1");
        assert_eq!(bitcoin.node_rpc_password, "123");
        assert_eq!(liquid.node_rpc_password, "123");
        assert_eq!(bitcoin.executable, PathBuf::from("nigiri"));
        assert_eq!(liquid.timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn default_configuration_uses_bitcoin_defaults() {
        assert_eq!(NigiriConfig::default(), NigiriConfig::bitcoin());
    }

    #[test]
    fn custom_configuration_normalizes_each_base_url() {
        let config = NigiriConfig {
            chopsticks_url: "https://fixture.invalid/chopsticks".parse().unwrap(),
            esplora_url: "http://fixture.invalid/esplora".parse().unwrap(),
            executable: PathBuf::from("/opt/nigiri"),
            timeout: Duration::from_secs(9),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            ..Default::default()
        }
        .validate_and_normalize()
        .unwrap();

        assert_eq!(
            config.chopsticks_url.as_str(),
            "https://fixture.invalid/chopsticks/"
        );
        assert_eq!(
            config.esplora_url.as_str(),
            "http://fixture.invalid/esplora/"
        );
        assert_eq!(config.executable, PathBuf::from("/opt/nigiri"));
        assert_eq!(config.timeout, Duration::from_secs(9));
    }

    #[test]
    fn custom_node_rpc_url_is_normalized() {
        let config = NigiriConfig {
            node_rpc_url: "https://fixture.invalid/rpc".parse().unwrap(),
            ..Default::default()
        }
        .validate_and_normalize()
        .unwrap();

        assert_eq!(config.node_rpc_url.as_str(), "https://fixture.invalid/rpc/");
    }

    #[test]
    fn non_http_node_rpc_url_is_rejected() {
        let config = NigiriConfig {
            node_rpc_url: "ftp://fixture.invalid/rpc".parse().unwrap(),
            ..Default::default()
        };

        assert!(config.validate_and_normalize().is_err());
    }
}
