use std::{error::Error, io, process::Command, time::Duration};

use nigiri_rs::{Bitcoin, NigiriClient, NigiriConfig};
use serde_json::Value;
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt, core::IntoContainerPort, runners::AsyncRunner,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{Barrier, Mutex},
    time::{Instant, sleep, timeout},
};
use url::Url;
use uuid::Uuid;

const BITCOIND_IMAGE: &str = "ghcr.io/getumbrel/docker-bitcoind";
const BITCOIND_TAG: &str = "v30.0";
const BITCOIND_DIGEST: &str =
    "sha256:f5826a32aed9287cc5ffdec0996f5272634c4b346529cb8627224986ff555101";
const ELECTRS_IMAGE: &str = "ghcr.io/vulpemventures/electrs";
const ELECTRS_TAG: &str = "latest";
const ELECTRS_DIGEST: &str =
    "sha256:999a2218f423c0fb167ee53b282aa7929a9d4abba38ef16f67f407acd00589d4";
const BITCOIND_RPC_PORT: u16 = 18_443;
const ELECTRS_HTTP_PORT: u16 = 30_000;
const ELECTRUM_TCP_PORT: u16 = 50_000;
const RPC_USER: &str = "admin1";
const RPC_PASSWORD: &str = "123";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const BITCOIND_DATA_VOLUME_DESTINATION: &str = "/data/.bitcoin";
const ANONYMOUS_VOLUME_ID_LENGTH: usize = 64;
const DOCKER_ANONYMOUS_VOLUME_LABEL: &str = "com.docker.volume.anonymous";

type TestResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Clone, Debug)]
struct TopologyNames {
    network: String,
    bitcoin: String,
    electrs: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CapturedAnonymousVolume {
    name: String,
}

#[derive(Debug)]
struct CleanupTargets {
    topology: TopologyNames,
    bitcoin_id: String,
    electrs_id: String,
    bitcoind_volume: CapturedAnonymousVolume,
}

struct SmokeInstance {
    electrs: ContainerAsync<GenericImage>,
    bitcoin: ContainerAsync<GenericImage>,
    topology: TopologyNames,
    bitcoin_id: String,
    electrs_id: String,
    bitcoind_volume: CapturedAnonymousVolume,
    rpc_port: u16,
    http_port: u16,
    electrum_port: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObservedBitcoind {
    name: String,
    id: String,
}

struct InitialMiningGate {
    permit: Mutex<()>,
    all_bitcoinds_observed: Barrier,
    expected_bitcoinds: usize,
    observed_bitcoinds: Mutex<Vec<ObservedBitcoind>>,
}

impl InitialMiningGate {
    fn new(expected_bitcoinds: usize) -> Self {
        assert!(
            expected_bitcoinds > 0,
            "initial mining gate needs a topology"
        );
        Self {
            permit: Mutex::new(()),
            all_bitcoinds_observed: Barrier::new(expected_bitcoinds),
            expected_bitcoinds,
            observed_bitcoinds: Mutex::new(Vec::with_capacity(expected_bitcoinds)),
        }
    }

    async fn observe_bitcoind(&self, bitcoin_id: &str, bitcoin_name: &str) -> TestResult<()> {
        let inspected = docker_inspect("container", bitcoin_id)?;
        if inspected.get("Id").and_then(Value::as_str) != Some(bitcoin_id)
            || inspected.pointer("/State/Running").and_then(Value::as_bool) != Some(true)
            || inspected.pointer("/Name").and_then(Value::as_str)
                != Some(&format!("/{bitcoin_name}"))
        {
            return Err(io::Error::other(format!(
                "initial mining observed unexpected Bitcoind state for {bitcoin_name} ({bitcoin_id}): {inspected}",
            ))
            .into());
        }

        let observed = ObservedBitcoind {
            name: bitcoin_name.to_owned(),
            id: bitcoin_id.to_owned(),
        };
        let mut observed_bitcoinds = self.observed_bitcoinds.lock().await;
        if observed_bitcoinds.contains(&observed) {
            return Err(io::Error::other(format!(
                "initial mining observed duplicate Bitcoind {bitcoin_name} ({bitcoin_id})",
            ))
            .into());
        }
        observed_bitcoinds.push(observed);
        Ok(())
    }

    async fn assert_all_bitcoinds_observed(&self) -> TestResult<Vec<ObservedBitcoind>> {
        let observed_bitcoinds = self.observed_bitcoinds.lock().await;
        if observed_bitcoinds.len() != self.expected_bitcoinds {
            return Err(io::Error::other(format!(
                "initial mining expected {} running Bitcoind containers before mining, observed {:?}",
                self.expected_bitcoinds, *observed_bitcoinds,
            ))
            .into());
        }
        Ok(observed_bitcoinds.clone())
    }

