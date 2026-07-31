use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
    process::{ExitStatus, Stdio},
};

use serde::de::DeserializeOwned;
use tokio::{
    io::{AsyncReadExt, Error as IoError},
    process::{Child, ChildStderr, ChildStdout, Command},
};

use crate::{NigiriClient, NigiriError, NigiriNetwork, http::MAX_BODY_BYTES};

const PIPE_READ_CHUNK_BYTES: usize = 8 * 1024;
const MAX_CAPTURE_BYTES: usize = MAX_BODY_BYTES + PIPE_READ_CHUNK_BYTES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RpcInvocation {
    pub(crate) executable: PathBuf,
    pub(crate) args: Vec<OsString>,
}

pub(crate) fn build_rpc_invocation<N, I, S>(
    executable: PathBuf,
    method: &'static str,
    args: I,
) -> RpcInvocation
where
    N: NigiriNetwork,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = N::rpc_prefix()
        .iter()
        .map(OsString::from)
        .chain(std::iter::once(OsString::from(method)))
        .chain(args.into_iter().map(|arg| arg.as_ref().to_os_string()))
        .collect();
    RpcInvocation { executable, args }
}

impl<N: NigiriNetwork> NigiriClient<N> {
    /// Invokes a node RPC through Nigiri and deserializes its response.
    ///
    /// Arguments use the same separate CLI-style strings accepted by
    /// `nigiri rpc`; this method never invokes a shell.
    ///
    /// # Errors
    ///
    /// Returns [`NigiriError`] when the method name is invalid, Nigiri cannot be
    /// executed, the process fails or times out, or the response does not match
    /// `R`. Caller arguments and successful response content are omitted from
    /// deserialization errors.
    ///
    /// # State changes
    ///
    /// RPC methods may mutate node wallets or active chain state. The caller owns
    /// synchronization and restoration for mutating host tests.
    pub async fn rpc<R, I, S>(&self, method: &'static str, args: I) -> Result<R, NigiriError>
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
        let stdout = self.rpc_stdout(method, args).await?;
        parse_rpc_response(method, &stdout)
    }

