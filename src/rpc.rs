use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
    process::Stdio,
};

use tokio::process::Command;

use crate::{NigiriClient, NigiriError, NigiriNetwork, http::MAX_BODY_BYTES};

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
    pub(crate) async fn rpc<I, S>(
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
        let child = command
            .spawn()
            .map_err(|source| NigiriError::ProcessSpawn { operation, source })?;
        let output = tokio::time::timeout(self.config.timeout, child.wait_with_output())
            .await
            .map_err(|_| NigiriError::Timeout {
                operation,
                duration: self.config.timeout,
            })?
            .map_err(|source| NigiriError::ProcessSpawn { operation, source })?;

        if !output.status.success() {
            return Err(NigiriError::RpcFailed {
                method: operation,
                exit_code: output.status.code(),
                stderr: bounded_redacted(&output.stderr, &invocation.args),
            });
        }
        if output.stdout.is_empty() && !output.stderr.is_empty() {
            return Err(NigiriError::RpcFailed {
                method: operation,
                exit_code: output.status.code(),
                stderr: bounded_redacted(&output.stderr, &invocation.args),
            });
        }
        if output.stdout.len() > MAX_BODY_BYTES {
            return Err(NigiriError::InvalidResponse {
                operation,
                detail: "RPC stdout exceeded the configured safety limit".to_owned(),
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
                stderr: bounded_redacted(stdout.as_bytes(), &invocation.args),
            });
        }
        Ok(stdout)
    }

    /// Creates a new native regtest address through the network node wallet.
    pub async fn new_address(&self) -> Result<N::Address, NigiriError> {
        const OPERATION: &str = "new address";
        let stdout = self
            .rpc("getnewaddress", std::iter::empty::<&str>())
            .await?;
        N::parse_address(OPERATION, &stdout)
    }

    /// Returns the native active-chain tip hash.
    pub async fn best_block_hash(&self) -> Result<N::BlockHash, NigiriError> {
        const OPERATION: &str = "best block hash";
        let stdout = self
            .rpc("getbestblockhash", std::iter::empty::<&str>())
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
            .rpc("generatetoaddress", [blocks.as_str(), address])
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
        let stdout = self.rpc(method, [hash.as_str()]).await?;
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

fn bounded_redacted(stderr: &[u8], args: &[OsString]) -> String {
    let bounded = &stderr[..stderr.len().min(MAX_BODY_BYTES)];
    let mut text = String::from_utf8_lossy(bounded).into_owned();
    for argument in args.iter().filter_map(|argument| argument.to_str()) {
        if !argument.is_empty() {
            text = text.replace(argument, "[redacted]");
        }
    }
    if stderr.len() > MAX_BODY_BYTES {
        text.push_str("…[truncated]");
    }
    text
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut bytes = input.bytes().peekable();
    while let Some(byte) = bytes.next() {
        if byte == 0x1b && bytes.peek() == Some(&b'[') {
            let _ = bytes.next();
            for control in bytes.by_ref() {
                if control.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            output.push(char::from(byte));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf, time::Duration};

    use url::Url;

    use crate::{Bitcoin, Liquid, NigiriClient, NigiriConfig, NigiriError};

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

        let error = client.rpc("fail", [secret]).await.unwrap_err();

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

        let error = client.rpc("rpc_error", [secret]).await.unwrap_err();

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

        let error = client.rpc("stderr_zero", [secret]).await.unwrap_err();

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
            .rpc("timeout", [marker.as_os_str()])
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
