use std::{fmt, path::Path, sync::Arc, time::Duration};

use tokio::io::AsyncReadExt;
use url::Url;

use crate::LndError;

/// Largest accepted PEM TLS certificate.
pub const MAX_TLS_CERTIFICATE_BYTES: usize = 1_048_576;
/// Largest accepted serialized macaroon.
pub const MAX_MACAROON_BYTES: usize = 65_536;

/// Immutable configuration for an authenticated LND gRPC connection.
#[derive(Clone)]
pub struct LndConfig {
    endpoint: Url,
    #[allow(dead_code)]
    certificate: Arc<[u8]>,
    #[allow(dead_code)]
    macaroon: Arc<[u8]>,
    timeout: Duration,
}

impl LndConfig {
    /// Validates a secure endpoint and bounded credential bytes without connecting to LND.
    pub fn new(
        endpoint: impl AsRef<str>,
        certificate: Vec<u8>,
        macaroon: Vec<u8>,
        timeout: Duration,
    ) -> Result<Self, LndError> {
        let endpoint = validate_endpoint(endpoint.as_ref())?;
        validate_credential(
            "TLS certificate",
            certificate.len(),
            MAX_TLS_CERTIFICATE_BYTES,
        )?;
        validate_credential("macaroon", macaroon.len(), MAX_MACAROON_BYTES)?;
        if timeout.is_zero() {
            return Err(invalid("timeout must be greater than zero"));
        }

        Ok(Self {
            endpoint,
            certificate: certificate.into(),
            macaroon: macaroon.into(),
            timeout,
        })
    }

    /// Reads bounded credential files before applying the same validation as [`Self::new`].
    pub async fn from_files(
        endpoint: impl AsRef<str>,
        certificate_path: impl AsRef<Path>,
        macaroon_path: impl AsRef<Path>,
        timeout: Duration,
    ) -> Result<Self, LndError> {
        let certificate_path = certificate_path.as_ref();
        let macaroon_path = macaroon_path.as_ref();
        let certificate = read_bounded(certificate_path, MAX_TLS_CERTIFICATE_BYTES).await?;
        let macaroon = read_bounded(macaroon_path, MAX_MACAROON_BYTES).await?;
        Self::new(endpoint, certificate, macaroon, timeout)
    }

    #[must_use]
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    #[allow(dead_code)]
    pub(crate) fn certificate(&self) -> &[u8] {
        &self.certificate
    }

    #[allow(dead_code)]
    pub(crate) fn macaroon(&self) -> &[u8] {
        &self.macaroon
    }
}

impl fmt::Debug for LndConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LndConfig")
            .field("endpoint", &self.endpoint)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

fn validate_endpoint(endpoint: &str) -> Result<Url, LndError> {
    let url = Url::parse(endpoint).map_err(|_| invalid("endpoint must be a valid HTTPS URL"))?;
    if url.scheme() != "https" {
        return Err(invalid("endpoint must use HTTPS"));
    }
    if url.host_str().is_none() {
        return Err(invalid("endpoint must include a host"));
    }
    if url.port().is_none() {
        return Err(invalid("endpoint must include a port"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("endpoint must not include userinfo"));
    }
    if url.query().is_some() {
        return Err(invalid("endpoint must not include a query"));
    }
    if url.fragment().is_some() {
        return Err(invalid("endpoint must not include a fragment"));
    }
    Ok(url)
}

fn validate_credential(name: &'static str, length: usize, maximum: usize) -> Result<(), LndError> {
    if length == 0 {
        return Err(invalid(match name {
            "TLS certificate" => "TLS certificate must not be empty",
            _ => "macaroon must not be empty",
        }));
    }
    if length > maximum {
        return Err(invalid(match name {
            "TLS certificate" => "TLS certificate exceeds MAX_TLS_CERTIFICATE_BYTES",
            _ => "macaroon exceeds MAX_MACAROON_BYTES",
        }));
    }
    Ok(())
}

async fn read_bounded(path: &Path, maximum: usize) -> Result<Vec<u8>, LndError> {
    let mut file =
        tokio::fs::File::open(path)
            .await
            .map_err(|source| LndError::CredentialRead {
                path: path.to_owned(),
                source,
            })?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8_192];
    loop {
        let read = file
            .read(&mut chunk)
            .await
            .map_err(|source| LndError::CredentialRead {
                path: path.to_owned(),
                source,
            })?;
        if read == 0 {
            return Ok(bytes);
        }
        let next_length = bytes
            .len()
            .checked_add(read)
            .ok_or_else(|| invalid("credential file length overflowed while reading"))?;
        if next_length > maximum {
            return Err(invalid(match maximum {
                MAX_TLS_CERTIFICATE_BYTES => "TLS certificate exceeds MAX_TLS_CERTIFICATE_BYTES",
                _ => "macaroon exceeds MAX_MACAROON_BYTES",
            }));
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

fn invalid(detail: &'static str) -> LndError {
    LndError::InvalidRequest {
        detail: detail.into(),
    }
}
