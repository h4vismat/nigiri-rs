use std::{path::PathBuf, time::Duration};

use url::Url;

use crate::NigiriError;

pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default retention ceiling for a single Nigiri CLI stream.
pub const DEFAULT_MAX_RPC_RESPONSE_BYTES: usize = 64 * 1024;

/// Complete immutable configuration for a Nigiri client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NigiriConfig {
    pub chopsticks_url: Url,
    pub esplora_url: Url,
    pub executable: PathBuf,
    pub timeout: Duration,
    /// Maximum bytes retained from a single Nigiri CLI stdout or stderr stream.
    ///
    /// Anything past this limit is rejected and the child is killed rather than
    /// buffered, so raise it when calling [`crate::NigiriClient::rpc`] with
    /// methods whose results are large (`listunspent`, `listtransactions`,
    /// `getblock <hash> 2`). Defaults to [`DEFAULT_MAX_RPC_RESPONSE_BYTES`].
    ///
    /// Raise it deliberately, and keep it in the low megabytes. Formatting a
    /// failed RPC's stderr costs a multiple of this value in transient allocation:
    /// a 4-byte-per-byte redaction map plus a lossy UTF-8 copy that can expand
    /// threefold, so peak usage is several times this value and a limit in the
    /// gigabyte range turns a single RPC failure into an out-of-memory abort. Low
    /// megabytes cover every Bitcoin Core and Elements response in a regtest
    /// environment.
    ///
    /// ```
    /// use nigiri_rs::{Bitcoin, DEFAULT_MAX_RPC_RESPONSE_BYTES, NigiriClient, NigiriConfig};
    ///
    /// # fn main() -> Result<(), nigiri_rs::NigiriError> {
    /// let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
    ///     chopsticks_url: "http://localhost:3000".parse().unwrap(),
    ///     esplora_url: "http://localhost:30000".parse().unwrap(),
    ///     executable: "nigiri".into(),
    ///     timeout: std::time::Duration::from_secs(30),
    ///     max_rpc_response_bytes: 4 * DEFAULT_MAX_RPC_RESPONSE_BYTES,
    /// })?;
    /// # let _ = client;
    /// # Ok(())
    /// # }
    /// ```
    pub max_rpc_response_bytes: usize,
}

impl NigiriConfig {
    pub(crate) fn bitcoin() -> Self {
        Self::defaults("http://localhost:3000", "http://localhost:30000")
    }

    pub(crate) fn liquid() -> Self {
        Self::defaults("http://localhost:3001", "http://localhost:30001")
    }

    fn defaults(chopsticks_url: &str, esplora_url: &str) -> Self {
        Self {
            chopsticks_url: Url::parse(chopsticks_url).expect("static Nigiri URL is valid"),
            esplora_url: Url::parse(esplora_url).expect("static Nigiri URL is valid"),
            executable: PathBuf::from("nigiri"),
            timeout: DEFAULT_TIMEOUT,
            max_rpc_response_bytes: DEFAULT_MAX_RPC_RESPONSE_BYTES,
        }
    }

    pub(crate) fn validate_and_normalize(mut self) -> Result<Self, NigiriError> {
        normalize_url(&mut self.chopsticks_url)?;
        normalize_url(&mut self.esplora_url)?;

        if self.executable.as_os_str().is_empty() {
            return Err(invalid_configuration("executable path must not be empty"));
        }
        if self.timeout.is_zero() {
            return Err(invalid_configuration("timeout must be greater than zero"));
        }
        if self.max_rpc_response_bytes == 0 {
            return Err(invalid_configuration(
                "max_rpc_response_bytes must be greater than zero",
            ));
        }
        Ok(self)
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
    NigiriError::InvalidResponse {
        operation: "configuration".into(),
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use super::{DEFAULT_MAX_RPC_RESPONSE_BYTES, DEFAULT_TIMEOUT, NigiriConfig};

    #[test]
    fn verified_network_defaults_include_both_service_endpoints() {
        let bitcoin = NigiriConfig::bitcoin();
        let liquid = NigiriConfig::liquid();

        assert_eq!(bitcoin.chopsticks_url.as_str(), "http://localhost:3000/");
        assert_eq!(bitcoin.esplora_url.as_str(), "http://localhost:30000/");
        assert_eq!(liquid.chopsticks_url.as_str(), "http://localhost:3001/");
        assert_eq!(liquid.esplora_url.as_str(), "http://localhost:30001/");
        assert_eq!(bitcoin.executable, PathBuf::from("nigiri"));
        assert_eq!(liquid.timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn custom_configuration_normalizes_each_base_url() {
        let config = NigiriConfig {
            chopsticks_url: "https://fixture.invalid/chopsticks".parse().unwrap(),
            esplora_url: "http://fixture.invalid/esplora".parse().unwrap(),
            executable: PathBuf::from("/opt/nigiri"),
            timeout: Duration::from_secs(9),
            max_rpc_response_bytes: DEFAULT_MAX_RPC_RESPONSE_BYTES,
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
}
