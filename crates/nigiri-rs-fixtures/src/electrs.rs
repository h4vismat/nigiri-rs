use url::Url;

use crate::{
    ContainerImage, ElectrumEndpoint, FixtureError,
    chain::FixtureChain,
    deadline::Deadline,
    endpoint::mapped_http_url,
    runtime::{ContainerEngine, RunningContainer, Startup, electrs_spec, runtime_error},
};

pub(crate) const SERVICE: &str = "electrs";

/// A running Electrs and the two endpoints a fixture serves from it.
pub(crate) struct StartedElectrs {
    pub(crate) container: RunningContainer,
    pub(crate) esplora_url: Url,
    pub(crate) electrum_endpoint: ElectrumEndpoint,
}

/// Starts Electrs against an already-running node and resolves both of its mapped ports.
///
/// Electrs is reached only through mapped ports, never the fixed container ports, so concurrent
/// fixtures cannot collide on the host.
pub(crate) async fn start_electrs<C: FixtureChain, E: ContainerEngine>(
    startup: &mut Startup<E>,
    image: &ContainerImage,
    network_name: &str,
    container_name: &str,
    node_name: &str,
    deadline: &Deadline,
) -> Result<StartedElectrs, FixtureError> {
    let spec = electrs_spec::<C>(
        image.clone(),
        network_name.to_owned(),
        container_name.to_owned(),
        node_name,
    )?;
    let container = match deadline
        .run(
            SERVICE,
            "starting Electrs container",
            startup.start_container(spec),
        )
        .await?
    {
        Ok(container) => container,
        Err(error) => {
            let error = runtime_error(SERVICE, error);
            return Err(crate::runtime::attach_container_log(
                startup,
                deadline,
                SERVICE,
                container_name,
                error,
            )
            .await);
        }
    };

    let host = container.host.clone();
    let esplora_port = *container.ports.get(&C::ELECTRS_HTTP_PORT).ok_or_else(|| {
        FixtureError::InvalidConfiguration {
            detail: format!(
                "container runtime omitted the mapped {} port for {SERVICE}",
                C::ELECTRS_HTTP_PORT
            ),
        }
    })?;
    let electrum_port = *container
        .ports
        .get(&C::ELECTRS_ELECTRUM_PORT)
        .ok_or_else(|| FixtureError::InvalidConfiguration {
            detail: format!(
                "container runtime omitted the mapped {} port for {SERVICE}",
                C::ELECTRS_ELECTRUM_PORT
            ),
        })?;

    Ok(StartedElectrs {
        container,
        esplora_url: mapped_http_url(&host, esplora_port)?,
        electrum_endpoint: ElectrumEndpoint::new(host, electrum_port).map_err(|_| {
            FixtureError::InvalidConfiguration {
                detail: "container runtime returned an invalid mapped Electrum endpoint".to_owned(),
            }
        })?,
    })
}
