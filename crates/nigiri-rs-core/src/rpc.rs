use serde::{Serialize, de::DeserializeOwned};

use crate::{NigiriClient, NigiriError, NigiriNetwork};

impl<N: NigiriNetwork> NigiriClient<N> {
    /// Invokes a node RPC over JSON-RPC and deserializes its response.
    ///
    /// Parameters are serialized as a JSON-RPC parameter value. The method name
    /// may be computed at runtime and is validated before any transport request.
    ///
    /// # Errors
    ///
    /// Returns [`NigiriError`] when the method name is invalid, Nigiri cannot be
    /// reached, the request times out, or the response does not match `R`.
    /// Successful response content is omitted from deserialization errors.
    ///
    /// Responses larger than [`NigiriConfig::max_response_bytes`] are rejected
    /// rather than buffered. Raise that limit for methods with large results,
    /// such as `listunspent` or `getblock <hash> 2`.
    ///
    /// # State changes
    ///
    /// RPC methods may mutate node wallets or active chain state. The caller owns
    /// synchronization and restoration for mutating host tests.
    ///
    /// [`NigiriConfig::max_response_bytes`]: crate::NigiriConfig::max_response_bytes
    pub async fn rpc<R, P>(&self, method: &str, params: P) -> Result<R, NigiriError>
    where
        R: DeserializeOwned,
        P: Serialize,
    {
        validate_rpc_method(method)?;
        crate::node_rpc::call(self, method, params).await
    }

    /// Creates a new native regtest address through the network node wallet.
    pub async fn new_address(&self) -> Result<N::Address, NigiriError> {
        let value: String = crate::node_rpc::call(self, "getnewaddress", ()).await?;
        N::parse_address("new address", &value)
    }

    /// Returns the native active-chain tip hash.
    pub async fn best_block_hash(&self) -> Result<N::BlockHash, NigiriError> {
        crate::node_rpc::call(self, "getbestblockhash", ()).await
    }