    async fn mine_initial_chain(
        &self,
        bitcoin_id: &str,
        bitcoin_name: &str,
        client: &NigiriClient<Bitcoin>,
        mining_address: &str,
    ) -> TestResult<()> {
        self.observe_bitcoind(bitcoin_id, bitcoin_name).await?;
        let barrier_result = timeout(STARTUP_TIMEOUT, self.all_bitcoinds_observed.wait())
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "not every Bitcoind topology reached initial mining before the 60-second deadline",
                )
            })?;
        let observed_bitcoinds = self.assert_all_bitcoinds_observed().await?;
        if barrier_result.is_leader() {
            eprintln!(
                "compatibility gate initial mining evidence: observed {} running Bitcoind containers before initial mining: {observed_bitcoinds:?}",
                self.expected_bitcoinds,
            );
        }

        // Bitcoin Core synchronously updates each wallet transaction while mining.
        // Keeping this one batch serial preserves concurrently isolated topologies
        // without oversubscribing the Docker host's disk and CPU resources.
        let _permit = self.permit.lock().await;
        client.generate_to_address(101, mining_address).await?;
        Ok(())
    }
}

fn mapped_http_url(host: &str, port: u16) -> TestResult<Url> {
    let mut url = Url::parse("http://localhost/")?;
    url.set_host(Some(host))?;
    url.set_port(Some(port))
        .map_err(|()| io::Error::new(io::ErrorKind::InvalidInput, "invalid mapped service port"))?;
    Ok(url)
}

fn rpc_client(node_rpc_url: Url) -> TestResult<NigiriClient<Bitcoin>> {
    Ok(NigiriClient::with_config(NigiriConfig {
        esplora_url: node_rpc_url.clone(),
        node_rpc_url,
        node_rpc_user: RPC_USER.to_owned(),
        node_rpc_password: RPC_PASSWORD.to_owned(),
        ..Default::default()
    })?)
}

fn docker_inspect(resource_kind: &str, target: &str) -> TestResult<Value> {
    let output = Command::new("docker")
        .args([resource_kind, "inspect", target])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "docker {resource_kind} inspect {target} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim(),
        ))
        .into());
    }

    let mut inspected: Vec<Value> = serde_json::from_slice(&output.stdout)?;
    if inspected.len() != 1 {
        return Err(io::Error::other(format!(
            "docker {resource_kind} inspect {target} returned {} objects",
            inspected.len()
        ))
        .into());
    }
    Ok(inspected.pop().expect("one inspected Docker object"))
}

fn docker_resource_is_removed(resource_kind: &str, target: &str) -> TestResult<bool> {
    let output = Command::new("docker")
        .args([resource_kind, "inspect", target])
        .output()?;
    if output.status.success() {
        return Ok(false);
    }

    let diagnostic = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(1) && is_missing_docker_resource_diagnostic(&diagnostic) {
        return Ok(true);
    }

    Err(io::Error::other(format!(
        "docker {resource_kind} inspect {target} failed while checking cleanup: {}",
        diagnostic.trim(),
    ))
    .into())
}

fn is_missing_docker_resource_diagnostic(diagnostic: &str) -> bool {
    let diagnostic = diagnostic.trim().to_ascii_lowercase();
    diagnostic.contains("no such container")
        || diagnostic.contains("no such volume")
        || diagnostic.contains("no such network")
        || (diagnostic.contains("network ") && diagnostic.ends_with(" not found"))
}

