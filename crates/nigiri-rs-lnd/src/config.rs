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
    tls: TlsConfig,
    macaroon: Arc<[u8]>,
}

/// Validated endpoint and certificate, independent of wallet authentication.
#[derive(Clone)]
pub(crate) struct TlsConfig {
    endpoint: Url,
    certificate: Arc<[u8]>,
    timeout: Duration,
}

impl TlsConfig {
    pub(crate) fn new(
        endpoint: &str,
        certificate: Vec<u8>,
        timeout: Duration,
    ) -> Result<Self, LndError> {
        let endpoint = validate_endpoint(endpoint)?;
        validate_credential(
            "TLS certificate",
            certificate.len(),
            MAX_TLS_CERTIFICATE_BYTES,
        )?;
        if timeout.is_zero() {
            return Err(invalid("timeout must be greater than zero"));
        }
        Ok(Self {
            endpoint,
            certificate: certificate.into(),
            timeout,
        })
    }
    pub(crate) fn from_url(
        endpoint: &Url,
        certificate: Vec<u8>,
        timeout: Duration,
    ) -> Result<Self, LndError> {
        // Url erases an explicitly supplied HTTPS default port. Restore it solely for the
        // shared authority validator, preserving every other URL component for validation.
        let mut endpoint_text = endpoint.as_str().to_owned();
        if endpoint.scheme() == "https" && endpoint.port().is_none() {
            let end = endpoint_text.find("://").map(|i| i + 3).unwrap_or(0);
            let authority_end = endpoint_text[end..]
                .find(['/', '?', '#'])
                .map_or(endpoint_text.len(), |i| end + i);
            endpoint_text.insert_str(authority_end, ":443");
        }
        Self::new(&endpoint_text, certificate, timeout)
    }
    pub(crate) fn endpoint(&self) -> &Url {
        &self.endpoint
    }
    pub(crate) fn certificate(&self) -> &[u8] {
        &self.certificate
    }
    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl LndConfig {
    /// Validates a secure endpoint and bounded credential bytes without connecting to LND.
    pub fn new(
        endpoint: impl AsRef<str>,
        certificate: Vec<u8>,
        macaroon: Vec<u8>,
        timeout: Duration,
    ) -> Result<Self, LndError> {
        Self::authenticated(
            TlsConfig::new(endpoint.as_ref(), certificate, timeout)?,
            macaroon,
        )
    }

    pub(crate) fn authenticated(tls: TlsConfig, macaroon: Vec<u8>) -> Result<Self, LndError> {
        validate_credential("macaroon", macaroon.len(), MAX_MACAROON_BYTES)?;
        Ok(Self {
            tls,
            macaroon: macaroon.into(),
        })
    }
    pub(crate) fn tls(&self) -> &TlsConfig {
        &self.tls
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
        self.tls.endpoint()
    }

    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.tls.timeout()
    }

    #[cfg(test)]
    pub(crate) fn certificate(&self) -> &[u8] {
        self.tls.certificate()
    }

    pub(crate) fn macaroon(&self) -> &[u8] {
        &self.macaroon
    }
}

impl fmt::Debug for LndConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LndConfig")
            .field("endpoint", &self.endpoint())
            .field("timeout", &self.timeout())
            .finish_non_exhaustive()
    }
}

fn validate_endpoint(endpoint: &str) -> Result<Url, LndError> {
    let authority = original_authority(endpoint)?;
    validate_authority(authority)?;
    let url = Url::parse(endpoint).map_err(|_| invalid("endpoint must be a valid HTTPS URL"))?;
    if url.scheme() != "https" {
        return Err(invalid("endpoint must use HTTPS"));
    }
    if url.host_str().is_none() {
        return Err(invalid("endpoint must include a host"));
    }
    if url.query().is_some() {
        return Err(invalid("endpoint must not include a query"));
    }
    if url.fragment().is_some() {
        return Err(invalid("endpoint must not include a fragment"));
    }
    Ok(url)
}

fn original_authority(endpoint: &str) -> Result<&str, LndError> {
    let (_, rest) = endpoint
        .split_once("://")
        .ok_or_else(|| invalid("endpoint must include a host"))?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Ok(&rest[..end])
}

fn validate_authority(authority: &str) -> Result<(), LndError> {
    if authority.contains('@') {
        return Err(invalid("endpoint must not include userinfo"));
    }

    let port = if authority.starts_with('[') {
        authority
            .find(']')
            .and_then(|end| authority.get(end + 1..))
            .and_then(|suffix| suffix.strip_prefix(':'))
    } else {
        authority.rsplit_once(':').map(|(_, port)| port)
    };
    let Some(port) = port else {
        return Err(invalid("endpoint must include a port"));
    };
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid("endpoint port must be in range"));
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| invalid("endpoint port must be in range"))?;
    if port == 0 {
        return Err(invalid("endpoint port must be greater than zero"));
    }
    Ok(())
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
