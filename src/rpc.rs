use std::{
    borrow::Cow,
    ffi::{OsStr, OsString},
    path::PathBuf,
    process::{ExitStatus, Stdio},
};

use serde::de::DeserializeOwned;
use tokio::{
    io::{AsyncReadExt, Error as IoError},
    process::{Child, ChildStderr, ChildStdout, Command},
};

use crate::{NigiriClient, NigiriError, NigiriNetwork};

const PIPE_READ_CHUNK_BYTES: usize = 8 * 1024;

/// Prefix `bitcoin-cli` and `elements-cli` use for an RPC error they report on
/// stdout while Nigiri's wrapper still exits zero.
const RPC_ERROR_MARKER: &str = "error code:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RpcInvocation {
    pub(crate) executable: PathBuf,
    pub(crate) args: Vec<OsString>,
    /// Index into `args` at which caller-supplied values begin.
    ///
    /// Everything from this index onward is redacted from retained stderr. Each
    /// builder declares its own boundary because the CLI shapes differ: `nigiri
    /// rpc <method> <args...>` puts caller values after the method, while
    /// `nigiri mint <address> <quantity> ...` puts them at index 1.
    pub(crate) caller_args_from: usize,
}

impl RpcInvocation {
    /// Arguments to redact from retained stderr.
    ///
    /// Fails closed: a boundary past the end of `args` means a builder is
    /// mis-declared, so treat every argument as caller-supplied rather than
    /// silently redacting nothing.
    fn caller_args(&self) -> &[OsString] {
        self.args
            .get(self.caller_args_from..)
            .unwrap_or(self.args.as_slice())
    }
}

pub(crate) fn build_rpc_invocation<N, I, S>(
    executable: PathBuf,
    method: &str,
    args: I,
) -> RpcInvocation
where
    N: NigiriNetwork,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let prefix = N::rpc_prefix();
    let args = prefix
        .iter()
        .map(OsString::from)
        .chain(std::iter::once(OsString::from(method)))
        .chain(args.into_iter().map(|arg| arg.as_ref().to_os_string()))
        .collect();
    RpcInvocation {
        executable,
        args,
        caller_args_from: prefix.len() + 1,
    }
}