fn bounded_docker_inspect_diagnostic(resource_kind: &str, target: &str) -> String {
    const MAX_DIAGNOSTIC_CHARS: usize = 4 * 1024;

    match Command::new("docker")
        .args([resource_kind, "inspect", target])
        .output()
    {
        Ok(output) => {
            let text = format!(
                "status={}\n{}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            let mut bounded = text.chars().take(MAX_DIAGNOSTIC_CHARS).collect::<String>();
            if text.chars().count() > MAX_DIAGNOSTIC_CHARS {
                bounded.push_str("\n[diagnostic truncated]");
            }
            format!("docker {resource_kind} inspect {target}: {bounded}")
        }
        Err(error) => format!(
            "docker {resource_kind} inspect {target} could not run while diagnosing cleanup: {error}",
        ),
    }
}

fn assert_string_array(
    inspected: &Value,
    field: &str,
    expected: &[String],
    resource_name: &str,
) -> TestResult<()> {
    let actual = inspected
        .pointer(field)
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other(format!("{resource_name} has no {field} array")))?
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                io::Error::other(format!("{resource_name} has a non-string {field} value"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if actual.as_slice() != expected {
        return Err(io::Error::other(format!(
            "{resource_name} {field} was {actual:?}, expected {expected:?}",
        ))
        .into());
    }
    Ok(())
}

enum ContainerMountPolicy<'a> {
    NoMounts,
    PinnedBitcoindAnonymousVolume(&'a CapturedAnonymousVolume),
}

fn assert_no_explicit_mount_configuration(
    inspected: &Value,
    resource_name: &str,
) -> TestResult<()> {
    for field in [
        "/HostConfig/Binds",
        "/HostConfig/Mounts",
        "/HostConfig/VolumesFrom",
        "/HostConfig/Tmpfs",
    ] {
        if let Some(value) = inspected.pointer(field)
            && !value.is_null()
            && value.as_array().is_none_or(|values| !values.is_empty())
        {
            return Err(io::Error::other(format!(
                "{resource_name} has unexpected mount configuration in {field}: {value}",
            ))
            .into());
        }
    }
    Ok(())
}

fn assert_no_mounts(inspected: &Value, resource_name: &str) -> TestResult<()> {
    let mounts = inspected
        .get("Mounts")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other(format!("{resource_name} has no Mounts array")))?;
    if !mounts.is_empty() {
        return Err(io::Error::other(format!(
            "{resource_name} has unexpected Docker mounts: {mounts:?}",
        ))
        .into());
    }
    Ok(())
}

fn is_anonymous_volume_id(name: &str) -> bool {
    name.len() == ANONYMOUS_VOLUME_ID_LENGTH && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn capture_pinned_bitcoind_anonymous_volume(
    inspected: &Value,
    resource_name: &str,
) -> TestResult<CapturedAnonymousVolume> {
    assert_no_explicit_mount_configuration(inspected, resource_name)?;

    let declared_volumes = inspected
        .pointer("/Config/Volumes")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            io::Error::other(format!(
                "{resource_name} does not declare its image storage in Config.Volumes",
            ))
        })?;
    if declared_volumes.len() != 1
        || !declared_volumes.contains_key(BITCOIND_DATA_VOLUME_DESTINATION)
    {
        return Err(io::Error::other(format!(
            "{resource_name} declared unexpected image volumes: {declared_volumes:?}",
        ))
        .into());
    }

    let mounts = inspected
        .get("Mounts")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other(format!("{resource_name} has no Mounts array")))?;
    let [mount] = mounts.as_slice() else {
        return Err(io::Error::other(format!(
            "{resource_name} must have exactly one image-declared anonymous volume mount, found {mounts:?}",
        ))
        .into());
    };

    let mount_type = mount.get("Type").and_then(Value::as_str);
    if mount_type != Some("volume") {
        return Err(io::Error::other(format!(
            "{resource_name} mount type was {mount_type:?}, expected image-declared volume",
        ))
        .into());
    }
    let destination = mount.get("Destination").and_then(Value::as_str);
    if destination != Some(BITCOIND_DATA_VOLUME_DESTINATION) {
        return Err(io::Error::other(format!(
            "{resource_name} volume destination was {destination:?}, expected {BITCOIND_DATA_VOLUME_DESTINATION}",
        ))
        .into());
    }
    if mount.get("Driver").and_then(Value::as_str) != Some("local") {
        return Err(io::Error::other(format!(
            "{resource_name} anonymous volume must use Docker's local driver: {mount}",
        ))
        .into());
    }
    if mount.get("RW").and_then(Value::as_bool) != Some(true) {
        return Err(io::Error::other(format!(
            "{resource_name} anonymous volume must be writable: {mount}",
        ))
        .into());
    }
    let name = mount
        .get("Name")
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::other(format!("{resource_name} volume has no name")))?;
    if !is_anonymous_volume_id(name) {
        return Err(io::Error::other(format!(
            "{resource_name} volume name {name:?} is not Docker's anonymous volume identifier",
        ))
        .into());
    }

    Ok(CapturedAnonymousVolume {
        name: name.to_owned(),
    })
}

fn assert_anonymous_volume_metadata(
    inspected: &Value,
    volume: &CapturedAnonymousVolume,
) -> TestResult<()> {
    let name = inspected
        .get("Name")
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::other("anonymous volume inspect result has no Name"))?;
    if name != volume.name {
        return Err(io::Error::other(format!(
            "anonymous volume inspect returned {name:?}, expected {:?}",
            volume.name,
        ))
        .into());
    }
    if inspected.get("Driver").and_then(Value::as_str) != Some("local")
        || inspected.get("Scope").and_then(Value::as_str) != Some("local")
    {
        return Err(io::Error::other(format!(
            "anonymous volume {} must use local Docker storage: {inspected}",
            volume.name,
        ))
        .into());
    }
    match inspected.get("Labels") {
        Some(Value::Object(labels))
            if labels.len() == 1
                && labels
                    .get(DOCKER_ANONYMOUS_VOLUME_LABEL)
                    .and_then(Value::as_str)
                    == Some("") => {}
        _ => {
            return Err(io::Error::other(format!(
                "anonymous volume {} does not have only Docker's anonymous marker: {inspected}",
                volume.name,
            ))
            .into());
        }
    }
    match inspected.get("Options") {
        Some(Value::Null) => {}
        Some(Value::Object(values)) if values.is_empty() => {}
        _ => {
            return Err(io::Error::other(format!(
                "anonymous volume {} has persistent or reused Options: {inspected}",
                volume.name,
            ))
            .into());
        }
    }
    Ok(())
}

fn capture_pinned_bitcoind_volume_from_docker(
    bitcoin_id: &str,
    bitcoin_name: &str,
) -> TestResult<CapturedAnonymousVolume> {
    let inspected = docker_inspect("container", bitcoin_id)?;
    let volume = capture_pinned_bitcoind_anonymous_volume(&inspected, bitcoin_name)?;
    let volume_inspected = docker_inspect("volume", &volume.name)?;
    assert_anonymous_volume_metadata(&volume_inspected, &volume)?;
    Ok(volume)
}

