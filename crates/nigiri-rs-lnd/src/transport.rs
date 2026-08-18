use std::{borrow::Cow, error::Error as StdError, fmt, future::Future, sync::Arc, time::Duration};

use tokio::sync::OnceCell;
use tonic::{
    Code, Request, Response, Status,
    metadata::AsciiMetadataValue,
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint},
};
use url::Host;

use crate::{LndConfig, LndError, error::bounded};

pub(crate) struct ClientInner {
    transport: LazyChannel,
    macaroon: AsciiMetadataValue,
    pub(crate) timeout: Duration,
}

impl ClientInner {
    pub(crate) fn authenticated(config: &LndConfig) -> Result<Arc<Self>, LndError> {
        let transport = LazyChannel::new(tls_endpoint(config)?);
        let macaroon = lowercase_hex(config.macaroon())
            .parse()
            .map_err(invalid_transport_configuration)?;
        Ok(Arc::new(Self {
            transport,
            macaroon,
            timeout: config.timeout(),
        }))
    }

    pub(crate) async fn channel(&self) -> Channel {
        self.transport.channel().await
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
    transport: LazyChannel,
    pub(crate) timeout: Duration,
}

impl UnauthenticatedLndClient {
    #[allow(dead_code)]
    pub(crate) async fn channel(&self) -> Channel {
        self.transport.channel().await
    }
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
        transport: LazyChannel::new(tls_endpoint(config)?),
        timeout: config.timeout(),
    })
}

struct LazyChannel {
    endpoint: Endpoint,
    channel: OnceCell<Channel>,
}

impl LazyChannel {
    fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            channel: OnceCell::new(),
        }
    }

    async fn channel(&self) -> Channel {
        self.channel
            .get_or_init(|| async { self.endpoint.connect_lazy() })
            .await
            .clone()
    }
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
    let deadline = operation_deadline(client.timeout)?;
    authenticated_request_until(client, deadline, client.timeout, operation, message, call).await
}

pub(crate) async fn authenticated_request_until<RequestMessage, ResponseMessage, Call, CallFuture>(
    client: &ClientInner,
    deadline: tokio::time::Instant,
    configured_duration: Duration,
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
    bounded_request_until(deadline, configured_duration, operation, call(request)).await
}

#[allow(dead_code)]
pub(crate) async fn bounded_request<ResponseMessage, CallFuture>(
    duration: Duration,
    operation: &'static str,
    call: CallFuture,
) -> Result<Response<ResponseMessage>, LndError>
where
    CallFuture: Future<Output = Result<Response<ResponseMessage>, Status>>,
{
    bounded_request_until(operation_deadline(duration)?, duration, operation, call).await
}

pub(crate) fn operation_deadline(duration: Duration) -> Result<tokio::time::Instant, LndError> {
    tokio::time::Instant::now()
        .checked_add(duration)
        .ok_or_else(|| LndError::InvalidRequest {
            detail: Cow::Borrowed("request timeout exceeds the supported instant range"),
        })
}

pub(crate) async fn bounded_request_until<ResponseMessage, CallFuture>(
    deadline: tokio::time::Instant,
    configured_duration: Duration,
    operation: &'static str,
    call: CallFuture,
) -> Result<Response<ResponseMessage>, LndError>
where
    CallFuture: Future<Output = Result<Response<ResponseMessage>, Status>>,
{
    match tokio::time::timeout_at(deadline, call).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(status)) => Err(map_status(operation, status)),
        Err(_) => Err(LndError::Timeout {
            operation: Cow::Borrowed(operation),
            duration: configured_duration,
        }),
    }
}