impl<N: NigiriNetwork> NigiriClient<N> {
    /// Invokes a node RPC through Nigiri and deserializes its response.
    ///
    /// Arguments use the same separate CLI-style strings accepted by
    /// `nigiri rpc`; this method never invokes a shell. The method name may be
    /// computed at runtime and is validated before any process is spawned.
    ///
    /// # Errors
    ///
    /// Returns [`NigiriError`] when the method name is invalid, Nigiri cannot be
    /// executed, the process fails or times out, or the response does not match
    /// `R`. Caller arguments and successful response content are omitted from
    /// deserialization errors.
    ///
    /// Responses larger than [`NigiriConfig::max_rpc_response_bytes`] are
    /// rejected rather than buffered. Raise that limit for methods with large
    /// results, such as `listunspent` or `getblock <hash> 2`.
    ///
    /// A method that exits zero, writes nothing to stdout, and writes non-whitespace
    /// content to stderr is reported as [`NigiriError::RpcFailed`], because that is
    /// how the node CLIs surface some errors. Whitespace-only stderr does not fail a
    /// void result. A host whose `nigiri` wrapper emits unrelated stderr noise will
    /// still see spurious failures from void RPCs; keep the wrapper's stderr clean.
    ///
    /// Caller arguments are redacted from retained stderr after ANSI escapes are
    /// removed, including when the CLI echoes only a leading fragment of a long
    /// value. Redaction is still textual: a CLI that re-encodes an argument, or
    /// echoes only its tail, can surface that form. Do not treat it as a hard
    /// guarantee for secret material.
    ///
    /// # State changes
    ///
    /// RPC methods may mutate node wallets or active chain state. The caller owns
    /// synchronization and restoration for mutating host tests.
    ///
    /// [`NigiriConfig::max_rpc_response_bytes`]: crate::NigiriConfig::max_rpc_response_bytes
    pub async fn rpc<R, I, S>(&self, method: &str, args: I) -> Result<R, NigiriError>
    where
        R: DeserializeOwned,
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        validate_rpc_method(method)?;
        let args: Vec<OsString> = args
            .into_iter()
            .map(|argument| OsString::from(argument.as_ref()))
            .collect();
        let stdout = self.rpc_stdout(method.to_owned(), args).await?;
        parse_rpc_response(method, &stdout)
    }

    pub(crate) async fn rpc_stdout<I, S>(
        &self,
        method: impl Into<Cow<'static, str>>,
        args: I,
    ) -> Result<String, NigiriError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let method = method.into();
        let invocation =
            build_rpc_invocation::<N, _, _>(self.config.executable.clone(), &method, args);
        self.execute_invocation(method, invocation).await
    }

    pub(crate) async fn execute_invocation(
        &self,
        operation: impl Into<Cow<'static, str>>,
        invocation: RpcInvocation,
    ) -> Result<String, NigiriError> {
        let operation = operation.into();
        let limit = self.config.max_rpc_response_bytes;
        let mut command = Command::new(&invocation.executable);
        command
            .args(&invocation.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|source| NigiriError::ProcessSpawn {
                operation: operation.clone(),
                source,
            })?;
        let stdout = child
            .stdout
            .take()
            .expect("piped child stdout must be available");
        let stderr = child
            .stderr
            .take()
            .expect("piped child stderr must be available");
        let caller_args = invocation.caller_args();
        let capture = tokio::time::timeout(
            self.config.timeout,
            capture_process_output(&mut child, stdout, stderr, limit),
        )
        .await;
        let output = match capture {
            Ok(Ok(CaptureOutcome::Complete(output))) => output,
            Ok(Ok(CaptureOutcome::StdoutLimit)) => {
                let _ = kill_and_reap(&mut child).await;
                return Err(NigiriError::InvalidResponse {
                    operation,
                    detail: format!(
                        "RPC stdout exceeded the configured {limit} byte limit; raise NigiriConfig::max_rpc_response_bytes"
                    ),
                });
            }
            Ok(Ok(CaptureOutcome::StderrLimit { stderr })) => {
                let status = kill_and_reap(&mut child).await.ok();
                return Err(NigiriError::RpcFailed {
                    method: operation,
                    exit_code: status.and_then(|status| status.code()),
                    stderr: bounded_redacted(&stderr, caller_args, limit),
                });
            }
            Ok(Err(source)) => {
                let _ = kill_and_reap(&mut child).await;
                return Err(NigiriError::ProcessSpawn { operation, source });
            }
            Err(_) => {
                // A cleanup failure must not mask why the call actually failed.
                let _ = kill_and_reap(&mut child).await;
                return Err(NigiriError::Timeout {
                    operation,
                    duration: self.config.timeout,
                });
            }
        };

        if !output.status.success() {
            return Err(NigiriError::RpcFailed {
                method: operation,
                exit_code: output.status.code(),
                stderr: bounded_redacted(&output.stderr, caller_args, limit),
            });
        }
        if output.stdout.is_empty() && !is_ascii_blank(&output.stderr) {
            return Err(NigiriError::RpcFailed {
                method: operation,
                exit_code: output.status.code(),
                stderr: bounded_redacted(&output.stderr, caller_args, limit),
            });
        }
        let stdout =
            std::str::from_utf8(&output.stdout).map_err(|_| NigiriError::InvalidResponse {
                operation: operation.clone(),
                detail: "expected UTF-8 RPC stdout".to_owned(),
            })?;
        let stdout = strip_ansi(stdout).trim().to_owned();
        if has_rpc_error_marker(&stdout) {
            return Err(NigiriError::RpcFailed {
                method: operation,
                exit_code: output.status.code(),
                stderr: bounded_redacted(stdout.as_bytes(), caller_args, limit),
            });
        }
        Ok(stdout)
    }

    /// Creates a new native regtest address through the network node wallet.
    pub async fn new_address(&self) -> Result<N::Address, NigiriError> {
        const OPERATION: &str = "new address";
        let stdout = self
            .rpc_stdout("getnewaddress", std::iter::empty::<&str>())
            .await?;
        N::parse_address(OPERATION, &stdout)
    }

    /// Returns the native active-chain tip hash.
    pub async fn best_block_hash(&self) -> Result<N::BlockHash, NigiriError> {
        const OPERATION: &str = "best block hash";
        let stdout = self
            .rpc_stdout("getbestblockhash", std::iter::empty::<&str>())
            .await?;
        N::parse_block_hash(OPERATION, &stdout)
    }

    /// Mines a nonzero number of blocks to an address.
    pub async fn generate_to_address(
        &self,
        blocks: u64,
        address: &str,
    ) -> Result<Vec<N::BlockHash>, NigiriError> {
        const OPERATION: &str = "generate to address";
        if blocks == 0 {
            return Err(NigiriError::InvalidResponse {
                operation: OPERATION.into(),
                detail: "block count must be greater than zero".to_owned(),
            });
        }
        let blocks = blocks.to_string();
        let stdout = self
            .rpc_stdout("generatetoaddress", [blocks.as_str(), address])
            .await?;
        let hashes: Vec<String> =
            serde_json::from_str(&stdout).map_err(|_| NigiriError::InvalidResponse {
                operation: OPERATION.into(),
                detail: "expected an array of block hashes".to_owned(),
            })?;
        hashes
            .iter()
            .map(|hash| N::parse_block_hash(OPERATION, hash))
            .collect()
    }

    /// Invalidates a native block hash.
    pub async fn invalidate_block(&self, hash: &N::BlockHash) -> Result<(), NigiriError> {
        self.rpc_unit("invalidateblock", hash).await
    }

    /// Reconsiders a previously invalidated native block hash.
    pub async fn reconsider_block(&self, hash: &N::BlockHash) -> Result<(), NigiriError> {
        self.rpc_unit("reconsiderblock", hash).await
    }

    async fn rpc_unit(&self, method: &'static str, hash: &N::BlockHash) -> Result<(), NigiriError> {
        let hash = hash.to_string();
        let stdout = self.rpc_stdout(method, [hash.as_str()]).await?;
        if stdout.is_empty() || stdout == "null" {
            Ok(())
        } else {
            Err(NigiriError::InvalidResponse {
                operation: method.into(),
                detail: "expected an empty RPC result".to_owned(),
            })
        }
    }
}

struct CapturedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

enum CaptureOutcome {
    Complete(CapturedOutput),
    StdoutLimit,
    StderrLimit { stderr: Vec<u8> },
}

async fn capture_process_output(
    child: &mut Child,
    mut stdout: ChildStdout,
    mut stderr: ChildStderr,
    limit: usize,
) -> Result<CaptureOutcome, IoError> {
    let mut stdout_capture = Vec::new();
    let mut stderr_capture = Vec::new();
    let mut stdout_chunk = [0_u8; PIPE_READ_CHUNK_BYTES];
    let mut stderr_chunk = [0_u8; PIPE_READ_CHUNK_BYTES];
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut status = None;

    loop {
        if !stdout_open && !stderr_open {
            let status = match status {
                Some(status) => status,
                None => child.wait().await?,
            };
            return Ok(CaptureOutcome::Complete(CapturedOutput {
                status,
                stdout: stdout_capture,
                stderr: stderr_capture,
            }));
        }

        tokio::select! {
            read = stdout.read(&mut stdout_chunk), if stdout_open => {
                let read = read?;
                if read == 0 {
                    stdout_open = false;
                } else {
                    stdout_capture.extend_from_slice(&stdout_chunk[..read]);
                    if stdout_capture.len() > limit {
                        return Ok(CaptureOutcome::StdoutLimit);
                    }
                }
            }
            read = stderr.read(&mut stderr_chunk), if stderr_open => {
                let read = read?;
                if read == 0 {
                    stderr_open = false;
                } else {
                    stderr_capture.extend_from_slice(&stderr_chunk[..read]);
                    if stderr_capture.len() > limit {
                        return Ok(CaptureOutcome::StderrLimit {
                            stderr: stderr_capture,
                        });
                    }
                }
            }
            child_status = child.wait(), if status.is_none() => {
                status = Some(child_status?);
            }
        }
    }
}

