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
    result: serde_json::Value,
    error: serde_json::Value,
    id: String,
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
    call_with_sensitive(client, method, params, &[]).await
}

pub(crate) async fn call_sensitive<N, P, R>(
    client: &NigiriClient<N>,
    method: &str,
    params: P,
    sensitive: &[&str],
) -> Result<R, NigiriError>
where
    N: NigiriNetwork,
    P: Serialize,
    R: DeserializeOwned,
{
    call_with_sensitive(client, method, params, sensitive).await
}

async fn call_with_sensitive<N, P, R>(
    client: &NigiriClient<N>,
    method: &str,
    params: P,
    sensitive: &[&str],
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
    let response = crate::http::send(
        client,
        method,
        client
            .http
            .post(client.config.node_rpc_url.clone())
            .basic_auth(
                &client.config.node_rpc_user,
                Some(&client.config.node_rpc_password),
            )
            .json(&request),
    )
    .await?;
    let status = response.status;
    let http_error = (!status.is_success()).then(|| response.status_error(method, sensitive));
    let body = response.into_body(method)?;
    let envelope = serde_json::from_slice::<Response>(&body)
        .ok()
        .filter(|response| response.id == "nigiri-rs");
    let Some(envelope) = envelope else {
        return Err(http_error.unwrap_or_else(|| {
            invalid_response(method, "expected a matching JSON-RPC response envelope")
        }));
    };
    let result = envelope.result;
    let error = envelope.error;
    if !error.is_null() {
        if !result.is_null() {
            return Err(invalid_response(
                method,
                "response contains both a result and an error",
            ));
        }
        let error: RpcErrorPayload = serde_json::from_value(error)
            .map_err(|_| invalid_response(method, "invalid JSON-RPC error payload"))?;
        return Err(NigiriError::RpcFailed {
            method: method.to_owned().into(),
            code: error.code,
            message: crate::http::redact_sensitive(error.message, sensitive),
        });
    }
    if let Some(error) = http_error {
        return Err(error);
    }
    serde_json::from_value(result)
        .map_err(|_| invalid_response(method, "result did not match the requested type"))
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

    async fn holding_server() -> (Url, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
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
    async fn malformed_envelopes_never_succeed() {
        for body in [
            r#"{"result":null,"result":1,"error":null,"id":"nigiri-rs"}"#,
            r#"{"result":null,"id":"nigiri-rs"}"#,
            r#"{"result":null,"error":{"code":"-8","message":"bad"},"id":"nigiri-rs"}"#,
            r#"{}"#,
            r#"{"error":null,"id":"nigiri-rs"}"#,
            r#"{"result":null,"error":null}"#,
            r#"{"result":null,"error":null,"id":"other"}"#,
            r#"{"result":null,"error":null,"id":1}"#,
            r#"{"result":1,"error":{"code":-8,"message":"bad"},"id":"nigiri-rs"}"#,
        ] {
            let (url, _) = one_shot_server("200 OK", body.to_owned()).await;
            let error = super::call::<_, _, serde_json::Value>(&client(url, 1024), "test", ())
                .await
                .unwrap_err();
            assert!(
                matches!(error, NigiriError::InvalidResponse { .. }),
                "{body}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn success_envelope_on_http_failure_is_not_success() {
        let (url, _) = one_shot_server(
            "503 Service Unavailable",
            r#"{"result":null,"error":null,"id":"nigiri-rs"}"#.to_owned(),
        )
        .await;
        assert!(matches!(
            super::call::<_, _, ()>(&client(url, 1024), "test", ()).await,
            Err(NigiriError::HttpStatus { .. })
        ));
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
                code: -8,
                ref message,
            } if method.as_ref() == "getblockhash" && message == "Block height out of range"
        ));
    }

    #[tokio::test]
    async fn request_timeout_preserves_operation_and_configured_duration() {
        let (url, server) = holding_server().await;
        let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
            node_rpc_url: url,
            timeout: Duration::from_millis(25),
            max_response_bytes: 1024,
            ..Default::default()
        })
        .unwrap();

        let error = super::call::<_, _, u64>(&client, "getblockcount", ())
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            NigiriError::Timeout {
                ref operation,
                duration,
            } if operation.as_ref() == "getblockcount"
                && duration == Duration::from_millis(25)
        ));
        server.abort();
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
    async fn non_envelope_success_body_becomes_invalid_response() {
        let (url, _) = one_shot_server("200 OK", "not JSON".to_owned()).await;
        let client = client(url, 1024);

        let error = super::call::<_, _, ()>(&client, "getblockcount", ())
            .await
            .unwrap_err();

        assert!(matches!(error, NigiriError::InvalidResponse { .. }));
        assert!(!error.to_string().contains("not JSON"));
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
