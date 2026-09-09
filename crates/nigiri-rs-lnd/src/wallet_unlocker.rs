use std::{fmt, future::Future, time::Duration};

use tonic::{Request, Response, Status, transport::Channel as TonicChannel};
use url::Url;

use crate::{
    LndConfig, LndError, MAX_MACAROON_BYTES,
    proto::lnrpc::{
        GenSeedRequest, GenSeedResponse, InitWalletRequest, InitWalletResponse,
        wallet_unlocker_client::WalletUnlockerClient,
    },
    transport::{bounded_request_until, operation_deadline, unauthenticated},
};

const MIN_WALLET_PASSWORD_BYTES: usize = 8;
const MAX_WALLET_PASSWORD_BYTES: usize = 65_536;
const CIPHER_SEED_WORDS: usize = 24;
const MAX_CIPHER_SEED_WORD_BYTES: usize = 1_024;
const MAX_CIPHER_SEED_BYTES: usize = CIPHER_SEED_WORDS * MAX_CIPHER_SEED_WORD_BYTES;

/// Secure endpoint material used only while creating a new stateless LND wallet.
pub struct LndBootstrapConfig {
    pub endpoint: Url,
    pub tls_certificate: Vec<u8>,
    pub timeout: Duration,
}

// Certificate bodies are public material, but keeping them out of Debug prevents full PEM blobs
// from entering fixture diagnostics and keeps this type aligned with `LndConfig`.
impl fmt::Debug for LndBootstrapConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LndBootstrapConfig")
            .field("endpoint", &self.endpoint)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

pub(crate) trait WalletUnlockerRpc: Send {
    fn gen_seed(
        &mut self,
        request: Request<GenSeedRequest>,
    ) -> impl Future<Output = Result<Response<GenSeedResponse>, Status>> + Send;

    fn init_wallet(
        &mut self,
        request: Request<InitWalletRequest>,
    ) -> impl Future<Output = Result<Response<InitWalletResponse>, Status>> + Send;
}

impl WalletUnlockerRpc for WalletUnlockerClient<TonicChannel> {
    fn gen_seed(
        &mut self,
        request: Request<GenSeedRequest>,
    ) -> impl Future<Output = Result<Response<GenSeedResponse>, Status>> + Send {
        WalletUnlockerClient::gen_seed(self, request)
    }

    fn init_wallet(
        &mut self,
        request: Request<InitWalletRequest>,
    ) -> impl Future<Output = Result<Response<InitWalletResponse>, Status>> + Send {
        WalletUnlockerClient::init_wallet(self, request)
    }
}

/// Generates a fresh seed, commits a stateless wallet, and returns authenticated configuration.
///
/// The generated seed and caller-owned password never occur in the returned error. Once
/// `InitWallet` is sent, a missing response or unusable returned macaroon is reported as an
/// uncertain outcome: the daemon may already have committed the wallet and a blind retry with a
/// different generated seed would be unsafe.
pub async fn initialize_wallet(
    config: LndBootstrapConfig,
    wallet_password: &[u8],
) -> Result<LndConfig, LndError> {
    let validated = validated_transport_config(&config, wallet_password)?;
    let transport = unauthenticated(&validated)?;
    let mut rpc = WalletUnlockerClient::new(transport.channel().await);
    initialize_wallet_validated(validated, wallet_password, &mut rpc).await
}

#[cfg(test)]
pub(crate) async fn initialize_wallet_with<R: WalletUnlockerRpc>(
    config: LndBootstrapConfig,
    wallet_password: &[u8],
    rpc: &mut R,
) -> Result<LndConfig, LndError> {
    let validated = validated_transport_config(&config, wallet_password)?;
    initialize_wallet_validated(validated, wallet_password, rpc).await
}