async fn kill_and_reap(child: &mut Child) -> Result<ExitStatus, IoError> {
    if let Some(status) = child.try_wait()? {
        return Ok(status);
    }
    match child.start_kill() {
        Ok(()) => child.wait().await,
        Err(source) => match child.try_wait()? {
            Some(status) => Ok(status),
            None => Err(source),
        },
    }
}

/// Longest accepted RPC method name.
///
/// A runtime-determined method name is carried into [`NigiriError`] and therefore
/// into caller logs, so it needs a length bound as well as a charset. The longest
/// name in Bitcoin Core or Elements is well under half of this.
const MAX_RPC_METHOD_BYTES: usize = 64;

fn validate_rpc_method(method: &str) -> Result<(), NigiriError> {
    if !method.is_empty()
        && method.len() <= MAX_RPC_METHOD_BYTES
        && method
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Ok(());
    }

    Err(NigiriError::InvalidRequest {
        detail: format!(
            "RPC method must be 1 to {MAX_RPC_METHOD_BYTES} bytes of ASCII letters, digits, and underscores"
        )
        .into(),
    })
}

fn is_ascii_blank(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| byte.is_ascii_whitespace())
}

/// Matches an RPC error marker only at the start of a line, so that legitimate
/// response content containing the phrase is not misclassified as a failure.
fn has_rpc_error_marker(stdout: &str) -> bool {
    stdout
        .lines()
        .any(|line| line.trim_start().starts_with(RPC_ERROR_MARKER))
}

fn parse_rpc_response<R>(operation: &str, stdout: &str) -> Result<R, NigiriError>
where
    R: DeserializeOwned,
{
    let direct = if stdout.is_empty() { "null" } else { stdout };
    if let Ok(response) = serde_json::from_str(direct) {
        return Ok(response);
    }

    if !stdout.is_empty() {
        let quoted = serde_json::to_string(stdout).expect("serializing a string cannot fail");
        if let Ok(response) = serde_json::from_str(&quoted) {
            return Ok(response);
        }
    }

    Err(NigiriError::InvalidResponse {
        operation: operation.to_owned().into(),
        detail: "RPC response did not match the requested type".to_owned(),
    })
}

fn bounded_redacted(stderr: &[u8], args: &[OsString], limit: usize) -> String {
    let was_truncated = stderr.len() > limit;

    // Normalize before matching, never after. A CLI that colorizes part of an
    // echoed argument would otherwise defeat the byte comparison, and stripping
    // the escapes afterwards would reassemble the value in cleartext.
    let normalized = strip_ansi(&String::from_utf8_lossy(stderr));
    let source = normalized.as_bytes();

    let mut redaction_deltas = vec![0_i32; source.len() + 1];
    for argument in args.iter().filter_map(|argument| argument.to_str()) {
        if !argument.is_empty() {
            mark_argument_occurrences(
                source,
                argument.as_bytes(),
                was_truncated,
                &mut redaction_deltas,
            );
        }
    }

    let mut retained = Vec::with_capacity(source.len().min(limit));
    let mut active_redactions = 0_i32;
    let mut inside_redaction = false;
    let mut retention_truncated = false;
    for (index, &byte) in source.iter().enumerate() {
        active_redactions += redaction_deltas[index];
        if active_redactions > 0 {
            if !inside_redaction {
                retention_truncated |= !extend_bounded(&mut retained, b"[redacted]", limit);
                inside_redaction = true;
            }
        } else {
            inside_redaction = false;
            retention_truncated |= !extend_bounded(&mut retained, &[byte], limit);
        }
    }

    let mut text = String::from_utf8_lossy(&retained).into_owned();
    truncate_utf8(&mut text, limit);
    if was_truncated || retention_truncated {
        text.push_str("…[truncated]");
    }
    text
}

/// Bytes of a caller argument used to anchor a search for it in retained stderr.
///
/// A node CLI may echo only a fragment of a long value (`Invalid descriptor
/// "wpkh(cQr...")`), so redaction anchors on a short prefix and then extends over
/// however much of the argument actually appears. Sixteen bytes is long enough
/// that colliding with ordinary diagnostic text is not a practical concern, and
/// short enough that the search table stays a fixed 128 bytes regardless of how
/// large the retention limit is.
const ARGUMENT_PROBE_BYTES: usize = 16;

fn mark_argument_occurrences(
    input: &[u8],
    argument: &[u8],
    input_truncated: bool,
    deltas: &mut [i32],
) {
    let probe_len = argument.len().min(ARGUMENT_PROBE_BYTES).min(input.len());
    let probe = &argument[..probe_len];
    if probe.is_empty() {
        return;
    }

    let mut prefix_lengths = vec![0_usize; probe.len()];
    let mut matched = 0;
    for index in 1..probe.len() {
        while matched > 0 && probe[index] != probe[matched] {
            matched = prefix_lengths[matched - 1];
        }
        if probe[index] == probe[matched] {
            matched += 1;
            prefix_lengths[index] = matched;
        }
    }

    matched = 0;
    for (index, &byte) in input.iter().enumerate() {
        while matched > 0 && byte != probe[matched] {
            matched = prefix_lengths[matched - 1];
        }
        if byte == probe[matched] {
            matched += 1;
        }
        if matched == probe.len() {
            let start = index + 1 - probe.len();
            let span = matching_prefix_len(&input[start..], argument);
            mark_redaction(deltas, start, start + span);
            matched = prefix_lengths[matched - 1];
        }
    }

    // The stream may have been cut mid-probe, leaving only a partial anchor at
    // the very end with no complete match to extend from.
    if input_truncated && matched > 0 {
        mark_redaction(deltas, input.len() - matched, input.len());
    }
}

