use std::time::Duration;

use nigiri_rs::NigiriError;

#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("invalid fixture configuration: {detail}")]
    InvalidConfiguration { detail: String },
    #[error("container runtime is unavailable")]
    RuntimeUnavailable {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("failed to start {service} from {image}: {diagnostics}")]
    ContainerStart {
        service: &'static str,
        image: String,
        diagnostics: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("failed to discover mapped {container_port} port for {service}: {diagnostics}")]
    PortDiscovery {
        service: &'static str,
        container_port: u16,
        diagnostics: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("Bitcoin wallet bootstrap failed during {operation}: {diagnostics}")]
    Bootstrap {
        operation: &'static str,
        diagnostics: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("{service} was not ready after {duration:?}: {last_observation}; {diagnostics}")]
    ReadinessTimeout {
        service: &'static str,
        duration: Duration,
        last_observation: String,
        diagnostics: String,
    },
    #[error(transparent)]
    Client(#[from] NigiriError),
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io, time::Duration};

    use crate::FixtureError;

    // Catches a regression that leaks runtime details into the stable unavailable-runtime message.
    #[test]
    fn runtime_unavailable_has_stable_display_and_preserves_its_source() {
        let error = FixtureError::RuntimeUnavailable {
            source: Box::new(io::Error::other("connection refused")),
        };

        assert_eq!(error.to_string(), "container runtime is unavailable");
        assert_eq!(
            Error::source(&error).map(ToString::to_string).as_deref(),
            Some("connection refused")
        );
    }

    // Catches a regression that omits the failing service, image, diagnostics, or underlying cause.
    #[test]
    fn container_start_reports_context_and_preserves_its_source() {
        let error = FixtureError::ContainerStart {
            service: "bitcoind",
            image: "registry.example/bitcoin:v1".to_owned(),
            diagnostics: "health check timed out".to_owned(),
            source: Box::new(io::Error::other("connection refused")),
        };

        assert_eq!(
            error.to_string(),
            "failed to start bitcoind from registry.example/bitcoin:v1: health check timed out"
        );
        assert_eq!(
            Error::source(&error).map(ToString::to_string).as_deref(),
            Some("connection refused")
        );
    }

    // Catches a regression that drops readiness diagnostics or attaches a synthetic source.
    #[test]
    fn readiness_timeout_reports_observation_without_a_source() {
        let error = FixtureError::ReadinessTimeout {
            service: "electrs",
            duration: Duration::from_secs(30),
            last_observation: "TCP port still closed".to_owned(),
            diagnostics: "container remains running".to_owned(),
        };

        assert_eq!(
            error.to_string(),
            "electrs was not ready after 30s: TCP port still closed; container remains running"
        );
        assert!(Error::source(&error).is_none());
    }
}
