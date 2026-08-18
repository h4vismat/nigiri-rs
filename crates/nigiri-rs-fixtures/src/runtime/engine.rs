use std::{collections::HashMap, error::Error, fmt};

use bollard::{
    Docker,
    errors::Error as BollardError,
    models::{ContainerCreateBody, HostConfig, NetworkCreateRequest, PortBinding},
    query_parameters::{
        CreateContainerOptionsBuilder, CreateImageOptionsBuilder, LogsOptionsBuilder,
        RemoveContainerOptionsBuilder,
    },
};
use futures_util::StreamExt;

use super::spec::ContainerSpec;

pub(crate) type EngineResult<T> = Result<T, EngineError>;

#[derive(Debug)]
pub(crate) struct EngineError {
    operation: &'static str,
    source: Box<dyn Error + Send + Sync>,
}

impl EngineError {
    pub(crate) fn new(operation: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            operation,
            source: Box::new(source),
        }
    }

    pub(crate) fn operation(&self) -> &'static str {
        self.operation
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.operation, self.source)
    }
}

impl Error for EngineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[allow(async_fn_in_trait)]
pub(crate) trait ContainerEngine: Clone + Send + Sync + 'static {
    fn endpoint_host(&self) -> &str;
    async fn create_network(
        &self,
        name: &str,
        labels: HashMap<String, String>,
    ) -> EngineResult<String>;
    async fn ensure_image(&self, spec: &ContainerSpec) -> EngineResult<()>;
    async fn create_container(
        &self,
        spec: &ContainerSpec,
        labels: HashMap<String, String>,
    ) -> EngineResult<String>;
    async fn start_container(&self, id: &str) -> EngineResult<()>;
    async fn mapped_port(&self, id: &str, container_port: u16) -> EngineResult<u16>;
    async fn logs(&self, id: &str) -> EngineResult<String>;
    async fn remove_container(&self, id_or_name: &str) -> EngineResult<()>;
    async fn remove_network(&self, id_or_name: &str) -> EngineResult<()>;
}

#[derive(Clone)]
pub(crate) struct BollardEngine {
    docker: Docker,
    endpoint_host: String,
}

impl BollardEngine {
    pub(crate) async fn connect() -> EngineResult<Self> {
        let configured_host = std::env::var("DOCKER_HOST").ok();
        let endpoint_host = endpoint_host(configured_host.as_deref());
        let docker = Docker::connect_with_defaults()
            .map_err(|error| EngineError::new("connect to container engine", error))?;
        docker
            .ping()
            .await
            .map_err(|error| EngineError::new("ping container engine", error))?;
        Ok(Self {
            docker,
            endpoint_host,
        })
    }
}

impl ContainerEngine for BollardEngine {
    fn endpoint_host(&self) -> &str {
        &self.endpoint_host
    }

    async fn create_network(
        &self,
        name: &str,
        labels: HashMap<String, String>,
    ) -> EngineResult<String> {
        self.docker
            .create_network(NetworkCreateRequest {
                name: name.to_owned(),
                labels: Some(labels),
                ..Default::default()
            })
            .await
            .map(|network| network.id)
            .map_err(|error| EngineError::new("create network", error))
    }

    async fn ensure_image(&self, spec: &ContainerSpec) -> EngineResult<()> {
        let descriptor = image_descriptor(spec);
        match self.docker.inspect_image(&descriptor).await {
            Ok(_) => return Ok(()),
            Err(error) if is_not_found(&error) => {}
            Err(error) => return Err(EngineError::new("inspect image", error)),
        }

        let options = CreateImageOptionsBuilder::default()
            .from_image(&descriptor)
            .build();
        let mut stream = self.docker.create_image(Some(options), None, None);
        while let Some(result) = stream.next().await {
            result.map_err(|error| EngineError::new("pull image", error))?;
        }
        Ok(())
    }

    async fn create_container(
        &self,
        spec: &ContainerSpec,
        labels: HashMap<String, String>,
    ) -> EngineResult<String> {
        let options = CreateContainerOptionsBuilder::default()
            .name(&spec.name)
            .build();
        self.docker
            .create_container(Some(options), container_body(spec, labels))
            .await
            .map(|container| container.id)
            .map_err(|error| EngineError::new("create container", error))
    }

    async fn start_container(&self, id: &str) -> EngineResult<()> {
        self.docker
            .start_container(id, None)
            .await
            .map_err(|error| EngineError::new("start container", error))
    }

