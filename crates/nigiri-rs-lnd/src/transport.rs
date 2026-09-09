use std::{borrow::Cow, error::Error as StdError, fmt, future::Future, sync::Arc, time::Duration};

use tokio::sync::OnceCell;
use tonic::{
    Code, Request, Response, Status,
    metadata::AsciiMetadataValue,
    transport::{Channel, ClientTlsConfig, Endpoint},
};
use url::Host;

use rustls::{
    CertificateError, DigitallySignedStruct, Error as RustlsError, SignatureScheme,
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        verify_server_name,
    },
    crypto::WebPkiSupportedAlgorithms,
    pki_types::{CertificateDer, ServerName, UnixTime, pem::PemObject},
    server::ParsedCertificate,
};

use crate::{LndConfig, LndError, error::bounded};

pub(crate) struct ClientInner {
    transport: LazyChannel,
    macaroon: AsciiMetadataValue,
    pub(crate) timeout: Duration,
}

impl ClientInner {
    pub(crate) fn authenticated(config: &LndConfig) -> Result<Arc<Self>, LndError> {
        let transport = LazyChannel::new(tls_endpoint(config.tls())?);
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

pub(crate) struct UnauthenticatedLndClient {
    transport: LazyChannel,
    pub(crate) timeout: Duration,
}

impl UnauthenticatedLndClient {
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

pub(crate) fn unauthenticated(
    config: &crate::config::TlsConfig,
) -> Result<UnauthenticatedLndClient, LndError> {
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

pub(crate) async fn authenticated_mutating_request<
    RequestMessage,
    ResponseMessage,
    Call,
    CallFuture,
>(
    client: &ClientInner,
    operation: &'static str,
    identifier: Option<String>,
    message: RequestMessage,
    call: Call,
) -> Result<Response<ResponseMessage>, LndError>
where
    Call: FnOnce(Request<RequestMessage>) -> CallFuture,
    CallFuture: Future<Output = Result<Response<ResponseMessage>, Status>>,
{
    let deadline = operation_deadline(client.timeout)?;
    authenticated_mutating_request_until(client, deadline, operation, identifier, message, call)
        .await
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

pub(crate) async fn authenticated_mutating_request_until<
    RequestMessage,
    ResponseMessage,
    Call,
    CallFuture,
>(
    client: &ClientInner,
    deadline: tokio::time::Instant,
    operation: &'static str,
    identifier: Option<String>,
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
    bounded_mutating_request_until(deadline, operation, identifier, call(request)).await
}

#[cfg(test)]
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

pub(crate) async fn bounded_mutating_request_until<ResponseMessage, CallFuture>(
    deadline: tokio::time::Instant,
    operation: &'static str,
    identifier: Option<String>,
    call: CallFuture,
) -> Result<Response<ResponseMessage>, LndError>
where
    CallFuture: Future<Output = Result<Response<ResponseMessage>, Status>>,
{
    match tokio::time::timeout_at(deadline, call).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(status)) if mutating_status_is_ambiguous(&status) => {
            Err(unknown_mutation_outcome(operation, identifier))
        }
        Ok(Err(status)) => Err(map_status(operation, status)),
        Err(_) => Err(unknown_mutation_outcome(operation, identifier)),
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
            code: status_code(code),
            operation,
            detail: bounded(format!("gRPC status {code}")),
        },
    }
}

#[cfg(test)]
#[test]
fn public_status_code_does_not_depend_on_daemon_diagnostics() {
    let error = map_status(
        "get info",
        Status::unavailable("arbitrary secret daemon detail"),
    );
    assert!(matches!(
        error,
        LndError::Status {
            code: crate::LndStatusCode::Unavailable,
            ..
        }
    ));
    assert!(!error.to_string().contains("secret"));
}

fn status_code(code: Code) -> crate::LndStatusCode {
    match code {
        Code::Ok => crate::LndStatusCode::Ok,
        Code::Cancelled => crate::LndStatusCode::Cancelled,
        Code::Unknown => crate::LndStatusCode::Unknown,
        Code::InvalidArgument => crate::LndStatusCode::InvalidArgument,
        Code::DeadlineExceeded => crate::LndStatusCode::DeadlineExceeded,
        Code::NotFound => crate::LndStatusCode::NotFound,
        Code::AlreadyExists => crate::LndStatusCode::AlreadyExists,
        Code::PermissionDenied => crate::LndStatusCode::PermissionDenied,
        Code::ResourceExhausted => crate::LndStatusCode::ResourceExhausted,
        Code::FailedPrecondition => crate::LndStatusCode::FailedPrecondition,
        Code::Aborted => crate::LndStatusCode::Aborted,
        Code::OutOfRange => crate::LndStatusCode::OutOfRange,
        Code::Unimplemented => crate::LndStatusCode::Unimplemented,
        Code::Internal => crate::LndStatusCode::Internal,
        Code::Unavailable => crate::LndStatusCode::Unavailable,
        Code::DataLoss => crate::LndStatusCode::DataLoss,
        Code::Unauthenticated => crate::LndStatusCode::Unauthenticated,
    }
}

fn unknown_mutation_outcome(operation: &'static str, identifier: Option<String>) -> LndError {
    LndError::OutcomeUnknown {
        operation: Cow::Borrowed(operation),
        identifier,
    }
}

/// This classifier is used only after dispatching a mutation. A local timeout or transport loss may
/// happen after LND accepted it but before the client observed a response. Tonic also maps
/// response-body decoder failures and HTTP/2 resets to `Internal` or `ResourceExhausted`, sometimes
/// without a `tonic::transport::Error` source. Those codes are therefore uncertain at this boundary
/// regardless of message text. Authentication, validation, and precondition codes remain definitive
/// so callers do not retry a known rejection as an unknown commit.
fn mutating_status_is_ambiguous(status: &Status) -> bool {
    has_transport_source(status)
        || matches!(
            status.code(),
            Code::Cancelled
                | Code::Unknown
                | Code::DeadlineExceeded
                | Code::ResourceExhausted
                | Code::Internal
                | Code::Unavailable
        )
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

fn tls_endpoint(config: &crate::config::TlsConfig) -> Result<Endpoint, LndError> {
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
    let verifier = Arc::new(PinnedServerCertificate::new(config.certificate())?);
    let tls = ClientTlsConfig::new().domain_name(domain);
    endpoint
        .tls_config_with_verifier(tls, verifier)
        .map_err(invalid_transport_configuration)
}

/// LND presents the same self-signed, CA=true certificate that it writes to `tls.cert`. Treat the
/// caller-provided certificate as that one exact server identity instead of reinterpreting it as a
/// general-purpose issuing CA. CertificateVerify signatures still use rustls' ring algorithms.
struct PinnedServerCertificate {
    identity: CertificateDer<'static>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl PinnedServerCertificate {
    fn new(pem: &[u8]) -> Result<Self, LndError> {
        let identity = parse_single_certificate(pem).map_err(invalid_transport_configuration)?;
        ParsedCertificate::try_from(&identity).map_err(invalid_transport_configuration)?;
        let algorithms = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        Ok(Self {
            identity,
            algorithms,
        })
    }

    fn verify_identity(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
    ) -> Result<(), RustlsError> {
        if intermediates.is_empty() && end_entity.as_ref() == self.identity.as_ref() {
            Ok(())
        } else {
            Err(CertificateError::UnknownIssuer.into())
        }
    }
}

impl fmt::Debug for PinnedServerCertificate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PinnedServerCertificate")
            .finish_non_exhaustive()
    }
}

impl ServerCertVerifier for PinnedServerCertificate {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        self.verify_identity(end_entity, intermediates)?;
        let parsed = ParsedCertificate::try_from(end_entity)?;
        verify_server_name(&parsed, server_name)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.verify_identity(certificate, &[])?;
        rustls::crypto::verify_tls12_signature(message, certificate, signature, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.verify_identity(certificate, &[])?;
        rustls::crypto::verify_tls13_signature(message, certificate, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

fn parse_single_certificate(pem: &[u8]) -> Result<CertificateDer<'static>, std::io::Error> {
    const BEGIN: &[u8] = b"-----BEGIN CERTIFICATE-----";
    const END: &[u8] = b"-----END CERTIFICATE-----";

    let pem = pem.trim_ascii();
    let Some(body) = pem.strip_prefix(BEGIN) else {
        return Err(invalid_certificate_pin());
    };
    let Some(end_offset) = body.windows(END.len()).position(|window| window == END) else {
        return Err(invalid_certificate_pin());
    };
    let trailing = &body[end_offset + END.len()..];
    if !trailing.trim_ascii().is_empty() {
        return Err(invalid_certificate_pin());
    }

    CertificateDer::from_pem_slice(pem).map_err(|_| invalid_certificate_pin())
}

fn invalid_certificate_pin() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "TLS certificate must contain exactly one PEM certificate",
    )
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
    use std::{
        error::Error as _,
        sync::{Arc, Once},
        time::Duration,
    };