    /// Mines a nonzero number of blocks to an address.
    pub async fn generate_to_address(
        &self,
        blocks: u64,
        address: &str,
    ) -> Result<Vec<N::BlockHash>, NigiriError> {
        if blocks == 0 {
            return Err(NigiriError::InvalidRequest {
                detail: "block count must be greater than zero".into(),
            });
        }
        crate::node_rpc::call(self, "generatetoaddress", (blocks, address)).await
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
        crate::node_rpc::call(self, method, (hash.to_string(),)).await
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use url::Url;

    use crate::{
        Bitcoin, DEFAULT_MAX_RESPONSE_BYTES, Liquid, NigiriClient, NigiriConfig, NigiriError,
    };

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

    async fn one_shot_server(body: String) -> (Url, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stream.read(&mut buffer).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                let header_end = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|index| index + 4);
                if let Some(header_end) = header_end {
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + content_length {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (Url::parse(&format!("http://{address}/")).unwrap(), task)
    }

    fn rpc_client<N: crate::NigiriNetwork>(node_rpc_url: Url) -> NigiriClient<N> {
        NigiriClient::with_config(NigiriConfig {
            node_rpc_url,
            timeout: Duration::from_secs(2),
            ..Default::default()
        })
        .unwrap()
    }

    fn rpc_result(result: &str) -> String {
        format!(r#"{{"result":{result},"error":null,"id":"nigiri-rs"}}"#)
    }

    const BITCOIN_ADDRESS: &str = "mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn";
    const LIQUID_ADDRESS: &str = "ert1qwhh2n5qypypm0eufahm2pvj8raj9zq5c27cysu";
    const BLOCK_HASH: &str = "5555555555555555555555555555555555555555555555555555555555555555";

    #[tokio::test]
    async fn curated_new_address_uses_json_rpc_for_both_networks() {
        let (bitcoin_url, bitcoin_request) =
            one_shot_server(rpc_result(&format!(r#""{BITCOIN_ADDRESS}""#))).await;
        let bitcoin = rpc_client::<Bitcoin>(bitcoin_url);

        assert_eq!(
            bitcoin.new_address().await.unwrap().to_string(),
            BITCOIN_ADDRESS
        );
        let bitcoin_request = bitcoin_request.await.unwrap();
        assert!(bitcoin_request.contains(r#""method":"getnewaddress""#));
        assert!(bitcoin_request.contains(r#""params":[]"#));

        let (liquid_url, liquid_request) =
            one_shot_server(rpc_result(&format!(r#""{LIQUID_ADDRESS}""#))).await;
        let liquid = rpc_client::<Liquid>(liquid_url);

        assert_eq!(
            liquid.new_address().await.unwrap().to_string(),
            LIQUID_ADDRESS
        );
        let liquid_request = liquid_request.await.unwrap();
        assert!(liquid_request.contains(r#""method":"getnewaddress""#));
        assert!(liquid_request.contains(r#""params":[]"#));
    }

    #[tokio::test]
    async fn curated_best_block_hash_uses_json_rpc() {
        let (url, request) = one_shot_server(rpc_result(&format!(r#""{BLOCK_HASH}""#))).await;
        let client = rpc_client::<Liquid>(url);

        assert_eq!(
            client.best_block_hash().await.unwrap().to_string(),
            BLOCK_HASH
        );
        let request = request.await.unwrap();
        assert!(request.contains(r#""method":"getbestblockhash""#));
        assert!(request.contains(r#""params":[]"#));
    }

    #[tokio::test]
    async fn curated_generate_to_address_uses_numeric_json_params() {
        let (url, request) = one_shot_server(rpc_result(&format!(r#"["{BLOCK_HASH}"]"#))).await;
        let client = rpc_client::<Bitcoin>(url);

        let hashes = client
            .generate_to_address(2, BITCOIN_ADDRESS)
            .await
            .unwrap();

        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0].to_string(), BLOCK_HASH);
        let request = request.await.unwrap();
        assert!(request.contains(r#""method":"generatetoaddress""#));
        assert!(request.contains(&format!(r#""params":[2,"{BITCOIN_ADDRESS}"]"#)));
        assert!(!request.contains(r#""params":["2""#));
    }

    #[tokio::test]
    async fn curated_invalidate_block_uses_json_rpc() {
        let (url, request) = one_shot_server(rpc_result("null")).await;
        let client = rpc_client::<Bitcoin>(url);
        let hash = BLOCK_HASH.parse().unwrap();

        client.invalidate_block(&hash).await.unwrap();

        let request = request.await.unwrap();
        assert!(request.contains(r#""method":"invalidateblock""#));
        assert!(request.contains(&format!(r#""params":["{BLOCK_HASH}"]"#)));
    }

    #[tokio::test]
    async fn curated_reconsider_block_uses_json_rpc() {
        let (url, request) = one_shot_server(rpc_result("null")).await;
        let client = rpc_client::<Liquid>(url);
        let hash = BLOCK_HASH.parse().unwrap();

        client.reconsider_block(&hash).await.unwrap();

        let request = request.await.unwrap();
        assert!(request.contains(r#""method":"reconsiderblock""#));
        assert!(request.contains(&format!(r#""params":["{BLOCK_HASH}"]"#)));
    }

    #[tokio::test]
    async fn a_zero_response_limit_is_rejected_during_configuration() {
        let error = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
            esplora_url: Url::parse("http://127.0.0.1:1").unwrap(),
            timeout: Duration::from_secs(1),
            max_response_bytes: 0,
            ..Default::default()
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
            esplora_url: Url::parse("http://127.0.0.1:1").unwrap(),
            timeout: Duration::from_secs(1),
            max_response_bytes: crate::MAX_RESPONSE_BYTES_LIMIT + 1,
            ..Default::default()
        })
        .unwrap_err();

        let NigiriError::InvalidRequest { detail } = &error else {
            panic!("expected an invalid request, got {error}");
        };
        assert!(
            detail.contains("MAX_RESPONSE_BYTES_LIMIT"),
            "unhelpful detail: {detail}"
        );

        // The boundary itself must be accepted.
        NigiriClient::<Bitcoin>::with_config(NigiriConfig {
            esplora_url: Url::parse("http://127.0.0.1:1").unwrap(),
            timeout: Duration::from_secs(1),
            max_response_bytes: crate::MAX_RESPONSE_BYTES_LIMIT,
            ..Default::default()
        })
        .expect("the documented maximum must be accepted");
    }

    #[tokio::test]
    async fn zero_block_generation_is_rejected_before_transport() {
        let config = NigiriConfig {
            esplora_url: Url::parse("http://127.0.0.1:1").unwrap(),
            node_rpc_url: Url::parse("http://127.0.0.1:1").unwrap(),
            timeout: Duration::from_secs(1),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            ..Default::default()
        };
        let client = NigiriClient::<Liquid>::with_config(config).unwrap();

        let error = client
            .generate_to_address(0, "ert1qdestination")
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            NigiriError::InvalidRequest { ref detail }
                if detail == "block count must be greater than zero"
        ));
    }
}