async fn initialize_wallet_validated<R: WalletUnlockerRpc>(
    validated: crate::config::TlsConfig,
    wallet_password: &[u8],
    rpc: &mut R,
) -> Result<LndConfig, LndError> {
    let timeout = validated.timeout();
    let deadline = operation_deadline(timeout)?;

    let seed = bounded_request_until(
        deadline,
        timeout,
        "generate wallet seed",
        rpc.gen_seed(Request::new(GenSeedRequest {
            aezeed_passphrase: Vec::new(),
            seed_entropy: Vec::new(),
        })),
    )
    .await?
    .into_inner()
    .cipher_seed_mnemonic;
    validate_cipher_seed(&seed)?;

    let initialized = bounded_request_until(
        deadline,
        timeout,
        "initialize wallet",
        rpc.init_wallet(Request::new(InitWalletRequest {
            wallet_password: wallet_password.to_vec(),
            cipher_seed_mnemonic: seed,
            aezeed_passphrase: Vec::new(),
            recovery_window: 0,
            channel_backups: None,
            stateless_init: true,
            extended_master_key: String::new(),
            extended_master_key_birthday_timestamp: 0,
            watch_only: None,
            macaroon_root_key: Vec::new(),
        })),
    )
    .await
    .map_err(|_| uncertain_initialization())?
    .into_inner();

    if initialized.admin_macaroon.is_empty()
        || initialized.admin_macaroon.len() > MAX_MACAROON_BYTES
    {
        return Err(uncertain_initialization());
    }

    LndConfig::authenticated(validated, initialized.admin_macaroon)
}

fn validated_transport_config(
    config: &LndBootstrapConfig,
    wallet_password: &[u8],
) -> Result<crate::config::TlsConfig, LndError> {
    validate_wallet_password(wallet_password)?;
    crate::config::TlsConfig::from_url(
        &config.endpoint,
        config.tls_certificate.clone(),
        config.timeout,
    )
}

fn validate_wallet_password(wallet_password: &[u8]) -> Result<(), LndError> {
    if wallet_password.len() < MIN_WALLET_PASSWORD_BYTES {
        return Err(invalid("wallet password must contain at least 8 bytes"));
    }
    if wallet_password.len() > MAX_WALLET_PASSWORD_BYTES {
        return Err(invalid(
            "wallet password exceeds the private bootstrap byte limit",
        ));
    }
    Ok(())
}

fn validate_cipher_seed(words: &[String]) -> Result<(), LndError> {
    if words.len() != CIPHER_SEED_WORDS {
        return Err(invalid_response(
            "generated cipher seed must contain exactly 24 words",
        ));
    }

    let mut total = 0_usize;
    for word in words {
        if word.is_empty() || word.len() > MAX_CIPHER_SEED_WORD_BYTES {
            return Err(invalid_response("generated cipher seed word is invalid"));
        }
        total = total
            .checked_add(word.len())
            .ok_or_else(|| invalid_response("generated cipher seed size overflowed"))?;
        if total > MAX_CIPHER_SEED_BYTES {
            return Err(invalid_response(
                "generated cipher seed exceeds its byte limit",
            ));
        }
    }
    Ok(())
}

fn invalid(detail: &'static str) -> LndError {
    LndError::InvalidRequest {
        detail: detail.into(),
    }
}

fn invalid_response(detail: &'static str) -> LndError {
    LndError::InvalidResponse {
        operation: "generate wallet seed".into(),
        detail: detail.into(),
        identifier: None,
    }
}

fn uncertain_initialization() -> LndError {
    LndError::OutcomeUnknown {
        operation: "initialize wallet".into(),
        identifier: None,
    }
}

#[cfg(test)]
mod tests {
    use std::{future::Future, time::Duration};

    use tonic::{Request, Response, Status};
    use url::Url;

    use crate::{
        LndBootstrapConfig, LndError,
        proto::lnrpc::{GenSeedRequest, GenSeedResponse, InitWalletRequest, InitWalletResponse},
    };

    use super::{WalletUnlockerRpc, initialize_wallet_with};

    const PASSWORD: &[u8] = b"wallet-password-that-must-stay-secret";
    const MNEMONIC: [&str; 24] = [
        "ability", "absent", "absorb", "abstract", "absurd", "abuse", "access", "accident",
        "account", "accuse", "achieve", "acid", "acoustic", "acquire", "across", "act", "action",
        "actor", "actress", "actual", "adapt", "add", "addict", "address",
    ];
    const ADMIN_MACAROON: &[u8] = b"binary-admin-macaroon";