pub(crate) fn map_status(operation: &'static str, status: Status) -> LndError {
    let operation = Cow::Borrowed(operation);
    if has_transport_source(&status) {
        return LndError::Transport {
            operation,
            detail: Cow::Borrowed("gRPC transport failed"),
            source: Box::new(status),
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

fn tls_endpoint(config: &LndConfig) -> Result<Endpoint, LndError> {
    let domain = match config
        .endpoint()
        .host()
        .ok_or_else(|| invalid_transport_configuration(missing_endpoint_host()))?
    {
        Host::Domain(domain) => domain.to_owned(),
        Host::Ipv4(address) => address.to_string(),
        Host::Ipv6(address) => address.to_string(),
    };
    let endpoint = Endpoint::from_shared(config.endpoint().as_str().to_owned())
        .map_err(invalid_transport_configuration)?;
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(config.certificate()))
        .domain_name(domain);
    endpoint
        .tls_config(tls)
        .map_err(invalid_transport_configuration)
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

fn invalid_transport_configuration(source: impl StdError + Send + Sync + 'static) -> LndError {
    LndError::Transport {
        operation: Cow::Borrowed("configure transport"),
        detail: Cow::Borrowed("invalid HTTPS transport configuration"),
        source: Box::new(source),
    }
}

fn missing_endpoint_host() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "validated endpoint has no host",
    )
}

#[cfg(test)]
mod harness {
    tonic::include_proto!("harness");
}

#[cfg(test)]
mod tests {
    use std::{error::Error as _, sync::Arc, time::Duration};

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
            Self::start_on("127.0.0.1:0", "localhost", "localhost", delay, status).await
        }

        async fn start_on(
            bind_address: &str,
            endpoint_host: &str,
            certificate_name: &str,
            delay: Duration,
            status: Option<Status>,
        ) -> Self {
            let CertifiedKey { cert, signing_key } =
                rcgen::generate_simple_self_signed(vec![certificate_name.into()]).unwrap();
            let certificate = cert.pem().into_bytes();
            let identity = Identity::from_pem(&certificate, signing_key.serialize_pem());
            let listener = tokio::net::TcpListener::bind(bind_address).await.unwrap();
            let endpoint = format!(
                "https://{endpoint_host}:{}",
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
        probe_with_client(client).await
    }

    async fn probe_with_client(client: LndClient) -> Result<String, LndError> {
        let mut harness = HarnessClient::new(client.inner.channel().await);
        let response = authenticated_request(&client.inner, "probe", ProbeRequest {}, |request| {
            harness.probe(request)
        })
        .await?;
        Ok(response.into_inner().message)
    }

    fn assert_source_chain_is_redacted(error: &LndError, forbidden: &[&str]) {
        let top_level = format!("{error} {error:?}");
        for marker in forbidden {
            assert!(!top_level.contains(marker));
        }

        let mut source = error.source();
        let mut depth = 0;
        while let Some(cause) = source {
            depth += 1;
            let message = cause.to_string();
            for marker in forbidden {
                assert!(!message.contains(marker));
            }
            source = cause.source();
        }
        assert!(depth > 0, "transport errors must retain a source chain");
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn authenticated_and_unauthenticated_clients_are_send_and_sync() {
        assert_send_sync::<LndClient>();
        assert_send_sync::<super::UnauthenticatedLndClient>();
    }

    #[test]
    fn unauthenticated_construction_does_not_require_a_tokio_runtime() {
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .unwrap()
            .cert
            .pem()
            .into_bytes();
        let config = LndConfig::new(
            "https://localhost:10009",
            certificate,
            MACAROON.to_vec(),
            Duration::from_secs(1),
        )
        .unwrap();

        let transport = unauthenticated(&config).unwrap();

        assert!(format!("{transport:?}").starts_with("UnauthenticatedLndClient"));
    }

    async fn probe_without_authentication(config: &LndConfig) -> Result<String, LndError> {
        let transport = unauthenticated(config)?;
        let mut harness = HarnessClient::new(transport.channel().await);
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
    async fn ipv4_ip_san_is_verified_without_dns_sni() {
        let server = TestServer::start_on(
            "127.0.0.1:0",
            "127.0.0.1",
            "127.0.0.1",
            Duration::ZERO,
            None,
        )
        .await;

        let response = probe_with_transport(server.config(Duration::from_secs(1)))
            .await
            .unwrap();

        assert_eq!(response, "ready");
    }

    #[tokio::test]
    async fn bracketed_ipv6_endpoint_verifies_an_ipv6_ip_san() {
        let server = TestServer::start_on("[::1]:0", "[::1]", "::1", Duration::ZERO, None).await;

        let response = probe_with_transport(server.config(Duration::from_secs(1)))
            .await
            .unwrap();

        assert_eq!(response, "ready");
    }

    #[tokio::test]
    async fn client_constructed_in_a_short_lived_runtime_works_in_another_runtime() {
        let server = TestServer::start(Duration::ZERO, None).await;
        let config = server.config(Duration::from_secs(1));
        let client = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move { LndClient::with_config(config).unwrap() })
        })
        .join()
        .unwrap();

        let response = probe_with_client(client).await.unwrap();

        assert_eq!(response, "ready");
    }

    #[tokio::test]
    async fn concurrent_first_calls_both_complete() {
        let server = TestServer::start(Duration::ZERO, None).await;
        let client = LndClient::with_config(server.config(Duration::from_secs(1))).unwrap();

        let (first, second) =
            tokio::join!(probe_with_client(client.clone()), probe_with_client(client));

        assert_eq!(first.unwrap(), "ready");
        assert_eq!(second.unwrap(), "ready");
    }

    #[test]
    fn invalid_pem_retains_a_redacted_transport_source_chain() {
        const PEM_MARKER: &str = "PEM-SOURCE-SECRET";
        const MACAROON_MARKER: &[u8] = b"macaroon-source-secret";
        const MACAROON_HEX: &str = "6d616361726f6f6e2d736f757263652d736563726574";
        let certificate =
            format!("-----BEGIN CERTIFICATE-----\n{PEM_MARKER}\n-----END CERTIFICATE-----\n");
        let config = LndConfig::new(
            "https://localhost:10009",
            certificate.as_bytes().to_vec(),
            MACAROON_MARKER.to_vec(),
            Duration::from_secs(1),
        )
        .unwrap();

        let error = LndClient::with_config(config).unwrap_err();

        assert_source_chain_is_redacted(&error, &[PEM_MARKER, &certificate, MACAROON_HEX]);
    }

    #[tokio::test]
    async fn untrusted_certificate_cannot_reach_the_service() {
        let server = TestServer::start(Duration::ZERO, None).await;
        let untrusted_certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .unwrap()
            .cert
            .pem()
            .into_bytes();
        let untrusted_pem = String::from_utf8(untrusted_certificate.clone()).unwrap();
        let config = LndConfig::new(
            &server.endpoint,
            untrusted_certificate,
            b"macaroon-source-secret".to_vec(),
            Duration::from_secs(1),
        )
        .unwrap();

        let error = probe_with_transport(config).await.unwrap_err();

        assert!(matches!(error, LndError::Transport { .. }));
        assert_source_chain_is_redacted(
            &error,
            &[
                &untrusted_pem,
                "macaroon-source-secret",
                "6d616361726f6f6e2d736f757263652d736563726574",
            ],
        );
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

    #[tokio::test]
    async fn unrepresentable_timeout_returns_an_error_without_panicking() {
        let task = tokio::spawn(async {
            bounded_request(Duration::MAX, "probe", async {
                Ok::<_, Status>(Response::new(()))
            })
            .await
        });

        let error = task
            .await
            .expect("deadline creation must not unwind")
            .unwrap_err();

        assert!(matches!(error, LndError::InvalidRequest { .. }));
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
