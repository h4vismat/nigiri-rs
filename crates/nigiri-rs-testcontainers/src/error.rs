use std::time::Duration;

use nigiri_rs_core::NigiriError;

/// Error model for starting and operating a composite Docker-backed fixture.
///
/// `#[non_exhaustive]`: variants are added as new composites land — Lightning channel wiring,
/// Ark round diagnostics, Liquid peg bootstrap — and a downstream match must carry a wildcard arm
/// rather than being broken by each addition.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
    #[error("{chain} wallet bootstrap failed during {operation}: {diagnostics}")]
    Bootstrap {
        chain: &'static str,
        operation: &'static str,
        diagnostics: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("{service} {operation} probe failed: {diagnostics}")]
    Probe {
        service: &'static str,
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

    // Catches a regression that omits the probed service, its operation, the bounded diagnostics, or
    // the underlying cause of a protocol probe failure.
    #[test]
    fn probe_reports_service_and_operation_and_preserves_its_source() {
        let error = FixtureError::Probe {
            service: "electrs",
            operation: "blockchain.headers.subscribe",
            diagnostics: "connection refused".to_owned(),
            source: Box::new(io::Error::other("connection refused")),
        };

        assert_eq!(
            error.to_string(),
            "electrs blockchain.headers.subscribe probe failed: connection refused"
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

    // Catches a regression that drops the chain from a bootstrap failure, which would make a
    // two-chain test run unable to say which stack failed.
    #[test]
    fn bootstrap_names_the_chain_that_failed() {
        let error = FixtureError::Bootstrap {
            chain: "Liquid",
            operation: "rescanblockchain",
            diagnostics: "wallet not loaded".to_owned(),
            source: Box::new(io::Error::other("wallet not loaded")),
        };

        assert_eq!(
            error.to_string(),
            "Liquid wallet bootstrap failed during rescanblockchain: wallet not loaded"
        );
        assert_eq!(
            Error::source(&error).map(ToString::to_string).as_deref(),
            Some("wallet not loaded")
        );
    }
}
