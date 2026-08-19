use std::{
    error::Error,
    fs,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bitcoin::secp256k1::PublicKey;
use nigiri_rs_lnd::{
    CreateInvoiceRequest, LndConfig, LndError, MAX_MACAROON_BYTES, MAX_TLS_CERTIFICATE_BYTES,
    Millisats, OpenChannelRequest, PaymentOptions, PeerAddress, Sats,
};

fn public_key() -> PublicKey {
    "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
        .parse()
        .unwrap()
}

#[test]
fn amount_conversion_is_checked() {
    assert_eq!(Millisats::try_from(Sats::new(21)).unwrap().as_u64(), 21_000);
    assert!(Millisats::try_from(Sats::new(u64::MAX)).is_err());
    assert!(Sats::try_from(Millisats::new(21_999)).is_err());
}

#[test]
fn configuration_rejects_unsafe_endpoints_and_hides_credentials() {
    let cert = b"certificate-marker".to_vec();
    let macaroon = b"macaroon-marker".to_vec();
    assert!(
        LndConfig::new(
            "http://localhost:10009",
            cert.clone(),
            macaroon.clone(),
            Duration::from_secs(1),
        )
        .is_err()
    );
    let config = LndConfig::new(
        "https://localhost:10009",
        cert,
        macaroon,
        Duration::from_secs(1),
    )
    .unwrap();
    let debug = format!("{config:?}");
    assert!(!debug.contains("certificate-marker"));
    assert!(!debug.contains("macaroon-marker"));

    let rejected = LndConfig::new(
        "https://localhost:10009?macaroon-marker",
        vec![1],
        vec![2],
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(!format!("{rejected:?}").contains("macaroon-marker"));
    assert!(Error::source(&rejected).is_none());
}

#[test]
fn configuration_applies_credential_limits_and_validates_all_endpoint_parts() {
    let certificate = vec![1; MAX_TLS_CERTIFICATE_BYTES + 1];
    let macaroon = vec![2; MAX_MACAROON_BYTES + 1];

    assert!(
        LndConfig::new(
            "https://localhost:10009",
            certificate,
            vec![1],
            Duration::from_secs(1),
        )
        .is_err()
    );

    assert!(
        LndConfig::new(
            "https://localhost:10009",
            vec![1],
            macaroon,
            Duration::from_secs(1),
        )
        .is_err()
    );

    for endpoint in [
        "https://localhost",
        "https://user:password@localhost:10009",
        "https://localhost:10009?token=credential-marker",
        "https://localhost:10009#certificate-marker",
    ] {
        assert!(LndConfig::new(endpoint, vec![1], vec![2], Duration::from_secs(1)).is_err());
    }
    assert!(
        LndConfig::new(
            "https://localhost:10009",
            vec![],
            vec![1],
            Duration::from_secs(1),
        )
        .is_err()
    );
    assert!(LndConfig::new("https://localhost:10009", vec![1], vec![], Duration::ZERO,).is_err());
}

#[test]
fn configuration_accepts_an_explicit_default_https_port_but_rejects_an_omitted_port() {
    let localhost = LndConfig::new(
        "https://localhost:443",
        vec![1],
        vec![2],
        Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(localhost.endpoint().as_str(), "https://localhost/");
    assert!(
        LndConfig::new(
            "https://[::1]:443",
            vec![1],
            vec![2],
            Duration::from_secs(1),
        )
        .is_ok()
    );
    assert!(
        LndConfig::new(
            "https://localhost",
            vec![1],
            vec![2],
            Duration::from_secs(1),
        )
        .is_err()
    );
    assert!(LndConfig::new("https://[::1]", vec![1], vec![2], Duration::from_secs(1),).is_err());
}

#[test]
fn configuration_rejects_zero_ports_for_every_supported_host_form() {
    for endpoint in [
        "https://localhost:0",
        "https://127.0.0.1:0",
        "https://[::1]:0",
    ] {
        let error = LndConfig::new(endpoint, vec![1], vec![2], Duration::from_secs(1))
            .expect_err("port zero cannot identify a usable LND endpoint");

        assert!(matches!(error, LndError::InvalidRequest { .. }));
    }
}

#[test]
fn configuration_rejects_empty_userinfo_delimiters() {
    assert!(
        LndConfig::new(
            "https://@localhost:10009",
            vec![1],
            vec![2],
            Duration::from_secs(1),
        )
        .is_err()
    );
}

#[test]
fn configuration_does_not_treat_an_at_sign_in_the_path_as_userinfo() {
    assert!(
        LndConfig::new(
            "https://localhost:10009/path@segment",
            vec![1],
            vec![2],
            Duration::from_secs(1),
        )
        .is_ok()
    );
}

#[test]
fn request_values_reject_invalid_inputs() {
    assert!(PeerAddress::new(public_key(), "   ", 9735).is_err());
    assert!(PeerAddress::new(public_key(), "localhost", 0).is_err());
    assert!(OpenChannelRequest::new(public_key(), Sats::new(0), Sats::new(0)).is_err());
    assert!(OpenChannelRequest::new(public_key(), Sats::new(10), Sats::new(10)).is_err());
    assert!(CreateInvoiceRequest::new(Millisats::new(0), "memo", Duration::from_secs(1)).is_err());
    assert!(CreateInvoiceRequest::new(Millisats::new(1), "memo", Duration::ZERO).is_err());
    assert!(PaymentOptions::new(Millisats::new(1), Duration::ZERO).is_err());
}

#[tokio::test]
async fn file_configuration_enforces_the_same_bounds_and_retains_only_file_error_sources() {
    let stem = format!(
        "nigiri-rs-lnd-foundation-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    );
    let directory = std::env::temp_dir().join(stem);
    fs::create_dir(&directory).unwrap();
    let certificate = directory.join("tls.cert");
    let macaroon = directory.join("admin.macaroon");
    fs::write(&certificate, [1]).unwrap();
    fs::write(&macaroon, [2]).unwrap();

    let config = LndConfig::from_files(
        "https://localhost:10009",
        &certificate,
        &macaroon,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(config.endpoint().as_str(), "https://localhost:10009/");

    fs::write(&certificate, vec![0; MAX_TLS_CERTIFICATE_BYTES + 1]).unwrap();
    assert!(
        LndConfig::from_files(
            "https://localhost:10009",
            &certificate,
            &macaroon,
            Duration::from_secs(1),
        )
        .await
        .is_err()
    );

    fs::write(&certificate, [1]).unwrap();
    fs::write(&macaroon, vec![0; MAX_MACAROON_BYTES + 1]).unwrap();
    assert!(
        LndConfig::from_files(
            "https://localhost:10009",
            &certificate,
            &macaroon,
            Duration::from_secs(1),
        )
        .await
        .is_err()
    );

    let missing = LndConfig::from_files(
        "https://localhost:10009",
        directory.join("missing.cert"),
        &macaroon,
        Duration::from_secs(1),
    )
    .await
    .unwrap_err();
    assert!(Error::source(&missing).is_some());
    assert!(!format!("{missing:?}").contains("macaroon-marker"));

    fs::remove_dir_all(directory).unwrap();
}