fn assert_mount_policy(
    inspected: &Value,
    resource_name: &str,
    policy: ContainerMountPolicy<'_>,
) -> TestResult<()> {
    match policy {
        ContainerMountPolicy::NoMounts => {
            assert_no_explicit_mount_configuration(inspected, resource_name)?;
            assert_no_mounts(inspected, resource_name)
        }
        ContainerMountPolicy::PinnedBitcoindAnonymousVolume(expected) => {
            let captured = capture_pinned_bitcoind_anonymous_volume(inspected, resource_name)?;
            if &captured != expected {
                return Err(io::Error::other(format!(
                    "{resource_name} anonymous volume changed from {} to {}",
                    expected.name, captured.name,
                ))
                .into());
            }
            Ok(())
        }
    }
}

fn assert_container_runtime(
    inspected: &Value,
    resource_name: &str,
    expected_descriptor: &str,
    expected_network: &str,
    expected_command: &[String],
    expected_ports: &[(u16, u16)],
    mount_policy: ContainerMountPolicy<'_>,
) -> TestResult<()> {
    let image = inspected
        .pointer("/Config/Image")
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::other(format!("{resource_name} has no Config.Image")))?;
    if image != expected_descriptor {
        return Err(io::Error::other(format!(
            "{resource_name} image was {image}, expected {expected_descriptor}",
        ))
        .into());
    }

    assert_string_array(inspected, "/Config/Cmd", expected_command, resource_name)?;

    assert_mount_policy(inspected, resource_name, mount_policy)?;

    if inspected
        .pointer("/Config/Labels")
        .and_then(Value::as_object)
        .is_some_and(|labels| labels.contains_key("com.docker.compose.project"))
    {
        return Err(io::Error::other(format!(
            "{resource_name} unexpectedly has a Docker Compose project label",
        ))
        .into());
    }

    let networks = inspected
        .pointer("/NetworkSettings/Networks")
        .and_then(Value::as_object)
        .ok_or_else(|| io::Error::other(format!("{resource_name} has no network settings")))?;
    if !networks.contains_key(expected_network) {
        return Err(io::Error::other(format!(
            "{resource_name} is not attached to private network {expected_network}",
        ))
        .into());
    }

    if inspected
        .pointer("/HostConfig/PublishAllPorts")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err(io::Error::other(format!(
            "{resource_name} did not ask Docker to choose host ports dynamically",
        ))
        .into());
    }
    if let Some(port_bindings) = inspected.pointer("/HostConfig/PortBindings")
        && !port_bindings.is_null()
        && port_bindings
            .as_object()
            .is_none_or(|bindings| !bindings.is_empty())
    {
        return Err(io::Error::other(format!(
            "{resource_name} has fixed host-port bindings: {port_bindings}",
        ))
        .into());
    }

    let port_mappings = inspected
        .pointer("/NetworkSettings/Ports")
        .and_then(Value::as_object)
        .ok_or_else(|| io::Error::other(format!("{resource_name} has no port mappings")))?;
    for (container_port, expected_host_port) in expected_ports {
        let port_name = format!("{container_port}/tcp");
        let mappings = port_mappings
            .get(&port_name)
            .and_then(Value::as_array)
            .ok_or_else(|| {
                io::Error::other(format!("{resource_name} has no mapping for {port_name}"))
            })?;
        let has_expected_port = mappings.iter().any(|mapping| {
            mapping
                .get("HostPort")
                .and_then(Value::as_str)
                .and_then(|port| port.parse::<u16>().ok())
                == Some(*expected_host_port)
        });
        if !has_expected_port {
            return Err(io::Error::other(format!(
                "{resource_name} did not map {port_name} to host port {expected_host_port}",
            ))
            .into());
        }
    }

    Ok(())
}