    enum Call {
        GenSeed(GenSeedRequest),
        InitWallet(Box<InitWalletRequest>),
    }

    struct FakeWalletUnlockerRpc {
        calls: Vec<Call>,
        seed: Result<GenSeedResponse, Status>,
        initialized: Result<InitWalletResponse, Status>,
    }

    impl FakeWalletUnlockerRpc {
        fn succeeding() -> Self {
            Self {
                calls: Vec::new(),
                seed: Ok(GenSeedResponse {
                    cipher_seed_mnemonic: MNEMONIC.iter().map(ToString::to_string).collect(),
                    enciphered_seed: Vec::new(),
                }),
                initialized: Ok(InitWalletResponse {
                    admin_macaroon: ADMIN_MACAROON.to_vec(),
                }),
            }
        }
    }

    impl WalletUnlockerRpc for FakeWalletUnlockerRpc {
        fn gen_seed(
            &mut self,
            request: Request<GenSeedRequest>,
        ) -> impl Future<Output = Result<Response<GenSeedResponse>, Status>> + Send {
            self.calls.push(Call::GenSeed(request.into_inner()));
            let response = self.seed.clone();
            async move { response.map(Response::new) }
        }

        fn init_wallet(
            &mut self,
            request: Request<InitWalletRequest>,
        ) -> impl Future<Output = Result<Response<InitWalletResponse>, Status>> + Send {
            self.calls
                .push(Call::InitWallet(Box::new(request.into_inner())));
            let response = self.initialized.clone();
            async move { response.map(Response::new) }
        }
    }

    fn config() -> LndBootstrapConfig {
        LndBootstrapConfig {
            endpoint: Url::parse("https://127.0.0.1:10009").unwrap(),
            tls_certificate: b"fixture TLS certificate".to_vec(),
            timeout: Duration::from_secs(17),
        }
    }

    #[tokio::test]
    async fn bootstrap_accepts_normalized_https_default_port() {
        let mut config = config();
        config.endpoint = Url::parse("https://localhost:443").unwrap();
        let mut rpc = FakeWalletUnlockerRpc::succeeding();
        let authenticated = initialize_wallet_with(config, PASSWORD, &mut rpc)
            .await
            .unwrap();
        assert_eq!(authenticated.endpoint().port_or_known_default(), Some(443));
        assert_eq!(rpc.calls.len(), 2);
    }

    #[test]
    fn bootstrap_debug_omits_the_certificate_body() {
        let rendered = format!("{:?}", config());

        assert!(rendered.contains("endpoint"));
        assert!(rendered.contains("10009"));
        assert!(rendered.contains("17s"));
        assert!(!rendered.contains("fixture TLS certificate"));
    }

