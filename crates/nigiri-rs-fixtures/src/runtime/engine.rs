use std::{
    collections::HashMap,
    error::Error,
    fmt,
    io::{self, Cursor, Read},
};

use bollard::{
    Docker,
    errors::Error as BollardError,
    models::{ContainerCreateBody, HostConfig, NetworkCreateRequest, PortBinding},
    query_parameters::{
        CreateContainerOptionsBuilder, CreateImageOptionsBuilder,
        DownloadFromContainerOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    },
};
use futures_util::StreamExt;

use super::spec::ContainerSpec;

// Docker's archive response adds 512-byte tar headers, file padding, end markers, and may add
// bounded PAX metadata. Reserving 64 KiB for all framing keeps the collector deterministic while
// accommodating substantially more metadata than either fixed credential path needs.
#[allow(
    dead_code,
    reason = "Task 6 file reads are consumed by the Task 7 LndPair startup"
)]
const TAR_ARCHIVE_OVERHEAD_BYTES: usize = 64 * 1024;

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
    #[allow(
        dead_code,
        reason = "Task 6 file reads are consumed by the Task 7 LndPair startup"
    )]
    async fn read_container_file(
        &self,
        id: &str,
        path: &str,
        max_bytes: usize,
    ) -> EngineResult<Vec<u8>>;
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

    async fn read_container_file(
        &self,
        id: &str,
        path: &str,
        max_bytes: usize,
    ) -> EngineResult<Vec<u8>> {
        let archive_limit = max_bytes
            .checked_add(TAR_ARCHIVE_OVERHEAD_BYTES)
            .ok_or_else(|| read_file_error("container file byte limit is too large"))?;
        let options = DownloadFromContainerOptionsBuilder::default()
            .path(path)
            .build();
        let mut stream = self.docker.download_from_container(id, Some(options));
        let mut archive = Vec::with_capacity(archive_limit.min(TAR_ARCHIVE_OVERHEAD_BYTES));

        while let Some(result) = stream.next().await {
            let chunk = result.map_err(|error| EngineError::new("read container file", error))?;
            append_archive_chunk(&mut archive, &chunk, archive_limit)
                .map_err(|error| EngineError::new("read container file", error))?;
        }

        decode_single_file_archive(&archive, max_bytes)
            .map_err(|error| EngineError::new("read container file", error))
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

#[allow(
    dead_code,
    reason = "Task 6 file reads are consumed by the Task 7 LndPair startup"
)]
fn append_archive_chunk(
    archive: &mut Vec<u8>,
    chunk: &[u8],
    maximum_bytes: usize,
) -> io::Result<()> {
    let remaining = maximum_bytes.saturating_sub(archive.len());
    if archive.len() > maximum_bytes || chunk.len() > remaining {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "container file archive exceeds its bounded overhead",
        ));
    }
    archive.extend_from_slice(chunk);
    Ok(())
}

#[allow(
    dead_code,
    reason = "Task 6 file reads are consumed by the Task 7 LndPair startup"
)]
fn decode_single_file_archive(archive_bytes: &[u8], max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut archive = tar::Archive::new(Cursor::new(archive_bytes));
    let mut entries = archive
        .entries()
        .map_err(|_| invalid_archive("container file archive is invalid"))?;

    let contents = {
        let first = entries
            .next()
            .ok_or_else(|| invalid_archive("container file archive is empty"))?
            .map_err(|_| invalid_archive("container file archive entry is invalid"))?;
        if !first.header().entry_type().is_file() {
            return Err(invalid_archive(
                "container file archive entry is not a regular file",
            ));
        }
        let declared_size = first
            .header()
            .size()
            .map_err(|_| invalid_archive("container file archive size is invalid"))?;
        let maximum_u64 = u64::try_from(max_bytes).unwrap_or(u64::MAX);
        if declared_size > maximum_u64 {
            return Err(invalid_archive(
                "container file exceeds requested byte limit",
            ));
        }

        let declared_capacity = usize::try_from(declared_size).unwrap_or(max_bytes);
        let mut contents = Vec::with_capacity(max_bytes.min(declared_capacity));
        first
            .take(maximum_u64.saturating_add(1))
            .read_to_end(&mut contents)
            .map_err(|_| invalid_archive("container file archive content is invalid"))?;
        if contents.len() > max_bytes {
            return Err(invalid_archive(
                "container file exceeds requested byte limit",
            ));
        }
        contents
    };

    match entries.next() {
        None => Ok(contents),
        Some(Ok(_)) => Err(invalid_archive(
            "container file archive contains multiple entries",
        )),
        Some(Err(_)) => Err(invalid_archive("container file archive entry is invalid")),
    }
}