fn expected_bitcoind_command() -> Vec<String> {
    [
        "-regtest=1",
        "-server=1",
        "-txindex=1",
        "-rpcbind=0.0.0.0:18443",
        "-rpcallowip=0.0.0.0/0",
        "-rpcuser=admin1",
        "-rpcpassword=123",
        "-fallbackfee=0.00001",
        "-printtoconsole=1",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn expected_electrs_command(bitcoin_name: &str) -> Vec<String> {
    [
        "-vvvv".to_owned(),
        "--network".to_owned(),
        "regtest".to_owned(),
        "--daemon-dir".to_owned(),
        "/tmp/bitcoin".to_owned(),
        "--db-dir".to_owned(),
        "/tmp/electrs".to_owned(),
        "--daemon-rpc-addr".to_owned(),
        format!("{bitcoin_name}:18443"),
        "--cookie".to_owned(),
        "admin1:123".to_owned(),
        "--http-addr".to_owned(),
        "0.0.0.0:30000".to_owned(),
        "--electrum-rpc-addr".to_owned(),
        "0.0.0.0:50000".to_owned(),
        "--cors".to_owned(),
        "*".to_owned(),
        "--jsonrpc-import".to_owned(),
    ]
    .to_vec()
}

impl SmokeInstance {
    fn assert_runtime_topology(&self) -> TestResult<()> {
        let bitcoin = docker_inspect("container", &self.bitcoin_id)?;
        assert_container_runtime(
            &bitcoin,
            &self.topology.bitcoin,
            &format!("{BITCOIND_IMAGE}:{BITCOIND_TAG}@{BITCOIND_DIGEST}"),
            &self.topology.network,
            &expected_bitcoind_command(),
            &[(BITCOIND_RPC_PORT, self.rpc_port)],
            ContainerMountPolicy::PinnedBitcoindAnonymousVolume(&self.bitcoind_volume),
        )?;
        let volume = docker_inspect("volume", &self.bitcoind_volume.name)?;
        assert_anonymous_volume_metadata(&volume, &self.bitcoind_volume)?;

        let electrs = docker_inspect("container", &self.electrs_id)?;
        assert_container_runtime(
            &electrs,
            &self.topology.electrs,
            &format!("{ELECTRS_IMAGE}:{ELECTRS_TAG}@{ELECTRS_DIGEST}"),
            &self.topology.network,
            &expected_electrs_command(&self.topology.bitcoin),
            &[
                (ELECTRS_HTTP_PORT, self.http_port),
                (ELECTRUM_TCP_PORT, self.electrum_port),
            ],
            ContainerMountPolicy::NoMounts,
        )?;
        assert_string_array(
            &electrs,
            "/Config/Entrypoint",
            &["/build/electrs".to_owned()],
            &self.topology.electrs,
        )?;

        eprintln!(
            "compatibility gate verified topology: network={} bitcoin={} ({}) volume={} electrs={} ({}) ports rpc={} http={} electrum={}",
            self.topology.network,
            self.topology.bitcoin,
            self.bitcoin_id,
            self.bitcoind_volume.name,
            self.topology.electrs,
            self.electrs_id,
            self.rpc_port,
            self.http_port,
            self.electrum_port,
        );
        Ok(())
    }

    async fn cleanup(self) -> TestResult<CleanupTargets> {
        let SmokeInstance {
            electrs,
            bitcoin,
            topology,
            bitcoin_id,
            electrs_id,
            bitcoind_volume,
            ..
        } = self;
        let cleanup = CleanupTargets {
            topology,
            bitcoin_id,
            electrs_id,
            bitcoind_volume,
        };

        let electrs_result = electrs.rm().await;
        let bitcoin_result = bitcoin.rm().await;
        electrs_result?;
        bitcoin_result?;

        eprintln!(
            "compatibility gate requested cleanup: network={} bitcoin={} ({}) volume={} electrs={} ({})",
            cleanup.topology.network,
            cleanup.topology.bitcoin,
            cleanup.bitcoin_id,
            cleanup.bitcoind_volume.name,
            cleanup.topology.electrs,
            cleanup.electrs_id,
        );
        Ok(cleanup)
    }
}

async fn assert_resources_are_removed(cleanup: &CleanupTargets) -> TestResult<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        let bitcoin_id_removed = docker_resource_is_removed("container", &cleanup.bitcoin_id)?;
        let bitcoin_name_removed =
            docker_resource_is_removed("container", &cleanup.topology.bitcoin)?;
        let electrs_id_removed = docker_resource_is_removed("container", &cleanup.electrs_id)?;
        let electrs_name_removed =
            docker_resource_is_removed("container", &cleanup.topology.electrs)?;
        let bitcoin_removed = bitcoin_id_removed && bitcoin_name_removed;
        let electrs_removed = electrs_id_removed && electrs_name_removed;
        let network_removed = docker_resource_is_removed("network", &cleanup.topology.network)?;
        let volume_removed = docker_resource_is_removed("volume", &cleanup.bitcoind_volume.name)?;
        if bitcoin_removed && electrs_removed && network_removed && volume_removed {
            eprintln!(
                "compatibility gate confirmed cleanup: network={} bitcoin={} volume={} electrs={}",
                cleanup.topology.network,
                cleanup.topology.bitcoin,
                cleanup.bitcoind_volume.name,
                cleanup.topology.electrs,
            );
            return Ok(());
        }
        if Instant::now() >= deadline {
            let mut diagnostics = Vec::new();
            if !bitcoin_id_removed {
                diagnostics.push(bounded_docker_inspect_diagnostic(
                    "container",
                    &cleanup.bitcoin_id,
                ));
            }
            if !bitcoin_name_removed {
                diagnostics.push(bounded_docker_inspect_diagnostic(
                    "container",
                    &cleanup.topology.bitcoin,
                ));
            }
            if !electrs_id_removed {
                diagnostics.push(bounded_docker_inspect_diagnostic(
                    "container",
                    &cleanup.electrs_id,
                ));
            }
            if !electrs_name_removed {
                diagnostics.push(bounded_docker_inspect_diagnostic(
                    "container",
                    &cleanup.topology.electrs,
                ));
            }
            if !network_removed {
                diagnostics.push(bounded_docker_inspect_diagnostic(
                    "network",
                    &cleanup.topology.network,
                ));
            }
            if !volume_removed {
                diagnostics.push(bounded_docker_inspect_diagnostic(
                    "volume",
                    &cleanup.bitcoind_volume.name,
                ));
            }
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "Docker resources remained after cleanup: network={} bitcoin={} volume={} electrs={}\n{}",
                    cleanup.topology.network,
                    cleanup.topology.bitcoin,
                    cleanup.bitcoind_volume.name,
                    cleanup.topology.electrs,
                    diagnostics.join("\n"),
                ),
            )
            .into());
        }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_bitcoind(client: &NigiriClient<Bitcoin>) -> TestResult<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if client
            .rpc::<Value, _>("getblockchaininfo", ())
            .await
            .is_ok()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "bitcoind did not accept RPC before the 60-second deadline",
            )
            .into());
        }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_esplora_height(http_url: &Url) -> TestResult<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let endpoint = http_url.join("blocks/tip/height")?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Ok(response) = client.get(endpoint.clone()).send().await
            && response.status().is_success()
            && response.text().await?.trim() == "101"
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Esplora did not report height 101 before the 60-second deadline",
            )
            .into());
        }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn assert_electrum_height(host: &str, port: u16) -> TestResult<()> {
    let mut stream = timeout(STARTUP_TIMEOUT, async {
        loop {
            match TcpStream::connect((host, port)).await {
                Ok(stream) => return stream,
                Err(_) => sleep(Duration::from_millis(200)).await,
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Electrum TCP did not become ready"))?;

    stream
        .write_all(b"{\"id\":1,\"method\":\"blockchain.headers.subscribe\",\"params\":[]}\n")
        .await?;
    let mut response = String::new();
    let mut reader = BufReader::new(stream).take(64 * 1024);
    timeout(Duration::from_secs(5), reader.read_line(&mut response))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Electrum response timed out"))??;
    let response: Value = serde_json::from_str(&response)?;
    if response.pointer("/result/height").and_then(Value::as_u64) != Some(101) {
        return Err(io::Error::other(format!(
            "Electrum returned an unexpected subscription response: {response}"
        ))
        .into());
    }
    Ok(())
}

async fn delayed_node_rpc_server(
    delay: Duration,
) -> TestResult<(Url, tokio::task::JoinHandle<TestResult<()>>)> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let address = listener.local_addr()?;
    let node_rpc_url = Url::parse(&format!("http://{address}/"))?;
    let task: tokio::task::JoinHandle<TestResult<()>> = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut request = [0_u8; 16 * 1024];
        let received = stream.read(&mut request).await?;
        if received == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "delayed RPC client closed before sending a request",
            )
            .into());
        }
        sleep(delay).await;

        let body = r#"{"result":{},"error":null,"id":"nigiri-rs"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len(),
        );
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await?;
        Ok(())
    });
    Ok((node_rpc_url, task))
}