    async fn mapped_port(&self, id: &str, container_port: u16) -> EngineResult<u16> {
        let inspected = self
            .docker
            .inspect_container(id, None)
            .await
            .map_err(|error| EngineError::new("inspect container ports", error))?;
        let key = format!("{container_port}/tcp");
        let mapped = inspected
            .network_settings
            .and_then(|settings| settings.ports)
            .and_then(|mut ports| ports.remove(&key))
            .flatten()
            .and_then(|bindings| bindings.into_iter().next())
            .and_then(|binding| binding.host_port)
            .and_then(|port| port.parse().ok())
            .filter(|port| *port != 0);

        mapped.ok_or_else(|| {
            EngineError::new(
                "inspect container ports",
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("container has no mapped {container_port}/tcp port"),
                ),
            )
        })
    }

    async fn logs(&self, id: &str) -> EngineResult<String> {
        let options = LogsOptionsBuilder::default()
            .stdout(true)
            .stderr(true)
            .tail("all")
            .build();
        let mut logs = self.docker.logs(id, Some(options));
        let mut output = String::new();
        while let Some(item) = logs.next().await {
            let item = item.map_err(|error| EngineError::new("read container logs", error))?;
            output.push_str(&String::from_utf8_lossy(&item.into_bytes()));
        }
        Ok(output)
    }

    async fn remove_container(&self, id_or_name: &str) -> EngineResult<()> {
        let options = RemoveContainerOptionsBuilder::default()
            .force(true)
            .v(true)
            .build();
        match self
            .docker
            .remove_container(id_or_name, Some(options))
            .await
        {
            Ok(()) => Ok(()),
            Err(error) if is_not_found(&error) => Ok(()),
            Err(error) => Err(EngineError::new("remove container", error)),
        }
    }

    async fn remove_network(&self, id_or_name: &str) -> EngineResult<()> {
        match self.docker.remove_network(id_or_name).await {
            Ok(()) => Ok(()),
            Err(error) if is_not_found(&error) => Ok(()),
            Err(error) => Err(EngineError::new("remove network", error)),
        }
    }
}

fn endpoint_host(configured_host: Option<&str>) -> String {
    configured_host
        .and_then(|host| url::Url::parse(host).ok())
        .and_then(|host| host.host_str().map(str::to_owned))
        .unwrap_or_else(|| "127.0.0.1".to_owned())
}

fn image_descriptor(spec: &ContainerSpec) -> String {
    format!("{}:{}", spec.image.name(), spec.image.reference_suffix())
}

fn is_not_found(error: &BollardError) -> bool {
    matches!(
        error,
        BollardError::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

fn container_body(spec: &ContainerSpec, labels: HashMap<String, String>) -> ContainerCreateBody {
    let exposed_ports: Vec<String> = spec
        .exposed_ports
        .iter()
        .map(|port| format!("{port}/tcp"))
        .collect();
    let port_bindings = exposed_ports
        .iter()
        .map(|port| {
            (
                port.clone(),
                Some(vec![PortBinding {
                    host_ip: Some("127.0.0.1".to_owned()),
                    host_port: None,
                }]),
            )
        })
        .collect();

    ContainerCreateBody {
        image: Some(image_descriptor(spec)),
        entrypoint: spec.entrypoint.clone().map(|entrypoint| vec![entrypoint]),
        cmd: Some(spec.command.clone()),
        labels: Some(labels),
        exposed_ports: Some(exposed_ports),
        host_config: Some(HostConfig {
            network_mode: Some(spec.network.clone()),
            port_bindings: Some(port_bindings),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use bollard::models::PortBinding;
    use nigiri_rs_core::Liquid;

    use super::container_body;
    use crate::{ContainerImage, runtime::spec::node_spec};

    #[test]
    fn container_body_preserves_the_spec_and_requests_random_host_ports() {
        let spec = node_spec::<Liquid>(
            ContainerImage::elements_default(),
            "fixture-network".to_owned(),
            "elements-node".to_owned(),
            Vec::new(),
        )
        .expect("the pinned Elements specification is valid");

        let body = container_body(
            &spec,
            HashMap::from([("nigiri-rs.fixture".to_owned(), "session-id".to_owned())]),
        );

        assert_eq!(
            body.image.as_deref(),
            Some(
                "blockstream/elementsd:23.3.3@sha256:1abe3ae514662492279c9ba8adc94fea46a0fa60efdd62f4eb93d3e803adff37"
            )
        );
        assert_eq!(body.entrypoint, Some(vec!["elementsd".to_owned()]));
        assert_eq!(body.cmd, Some(spec.command));
        assert_eq!(
            body.labels
                .and_then(|labels| labels.get("nigiri-rs.fixture").cloned())
                .as_deref(),
            Some("session-id")
        );
        assert_eq!(body.exposed_ports, Some(vec!["18884/tcp".to_owned()]));

        let host = body
            .host_config
            .expect("container body carries host configuration");
        assert_eq!(host.network_mode.as_deref(), Some("fixture-network"));
        assert_eq!(
            host.port_bindings,
            Some(HashMap::from([(
                "18884/tcp".to_owned(),
                Some(vec![PortBinding {
                    host_ip: Some("127.0.0.1".to_owned()),
                    host_port: None,
                }]),
            )]))
        );
    }

    #[test]
    fn runtime_endpoint_host_tracks_local_and_remote_engine_addresses() {
        assert_eq!(super::endpoint_host(None), "127.0.0.1");
        assert_eq!(
            super::endpoint_host(Some("unix:///var/run/docker.sock")),
            "127.0.0.1"
        );
        assert_eq!(
            super::endpoint_host(Some("tcp://engine.example:2375")),
            "engine.example"
        );
        assert_eq!(
            super::endpoint_host(Some("ssh://operator@engine.example:22")),
            "engine.example"
        );
    }
}
