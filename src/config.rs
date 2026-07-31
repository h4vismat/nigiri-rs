use std::{path::PathBuf, time::Duration};

use url::Url;

use crate::NigiriError;

pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Complete immutable configuration for a Nigiri client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NigiriConfig {
    pub chopsticks_url: Url,
    pub esplora_url: Url,
    pub executable: PathBuf,
    pub timeout: Duration,
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
        operation: "configuration",
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use super::{DEFAULT_TIMEOUT, NigiriConfig};

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