async fn start_smoke_instance(
    id: &str,
    initial_mining_gate: &InitialMiningGate,
) -> TestResult<SmokeInstance> {
    let suffix = Uuid::new_v4().simple().to_string();
    let network_name = format!("nigiri-test-{id}-{suffix}");
    let bitcoin_name = format!("nigiri-bitcoind-{id}-{suffix}");
    let electrs_name = format!("nigiri-electrs-{id}-{suffix}");

    let bitcoind_tag = format!("{BITCOIND_TAG}@{BITCOIND_DIGEST}");
    let bitcoin = GenericImage::new(BITCOIND_IMAGE, bitcoind_tag.as_str())
        .with_exposed_port(BITCOIND_RPC_PORT.tcp())
        .with_network(network_name.clone())
        .with_container_name(bitcoin_name.clone())
        .with_cmd([
            "-regtest=1",
            "-server=1",
            "-txindex=1",
            "-rpcbind=0.0.0.0:18443",
            "-rpcallowip=0.0.0.0/0",
            "-rpcuser=admin1",
            "-rpcpassword=123",
            "-fallbackfee=0.00001",
            "-printtoconsole=1",
        ])
        .start()
        .await?;
    let bitcoin_id = bitcoin.id().to_owned();
    let bitcoind_volume = capture_pinned_bitcoind_volume_from_docker(&bitcoin_id, &bitcoin_name)?;

    let bitcoin_host = bitcoin.get_host().await?.to_string();
    let rpc_port = bitcoin.get_host_port_ipv4(BITCOIND_RPC_PORT.tcp()).await?;
    let mut root_rpc_url = mapped_http_url(&bitcoin_host, rpc_port)?;
    let root_client = rpc_client(root_rpc_url.clone())?;
    wait_for_bitcoind(&root_client).await?;

    let wallet_name = format!("compat-{suffix}");
    root_client
        .rpc::<Value, _>("createwallet", (wallet_name.clone(),))
        .await?;
    root_rpc_url
        .path_segments_mut()
        .map_err(|()| io::Error::new(io::ErrorKind::InvalidInput, "RPC URL is not hierarchical"))?
        .push("wallet")
        .push(&wallet_name);
    let wallet_client = rpc_client(root_rpc_url.clone())?;
    let mining_address = wallet_client.new_address().await?.to_string();
    initial_mining_gate
        .mine_initial_chain(&bitcoin_id, &bitcoin_name, &wallet_client, &mining_address)
        .await?;

    let electrs_tag = format!("{ELECTRS_TAG}@{ELECTRS_DIGEST}");
    let electrs = GenericImage::new(ELECTRS_IMAGE, electrs_tag.as_str())
        .with_entrypoint("/build/electrs")
        .with_exposed_port(ELECTRS_HTTP_PORT.tcp())
        .with_exposed_port(ELECTRUM_TCP_PORT.tcp())
        .with_network(network_name.clone())
        .with_container_name(electrs_name.clone())
        .with_cmd([
            "-vvvv".to_owned(),
            "--network".to_owned(),
            "regtest".to_owned(),
            "--daemon-dir".to_owned(),
            "/tmp/bitcoin".to_owned(),
            "--db-dir".to_owned(),
            "/tmp/electrs".to_owned(),
            "--daemon-rpc-addr".to_owned(),
            format!("{bitcoin_name}:18443"),
            "--cookie".to_owned(),
            "admin1:123".to_owned(),
            "--http-addr".to_owned(),
            "0.0.0.0:30000".to_owned(),
            "--electrum-rpc-addr".to_owned(),
            "0.0.0.0:50000".to_owned(),
            "--cors".to_owned(),
            "*".to_owned(),
            "--jsonrpc-import".to_owned(),
        ])
        .start()
        .await?;

    let electrs_host = electrs.get_host().await?.to_string();
    let http_port = electrs.get_host_port_ipv4(ELECTRS_HTTP_PORT.tcp()).await?;
    let electrum_port = electrs.get_host_port_ipv4(ELECTRUM_TCP_PORT.tcp()).await?;
    wait_for_esplora_height(&mapped_http_url(&electrs_host, http_port)?).await?;
    assert_electrum_height(&electrs_host, electrum_port).await?;

    Ok(SmokeInstance {
        topology: TopologyNames {
            network: network_name,
            bitcoin: bitcoin_name,
            electrs: electrs_name,
        },
        bitcoin_id,
        electrs_id: electrs.id().to_owned(),
        bitcoind_volume,
        rpc_port,
        http_port,
        electrum_port,
        electrs,
        bitcoin,
    })
}

