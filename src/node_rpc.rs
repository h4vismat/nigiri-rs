// Phase 2 wires this transport into the public RPC methods and removes this allow.
#![allow(dead_code)]

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{NigiriClient, NigiriError, NigiriNetwork};

#[derive(Serialize)]
struct Request<'a, P> {
    jsonrpc: &'static str,
    id: &'static str,
    method: &'a str,
    params: P,
}

#[derive(Deserialize)]
struct Response {
    result: Option<serde_json::Value>,
    error: Option<RpcErrorPayload>,
}

#[derive(Deserialize)]
struct RpcErrorPayload {
    code: i32,
    message: String,
}

pub(crate) async fn call<N, P, R>(
    client: &NigiriClient<N>,
    method: &str,
    params: P,
) -> Result<R, NigiriError>
where
    N: NigiriNetwork,
    P: Serialize,
    R: DeserializeOwned,
{
    let mut params = serde_json::to_value(params).map_err(|_| NigiriError::InvalidRequest {
        detail: "node RPC parameters could not be serialized".into(),
    })?;
    if params.is_null() {
        params = serde_json::Value::Array(Vec::new());
    }

    // A constant id is safe while each POST contains one request and its response
    // is fully read before the next call. Batching or pipelining must make it unique.
    let request = Request {
        jsonrpc: "1.0",
        id: "nigiri-rs",
        method,
        params,
    };
    let response = client
        .http
        .post(client.config.node_rpc_url.clone())
        .basic_auth(
            &client.config.node_rpc_user,
            Some(&client.config.node_rpc_password),
        )
        .json(&request)
        .send()
        .await
        .map_err(|source| transport_error(client, method, source))?;
    let status = response.status();
    let body = read_bounded(client, method, response).await?;

    let response = match serde_json::from_slice::<Response>(&body) {
        Ok(response) => response,
        Err(_) if !status.is_success() => {
            return Err(NigiriError::HttpStatus {
                operation: method.to_owned().into(),
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        Err(_) => {
            return Err(invalid_response(
                method,
                "expected a JSON-RPC response envelope",
            ));
        }
    };

    if let Some(error) = response.error {
        // Temporary Phase 1 mapping. Phase 2 reshapes RpcFailed around code/message.
        return Err(NigiriError::RpcFailed {
            method: method.to_owned().into(),
            exit_code: Some(error.code),
            stderr: error.message,
        });
    }

    serde_json::from_value(response.result.unwrap_or(serde_json::Value::Null))
        .map_err(|_| invalid_response(method, "result did not match the requested type"))
}

async fn read_bounded<N: NigiriNetwork>(
    client: &NigiriClient<N>,
    method: &str,
    mut response: reqwest::Response,
) -> Result<Vec<u8>, NigiriError> {
    let mut body = Vec::new();
    let mut exceeded = false;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|source| transport_error(client, method, source))?
    {
        let remaining = client.config.max_response_bytes.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() > remaining {
            exceeded = true;
        }
    }

    if exceeded {
        return Err(invalid_response(
            method,
            "response body exceeded the configured safety limit",
        ));
    }
    Ok(body)
}

fn transport_error<N: NigiriNetwork>(
    client: &NigiriClient<N>,
    method: &str,
    source: reqwest::Error,
) -> NigiriError {
    if source.is_timeout() {
        NigiriError::Timeout {
            operation: method.to_owned().into(),
            duration: client.config.timeout,
        }
    } else {
        NigiriError::HttpTransport {
            operation: method.to_owned().into(),
            source: source.without_url(),
        }
    }
}

fn invalid_response(method: &str, detail: &'static str) -> NigiriError {
    NigiriError::InvalidResponse {
        operation: method.to_owned().into(),
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use url::Url;

    use crate::{Bitcoin, NigiriClient, NigiriConfig, NigiriError};

    async fn one_shot_server(status: &str, body: String) -> (Url, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_owned();
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
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (Url::parse(&format!("http://{address}/")).unwrap(), task)
    }

    fn client(node_rpc_url: Url, max_response_bytes: usize) -> NigiriClient<Bitcoin> {
        NigiriClient::with_config(NigiriConfig {
            node_rpc_url,
            timeout: Duration::from_secs(2),
            max_response_bytes,
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn request_envelope_preserves_numeric_params() {
        let body = r#"{"result":null,"error":null,"id":"nigiri-rs"}"#.to_owned();
        let (url, request) = one_shot_server("200 OK", body).await;
        let client = client(url, 1024);

        super::call::<_, _, ()>(&client, "generatetoaddress", (100_u64,))
            .await
            .unwrap();

        let request = request.await.unwrap();
        assert!(request.contains(r#""jsonrpc":"1.0""#));
        assert!(request.contains(r#""id":"nigiri-rs""#));
        assert!(request.contains(r#""method":"generatetoaddress""#));
        assert!(request.contains(r#""params":[100]"#));
        assert!(!request.contains(r#""params":["100"]"#));
    }

    #[tokio::test]
    async fn unit_params_are_sent_as_an_empty_array() {
        let body = r#"{"result":null,"error":null,"id":"nigiri-rs"}"#.to_owned();
        let (url, request) = one_shot_server("200 OK", body).await;
        let client = client(url, 1024);

        super::call::<_, _, ()>(&client, "getblockcount", ())
            .await
            .unwrap();

        let request = request.await.unwrap();
        assert!(request.contains(r#""params":[]"#));
        assert!(!request.contains(r#""params":null"#));
    }

    #[tokio::test]
    async fn successful_result_deserializes_into_the_requested_type() {
        let body = r#"{"result":123,"error":null,"id":"nigiri-rs"}"#.to_owned();
        let (url, _) = one_shot_server("200 OK", body).await;
        let client = client(url, 1024);

        let result: u64 = super::call(&client, "getblockcount", ()).await.unwrap();

        assert_eq!(result, 123);
    }

    #[tokio::test]
    async fn null_result_deserializes_into_unit() {
        let body = r#"{"result":null,"error":null,"id":"nigiri-rs"}"#.to_owned();
        let (url, _) = one_shot_server("200 OK", body).await;
        let client = client(url, 1024);

        let result: () = super::call(&client, "invalidateblock", ()).await.unwrap();

        assert_eq!(result, ());
    }

    #[tokio::test]
    async fn rpc_error_envelope_on_http_500_preserves_code_and_message() {
        let body = r#"{"result":null,"error":{"code":-8,"message":"Block height out of range"},"id":"nigiri-rs"}"#.to_owned();
        let (url, _) = one_shot_server("500 Internal Server Error", body).await;
        let client = client(url, 1024);

        let error = super::call::<_, _, ()>(&client, "getblockhash", (999_u64,))
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            NigiriError::RpcFailed {
                ref method,
                exit_code: Some(-8),
                ref stderr,
            } if method.as_ref() == "getblockhash" && stderr == "Block height out of range"
        ));
    }

    #[tokio::test]
    async fn non_envelope_non_success_body_becomes_http_status() {
        let (url, _) = one_shot_server("502 Bad Gateway", "gateway down".to_owned()).await;
        let client = client(url, 1024);

        let error = super::call::<_, _, ()>(&client, "getblockcount", ())
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            NigiriError::HttpStatus { status, ref body, .. }
                if status.as_u16() == 502 && body == "gateway down"
        ));
    }

    #[tokio::test]
    async fn oversized_response_body_becomes_invalid_response() {
        let (url, _) = one_shot_server("200 OK", "x".repeat(65)).await;
        let client = client(url, 64);

        let error = super::call::<_, _, ()>(&client, "getblockcount", ())
            .await
            .unwrap_err();

        assert!(matches!(error, NigiriError::InvalidResponse { .. }));
    }

    #[tokio::test]
    async fn result_type_mismatch_omits_response_body_from_error() {
        let secret = "not a number";
        let body = format!(r#"{{"result":"{secret}","error":null,"id":"nigiri-rs"}}"#);
        let (url, _) = one_shot_server("200 OK", body).await;
        let client = client(url, 1024);

        let error = super::call::<_, _, u64>(&client, "getblockcount", ())
            .await
            .unwrap_err();

        assert!(matches!(error, NigiriError::InvalidResponse { .. }));
        assert!(!error.to_string().contains(secret));
    }

    #[tokio::test]
    async fn request_uses_configured_basic_auth() {
        let body = r#"{"result":null,"error":null,"id":"nigiri-rs"}"#.to_owned();
        let (url, request) = one_shot_server("200 OK", body).await;
        let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
            node_rpc_url: url,
            node_rpc_user: "rpc-user".to_owned(),
            node_rpc_password: "rpc-pass".to_owned(),
            timeout: Duration::from_secs(2),
            max_response_bytes: 1024,
            ..Default::default()
        })
        .unwrap();

        super::call::<_, _, ()>(&client, "getblockcount", ())
            .await
            .unwrap();

        let request = request.await.unwrap().to_ascii_lowercase();
        assert!(request.contains("authorization: basic cnbjlxvzzxi6cnbjlxbhc3m="));
    }
}
