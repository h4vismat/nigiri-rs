mod engine;
mod resources;
mod spec;
mod supervisor;

use crate::{
    FixtureError,
    deadline::Deadline,
    diagnostics::{join_diagnostics, redacted_source, redacted_tail},
};

pub(crate) use engine::{BollardEngine, ContainerEngine};
#[cfg(test)]
pub(crate) use engine::{EngineError, EngineResult};
#[cfg(test)]
pub(crate) use spec::ContainerSpec;
#[allow(
    unused_imports,
    reason = "Task 6 specification is consumed by the Task 7 LndPair startup"
)]
pub(crate) use spec::{electrs_spec, lnd_spec, node_spec};
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
        FixtureError::Bootstrap {
            chain,
            operation,
            diagnostics,
            source,
        } => FixtureError::Bootstrap {
            chain,
            operation,
            diagnostics: join_diagnostics(&diagnostics, &addition),
            source,
        },
        FixtureError::Probe {
            service,
            operation,
            diagnostics,
            source,
        } => FixtureError::Probe {
            service,
            operation,
            diagnostics: join_diagnostics(&diagnostics, &addition),
            source,
        },
        other => other,
    }
}

pub(crate) async fn attach_container_log<E: ContainerEngine>(
    startup: &mut Startup<E>,
    deadline: &Deadline,
    service: &'static str,
    id_or_name: &str,
    error: FixtureError,
) -> FixtureError {
    let diagnostics = match deadline
        .run(
            service,
            "reading bounded startup diagnostics",
            startup.logs(id_or_name),
        )
        .await
    {
        Ok(Ok(logs)) => redacted_tail(&format!("{service} log:\n{logs}\n[end {service} log]")),
        Ok(Err(failure)) => redacted_tail(&format!(
            "could not read the {service} diagnostic log: {failure}"
        )),
        Err(_) => format!("skipped the {service} diagnostic log: startup deadline exhausted"),
    };
    attach_diagnostics(error, diagnostics)
}