#[test]
fn pinned_bitcoind_storage_policy_captures_only_the_image_declared_anonymous_volume() {
    const ANONYMOUS_VOLUME: &str =
        "a425523e00ca1291bf67e5a8111df5695e10e08f88dc2a7332a4577a1226e08b";

    let container = serde_json::json!({
        "Config": {
            "Volumes": {
                "/data/.bitcoin": {}
            }
        },
        "HostConfig": {
            "Binds": [],
            "Mounts": [],
            "VolumesFrom": []
        },
        "Mounts": [{
            "Type": "volume",
            "Name": ANONYMOUS_VOLUME,
            "Driver": "local",
            "Destination": "/data/.bitcoin",
            "RW": true
        }]
    });
    let volume = serde_json::json!({
        "Name": ANONYMOUS_VOLUME,
        "Driver": "local",
        "Scope": "local",
        "Labels": {"com.docker.volume.anonymous": ""},
        "Options": null
    });

    let captured =
        capture_pinned_bitcoind_anonymous_volume(&container, "nigiri-bitcoind-test").unwrap();
    assert_eq!(captured.name, ANONYMOUS_VOLUME);
    assert_anonymous_volume_metadata(&volume, &captured).unwrap();
}

#[test]
fn pinned_bitcoind_storage_policy_rejects_named_shared_host_and_unexpected_mounts() {
    const ANONYMOUS_VOLUME: &str =
        "a425523e00ca1291bf67e5a8111df5695e10e08f88dc2a7332a4577a1226e08b";

    let cases = [
        (
            "named volume",
            serde_json::json!({
                "Config": {"Volumes": {"/data/.bitcoin": {}}},
                "HostConfig": {"Binds": [], "Mounts": [], "VolumesFrom": []},
                "Mounts": [{
                    "Type": "volume",
                    "Name": "persistent-fixture-data",
                    "Driver": "local",
                    "Destination": "/data/.bitcoin",
                    "RW": true
                }]
            }),
        ),
        (
            "shared volume",
            serde_json::json!({
                "Config": {"Volumes": {"/data/.bitcoin": {}}},
                "HostConfig": {"Binds": [], "Mounts": [], "VolumesFrom": ["another-container"]},
                "Mounts": [{
                    "Type": "volume",
                    "Name": ANONYMOUS_VOLUME,
                    "Driver": "local",
                    "Destination": "/data/.bitcoin",
                    "RW": true
                }]
            }),
        ),
        (
            "host bind mount",
            serde_json::json!({
                "Config": {"Volumes": {"/data/.bitcoin": {}}},
                "HostConfig": {"Binds": ["/tmp/bitcoin:/data/.bitcoin"], "Mounts": [], "VolumesFrom": []},
                "Mounts": [{
                    "Type": "bind",
                    "Source": "/tmp/bitcoin",
                    "Destination": "/data/.bitcoin",
                    "RW": true
                }]
            }),
        ),
        (
            "unexpected destination",
            serde_json::json!({
                "Config": {"Volumes": {"/data/.bitcoin": {}}},
                "HostConfig": {"Binds": [], "Mounts": [], "VolumesFrom": []},
                "Mounts": [{
                    "Type": "volume",
                    "Name": ANONYMOUS_VOLUME,
                    "Driver": "local",
                    "Destination": "/var/lib/bitcoin",
                    "RW": true
                }]
            }),
        ),
    ];

    for (case, inspected) in cases {
        assert!(
            capture_pinned_bitcoind_anonymous_volume(&inspected, "nigiri-bitcoind-test").is_err(),
            "storage policy accepted {case}",
        );
    }
}

