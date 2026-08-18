use std::{borrow::Cow, error::Error as StdError, fmt, future::Future, sync::Arc, time::Duration};

use tonic::{
    Code, Request, Response, Status,
    metadata::AsciiMetadataValue,
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint},
};

use crate::{LndConfig, LndError, error::bounded};

pub(crate) struct ClientInner {
    pub(crate) channel: Channel,
    macaroon: AsciiMetadataValue,
    pub(crate) timeout: Duration,
}

impl ClientInner {
    pub(crate) fn authenticated(config: &LndConfig) -> Result<Arc<Self>, LndError> {
        let channel = tls_channel(config)?;
        let macaroon = lowercase_hex(config.macaroon())
            .parse()
            .map_err(|_| invalid_transport_configuration())?;
        Ok(Arc::new(Self {
            channel,
            macaroon,
            timeout: config.timeout(),
        }))
    }
}

impl fmt::Debug for ClientInner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientInner")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

#[allow(dead_code)]
pub(crate) struct UnauthenticatedLndClient {
    pub(crate) channel: Channel,
    pub(crate) timeout: Duration,
}

impl fmt::Debug for UnauthenticatedLndClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UnauthenticatedLndClient")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

#[allow(dead_code)]
pub(crate) fn unauthenticated(config: &LndConfig) -> Result<UnauthenticatedLndClient, LndError> {
    Ok(UnauthenticatedLndClient {
        channel: tls_channel(config)?,
        timeout: config.timeout(),
    })
}

pub(crate) async fn authenticated_request<RequestMessage, ResponseMessage, Call, CallFuture>(
    client: &ClientInner,
    operation: &'static str,
    message: RequestMessage,
    call: Call,
) -> Result<Response<ResponseMessage>, LndError>
where
    Call: FnOnce(Request<RequestMessage>) -> CallFuture,
    CallFuture: Future<Output = Result<Response<ResponseMessage>, Status>>,
{
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert("macaroon", client.macaroon.clone());
    bounded_request(client.timeout, operation, call(request)).await
}

pub(crate) async fn bounded_request<ResponseMessage, CallFuture>(
    duration: Duration,
    operation: &'static str,
    call: CallFuture,
) -> Result<Response<ResponseMessage>, LndError>
where
    CallFuture: Future<Output = Result<Response<ResponseMessage>, Status>>,
{
    match tokio::time::timeout(duration, call).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(status)) => Err(map_status(operation, status)),
        Err(_) => Err(LndError::Timeout {
            operation: Cow::Borrowed(operation),
            duration,
        }),
    }
}

pub(crate) fn map_status(operation: &'static str, status: Status) -> LndError {
    let operation = Cow::Borrowed(operation);
    if has_transport_source(&status) {
        return LndError::Transport {
            operation,
            detail: Cow::Borrowed("gRPC transport failed"),
        };
    }
    match status.code() {
        Code::Unauthenticated | Code::PermissionDenied => LndError::Authentication {
            operation,
            detail: Cow::Borrowed("credentials were rejected"),
        },
        code => LndError::Status {
            operation,
            detail: bounded(format!("gRPC status {code}")),
        },
    }
}

fn has_transport_source(status: &Status) -> bool {
    let mut source = status.source();
    while let Some(error) = source {
        if error.downcast_ref::<tonic::transport::Error>().is_some() {
            return true;
        }
        source = error.source();
    }
    false
}

fn tls_channel(config: &LndConfig) -> Result<Channel, LndError> {
    let domain = config
        .endpoint()
        .host_str()
        .ok_or_else(invalid_transport_configuration)?
        .to_owned();
    let endpoint = Endpoint::from_shared(config.endpoint().as_str().to_owned())
        .map_err(|_| invalid_transport_configuration())?;
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(config.certificate()))
        .domain_name(domain);
    let endpoint = endpoint
        .tls_config(tls)
        .map_err(|_| invalid_transport_configuration())?;
    Ok(endpoint.connect_lazy())
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[usize::from(byte >> 4)] as char);
        output.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn invalid_transport_configuration() -> LndError {
    LndError::Transport {
        operation: Cow::Borrowed("configure transport"),
        detail: Cow::Borrowed("invalid HTTPS transport configuration"),
    }
}

#[cfg(test)]
mod harness {
    tonic::include_proto!("harness");
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use rcgen::CertifiedKey;
    use tokio::sync::{Mutex, oneshot};
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::{
        Request, Response, Status,
        transport::{Identity, Server, ServerTlsConfig},
    };

    use super::{authenticated_request, bounded_request, map_status, unauthenticated};
    use crate::{LndClient, LndConfig, LndError};

    use super::harness::{
        ProbeRequest, ProbeResponse,
        harness_client::HarnessClient,
        harness_server::{Harness, HarnessServer},
    };

    const MACAROON: &[u8] = &[0xab, 0xcd, 0x01, 0x23];
    const MACAROON_HEX: &str = "abcd0123";

    #[derive(Clone)]
    struct ProbeService {
        last_macaroon: Arc<Mutex<Option<String>>>,
        delay: Duration,
        status: Option<Status>,
    }

