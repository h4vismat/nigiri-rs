use std::{collections::HashMap, error::Error, fmt, future::Future, io};

use bollard::{
    Docker,
    errors::Error as BollardError,
    models::{ContainerCreateBody, HostConfig, NetworkCreateRequest, PortBinding},
    query_parameters::{
        CreateContainerOptionsBuilder, CreateImageOptionsBuilder, LogsOptionsBuilder,
        RemoveContainerOptionsBuilder,
    },
};
#[cfg(any(feature = "lnd", test))]
use bollard::{
    container::PathStatResponse,
    query_parameters::{ContainerArchiveInfoOptionsBuilder, DownloadFromContainerOptionsBuilder},
};
#[cfg(any(feature = "lnd", test))]
use std::io::{Cursor, Read};

use futures_util::StreamExt;

use super::spec::ContainerSpec;

// Docker's archive response adds 512-byte tar headers, file padding, and end markers. Reserving 64
// KiB for framing keeps the collector deterministic; the decoder separately rejects unexpected
// metadata entries rather than letting them supply alternate path or size authority.
#[cfg(any(feature = "lnd", test))]
const TAR_ARCHIVE_OVERHEAD_BYTES: usize = 64 * 1024;
#[cfg(any(feature = "lnd", test))]
const TAR_BLOCK_BYTES: usize = 512;
#[cfg(any(feature = "lnd", test))]
const TAR_END_BLOCKS: usize = 2;
// Docker serializes Go's os.FileMode. A regular file has none of these ModeType bits set; special
// permission bits such as setuid are intentionally not part of this mask.
#[cfg(any(feature = "lnd", test))]
const DOCKER_FILE_MODE_TYPE_MASK: u32 = 0x8f28_0000;

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

    #[cfg(feature = "lnd")]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.operation == "start fixture"
            && self
                .source
                .downcast_ref::<io::Error>()
                .is_some_and(|source| source.kind() == io::ErrorKind::Interrupted)
    }

    /// A bounded file read may be retried only when Docker reports that the path is not present
    /// yet. Archive framing, metadata, path, and size failures are permanent safety violations.
    #[cfg(feature = "lnd")]
    pub(crate) fn is_transient_file_unavailable(&self) -> bool {
        self.operation == "read container file"
            && self
                .source
                .downcast_ref::<io::Error>()
                .is_some_and(|source| {
                    matches!(
                        source.kind(),
                        io::ErrorKind::NotFound
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::ConnectionRefused
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::NotConnected
                    )
                })
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

pub(crate) trait ContainerEngine: Clone + Send + Sync + 'static {
    fn endpoint_host(&self) -> &str;
    fn create_network(
        &self,
        name: &str,
        labels: HashMap<String, String>,
    ) -> impl Future<Output = EngineResult<String>> + Send;
    fn ensure_image(&self, spec: &ContainerSpec) -> impl Future<Output = EngineResult<()>> + Send;
    fn create_container(
        &self,
        spec: &ContainerSpec,
        labels: HashMap<String, String>,
    ) -> impl Future<Output = EngineResult<String>> + Send;
    fn start_container(&self, id: &str) -> impl Future<Output = EngineResult<()>> + Send;
    fn mapped_port(
        &self,
        id: &str,
        container_port: u16,
    ) -> impl Future<Output = EngineResult<u16>> + Send;
    fn logs(&self, id: &str) -> impl Future<Output = EngineResult<String>> + Send;
    #[cfg(any(feature = "lnd", test))]
    fn read_container_file(
        &self,
        id: &str,
        path: &str,
        max_bytes: usize,
    ) -> impl Future<Output = EngineResult<Vec<u8>>> + Send;
    fn remove_container(&self, id_or_name: &str) -> impl Future<Output = EngineResult<()>> + Send;
    fn remove_network(&self, id_or_name: &str) -> impl Future<Output = EngineResult<()>> + Send;
}

#[derive(Clone)]
pub(crate) struct BollardEngine {
    docker: Docker,
    endpoint_host: String,
}

impl BollardEngine {
    pub(crate) async fn connect() -> EngineResult<Self> {
        let configured_host = std::env::var("DOCKER_HOST").ok();
        let endpoint_host = endpoint_host(configured_host.as_deref())?;
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
        // Start at the beginning to retain multiline secret opening context. Collection stops
        // promptly at its byte/chunk bound; the result explicitly describes an initial sample.
        collect_log_sample(self.docker.logs(id, Some(options)).map(|item| {
            item.map(|item| item.into_bytes())
                .map_err(|error| EngineError::new("read container logs", error))
        }))
        .await
    }