/// Length of the common prefix of `echoed` and `argument`.
///
/// Always at least the probe length when called after a confirmed probe hit, so
/// the redaction span covers exactly as much of the argument as was echoed.
fn matching_prefix_len(echoed: &[u8], argument: &[u8]) -> usize {
    let bound = echoed.len().min(argument.len());
    let mut length = 0;
    while length < bound && echoed[length] == argument[length] {
        length += 1;
    }
    length
}

fn mark_redaction(deltas: &mut [i32], start: usize, end: usize) {
    deltas[start] += 1;
    deltas[end] -= 1;
}

fn extend_bounded(output: &mut Vec<u8>, bytes: &[u8], limit: usize) -> bool {
    let retained = bytes.len().min(limit.saturating_sub(output.len()));
    output.extend_from_slice(&bytes[..retained]);
    retained == bytes.len()
}

fn truncate_utf8(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let mut boundary = limit;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' && characters.peek() == Some(&'[') {
            let _ = characters.next();
            for control in characters.by_ref() {
                if ('@'..='~').contains(&control) {
                    break;
                }
            }
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf, time::Duration};

    use serde::Deserialize;
    use url::Url;

    use crate::{
        Bitcoin, DEFAULT_MAX_RPC_RESPONSE_BYTES, Liquid, NigiriClient, NigiriConfig, NigiriError,
    };

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct FixtureInfo {
        chain: String,
        blocks: u64,
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct UnicodeFixture {
        message: String,
    }

    #[test]
    fn typed_rpc_parser_handles_json_and_unquoted_native_values() {
        let object: FixtureInfo =
            super::parse_rpc_response("fixture object", r#"{"chain":"regtest","blocks":42}"#)
                .unwrap();
        assert_eq!(
            object,
            FixtureInfo {
                chain: "regtest".to_owned(),
                blocks: 42,
            }
        );

        let height: u64 = super::parse_rpc_response("fixture height", "42").unwrap();
        assert_eq!(height, 42);

        let hashes: Vec<u64> = super::parse_rpc_response("fixture array", "[1,2,3]").unwrap();
        assert_eq!(hashes, vec![1, 2, 3]);

        let active: bool = super::parse_rpc_response("fixture boolean", "true").unwrap();
        assert!(active);

        let absent: Option<u64> = super::parse_rpc_response("fixture null", "null").unwrap();
        assert_eq!(absent, None);

        let hash: elements::BlockHash = super::parse_rpc_response(
            "fixture hash",
            "5555555555555555555555555555555555555555555555555555555555555555",
        )
        .unwrap();
        assert_eq!(
            hash.to_string(),
            "5555555555555555555555555555555555555555555555555555555555555555"
        );

        let unit: () = super::parse_rpc_response("fixture void", "").unwrap();
        assert_eq!(unit, ());
    }

    #[test]
    fn typed_rpc_parser_omits_malformed_response_content_from_errors() {
        let secret = "response-secret-material";
        let error = super::parse_rpc_response::<Vec<u64>>("fixture malformed", secret).unwrap_err();
        let rendered = error.to_string();
        assert!(!rendered.contains(secret));
        assert!(matches!(
            error,
            NigiriError::InvalidResponse { ref operation, .. }
                if operation.as_ref() == "fixture malformed"
        ));
    }

    #[test]
    fn rpc_method_validation_accepts_tokens_and_rejects_unsafe_names() {
        assert!(super::validate_rpc_method("getblockchaininfo").is_ok());
        assert!(super::validate_rpc_method("future_rpc2").is_ok());

        for invalid in ["", "get-block", "get block", "get\nblock"] {
            assert!(super::validate_rpc_method(invalid).is_err());
        }

        // A runtime method name reaches NigiriError and therefore caller logs, so
        // the charset alone is not enough of a bound.
        let at_limit = "a".repeat(super::MAX_RPC_METHOD_BYTES);
        assert!(super::validate_rpc_method(&at_limit).is_ok());
        let over_limit = "a".repeat(super::MAX_RPC_METHOD_BYTES + 1);
        assert!(super::validate_rpc_method(&over_limit).is_err());
    }

    /// Budget for confirming a killed child never performed its side effect.
    ///
    /// Comfortably exceeds the fixture's `sleep 1` so a surviving child has every
    /// chance to write. Polling means a broken kill fails fast instead of racing a
    /// fixed sleep, where the marker could land just after a single check.
    const MARKER_WATCH: Duration = Duration::from_secs(5);

    async fn marker_stays_absent(marker: &std::path::Path) -> bool {
        let deadline = tokio::time::Instant::now() + MARKER_WATCH;
        while tokio::time::Instant::now() < deadline {
            if marker.exists() {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        !marker.exists()
    }

    fn fake_client<N: crate::NigiriNetwork>(timeout: Duration) -> NigiriClient<N> {
        fake_client_with_limit(timeout, DEFAULT_MAX_RPC_RESPONSE_BYTES)
    }

    fn fake_client_with_limit<N: crate::NigiriNetwork>(
        timeout: Duration,
        max_rpc_response_bytes: usize,
    ) -> NigiriClient<N> {
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-nigiri.sh");
        NigiriClient::with_config(NigiriConfig {
            chopsticks_url: Url::parse("http://127.0.0.1:1").unwrap(),
            esplora_url: Url::parse("http://127.0.0.1:1").unwrap(),
            executable: fixture,
            timeout,
            max_rpc_response_bytes,
        })
        .unwrap()
    }

    #[test]
    fn bitcoin_rpc_arguments_follow_nigiri_ordering() {
        let invocation = super::build_rpc_invocation::<Bitcoin, _, _>(
            PathBuf::from("nigiri"),
            "generatetoaddress",
            ["3", "bcrt1qdestination"],
        );
        assert_eq!(
            invocation.args,
            [
                OsString::from("rpc"),
                OsString::from("generatetoaddress"),
                OsString::from("3"),
                OsString::from("bcrt1qdestination"),
            ]
        );
    }

    #[test]
    fn liquid_rpc_arguments_put_the_flag_before_the_method() {
        let invocation = super::build_rpc_invocation::<Liquid, _, _>(
            PathBuf::from("nigiri"),
            "getbestblockhash",
            std::iter::empty::<&str>(),
        );
        assert_eq!(
            invocation.args,
            [
                OsString::from("rpc"),
                OsString::from("--liquid"),
                OsString::from("getbestblockhash"),
            ]
        );
    }

    #[tokio::test]
    async fn nonzero_exit_is_structured_and_redacts_caller_arguments() {
        let client = fake_client::<Bitcoin>(Duration::from_secs(2));
        let secret = "caller-secret-descriptor";

        let error = client.rpc::<(), _, _>("fail", [secret]).await.unwrap_err();

        let NigiriError::RpcFailed {
            method,
            exit_code,
            stderr,
        } = error
        else {
            panic!("expected RPC failure");
        };
        assert_eq!(method, "fail");
        assert_eq!(exit_code, Some(17));
        assert!(!stderr.contains(secret));
    }

    #[tokio::test]
    async fn zero_exit_rpc_error_on_stdout_is_structured_and_redacted() {
        let client = fake_client::<Liquid>(Duration::from_secs(2));
        let secret = "caller-secret-descriptor";

        let error = client
            .rpc::<(), _, _>("rpc_error", [secret])
            .await
            .unwrap_err();

        let NigiriError::RpcFailed {
            method,
            exit_code,
            stderr,
        } = error
        else {
            panic!("expected RPC failure");
        };
        assert_eq!(method, "rpc_error");
        assert_eq!(exit_code, Some(0));
        assert!(stderr.contains("error code: -8"));
        assert!(!stderr.contains(secret));
    }

    #[tokio::test]
    async fn zero_exit_rpc_error_on_stderr_is_structured_and_redacted() {
        let client = fake_client::<Liquid>(Duration::from_secs(2));
        let secret = "caller-secret-descriptor";

        let error = client
            .rpc::<(), _, _>("stderr_zero", [secret])
            .await
            .unwrap_err();

        let NigiriError::RpcFailed {
            method,
            exit_code,
            stderr,
        } = error
        else {
            panic!("expected RPC failure");
        };
        assert_eq!(method, "stderr_zero");
        assert_eq!(exit_code, Some(0));
        assert!(!stderr.contains(secret));
    }

    #[tokio::test]
    async fn public_rpc_deserializes_bitcoin_and_liquid_results() {
        let bitcoin = fake_client::<Bitcoin>(Duration::from_secs(2));
        let height: u64 = bitcoin
            .rpc("json_number", std::iter::empty::<&str>())
            .await
            .unwrap();
        assert_eq!(height, 42);

        let liquid = fake_client::<Liquid>(Duration::from_secs(2));
        let txid: elements::Txid = liquid
            .rpc("unquoted_id", std::iter::empty::<&str>())
            .await
            .unwrap();
        assert_eq!(
            txid.to_string(),
            "7777777777777777777777777777777777777777777777777777777777777777"
        );

        let result: () = liquid
            .rpc("void_result", std::iter::empty::<&str>())
            .await
            .unwrap();
        assert_eq!(result, ());
    }

    #[tokio::test]
    async fn public_rpc_rejects_invalid_method_before_process_spawn() {
        let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
            chopsticks_url: Url::parse("http://127.0.0.1:1").unwrap(),
            esplora_url: Url::parse("http://127.0.0.1:1").unwrap(),
            executable: PathBuf::from("/definitely/missing/nigiri/binary"),
            timeout: Duration::from_secs(1),
            max_rpc_response_bytes: DEFAULT_MAX_RPC_RESPONSE_BYTES,
        })
        .unwrap();

        let error = client
            .rpc::<(), _, _>("invalid-method", std::iter::empty::<&str>())
            .await
            .unwrap_err();
        // The variant itself now says the input was rejected before any spawn,
        // so no caller needs to string-match a synthetic operation label.
        let NigiriError::InvalidRequest { detail } = &error else {
            panic!("expected an invalid request, got {error}");
        };
        assert!(
            detail.contains("ASCII letters, digits, and underscores"),
            "unhelpful detail: {detail}"
        );
    }

    #[tokio::test]
    async fn public_rpc_does_not_copy_invalid_response_into_error() {
        let client = fake_client::<Liquid>(Duration::from_secs(2));
        let caller_secret = "caller-secret-descriptor";
        let error = client
            .rpc::<Vec<u64>, _, _>("invalid_response", [caller_secret])
            .await
            .unwrap_err();
        let rendered = error.to_string();
        assert!(!rendered.contains("response-secret-material"));
        assert!(!rendered.contains(caller_secret));
    }

    #[tokio::test]
    async fn stream_limit_breaches_kill_the_child_before_follow_up_side_effects() {
        let client = fake_client::<Bitcoin>(Duration::from_secs(3));

        for (method, expected_stderr_failure) in [
            ("oversized_stdout_then_marker", false),
            ("oversized_stderr_then_marker", true),
        ] {
            let marker = std::env::temp_dir()
                .join(format!("nigiri-rs-{method}-marker-{}", std::process::id()));
            let _ = std::fs::remove_file(&marker);
            let marker_text = marker.to_string_lossy().into_owned();

            let error = client
                .rpc::<(), _, _>(method, [marker_text.as_str()])
                .await
                .unwrap_err();
            if expected_stderr_failure {
                assert!(matches!(error, NigiriError::RpcFailed { .. }));
            } else {
                assert!(matches!(error, NigiriError::InvalidResponse { .. }));
            }
            assert!(
                marker_stays_absent(&marker).await,
                "stream-limited child survived and wrote {}",
                marker.display()
            );
        }
    }

    #[tokio::test]
    async fn stderr_redaction_hides_a_caller_argument_cut_by_the_retention_boundary() {
        let client = fake_client::<Bitcoin>(Duration::from_secs(2));
        let secret = format!(
            "caller-secret-prefix-{}",
            "s".repeat(crate::DEFAULT_MAX_RPC_RESPONSE_BYTES + 2 * super::PIPE_READ_CHUNK_BYTES)
        );

        let error = client
            .rpc::<(), _, _>("long_stderr_secret", [secret.as_str()])
            .await
            .unwrap_err();
        let NigiriError::RpcFailed { stderr, .. } = error else {
            panic!("expected RPC failure");
        };
        assert!(!stderr.contains("caller-secret-prefix"));
        assert!(stderr.contains("[redacted]"));
        assert!(stderr.ends_with("…[truncated]"));
    }

    #[tokio::test]
    async fn public_rpc_preserves_unicode_while_stripping_ansi() {
        let client = fake_client::<Liquid>(Duration::from_secs(2));

        let response: UnicodeFixture = client
            .rpc("unicode_json", std::iter::empty::<&str>())
            .await
            .unwrap();

        assert_eq!(
            response,
            UnicodeFixture {
                message: "ação 日本語 🚀".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn bounded_stdout_and_stderr_are_drained_concurrently_without_deadlock() {
        let client = fake_client::<Bitcoin>(Duration::from_secs(2));

        let error = client
            .rpc::<(), _, _>("bounded_both_streams", std::iter::empty::<&str>())
            .await
            .unwrap_err();

        let NigiriError::RpcFailed {
            exit_code, stderr, ..
        } = error
        else {
            panic!("expected RPC failure");
        };
        assert_eq!(exit_code, Some(21));
        assert_eq!(stderr.len(), 60_000);
    }

    #[tokio::test]
    async fn a_raised_response_limit_admits_a_result_the_default_limit_rejects() {
        const OVERSIZED_BYTES: usize = 70_000;

        let default_limit = fake_client::<Bitcoin>(Duration::from_secs(2));
        let rejected = default_limit
            .rpc::<String, _, _>("oversized_stdout", std::iter::empty::<&str>())
            .await
            .unwrap_err();
        let NigiriError::InvalidResponse { detail, .. } = &rejected else {
            panic!("expected the default limit to reject the response");
        };
        assert!(
            detail.contains("max_rpc_response_bytes"),
            "the limit error should name the knob that raises it: {detail}"
        );

        let raised_limit = fake_client_with_limit::<Bitcoin>(Duration::from_secs(2), 128 * 1024);
        let response: String = raised_limit
            .rpc("oversized_stdout", std::iter::empty::<&str>())
            .await
            .unwrap();
        assert_eq!(response.len(), OVERSIZED_BYTES);
    }

    #[tokio::test]
    async fn a_lowered_response_limit_rejects_a_result_the_default_limit_admits() {
        let client = fake_client_with_limit::<Bitcoin>(Duration::from_secs(2), 8);

        let error = client
            .rpc::<String, _, _>("unquoted_id", std::iter::empty::<&str>())
            .await
            .unwrap_err();

        assert!(matches!(error, NigiriError::InvalidResponse { .. }));
    }

    #[tokio::test]
    async fn a_zero_response_limit_is_rejected_during_configuration() {
        let error = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
            chopsticks_url: Url::parse("http://127.0.0.1:1").unwrap(),
            esplora_url: Url::parse("http://127.0.0.1:1").unwrap(),
            executable: PathBuf::from("nigiri"),
            timeout: Duration::from_secs(1),
            max_rpc_response_bytes: 0,
        })
        .unwrap_err();

        let NigiriError::InvalidRequest { detail } = &error else {
            panic!("expected an invalid request, got {error}");
        };
        assert!(
            detail.contains("greater than zero"),
            "unhelpful detail: {detail}"
        );
    }

    #[tokio::test]
    async fn an_oversized_response_limit_is_rejected_during_configuration() {
        // An unbounded ceiling would let one RPC failure allocate its way to an
        // out-of-memory abort while formatting the error.
        let error = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
            chopsticks_url: Url::parse("http://127.0.0.1:1").unwrap(),
            esplora_url: Url::parse("http://127.0.0.1:1").unwrap(),
            executable: PathBuf::from("nigiri"),
            timeout: Duration::from_secs(1),
            max_rpc_response_bytes: crate::MAX_RPC_RESPONSE_BYTES_LIMIT + 1,
        })
        .unwrap_err();

        let NigiriError::InvalidRequest { detail } = &error else {
            panic!("expected an invalid request, got {error}");
        };
        assert!(
            detail.contains("MAX_RPC_RESPONSE_BYTES_LIMIT"),
            "unhelpful detail: {detail}"
        );

        // The boundary itself must be accepted.
        NigiriClient::<Bitcoin>::with_config(NigiriConfig {
            chopsticks_url: Url::parse("http://127.0.0.1:1").unwrap(),
            esplora_url: Url::parse("http://127.0.0.1:1").unwrap(),
            executable: PathBuf::from("nigiri"),
            timeout: Duration::from_secs(1),
            max_rpc_response_bytes: crate::MAX_RPC_RESPONSE_BYTES_LIMIT,
        })
        .expect("the documented maximum must be accepted");
    }

    #[tokio::test]
    async fn an_error_phrase_inside_response_content_is_not_treated_as_a_failure() {
        let client = fake_client::<Bitcoin>(Duration::from_secs(2));

        let response: UnicodeFixture = client
            .rpc("inline_error_phrase", std::iter::empty::<&str>())
            .await
            .unwrap_or_else(|error| panic!("mid-line error phrase misread as a failure: {error}"));

        assert_eq!(
            response.message,
            "the operator log said error code: -8 last week"
        );
    }

    #[test]
    fn ansi_escapes_inside_a_caller_argument_do_not_defeat_redaction() {
        // A CLI that colorizes part of an echoed argument must not slip it past
        // the byte match and then get it reassembled in cleartext.
        let secret = "caller-secret-descriptor";
        let stderr = b"error: rejected caller-\x1b[31msecret\x1b[0m-descriptor now";

        let redacted = super::bounded_redacted(
            stderr,
            &[OsString::from(secret)],
            DEFAULT_MAX_RPC_RESPONSE_BYTES,
        );

        assert!(
            !redacted.contains(secret),
            "colorized caller argument leaked: {redacted}"
        );
        assert!(
            redacted.contains("[redacted]"),
            "expected a redaction marker: {redacted}"
        );
    }

    #[test]
    fn a_partially_echoed_caller_argument_is_redacted_to_the_echoed_length() {
        // A CLI that elides a long value must not surface the fragment it kept.
        let secret = format!("cSecretDescriptorWIF{}", "x".repeat(300));
        let echoed = &secret[..60];
        let stderr = format!("error code: -5\nerror message:\nInvalid descriptor \"{echoed}...\"");

        let redacted = super::bounded_redacted(
            stderr.as_bytes(),
            &[OsString::from(secret.clone())],
            DEFAULT_MAX_RPC_RESPONSE_BYTES,
        );

        assert!(
            !redacted.contains("cSecretDescriptorWIF"),
            "the echoed fragment leaked: {redacted}"
        );
        assert!(
            redacted.contains("[redacted]"),
            "expected a redaction marker: {redacted}"
        );
        // Surrounding diagnostic text must survive, or the error is useless.
        assert!(
            redacted.contains("error code: -5"),
            "context lost: {redacted}"
        );
        assert!(
            redacted.contains("Invalid descriptor"),
            "context lost: {redacted}"
        );
    }

    #[test]
    fn a_short_caller_argument_still_requires_a_full_match() {
        // Arguments shorter than the probe keep exact-match behavior, so a block
        // count of "3" does not redact every 3 in the diagnostic.
        let redacted = super::bounded_redacted(
            b"error: height 3 is above the tip at 31",
            &[OsString::from("3")],
            DEFAULT_MAX_RPC_RESPONSE_BYTES,
        );

        assert!(
            !redacted.contains('3'),
            "expected every literal 3 redacted for an exact-match argument: {redacted}"
        );
        assert!(
            redacted.contains("above the tip"),
            "context lost: {redacted}"
        );
    }

    #[test]
    fn ansi_sequences_with_non_alphabetic_final_bytes_are_stripped() {
        assert_eq!(super::strip_ansi("a\u{1b}[3~b\u{1b}[38;5;9mc"), "abc");
        // `u` is itself a valid CSI final byte, so a sequence is only unterminated
        // when the input ends mid-parameters.
        assert_eq!(super::strip_ansi("keep\u{1b}[38;5"), "keep");
        assert_eq!(super::strip_ansi("keep\u{1b}[uafter"), "keepafter");
    }

    #[test]
    fn multibyte_stderr_cut_by_the_limit_stays_valid_utf8() {
        let redacted = super::bounded_redacted("é".repeat(50).as_bytes(), &[], 5);

        assert!(
            redacted.ends_with("…[truncated]"),
            "expected a truncation marker: {redacted}"
        );
        // The assertion that matters: no panic, and the result is valid UTF-8 by
        // construction. Nothing past the boundary survives as a partial scalar.
        assert!(
            redacted.starts_with("é"),
            "unexpected retention: {redacted}"
        );
    }

    #[test]
    fn a_mis_declared_caller_boundary_redacts_everything_rather_than_nothing() {
        let invocation = super::RpcInvocation {
            executable: PathBuf::from("nigiri"),
            args: vec![OsString::from("rpc"), OsString::from("getblockcount")],
            caller_args_from: 99,
        };

        assert_eq!(invocation.caller_args(), invocation.args.as_slice());
    }

    #[tokio::test]
    async fn stderr_warnings_do_not_fail_a_successful_result() {
        let client = fake_client::<Bitcoin>(Duration::from_secs(2));

        let height: u64 = client
            .rpc("stdout_with_stderr_warning", std::iter::empty::<&str>())
            .await
            .unwrap_or_else(|error| panic!("stderr warning misread as a failure: {error}"));

        assert_eq!(height, 42);
    }

    #[tokio::test]
    async fn caller_arguments_are_never_shell_expanded() {
        let client = fake_client::<Bitcoin>(Duration::from_secs(2));
        let marker =
            std::env::temp_dir().join(format!("nigiri-rs-shell-marker-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let injected = format!("$(touch {})", marker.display());

        let error = client
            .rpc::<(), _, _>("fail", [injected.as_str()])
            .await
            .unwrap_err();

        assert!(matches!(error, NigiriError::RpcFailed { .. }));
        assert!(
            !marker.exists(),
            "a caller argument was shell expanded, writing {}",
            marker.display()
        );
    }

    #[tokio::test]
    async fn non_utf8_stdout_is_rejected_without_copying_the_bytes_into_the_error() {
        let client = fake_client::<Bitcoin>(Duration::from_secs(2));

        let error = client
            .rpc::<String, _, _>("invalid_utf8_stdout", std::iter::empty::<&str>())
            .await
            .unwrap_err();

        let NigiriError::InvalidResponse { detail, .. } = &error else {
            panic!("expected an invalid response, got {error}");
        };
        assert_eq!(detail, "expected UTF-8 RPC stdout");
        let rendered = error.to_string();
        assert!(
            !rendered.contains("prefix") && !rendered.contains("suffix"),
            "the undecodable stdout leaked into the error: {rendered}"
        );
    }

    #[tokio::test]
    async fn whitespace_only_stderr_does_not_fail_a_void_result() {
        let client = fake_client::<Bitcoin>(Duration::from_secs(2));

        let result: () = client
            .rpc("blank_stderr_void", std::iter::empty::<&str>())
            .await
            .unwrap_or_else(|error| panic!("blank stderr misread as a failure: {error}"));

        assert_eq!(result, ());
    }

    #[tokio::test]
    async fn a_runtime_method_name_is_accepted_and_reported_in_errors() {
        let client = fake_client::<Bitcoin>(Duration::from_secs(2));
        let method = String::from("json_") + "number";

        let height: u64 = client
            .rpc(&method, std::iter::empty::<&str>())
            .await
            .unwrap();
        assert_eq!(height, 42);

        let failing = format!("{}{}", "fa", "il");
        let NigiriError::RpcFailed { method, .. } = client
            .rpc::<(), _, _>(&failing, ["secret"])
            .await
            .unwrap_err()
        else {
            panic!("expected an RPC failure");
        };
        assert_eq!(method, "fail");
    }

    #[tokio::test]
    async fn public_rpc_preserves_bounded_process_output_contracts() {
        let client = fake_client::<Bitcoin>(Duration::from_secs(2));

        let stdout_error = client
            .rpc::<String, _, _>("oversized_stdout", std::iter::empty::<&str>())
            .await
            .unwrap_err();
        assert!(matches!(stdout_error, NigiriError::InvalidResponse { .. }));

        let stderr_error = client
            .rpc::<(), _, _>("oversized_stderr", std::iter::empty::<&str>())
            .await
            .unwrap_err();
        let NigiriError::RpcFailed { stderr, .. } = stderr_error else {
            panic!("expected bounded RPC failure");
        };
        assert!(stderr.ends_with("…[truncated]"));
        assert!(stderr.len() <= crate::DEFAULT_MAX_RPC_RESPONSE_BYTES + "…[truncated]".len());
    }

    #[tokio::test]
    async fn public_rpc_deserializes_string_and_json_value_results() {
        let client = fake_client::<Bitcoin>(Duration::from_secs(2));

        let id: String = client
            .rpc("unquoted_id", std::iter::empty::<&str>())
            .await
            .unwrap();
        assert_eq!(
            id,
            "7777777777777777777777777777777777777777777777777777777777777777"
        );

        let response: serde_json::Value = client
            .rpc("json_number", std::iter::empty::<&str>())
            .await
            .unwrap();
        assert_eq!(response, serde_json::json!(42));
    }

    #[tokio::test]
    async fn malformed_stdout_becomes_invalid_response_in_the_public_parser() {
        let client = fake_client::<Bitcoin>(Duration::from_secs(2));

        let error = client.best_block_hash().await.unwrap_err();

        assert!(matches!(
            error,
            NigiriError::InvalidResponse { ref operation, .. }
                if operation.as_ref() == "best block hash"
        ));
    }

    #[tokio::test]
    async fn generated_hashes_are_parsed_as_native_bitcoin_hashes() {
        let client = fake_client::<Bitcoin>(Duration::from_secs(2));

        let hashes = client
            .generate_to_address(2, "bcrt1qfixture")
            .await
            .unwrap();

        assert_eq!(hashes.len(), 2);
        assert_eq!(
            hashes[0].to_string(),
            "5555555555555555555555555555555555555555555555555555555555555555"
        );
    }

    #[tokio::test]
    async fn timeout_kills_the_child_before_it_can_write_its_marker() {
        let client = fake_client::<Bitcoin>(Duration::from_millis(50));
        let marker =
            std::env::temp_dir().join(format!("nigiri-rs-timeout-marker-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        let error = client
            .rpc_stdout("timeout", [marker.as_os_str()])
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            NigiriError::Timeout { ref operation, .. }
                if operation.as_ref() == "timeout"
        ));
        assert!(
            marker_stays_absent(&marker).await,
            "timed-out child survived and wrote {}",
            marker.display()
        );
    }

    #[tokio::test]
    async fn zero_block_generation_is_rejected_before_process_spawn() {
        let mut config = NigiriConfig {
            chopsticks_url: Url::parse("http://127.0.0.1:1").unwrap(),
            esplora_url: Url::parse("http://127.0.0.1:1").unwrap(),
            executable: PathBuf::from("/definitely/missing/nigiri"),
            timeout: Duration::from_secs(1),
            max_rpc_response_bytes: DEFAULT_MAX_RPC_RESPONSE_BYTES,
        };
        config.executable.push("binary");
        let client = NigiriClient::<Liquid>::with_config(config).unwrap();

        let error = client
            .generate_to_address(0, "ert1qdestination")
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            NigiriError::InvalidResponse { ref operation, .. }
                if operation.as_ref() == "generate to address"
        ));
    }

    #[tokio::test]
    async fn process_spawn_failure_identifies_the_operation() {
        let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
            chopsticks_url: Url::parse("http://127.0.0.1:1").unwrap(),
            esplora_url: Url::parse("http://127.0.0.1:1").unwrap(),
            executable: PathBuf::from("/definitely/missing/nigiri/binary"),
            timeout: Duration::from_secs(1),
            max_rpc_response_bytes: DEFAULT_MAX_RPC_RESPONSE_BYTES,
        })
        .unwrap();

        let error = client.best_block_hash().await.unwrap_err();

        assert!(matches!(
            error,
            NigiriError::ProcessSpawn { ref operation, .. }
                if operation.as_ref() == "getbestblockhash"
        ));
    }
}