    #[tonic::async_trait]
    impl Harness for ProbeService {
        async fn probe(
            &self,
            request: Request<ProbeRequest>,
        ) -> Result<Response<ProbeResponse>, Status> {
            *self.last_macaroon.lock().await = request
                .metadata()
                .get("macaroon")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            tokio::time::sleep(self.delay).await;
            if let Some(status) = &self.status {
                return Err(Status::new(status.code(), status.message()));
            }
            Ok(Response::new(ProbeResponse {
                message: "ready".into(),
            }))
        }
    }

    struct TestServer {
        endpoint: String,
        certificate: Vec<u8>,
        last_macaroon: Arc<Mutex<Option<String>>>,
        shutdown: Option<oneshot::Sender<()>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl TestServer {
        async fn start(delay: Duration, status: Option<Status>) -> Self {
            let CertifiedKey { cert, signing_key } =
                rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
            let certificate = cert.pem().into_bytes();
            let identity = Identity::from_pem(&certificate, signing_key.serialize_pem());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!(
                "https://localhost:{}",
                listener.local_addr().unwrap().port()
            );
            let last_macaroon = Arc::new(Mutex::new(None));
            let service = ProbeService {
                last_macaroon: Arc::clone(&last_macaroon),
                delay,
                status,
            };
            let (shutdown, receive_shutdown) = oneshot::channel();
            let task = tokio::spawn(async move {
                Server::builder()
                    .tls_config(ServerTlsConfig::new().identity(identity))
                    .unwrap()
                    .add_service(HarnessServer::new(service))
                    .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                        let _ = receive_shutdown.await;
                    })
                    .await
                    .unwrap();
            });
            Self {
                endpoint,
                certificate,
                last_macaroon,
                shutdown: Some(shutdown),
                task,
            }
        }

        fn config(&self, timeout: Duration) -> LndConfig {
            LndConfig::new(
                &self.endpoint,
                self.certificate.clone(),
                MACAROON.to_vec(),
                timeout,
            )
            .unwrap()
        }

        fn macaroon_hex(&self) -> &'static str {
            MACAROON_HEX
        }

        async fn last_macaroon(&self) -> Option<String> {
            self.last_macaroon.lock().await.clone()
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            self.task.abort();
        }
    }

    async fn probe_with_transport(config: LndConfig) -> Result<String, LndError> {
        let client = LndClient::with_config(config)?;
        let mut harness = HarnessClient::new(client.inner.channel.clone());
        let response = authenticated_request(&client.inner, "probe", ProbeRequest {}, |request| {
            harness.probe(request)
        })
        .await?;
        Ok(response.into_inner().message)
    }

    async fn probe_without_authentication(config: &LndConfig) -> Result<String, LndError> {
        let transport = unauthenticated(config)?;
        let mut harness = HarnessClient::new(transport.channel.clone());
        let response =
            bounded_request(transport.timeout, "probe", harness.probe(ProbeRequest {})).await?;
        Ok(response.into_inner().message)
    }

    #[tokio::test]
    async fn trusted_tls_attaches_lowercase_macaroon_metadata() {
        let server = TestServer::start(Duration::ZERO, None).await;

        let response = probe_with_transport(server.config(Duration::from_secs(1)))
            .await
            .unwrap();

        assert_eq!(response, "ready");
        assert_eq!(
            server.last_macaroon().await.as_deref(),
            Some(server.macaroon_hex())
        );
    }

    #[tokio::test]
    async fn untrusted_certificate_cannot_reach_the_service() {
        let server = TestServer::start(Duration::ZERO, None).await;
        let untrusted_certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .unwrap()
            .cert
            .pem()
            .into_bytes();
        let config = LndConfig::new(
            &server.endpoint,
            untrusted_certificate,
            MACAROON.to_vec(),
            Duration::from_secs(1),
        )
        .unwrap();

        let error = probe_with_transport(config).await.unwrap_err();

        assert!(matches!(error, LndError::Transport { .. }));
        assert_eq!(server.last_macaroon().await, None);
    }

    #[tokio::test]
    async fn unauthenticated_transport_uses_tls_without_macaroon_metadata() {
        let server = TestServer::start(Duration::ZERO, None).await;

        let response = probe_without_authentication(&server.config(Duration::from_secs(1)))
            .await
            .unwrap();

        assert_eq!(response, "ready");
        assert_eq!(server.last_macaroon().await, None);
    }

    #[tokio::test]
    async fn unauthenticated_status_maps_to_authentication_error() {
        let server = TestServer::start(
            Duration::ZERO,
            Some(Status::unauthenticated("macaroon rejected")),
        )
        .await;

        let error = probe_with_transport(server.config(Duration::from_secs(1)))
            .await
            .unwrap_err();

        assert!(matches!(error, LndError::Authentication { .. }));
    }

    #[tokio::test]
    async fn request_delayed_past_deadline_maps_to_timeout() {
        let server = TestServer::start(Duration::from_secs(1), None).await;

        let error = probe_with_transport(server.config(Duration::from_millis(20)))
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            LndError::Timeout {
                operation,
                duration
            } if operation == "probe" && duration == Duration::from_millis(20)
        ));
    }

    #[test]
    fn permission_denied_maps_to_authentication_without_secret_details() {
        let secret = "macaroon-deadbeefcafebabe";
        let error = map_status("probe", Status::permission_denied(secret));
        let display = error.to_string();
        let debug = format!("{error:?}");

        assert!(matches!(error, LndError::Authentication { .. }));
        assert!(!display.contains(secret));
        assert!(!debug.contains(secret));
    }
}