    // Catches a regression that initializes from an unrelated seed, persists the only macaroon to
    // the container, or builds the returned authenticated configuration with different transport
    // inputs from the bootstrap call.
    #[tokio::test]
    async fn generates_a_seed_then_performs_stateless_initialization() {
        let mut rpc = FakeWalletUnlockerRpc::succeeding();

        let initialized = initialize_wallet_with(config(), PASSWORD, &mut rpc)
            .await
            .expect("a complete WalletUnlocker exchange must return authenticated configuration");

        assert_eq!(rpc.calls.len(), 2);
        let Call::GenSeed(request) = &rpc.calls[0] else {
            panic!("GenSeed must be the first call");
        };
        assert!(request.aezeed_passphrase.is_empty());
        assert!(request.seed_entropy.is_empty());
        let Call::InitWallet(request) = &rpc.calls[1] else {
            panic!("InitWallet must follow GenSeed");
        };
        assert_eq!(request.wallet_password, PASSWORD);
        assert_eq!(
            request.cipher_seed_mnemonic,
            MNEMONIC.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
        assert!(request.stateless_init);
        assert!(request.aezeed_passphrase.is_empty());
        assert_eq!(request.recovery_window, 0);
        assert!(request.channel_backups.is_none());
        assert!(request.extended_master_key.is_empty());
        assert_eq!(request.extended_master_key_birthday_timestamp, 0);
        assert!(request.watch_only.is_none());
        assert!(request.macaroon_root_key.is_empty());
        assert_eq!(initialized.endpoint().as_str(), "https://127.0.0.1:10009/");
        assert_eq!(initialized.certificate(), b"fixture TLS certificate");
        assert_eq!(initialized.macaroon(), ADMIN_MACAROON);
        assert_eq!(initialized.timeout(), Duration::from_secs(17));
    }

    // Catches a regression that sends a password LND will reject, or lets rejected secret bytes
    // escape through either Display or Debug.
    #[tokio::test]
    async fn password_bounds_are_checked_before_either_rpc_without_rendering_secrets() {
        for password in [b"short".as_slice(), vec![b'p'; 65_537].as_slice()] {
            let mut rpc = FakeWalletUnlockerRpc::succeeding();

            let error = initialize_wallet_with(config(), password, &mut rpc)
                .await
                .expect_err("an out-of-bounds password must be rejected locally");

            assert!(matches!(error, LndError::InvalidRequest { .. }));
            assert!(rpc.calls.is_empty());
            let rendered_password = String::from_utf8_lossy(password);
            assert!(!error.to_string().contains(rendered_password.as_ref()));
            assert!(!format!("{error:?}").contains(rendered_password.as_ref()));
        }
    }

    // Catches a regression that forwards an empty or oversized daemon seed into the committing
    // call, or repeats mnemonic material in its error.
    #[tokio::test]
    async fn malformed_seed_material_is_rejected_before_init_wallet_and_remains_secret() {
        for mnemonic in [
            Vec::new(),
            vec![String::new(); 24],
            vec!["seed-secret".repeat(100); 24],
        ] {
            let mut rpc = FakeWalletUnlockerRpc::succeeding();
            rpc.seed = Ok(GenSeedResponse {
                cipher_seed_mnemonic: mnemonic.clone(),
                enciphered_seed: Vec::new(),
            });

            let error = initialize_wallet_with(config(), PASSWORD, &mut rpc)
                .await
                .expect_err("malformed cipher-seed words must not reach InitWallet");

            assert!(matches!(error, LndError::InvalidResponse { .. }));
            assert_eq!(rpc.calls.len(), 1);
            for word in mnemonic.iter().filter(|word| !word.is_empty()) {
                assert!(!error.to_string().contains(word));
                assert!(!format!("{error:?}").contains(word));
            }
        }
    }

    // Catches a regression that treats a missing stateless macaroon as a safe retry even though
    // InitWallet may already have committed the generated seed.
    #[tokio::test]
    async fn missing_or_oversized_admin_macaroon_reports_an_uncertain_committed_outcome() {
        for macaroon in [Vec::new(), vec![0xab; 65_537]] {
            let mut rpc = FakeWalletUnlockerRpc::succeeding();
            rpc.initialized = Ok(InitWalletResponse {
                admin_macaroon: macaroon,
            });

            let error = initialize_wallet_with(config(), PASSWORD, &mut rpc)
                .await
                .expect_err("an unusable stateless macaroon must not produce a client config");

            assert!(matches!(error, LndError::OutcomeUnknown { .. }));
            assert_eq!(rpc.calls.len(), 2);
        }
    }

    // Catches a regression that reports a failed InitWallet RPC as definitively uncommitted and
    // encourages callers to retry with a different generated seed.
    #[tokio::test]
    async fn init_wallet_status_is_reported_as_an_uncertain_committed_outcome() {
        let mut rpc = FakeWalletUnlockerRpc::succeeding();
        rpc.initialized = Err(Status::unavailable(format!(
            "lost reply after committing {} {}",
            String::from_utf8_lossy(PASSWORD),
            MNEMONIC.join(" ")
        )));

        let error = initialize_wallet_with(config(), PASSWORD, &mut rpc)
            .await
            .expect_err("the caller cannot know whether InitWallet committed");

        assert!(matches!(error, LndError::OutcomeUnknown { .. }));
        assert!(!error.to_string().contains("wallet-password"));
        assert!(!error.to_string().contains(MNEMONIC[0]));
        assert!(!format!("{error:?}").contains("wallet-password"));
        assert!(!format!("{error:?}").contains(MNEMONIC[0]));
    }
}