    use rcgen::{BasicConstraints, CertificateParams, CertifiedKey, IsCa, KeyPair};
    use rustls::{
        CertificateError, Error as RustlsError,
        pki_types::{CertificateDer, pem::PemObject},
    };
    use tokio::sync::{Mutex, oneshot};
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::{
        Request, Response, Status,
        transport::{Identity, Server, ServerTlsConfig},
    };

    use super::{
        PinnedServerCertificate, authenticated_request, bounded_request, map_status,
        unauthenticated,
    };
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
            install_test_crypto_provider();
            let CertifiedKey { cert, signing_key } =
                rcgen::generate_simple_self_signed(vec![certificate_name.into()]).unwrap();
            Self::start_with_identity(
                bind_address,
                endpoint_host,
                cert.pem().into_bytes(),
                signing_key.serialize_pem(),
                delay,
                status,
            )
            .await
        }

        async fn start_with_ca_certificate() -> Self {
            install_test_crypto_provider();
            let mut params = CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            let signing_key = KeyPair::generate().unwrap();
            let certificate = params.self_signed(&signing_key).unwrap();
            Self::start_with_identity(
                "127.0.0.1:0",
                "localhost",
                certificate.pem().into_bytes(),
                signing_key.serialize_pem(),
                Duration::ZERO,
                None,
            )
            .await
        }

