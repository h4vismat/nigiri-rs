use crate::NigiriError;

/// A host and TCP port serving the Electrum protocol.
///
/// Kept separate from the HTTP endpoints because Electrum is a raw TCP protocol: there is no URL
/// scheme to normalize and no path to preserve, and callers need the two parts individually to
/// build whatever connection string their Electrum client expects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElectrumEndpoint {
    host: String,
    port: u16,
}

impl ElectrumEndpoint {
    /// Validates and stores a host and port.
    pub fn new(host: impl Into<String>, port: u16) -> Result<Self, NigiriError> {
        let host = host.into();

        if host.is_empty() {
            return Err(NigiriError::InvalidRequest {
                detail: "endpoint host must not be empty".into(),
            });
        }

        if port == 0 {
            return Err(NigiriError::InvalidRequest {
                detail: "endpoint port must not be zero".into(),
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
    use crate::{ElectrumEndpoint, NigiriError};

    fn assert_invalid(error: NigiriError, field: &str) {
        let NigiriError::InvalidRequest { detail } = error else {
            panic!("expected an invalid-request error");
        };
        assert!(
            detail.contains(field),
            "detail should identify {field}: {detail}"
        );
    }

    // Catches a regression that conflates the hostname with its TCP port.
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

        assert_invalid(error, "endpoint host");
    }

    // Catches a regression that allows an endpoint with an unusable zero port.
    #[test]
    fn zero_endpoint_port_is_rejected() {
        let error = ElectrumEndpoint::new("docker.example", 0)
            .expect_err("a zero endpoint port must be rejected");

        assert_invalid(error, "endpoint port");
    }
}
