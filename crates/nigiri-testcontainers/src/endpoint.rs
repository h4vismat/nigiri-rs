use url::Url;

use crate::FixtureError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElectrumEndpoint {
    host: String,
    port: u16,
}

impl ElectrumEndpoint {
    pub(crate) fn new(host: impl Into<String>, port: u16) -> Result<Self, FixtureError> {
        let host = host.into();

        if host.is_empty() {
            return Err(FixtureError::InvalidConfiguration {
                detail: "endpoint host must not be empty".to_owned(),
            });
        }

        if port == 0 {
            return Err(FixtureError::InvalidConfiguration {
                detail: "endpoint port must not be zero".to_owned(),
            });
        }

        Ok(Self { host, port })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Builds the `http://host:port/` base URL for a mapped container port.
///
/// The runtime chooses both the host and the port, so neither is trusted into a URL by string
/// concatenation.
pub(crate) fn mapped_http_url(host: &str, port: u16) -> Result<Url, FixtureError> {
    let mut url = Url::parse("http://localhost/").expect("the static mapped URL is valid");
    // A URL host must bracket an IPv6 literal, but a container runtime reports one bare, so the
    // bracketed form is tried before the host is called invalid.
    url.set_host(Some(host))
        .or_else(|error| match host.parse::<std::net::Ipv6Addr>() {
            Ok(address) => url.set_host(Some(&format!("[{address}]"))),
            Err(_) => Err(error),
        })
        .map_err(|_| FixtureError::InvalidConfiguration {
            detail: "container runtime returned an invalid mapped host".to_owned(),
        })?;
    url.set_port(Some(port))
        .map_err(|()| FixtureError::InvalidConfiguration {
            detail: "container runtime returned an invalid mapped port".to_owned(),
        })?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::mapped_http_url;
    use crate::{ElectrumEndpoint, FixtureError};

    // Catches a regression that builds a mapped base URL by concatenation, which a runtime-supplied
    // host such as an IPv6 literal would silently corrupt.
    #[test]
    fn a_mapped_url_keeps_its_runtime_host_and_port() {
        assert_eq!(
            mapped_http_url("127.0.0.1", 32_768)
                .expect("a loopback host and port are valid")
                .as_str(),
            "http://127.0.0.1:32768/"
        );
        assert_eq!(
            mapped_http_url("::1", 32_768)
                .expect("an IPv6 host is valid")
                .as_str(),
            "http://[::1]:32768/"
        );
    }

    fn assert_invalid_configuration(error: FixtureError, field: &str) {
        let FixtureError::InvalidConfiguration { detail } = error else {
            panic!("expected invalid configuration error");
        };

        assert!(
            detail.contains(field),
            "configuration detail should identify {field}: {detail}"
        );
        assert!(
            detail.len() <= 256,
            "configuration detail should remain bounded: {detail}"
        );
    }

    // Catches a regression that conflates the runtime hostname with its mapped TCP port.
    #[test]
    fn endpoint_keeps_hostname_and_port_separate() {
        let endpoint = ElectrumEndpoint::new("docker.example", 50_123)
            .expect("a hostname and non-zero port are valid");

        assert_eq!(endpoint.host(), "docker.example");
        assert_eq!(endpoint.port(), 50_123);
    }

    // Catches a regression that allows an endpoint with no target hostname.
    #[test]
    fn empty_endpoint_host_is_rejected() {
        let error =
            ElectrumEndpoint::new("", 50_001).expect_err("an empty endpoint host must be rejected");

        assert_invalid_configuration(error, "endpoint host");
    }

    // Catches a regression that allows an endpoint with an unusable zero port.
    #[test]
    fn zero_endpoint_port_is_rejected() {
        let error = ElectrumEndpoint::new("docker.example", 0)
            .expect_err("a zero endpoint port must be rejected");

        assert_invalid_configuration(error, "endpoint port");
    }
}
