use crate::FixtureError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElectrumEndpoint {
    host: String,
    port: u16,
}

impl ElectrumEndpoint {
    #[cfg_attr(not(test), allow(dead_code))]
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

#[cfg(test)]
mod tests {
    use crate::{ElectrumEndpoint, FixtureError};

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
