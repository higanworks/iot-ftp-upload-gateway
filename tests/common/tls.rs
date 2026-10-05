//! Test PKI and TLS helpers shared by the backend-FTPS integration tests.
#![allow(dead_code)]

use std::sync::Arc;

use iot_ftp_upload_gateway::backend::tls::BackendTlsConnector;
use iot_ftp_upload_gateway::config::{BackendTlsConfig, BackendTlsMaxVersion};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::TlsAcceptor;

pub struct TestPki {
    pub ca_pem: String,
    pub leaf_der: CertificateDer<'static>,
    pub leaf_key_der: Vec<u8>,
}

/// A fresh CA plus a leaf certificate for `leaf_name` signed by it.
pub fn test_pki(leaf_name: &str) -> TestPki {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();

    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec![leaf_name.to_string()])
        .unwrap()
        .signed_by(&leaf_key, &ca, &ca_key)
        .unwrap();

    TestPki {
        ca_pem: ca.pem(),
        leaf_der: leaf.der().clone(),
        leaf_key_der: leaf_key.serialize_der(),
    }
}

/// A gateway-side connector that trusts only `ca_pem`.
pub fn connector_trusting(
    ca_pem: &str,
    server_name: Option<&str>,
    max_version: BackendTlsMaxVersion,
    tag: &str,
) -> BackendTlsConnector {
    let path = std::env::temp_dir().join(format!(
        "iot-ftp-gw-test-ca-{}-{tag}.pem",
        std::process::id()
    ));
    std::fs::write(&path, ca_pem).unwrap();
    let connector = BackendTlsConnector::new(&BackendTlsConfig {
        ca_file: Some(path.clone()),
        server_name: server_name.map(str::to_string),
        max_version,
        ..BackendTlsConfig::default()
    })
    .unwrap();
    let _ = std::fs::remove_file(path);
    connector
}

/// A backend-side TLS acceptor serving `pki`'s leaf. One acceptor (one `ServerConfig`) should be
/// shared by a mock backend's control and data connections so its session cache can resume.
pub fn acceptor_for(pki: &TestPki) -> TlsAcceptor {
    let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![pki.leaf_der.clone()],
        PrivateKeyDer::from(PrivatePkcs8KeyDer::from(pki.leaf_key_der.clone())),
    )
    .unwrap();
    TlsAcceptor::from(Arc::new(server_config))
}