    pub(crate) async fn rpc_stdout<I, S>(
        &self,
        method: &'static str,
        args: I,
    ) -> Result<String, NigiriError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let invocation =
            build_rpc_invocation::<N, _, _>(self.config.executable.clone(), method, args);
        self.execute_invocation(method, invocation).await
    }

    pub(crate) async fn execute_invocation(
        &self,
        operation: &'static str,
        invocation: RpcInvocation,
    ) -> Result<String, NigiriError> {
        let mut command = Command::new(&invocation.executable);
        command
            .args(&invocation.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|source| NigiriError::ProcessSpawn { operation, source })?;
        let stdout = child
            .stdout
            .take()
            .expect("piped child stdout must be available");
        let stderr = child
            .stderr
            .take()
            .expect("piped child stderr must be available");
        let caller_args = &invocation.args[N::rpc_prefix().len() + 1..];
        let capture = tokio::time::timeout(
            self.config.timeout,
            capture_process_output(&mut child, stdout, stderr),
        )
        .await;
        let output = match capture {
            Ok(Ok(CaptureOutcome::Complete(output))) => output,
            Ok(Ok(CaptureOutcome::StdoutLimit)) => {
                kill_and_reap(&mut child)
                    .await
                    .map_err(|source| NigiriError::ProcessSpawn { operation, source })?;
                return Err(NigiriError::InvalidResponse {
                    operation,
                    detail: "RPC stdout exceeded the configured safety limit".to_owned(),
                });
            }
            Ok(Ok(CaptureOutcome::StderrLimit { stderr })) => {
                let status = kill_and_reap(&mut child)
                    .await
                    .map_err(|source| NigiriError::ProcessSpawn { operation, source })?;
                return Err(NigiriError::RpcFailed {
                    method: operation,
                    exit_code: status.code(),
                    stderr: bounded_redacted(&stderr, caller_args),
                });
            }
            Ok(Err(source)) => {
                kill_and_reap(&mut child).await.map_err(|kill_source| {
                    NigiriError::ProcessSpawn {
                        operation,
                        source: kill_source,
                    }
                })?;
                return Err(NigiriError::ProcessSpawn { operation, source });
            }
            Err(_) => {
                kill_and_reap(&mut child)
                    .await
                    .map_err(|source| NigiriError::ProcessSpawn { operation, source })?;
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
                stderr: bounded_redacted(&output.stderr, caller_args),
            });
        }
        if output.stdout.is_empty() && !output.stderr.is_empty() {
            return Err(NigiriError::RpcFailed {
                method: operation,
                exit_code: output.status.code(),
                stderr: bounded_redacted(&output.stderr, caller_args),
            });
        }
        let stdout =
            std::str::from_utf8(&output.stdout).map_err(|_| NigiriError::InvalidResponse {
                operation,
                detail: "expected UTF-8 RPC stdout".to_owned(),
            })?;
        let stdout = strip_ansi(stdout).trim().to_owned();
        if stdout.contains("error code:") {
            return Err(NigiriError::RpcFailed {
                method: operation,
                exit_code: output.status.code(),
                stderr: bounded_redacted(stdout.as_bytes(), caller_args),
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
                operation: OPERATION,
                detail: "block count must be greater than zero".to_owned(),
            });
        }
        let blocks = blocks.to_string();
        let stdout = self
            .rpc_stdout("generatetoaddress", [blocks.as_str(), address])
            .await?;
        let hashes: Vec<String> =
            serde_json::from_str(&stdout).map_err(|_| NigiriError::InvalidResponse {
                operation: OPERATION,
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
                operation: method,
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
) -> Result<CaptureOutcome, IoError> {
    let mut stdout_capture = Vec::with_capacity(MAX_CAPTURE_BYTES);
    let mut stderr_capture = Vec::with_capacity(MAX_CAPTURE_BYTES);
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
                    if stdout_capture.len() > MAX_BODY_BYTES {
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
                    if stderr_capture.len() > MAX_BODY_BYTES {
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

fn validate_rpc_method(method: &'static str) -> Result<(), NigiriError> {
    if !method.is_empty()
        && method
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Ok(());
    }

    Err(NigiriError::InvalidResponse {
        operation: "RPC method validation",
        detail: "method must contain only ASCII letters, digits, and underscores".to_owned(),
    })
}

fn parse_rpc_response<R>(operation: &'static str, stdout: &str) -> Result<R, NigiriError>
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
        operation,
        detail: "RPC response did not match the requested type".to_owned(),
    })
}

fn bounded_redacted(stderr: &[u8], args: &[OsString]) -> String {
    let was_truncated = stderr.len() > MAX_BODY_BYTES;
    let mut redaction_deltas = vec![0_i32; stderr.len() + 1];
    for argument in args.iter().filter_map(|argument| argument.to_str()) {
        if !argument.is_empty() {
            mark_argument_occurrences(
                stderr,
                argument.as_bytes(),
                was_truncated,
                &mut redaction_deltas,
            );
        }
    }

    let mut retained = Vec::with_capacity(MAX_BODY_BYTES);
    let mut active_redactions = 0_i32;
    let mut inside_redaction = false;
    let mut retention_truncated = false;
    for (index, &byte) in stderr.iter().enumerate() {
        active_redactions += redaction_deltas[index];
        if active_redactions > 0 {
            if !inside_redaction {
                retention_truncated |=
                    !extend_bounded(&mut retained, b"[redacted]", MAX_BODY_BYTES);
                inside_redaction = true;
            }
        } else {
            inside_redaction = false;
            retention_truncated |= !extend_bounded(&mut retained, &[byte], MAX_BODY_BYTES);
        }
    }

    let mut text = String::from_utf8_lossy(&retained).into_owned();
    truncate_utf8(&mut text, MAX_BODY_BYTES);
    if was_truncated || retention_truncated {
        text.push_str("…[truncated]");
    }
    text
}

fn mark_argument_occurrences(
    input: &[u8],
    argument: &[u8],
    input_truncated: bool,
    deltas: &mut [i32],
) {
    let pattern = &argument[..argument.len().min(input.len())];
    if pattern.is_empty() {
        return;
    }
    let mut prefix_lengths = vec![0_usize; pattern.len()];
    let mut matched = 0;
    for index in 1..pattern.len() {
        while matched > 0 && pattern[index] != pattern[matched] {
            matched = prefix_lengths[matched - 1];
        }
        if pattern[index] == pattern[matched] {
            matched += 1;
            prefix_lengths[index] = matched;
        }
    }

    matched = 0;
    for (index, &byte) in input.iter().enumerate() {
        while matched > 0 && byte != pattern[matched] {
            matched = prefix_lengths[matched - 1];
        }
        if byte == pattern[matched] {
            matched += 1;
        }
        if matched == pattern.len() {
            let end = index + 1;
            if argument.len() == pattern.len() || (input_truncated && end == input.len()) {
                mark_redaction(deltas, end - pattern.len(), end);
            }
            matched = prefix_lengths[matched - 1];
        }
    }

    if input_truncated && matched > 0 {
        mark_redaction(deltas, input.len() - matched, input.len());
    }
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

    use crate::{Bitcoin, Liquid, NigiriClient, NigiriConfig, NigiriError};

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
            NigiriError::InvalidResponse {
                operation: "fixture malformed",
                ..
            }
        ));
    }

    #[test]
    fn rpc_method_validation_accepts_tokens_and_rejects_unsafe_names() {
        assert!(super::validate_rpc_method("getblockchaininfo").is_ok());
        assert!(super::validate_rpc_method("future_rpc2").is_ok());

        for invalid in ["", "get-block", "get block", "get\nblock"] {
            assert!(super::validate_rpc_method(invalid).is_err());
        }
    }

    fn fake_client<N: crate::NigiriNetwork>(timeout: Duration) -> NigiriClient<N> {
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-nigiri.sh");
        NigiriClient::with_config(NigiriConfig {
            chopsticks_url: Url::parse("http://127.0.0.1:1").unwrap(),
            esplora_url: Url::parse("http://127.0.0.1:1").unwrap(),
            executable: fixture,
            timeout,
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
        })
        .unwrap();

        let error = client
            .rpc::<(), _, _>("invalid-method", std::iter::empty::<&str>())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            NigiriError::InvalidResponse {
                operation: "RPC method validation",
                ..
            }
        ));
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
            tokio::time::sleep(Duration::from_millis(1_200)).await;
            assert!(
                !marker.exists(),
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
            "s".repeat(super::MAX_BODY_BYTES + 2 * super::PIPE_READ_CHUNK_BYTES)
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
        assert!(stderr.len() <= super::MAX_BODY_BYTES + "…[truncated]".len());
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
            NigiriError::InvalidResponse {
                operation: "best block hash",
                ..
            }
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
            NigiriError::Timeout {
                operation: "timeout",
                ..
            }
        ));
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(
            !marker.exists(),
            "timed-out child survived and wrote marker"
        );
    }

    #[tokio::test]
    async fn zero_block_generation_is_rejected_before_process_spawn() {
        let mut config = NigiriConfig {
            chopsticks_url: Url::parse("http://127.0.0.1:1").unwrap(),
            esplora_url: Url::parse("http://127.0.0.1:1").unwrap(),
            executable: PathBuf::from("/definitely/missing/nigiri"),
            timeout: Duration::from_secs(1),
        };
        config.executable.push("binary");
        let client = NigiriClient::<Liquid>::with_config(config).unwrap();

        let error = client
            .generate_to_address(0, "ert1qdestination")
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            NigiriError::InvalidResponse {
                operation: "generate to address",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn process_spawn_failure_identifies_the_operation() {
        let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
            chopsticks_url: Url::parse("http://127.0.0.1:1").unwrap(),
            esplora_url: Url::parse("http://127.0.0.1:1").unwrap(),
            executable: PathBuf::from("/definitely/missing/nigiri/binary"),
            timeout: Duration::from_secs(1),
        })
        .unwrap();

        let error = client.best_block_hash().await.unwrap_err();

        assert!(matches!(
            error,
            NigiriError::ProcessSpawn {
                operation: "getbestblockhash",
                ..
            }
        ));
    }
}