        async fn start_with_identity(
            bind_address: &str,
            endpoint_host: &str,
            certificate: Vec<u8>,
            private_key: String,
            delay: Duration,
            status: Option<Status>,
        ) -> Self {
            let identity = Identity::from_pem(&certificate, private_key);
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

    // Tonic's client path selects its compiled provider explicitly, but its test-server path uses
    // rustls' process default. A full workspace build also enables reqwest's AWS-LC provider, so
    // rustls cannot infer a default from features alone. Keep this selection inside the test
    // harness: production clients remain compatible with an embedding application's provider.
    fn install_test_crypto_provider() {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
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

    // Catches the workspace feature-unification case where both rustls providers are compiled and
    // tonic's server builder would otherwise panic while trying to infer one.
    #[test]
    fn tls_test_provider_selection_is_idempotent() {
        install_test_crypto_provider();
        install_test_crypto_provider();
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

        let transport = unauthenticated(config.tls()).unwrap();

        assert!(format!("{transport:?}").starts_with("UnauthenticatedLndClient"));
    }

    async fn probe_without_authentication(config: &LndConfig) -> Result<String, LndError> {
        let transport = unauthenticated(config.tls())?;
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

    // LND generates one self-signed certificate with CA=true and presents that same certificate
    // as the server identity. WebPKI correctly refuses to reinterpret it as an ordinary leaf, but
    // the LND client contract is an exact certificate pin: the configured DER must be precisely
    // the identity the server presents. A different pin must still fail before application data.
    #[tokio::test]
    async fn exact_pin_accepts_lnd_style_ca_identity_and_rejects_a_different_certificate() {
        let server = TestServer::start_with_ca_certificate().await;

        let response = probe_with_transport(server.config(Duration::from_secs(1)))
            .await
            .expect("the exact configured LND certificate must authenticate its server identity");
        assert_eq!(response, "ready");

        let different_certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .unwrap()
            .cert
            .pem()
            .into_bytes();
        let wrong_pin = LndConfig::new(
            &server.endpoint,
            different_certificate,
            MACAROON.to_vec(),
            Duration::from_secs(1),
        )
        .unwrap();
        let error = probe_with_transport(wrong_pin)
            .await
            .expect_err("a server certificate different from the configured pin must be rejected");
        assert!(matches!(error, LndError::Transport { .. }));
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

    #[test]
    fn certificate_pin_rejects_multiple_pem_identities() {
        let first = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .unwrap()
            .cert
            .pem();
        let second = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .unwrap()
            .cert
            .pem();
        let certificate = format!("{first}{second}");
        let config = LndConfig::new(
            "https://localhost:10009",
            certificate.as_bytes().to_vec(),
            MACAROON.to_vec(),
            Duration::from_secs(1),
        )
        .unwrap();

        let error = LndClient::with_config(config)
            .expect_err("an exact server identity pin must contain one certificate");

        assert!(matches!(error, LndError::Transport { .. }));
        assert_source_chain_is_redacted(&error, &[&first, &second, &certificate]);
    }

    #[test]
    fn exact_pin_rejects_a_presented_certificate_chain() {
        let identity = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .unwrap()
            .cert
            .pem();
        let intermediate = rcgen::generate_simple_self_signed(vec!["intermediate".into()])
            .unwrap()
            .cert
            .pem();
        let verifier = PinnedServerCertificate::new(identity.as_bytes()).unwrap();
        let end_entity = CertificateDer::from_pem_slice(identity.as_bytes()).unwrap();
        let intermediates = [CertificateDer::from_pem_slice(intermediate.as_bytes()).unwrap()];

        let error = verifier
            .verify_identity(&end_entity, &intermediates)
            .expect_err("an exact identity pin must reject unexpected intermediates");

        assert!(matches!(
            error,
            RustlsError::InvalidCertificate(CertificateError::UnknownIssuer)
        ));
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