#[allow(
    dead_code,
    reason = "Task 6 file reads are consumed by the Task 7 LndPair startup"
)]
fn invalid_archive(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[allow(
    dead_code,
    reason = "Task 6 file reads are consumed by the Task 7 LndPair startup"
)]
fn read_file_error(message: &'static str) -> EngineError {
    EngineError::new("read container file", invalid_archive(message))
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
    use std::{collections::HashMap, error::Error, io::Cursor};

    use bollard::models::PortBinding;
    use nigiri_rs_core::Liquid;
    use tar::{Builder, EntryType, Header};

    use super::{EngineError, append_archive_chunk, container_body, decode_single_file_archive};
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

    fn tar_archive(entries: &[(&str, EntryType, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = Builder::new(&mut bytes);
            for (path, entry_type, contents) in entries {
                let mut header = Header::new_gnu();
                header.set_entry_type(*entry_type);
                header.set_mode(0o600);
                header.set_size(contents.len() as u64);
                header.set_cksum();
                builder
                    .append_data(&mut header, path, Cursor::new(*contents))
                    .expect("the test archive is valid");
            }
            builder.finish().expect("the test archive is complete");
        }
        bytes
    }

    // Catches a regression that trusts a tar header or reads one byte past either credential's
    // configured cap. The marker also proves errors never render file content.
    #[test]
    fn container_archive_decoder_enforces_file_bounds_without_exposing_contents() {
        const LIMIT: usize = 64 * 1024;
        let exact = vec![b'm'; LIMIT];
        let decoded = decode_single_file_archive(
            &tar_archive(&[("admin.macaroon", EntryType::Regular, &exact)]),
            LIMIT,
        )
        .expect("a regular file at the limit is accepted");
        assert_eq!(decoded, exact);

        let marker = b"credential-marker-never-render";
        let mut oversized = vec![b'x'; LIMIT + 1];
        oversized[..marker.len()].copy_from_slice(marker);
        let error = decode_single_file_archive(
            &tar_archive(&[("admin.macaroon", EntryType::Regular, &oversized)]),
            LIMIT,
        )
        .expect_err("one byte over the limit is rejected");
        let error = EngineError::new("read container file", error);
        assert!(!error.to_string().contains("credential-marker"));
        assert!(!format!("{error:?}").contains("credential-marker"));
        assert!(
            !Error::source(&error)
                .expect("an engine failure preserves its sanitized cause")
                .to_string()
                .contains("credential-marker")
        );
    }

    // Catches accepting an archive shape that could make a credential read follow a link, select
    // an arbitrary entry, or silently treat a directory as an empty credential.
    #[test]
    fn container_archive_decoder_accepts_exactly_one_regular_file() {
        let directory = tar_archive(&[("tls.cert", EntryType::Directory, b"")]);
        assert!(decode_single_file_archive(&directory, 1_048_576).is_err());

        let link = tar_archive(&[("tls.cert", EntryType::Symlink, b"")]);
        assert!(decode_single_file_archive(&link, 1_048_576).is_err());

        let multiple = tar_archive(&[
            ("tls.cert", EntryType::Regular, b"certificate"),
            ("unexpected", EntryType::Regular, b"second"),
        ]);
        assert!(decode_single_file_archive(&multiple, 1_048_576).is_err());

        let empty = tar_archive(&[]);
        assert!(decode_single_file_archive(&empty, 1_048_576).is_err());
    }

    // Catches a stream collector that appends an over-limit Docker chunk before checking it,
    // temporarily buffering unbounded credential data despite ultimately returning an error.
    #[test]
    fn archive_chunk_collection_never_grows_past_its_bound() {
        let mut archive = vec![0_u8; 7];
        append_archive_chunk(&mut archive, &[1, 2, 3], 10)
            .expect("a chunk ending at the bound is accepted");
        assert_eq!(archive.len(), 10);

        let error = append_archive_chunk(&mut archive, b"credential-marker", 10)
            .expect_err("a chunk beyond the bound is rejected before append");
        assert_eq!(archive.len(), 10);
        assert!(!error.to_string().contains("credential-marker"));
    }
}