    #[cfg(any(feature = "lnd", test))]
    async fn read_container_file(
        &self,
        id: &str,
        path: &str,
        max_bytes: usize,
    ) -> EngineResult<Vec<u8>> {
        let expected_basename = expected_archive_basename(path)
            .map_err(|error| EngineError::new("read container file", error))?;
        let stat_options = ContainerArchiveInfoOptionsBuilder::default()
            .path(path)
            .build();
        let stat = self
            .docker
            .get_container_archive_info(id, Some(stat_options))
            .await
            .map_err(sanitized_read_file_api_error)?;
        let expected_size = validate_container_file_stat(&stat, expected_basename, max_bytes)
            .map_err(|error| EngineError::new("read container file", error))?;
        let archive_limit = max_bytes
            .checked_add(TAR_ARCHIVE_OVERHEAD_BYTES)
            .ok_or_else(|| read_file_error("container file byte limit is too large"))?;
        let options = DownloadFromContainerOptionsBuilder::default()
            .path(path)
            .build();
        let mut stream = self.docker.download_from_container(id, Some(options));
        let mut archive = Vec::with_capacity(archive_limit.min(TAR_ARCHIVE_OVERHEAD_BYTES));

        while let Some(result) = stream.next().await {
            let chunk = result.map_err(sanitized_read_file_api_error)?;
            append_archive_chunk(&mut archive, &chunk, archive_limit)
                .map_err(|error| EngineError::new("read container file", error))?;
        }

        decode_single_file_archive(&archive, expected_basename, expected_size, max_bytes)
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

#[cfg(any(feature = "lnd", test))]
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

#[cfg(any(feature = "lnd", test))]
fn expected_archive_basename(path: &str) -> io::Result<&str> {
    let Some(relative) = path.strip_prefix('/') else {
        return Err(invalid_archive("container file path is not canonical"));
    };
    if relative.is_empty() || path.contains(['\\', '\0']) {
        return Err(invalid_archive("container file path is not canonical"));
    }

    let mut basename = None;
    for component in relative.split('/') {
        if component.is_empty() || matches!(component, "." | "..") {
            return Err(invalid_archive("container file path is not canonical"));
        }
        basename = Some(component);
    }
    basename.ok_or_else(|| invalid_archive("container file path is not canonical"))
}

#[cfg(any(feature = "lnd", test))]
fn validate_container_file_stat(
    stat: &PathStatResponse,
    expected_basename: &str,
    max_bytes: usize,
) -> io::Result<usize> {
    if stat.name.as_bytes() != expected_basename.as_bytes() {
        return Err(invalid_archive("container file metadata name is invalid"));
    }
    if !stat.link_target.is_empty() || stat.file_mode & DOCKER_FILE_MODE_TYPE_MASK != 0 {
        return Err(invalid_archive(
            "container file metadata is not a regular file",
        ));
    }
    let size = usize::try_from(stat.size)
        .map_err(|_| invalid_archive("container file metadata size is invalid"))?;
    if size > max_bytes {
        return Err(invalid_archive(
            "container file exceeds requested byte limit",
        ));
    }
    Ok(size)
}

#[cfg(any(feature = "lnd", test))]
fn decode_single_file_archive(
    archive_bytes: &[u8],
    expected_basename: &str,
    expected_size: usize,
    max_bytes: usize,
) -> io::Result<Vec<u8>> {
    if expected_size > max_bytes {
        return Err(invalid_archive(
            "container file exceeds requested byte limit",
        ));
    }
    validate_strict_tar_framing(archive_bytes, expected_size)?;

    let mut archive = tar::Archive::new(Cursor::new(archive_bytes));
    let mut entries = archive
        .entries()
        .map_err(|_| invalid_archive("container file archive is invalid"))?
        // Credential basenames are fixed and short. Exposing extension entries makes PAX/GNU
        // metadata fail the same exactly-one-regular-entry policy as every other extra member.
        .raw(true);

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
        if first.path_bytes().as_ref() != expected_basename.as_bytes() {
            return Err(invalid_archive(
                "container file archive entry name is invalid",
            ));
        }
        let declared_size = first
            .header()
            .entry_size()
            .map_err(|_| invalid_archive("container file archive size is invalid"))?;
        let expected_u64 = u64::try_from(expected_size)
            .map_err(|_| invalid_archive("container file archive size is invalid"))?;
        if declared_size != expected_u64 {
            return Err(invalid_archive(
                "container file archive size does not match metadata",
            ));
        }

        let mut contents = Vec::with_capacity(expected_size);
        first
            .take(expected_u64.saturating_add(1))
            .read_to_end(&mut contents)
            .map_err(|_| invalid_archive("container file archive content is invalid"))?;
        if contents.len() != expected_size {
            return Err(invalid_archive(
                "container file archive content is truncated",
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

#[cfg(any(feature = "lnd", test))]
fn validate_strict_tar_framing(archive_bytes: &[u8], file_size: usize) -> io::Result<()> {
    let padded_file_size = file_size
        .checked_add(TAR_BLOCK_BYTES - 1)
        .map(|size| size / TAR_BLOCK_BYTES * TAR_BLOCK_BYTES)
        .ok_or_else(|| invalid_archive("container file archive size is invalid"))?;
    let content_end = TAR_BLOCK_BYTES
        .checked_add(file_size)
        .ok_or_else(|| invalid_archive("container file archive size is invalid"))?;
    let padding_end = TAR_BLOCK_BYTES
        .checked_add(padded_file_size)
        .ok_or_else(|| invalid_archive("container file archive size is invalid"))?;
    let termination_end = TAR_BLOCK_BYTES
        .checked_mul(TAR_END_BLOCKS)
        .and_then(|terminators| padding_end.checked_add(terminators))
        .ok_or_else(|| invalid_archive("container file archive size is invalid"))?;

    if archive_bytes.len() < termination_end || !archive_bytes.len().is_multiple_of(TAR_BLOCK_BYTES)
    {
        return Err(invalid_archive(
            "container file archive framing is truncated",
        ));
    }
    if archive_bytes[content_end..padding_end]
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(invalid_archive("container file archive padding is invalid"));
    }
    if archive_bytes[padding_end..].iter().any(|byte| *byte != 0) {
        return Err(invalid_archive(
            "container file archive termination is invalid",
        ));
    }
    Ok(())
}

#[cfg(any(feature = "lnd", test))]
fn invalid_archive(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(any(feature = "lnd", test))]
fn read_file_error(message: &'static str) -> EngineError {
    EngineError::new("read container file", invalid_archive(message))
}

#[cfg(any(feature = "lnd", test))]
fn sanitized_read_file_api_error(error: BollardError) -> EngineError {
    if is_not_found(&error) {
        return EngineError::new(
            "read container file",
            io::Error::new(
                io::ErrorKind::NotFound,
                "container file is not available yet",
            ),
        );
    }
    read_file_error("container file archive request failed")
}

fn endpoint_host(configured_host: Option<&str>) -> EngineResult<String> {
    let supported = match configured_host {
        None => true,
        Some(host) => url::Url::parse(host).is_ok_and(|url| match url.scheme() {
            "unix" => url.host_str().is_none(),
            "npipe" => matches!(url.host_str(), None | Some(".")),
            "tcp" | "http" | "https" => url.host().is_some_and(|host| match host {
                url::Host::Domain(host) => {
                    host == "localhost"
                        || host
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|address| address.is_loopback())
                }
                url::Host::Ipv4(address) => address.is_loopback(),
                url::Host::Ipv6(address) => address.is_loopback(),
            }),
            _ => false,
        }),
    };
    if !supported {
        return Err(EngineError::new(
            "connect to container engine",
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported Docker endpoint: fixtures require a local socket or loopback daemon because service ports bind to 127.0.0.1",
            ),
        ));
    }
    Ok("127.0.0.1".to_owned())
}

/// Bound both retained bytes and stream polling work, including streams of empty chunks.
/// Keeping the prefix is necessary: an arbitrary tail might start inside a private-key block.
const MAX_LOG_SAMPLE_BYTES: usize = 64 * 1024;
const MAX_LOG_SAMPLE_CHUNKS: usize = 1024;

async fn collect_log_sample<B: AsRef<[u8]>>(
    stream: impl futures_util::Stream<Item = EngineResult<B>>,
) -> EngineResult<String> {
    futures_util::pin_mut!(stream);
    let mut bytes = Vec::new();
    for _ in 0..MAX_LOG_SAMPLE_CHUNKS {
        let Some(chunk) = stream.next().await else {
            return Ok(crate::diagnostics::redacted_tail(&String::from_utf8_lossy(
                &bytes,
            )));
        };
        let chunk = chunk?;
        let chunk = chunk.as_ref();
        let remaining = MAX_LOG_SAMPLE_BYTES - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if bytes.len() == MAX_LOG_SAMPLE_BYTES {
            break;
        }
    }
    // Discard the incomplete final line so cutting through a credential anchor cannot reveal a
    // fragment. A multiline block that began earlier is still redacted across all retained lines.
    bytes.truncate(
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1),
    );
    let redacted = crate::diagnostics::redacted_tail(&String::from_utf8_lossy(&bytes));
    Ok(format!(
        "{redacted}\n[TRUNCATED: initial log sample only; later logs were not collected]"
    ))
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
    use futures_util::StreamExt;
    use std::{collections::HashMap, error::Error, io::Cursor};

    use bollard::{container::PathStatResponse, models::PortBinding};
    use nigiri_rs_core::Liquid;
    use tar::{Builder, EntryType, Header};

    use super::{
        BollardError, EngineError, append_archive_chunk, container_body,
        decode_single_file_archive, expected_archive_basename, sanitized_read_file_api_error,
        validate_container_file_stat,
    };
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
    fn runtime_endpoint_host_rejects_remote_engines() {
        assert_eq!(super::endpoint_host(None).unwrap(), "127.0.0.1");
        for local in [
            "unix:///var/run/docker.sock",
            "tcp://127.0.0.1:2375",
            "http://localhost:2375",
            "tcp://[::1]:2375",
        ] {
            assert_eq!(super::endpoint_host(Some(local)).unwrap(), "127.0.0.1");
        }
        for remote in [
            "tcp://engine.example:2375",
            "ssh://operator@engine.example:22",
            "npipe://remote/pipe/docker_engine",
            "not a URL",
        ] {
            let error = super::endpoint_host(Some(remote)).unwrap_err();
            assert!(error.to_string().contains("loopback"));
            assert!(!error.to_string().contains("operator"));
        }
    }

    #[tokio::test]
    async fn log_collection_stops_at_byte_or_chunk_budget() {
        let oversized = vec![b'x'; super::MAX_LOG_SAMPLE_BYTES + 1];
        let sample = super::collect_log_sample(
            futures_util::stream::once(async { Ok::<_, super::EngineError>(oversized) })
                .chain(futures_util::stream::pending()),
        )
        .await
        .unwrap();
        assert!(sample.len() <= super::MAX_LOG_SAMPLE_BYTES + 200);
        assert!(sample.contains("initial log sample"));
        let empty = futures_util::stream::repeat_with(|| Ok::<_, super::EngineError>(Vec::new()));
        let sample = super::collect_log_sample(empty).await.unwrap();
        assert!(sample.contains("initial log sample"));
    }

    #[tokio::test]
    async fn bounded_log_sample_redacts_multiline_secrets_across_chunks() {
        let chunks = vec![
            b"ready\n-----BEGIN PRIVATE KEY-----\n".to_vec(),
            b"never reveal this\n".to_vec(),
            vec![b'x'; super::MAX_LOG_SAMPLE_BYTES],
        ];
        let sample = super::collect_log_sample(futures_util::stream::iter(
            chunks.into_iter().map(Ok::<_, super::EngineError>),
        ))
        .await
        .unwrap();
        assert!(!sample.contains("never reveal"));
        assert!(!sample.contains("xxxxx"));
        assert!(sample.contains("REDACTED"));
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

    fn tar_archive_with_raw_member_name(path: &[u8], contents: &[u8]) -> Vec<u8> {
        assert!(
            path.len() <= 100,
            "the raw test path fits the tar name field"
        );

        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Regular);
        header.set_mode(0o600);
        header.set_size(contents.len() as u64);
        header.as_mut_bytes()[..100].fill(0);
        header.as_mut_bytes()[..path.len()].copy_from_slice(path);
        header.set_cksum();

        let mut bytes = Vec::from(header.as_bytes().as_slice());
        bytes.extend_from_slice(contents);
        bytes.resize(bytes.len().next_multiple_of(512), 0);
        bytes.resize(bytes.len() + 1_024, 0);
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
            "admin.macaroon",
            LIMIT,
            LIMIT,
        )
        .expect("a regular file at the limit is accepted");
        assert_eq!(decoded, exact);

        let marker = b"credential-marker-never-render";
        let mut oversized = vec![b'x'; LIMIT + 1];
        oversized[..marker.len()].copy_from_slice(marker);
        let error = decode_single_file_archive(
            &tar_archive(&[("admin.macaroon", EntryType::Regular, &oversized)]),
            "admin.macaroon",
            LIMIT + 1,
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
        assert!(decode_single_file_archive(&directory, "tls.cert", 0, 1_048_576).is_err());

        let link = tar_archive(&[("tls.cert", EntryType::Symlink, b"")]);
        assert!(decode_single_file_archive(&link, "tls.cert", 0, 1_048_576).is_err());

        let hard_link = tar_archive(&[("tls.cert", EntryType::Link, b"")]);
        assert!(decode_single_file_archive(&hard_link, "tls.cert", 0, 1_048_576).is_err());

        for extension_type in [EntryType::XHeader, EntryType::GNULongName] {
            let extended = tar_archive(&[
                ("extension", extension_type, b"tls.cert\0"),
                ("tls.cert", EntryType::Regular, b"certificate"),
            ]);
            assert!(
                decode_single_file_archive(&extended, "tls.cert", b"certificate".len(), 1_048_576,)
                    .is_err(),
                "extended metadata must not supply alternate path or size authority"
            );
        }

        let multiple = tar_archive(&[
            ("tls.cert", EntryType::Regular, b"certificate"),
            ("unexpected", EntryType::Regular, b"second"),
        ]);
        assert!(
            decode_single_file_archive(&multiple, "tls.cert", b"certificate".len(), 1_048_576)
                .is_err()
        );

        let empty = tar_archive(&[]);
        assert!(decode_single_file_archive(&empty, "tls.cert", 0, 1_048_576).is_err());
    }

    // Catches accepting Docker metadata for a resolved symlink, directory, device, wrong file,
    // negative size, or one byte above the caller's bound. Marker values must never be rendered.
    #[test]
    fn container_file_stat_accepts_only_the_exact_bounded_regular_file() {
        const LIMIT: usize = 1_048_576;

        let regular = PathStatResponse {
            name: "tls.cert".to_owned(),
            size: 11,
            file_mode: 0o600,
            modification_time: None,
            link_target: String::new(),
        };
        assert_eq!(
            validate_container_file_stat(&regular, "tls.cert", LIMIT)
                .expect("the exact regular file is accepted"),
            11
        );

        let rejected = [
            PathStatResponse {
                name: "tls.cert".to_owned(),
                size: 11,
                file_mode: (1 << 27) | 0o777,
                modification_time: None,
                link_target: "credential-marker-target".to_owned(),
            },
            PathStatResponse {
                name: "tls.cert".to_owned(),
                size: 0,
                file_mode: (1 << 31) | 0o755,
                modification_time: None,
                link_target: String::new(),
            },
            PathStatResponse {
                name: "tls.cert".to_owned(),
                size: 0,
                file_mode: (1 << 26) | 0o600,
                modification_time: None,
                link_target: String::new(),
            },
            PathStatResponse {
                name: "credential-marker-name".to_owned(),
                size: 11,
                file_mode: 0o600,
                modification_time: None,
                link_target: String::new(),
            },
            PathStatResponse {
                name: "tls.cert".to_owned(),
                size: -1,
                file_mode: 0o600,
                modification_time: None,
                link_target: String::new(),
            },
            PathStatResponse {
                name: "tls.cert".to_owned(),
                size: 1_048_577,
                file_mode: 0o600,
                modification_time: None,
                link_target: String::new(),
            },
        ];

        for stat in &rejected {
            let error = validate_container_file_stat(stat, "tls.cert", LIMIT)
                .expect_err("unsafe Docker path metadata must be rejected");
            assert!(!error.to_string().contains("credential-marker"));
            assert!(!format!("{error:?}").contains("credential-marker"));
        }
    }

    // Catches Docker's response body echoing a requested credential path into the retained source
    // chain. The file-read boundary must classify upstream failures with static text only.
    #[test]
    fn container_file_api_errors_discard_upstream_response_text() {
        let error = sanitized_read_file_api_error(BollardError::DockerResponseServerError {
            status_code: 500,
            message: "credential-marker-path".to_owned(),
        });

        assert!(!error.to_string().contains("credential-marker"));
        assert!(!format!("{error:?}").contains("credential-marker"));
        assert!(
            !Error::source(&error)
                .expect("the engine error retains a static classified source")
                .to_string()
                .contains("credential-marker")
        );
    }

    // Catches host-platform path parsing, traversal, or Docker path cleaning changing the archive
    // member the decoder authenticates. Container paths are canonical absolute POSIX paths.
    #[test]
    fn archive_basename_is_derived_only_from_a_canonical_container_path() {
        assert_eq!(
            expected_archive_basename("/root/.lnd/tls.cert")
                .expect("the fixed credential path is canonical"),
            "tls.cert"
        );

        for path in [
            "root/.lnd/tls.cert",
            "/root/../tls.cert",
            "/root/./tls.cert",
            "/root//tls.cert",
            "/root/.lnd/",
            "/root/.lnd\\tls.cert",
        ] {
            assert!(
                expected_archive_basename(path).is_err(),
                "noncanonical path must be rejected"
            );
        }
    }

    // Catches accepting a regular entry whose raw member name is not exactly the basename Docker
    // returns for the requested absolute path.
    #[test]
    fn container_archive_decoder_authenticates_the_exact_entry_name_and_stat_size() {
        for path in [
            b"unexpected".as_slice(),
            b"dir/tls.cert".as_slice(),
            b"./tls.cert".as_slice(),
            b"/tls.cert".as_slice(),
        ] {
            let archive = tar_archive_with_raw_member_name(path, b"certificate");
            assert!(
                decode_single_file_archive(&archive, "tls.cert", b"certificate".len(), 1_048_576,)
                    .is_err(),
                "a different raw member path must be rejected"
            );
        }

        let archive = tar_archive(&[("tls.cert", EntryType::Regular, b"certificate")]);
        assert!(
            decode_single_file_archive(&archive, "tls.cert", 10, 1_048_576).is_err(),
            "the tar size must agree with Docker's preflight stat"
        );
    }

    // Catches tar-rs's permissive EOF/first-zero behavior accepting missing terminators,
    // concatenated archives, or nonzero data after a nominal end marker.
    #[test]
    fn container_archive_decoder_requires_strict_zero_terminated_framing() {
        let valid = tar_archive(&[("tls.cert", EntryType::Regular, b"certificate")]);
        assert_eq!(
            valid.len(),
            2_048,
            "the hand-checked fixture has minimal framing"
        );

        let no_end_blocks = valid[..valid.len() - 1_024].to_vec();
        assert!(
            decode_single_file_archive(
                &no_end_blocks,
                "tls.cert",
                b"certificate".len(),
                1_048_576,
            )
            .is_err()
        );

        let one_end_block = valid[..valid.len() - 512].to_vec();
        assert!(
            decode_single_file_archive(
                &one_end_block,
                "tls.cert",
                b"certificate".len(),
                1_048_576,
            )
            .is_err()
        );

        let mut nonzero_trailer = valid.clone();
        nonzero_trailer.extend([1_u8; 512]);
        assert!(
            decode_single_file_archive(
                &nonzero_trailer,
                "tls.cert",
                b"certificate".len(),
                1_048_576,
            )
            .is_err()
        );

        let mut concatenated = valid;
        concatenated.extend(tar_archive(&[("tls.cert", EntryType::Regular, b"second")]));
        assert!(
            decode_single_file_archive(&concatenated, "tls.cert", b"certificate".len(), 1_048_576,)
                .is_err()
        );
    }

    // Catches a decoder that lets the mandatory content-padding boundary borrow bytes from the
    // terminator after either the declared content or its zero padding is truncated.
    #[test]
    fn container_archive_decoder_rejects_truncated_content_and_padding() {
        let valid = tar_archive(&[("tls.cert", EntryType::Regular, b"certificate")]);
        let content_end = 512 + b"certificate".len();
        let padding_end = 1_024;

        let mut truncated_content = valid[..content_end - 1].to_vec();
        truncated_content.extend([0_u8; 1_024]);
        assert!(
            decode_single_file_archive(
                &truncated_content,
                "tls.cert",
                b"certificate".len(),
                1_048_576,
            )
            .is_err()
        );

        let mut truncated_padding = valid[..padding_end - 1].to_vec();
        truncated_padding.extend([0_u8; 1_024]);
        assert!(
            decode_single_file_archive(
                &truncated_padding,
                "tls.cert",
                b"certificate".len(),
                1_048_576,
            )
            .is_err()
        );
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