#[test]
fn anonymous_volume_metadata_rejects_persistent_or_reused_storage() {
    const ANONYMOUS_VOLUME: &str =
        "a425523e00ca1291bf67e5a8111df5695e10e08f88dc2a7332a4577a1226e08b";
    let captured = CapturedAnonymousVolume {
        name: ANONYMOUS_VOLUME.to_owned(),
    };
    let cases = [
        (
            "missing Docker anonymous marker",
            serde_json::json!({
                "Name": ANONYMOUS_VOLUME,
                "Driver": "local",
                "Scope": "local",
                "Labels": null,
                "Options": null
            }),
        ),
        (
            "configured volume options",
            serde_json::json!({
                "Name": ANONYMOUS_VOLUME,
                "Driver": "local",
                "Scope": "local",
                "Labels": null,
                "Options": {"device": "/host/state"}
            }),
        ),
        (
            "labelled prior volume",
            serde_json::json!({
                "Name": ANONYMOUS_VOLUME,
                "Driver": "local",
                "Scope": "local",
                "Labels": {"owner": "prior-run"},
                "Options": null
            }),
        ),
        (
            "additional volume label",
            serde_json::json!({
                "Name": ANONYMOUS_VOLUME,
                "Driver": "local",
                "Scope": "local",
                "Labels": {
                    "com.docker.volume.anonymous": "",
                    "owner": "prior-run"
                },
                "Options": null
            }),
        ),
        (
            "nonlocal volume driver",
            serde_json::json!({
                "Name": ANONYMOUS_VOLUME,
                "Driver": "nfs",
                "Scope": "global",
                "Labels": null,
                "Options": null
            }),
        ),
    ];

    for (case, inspected) in cases {
        assert!(
            assert_anonymous_volume_metadata(&inspected, &captured).is_err(),
            "storage policy accepted {case}",
        );
    }
}

#[test]
fn docker_cleanup_recognizes_the_observed_missing_resource_diagnostics() {
    for diagnostic in [
        "Error response from daemon: No such container: exact-container-id",
        "Error response from daemon: network exact-network-name not found",
        "Error response from daemon: get exact-volume-id: no such volume",
    ] {
        assert!(
            is_missing_docker_resource_diagnostic(diagnostic),
            "cleanup did not recognize a missing resource: {diagnostic}",
        );
    }
    assert!(!is_missing_docker_resource_diagnostic(
        "Error response from daemon: permission denied while trying to connect to the Docker daemon socket",
    ));
}

#[tokio::test]
async fn fixture_rpc_client_allows_a_node_operation_past_the_legacy_five_second_override() {
    let (node_rpc_url, server) = delayed_node_rpc_server(Duration::from_secs(6))
        .await
        .unwrap();
    let client = rpc_client(node_rpc_url).unwrap();

    let response = client
        .rpc::<Value, _>("getblockchaininfo", ())
        .await
        .unwrap();

    assert_eq!(response, serde_json::json!({}));
    server.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and pulls pinned Bitcoin images"]
async fn pinned_images_work_without_nigiri_volumes_or_compose() {
    let initial_mining_gate = InitialMiningGate::new(1);
    let instance = start_smoke_instance("single", &initial_mining_gate)
        .await
        .unwrap();
    assert_ne!(instance.rpc_port, BITCOIND_RPC_PORT);
    assert_ne!(instance.http_port, ELECTRS_HTTP_PORT);
    assert_ne!(instance.electrum_port, ELECTRUM_TCP_PORT);
    instance.assert_runtime_topology().unwrap();
    let cleanup = instance.cleanup().await.unwrap();
    assert_resources_are_removed(&cleanup).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker and pulls pinned Bitcoin images"]
async fn two_pinned_topologies_coexist() {
    let initial_mining_gate = InitialMiningGate::new(2);
    let (left, right) = tokio::join!(
        start_smoke_instance("left", &initial_mining_gate),
        start_smoke_instance("right", &initial_mining_gate),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    initial_mining_gate
        .assert_all_bitcoinds_observed()
        .await
        .unwrap();
    assert_ne!(left.rpc_port, right.rpc_port);
    assert_ne!(left.http_port, right.http_port);
    assert_ne!(left.bitcoin_id, right.bitcoin_id);
    assert_ne!(left.electrs_id, right.electrs_id);
    left.assert_runtime_topology().unwrap();
    right.assert_runtime_topology().unwrap();
    let (left_cleanup, right_cleanup) = tokio::join!(left.cleanup(), right.cleanup());
    assert_resources_are_removed(&left_cleanup.unwrap())
        .await
        .unwrap();
    assert_resources_are_removed(&right_cleanup.unwrap())
        .await
        .unwrap();
}
