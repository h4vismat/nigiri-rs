mod engine;
mod resources;
mod spec;
mod supervisor;

use crate::{
    FixtureError,
    diagnostics::{join_diagnostics, redacted_source, redacted_tail},
};

pub(crate) use engine::{BollardEngine, ContainerEngine};
pub(crate) use spec::{electrs_spec, node_spec};
pub(crate) use supervisor::{RunningContainer, RuntimeHandle, Startup, supervise};

pub(crate) fn runtime_error(
    resource: impl Into<String>,
    error: engine::EngineError,
) -> FixtureError {
    let operation = error.operation().to_owned();
    let diagnostics = redacted_tail(&error.to_string());
    FixtureError::Runtime {
        operation,
        resource: resource.into(),
        diagnostics,
        source: redacted_source(error),
    }
}

impl From<engine::EngineError> for FixtureError {
    fn from(error: engine::EngineError) -> Self {
        runtime_error("fixture supervisor", error)
    }
}

pub(crate) fn attach_diagnostics(error: FixtureError, addition: String) -> FixtureError {
    match error {
        FixtureError::Runtime {
            operation,
            resource,
            diagnostics,
            source,
        } => FixtureError::Runtime {
            operation,
            resource,
            diagnostics: join_diagnostics(&diagnostics, &addition),
            source,
        },
        FixtureError::ReadinessTimeout {
            service,
            duration,
            last_observation,
            diagnostics,
        } => FixtureError::ReadinessTimeout {
            service,
            duration,
            last_observation,
            diagnostics: join_diagnostics(&diagnostics, &addition),
        },
        other => other,
    }
}

pub(crate) async fn attach_container_log<E: ContainerEngine>(
    startup: &mut Startup<E>,
    service: &'static str,
    id_or_name: &str,
    error: FixtureError,
) -> FixtureError {
    let diagnostics = match startup.logs(id_or_name).await {
        Ok(logs) => redacted_tail(&format!("{service} log:\n{logs}\n[end {service} log]")),
        Err(failure) => redacted_tail(&format!(
            "could not read the {service} diagnostic log: {failure}"
        )),
    };
    attach_diagnostics(error, diagnostics)
}
