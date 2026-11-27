use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use rustls::{
    CertificateError, DigitallySignedStruct, Error, RootCertStore, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    pki_types::{CertificateDer, ServerName, UnixTime, pem::PemObject},
};
use tonic::transport::{ClientTlsConfig, Endpoint};

#[allow(
    clippy::disallowed_types,
    reason = "immutable verifier shared only by TLS control connections"
)]
#[derive(Debug)]
struct PinnedLeaf {
    webpki: Arc<WebPkiServerVerifier>,
    expected_leaf: Box<[u8]>,
}

impl ServerCertVerifier for PinnedLeaf {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, Error> {
        if end_entity.as_ref() != self.expected_leaf.as_ref() {
            return Err(Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ));
        }
        self.webpki
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        self.webpki.verify_tls12_signature(message, cert, signature)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        self.webpki.verify_tls13_signature(message, cert, signature)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}

#[allow(
    clippy::disallowed_types,
    reason = "rustls and tonic own TLS control verifiers through Arc"
)]
pub(super) fn configure(
    endpoint: Endpoint,
    tls: ClientTlsConfig,
    root_pem: &[u8],
    expected_leaf: Box<[u8]>,
) -> Result<Endpoint> {
    ensure!(
        !expected_leaf.is_empty(),
        "neighbor server certificate pin is empty"
    );
    let mut roots = RootCertStore::empty();
    for certificate in CertificateDer::pem_slice_iter(root_pem) {
        roots.add(certificate.context("reading regional server CA certificate")?)?;
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
        .build()
        .context("configuring regional server certificate validation")?;
    Ok(endpoint.tls_config_with_verifier(
        tls,
        Arc::new(PinnedLeaf {
            webpki,
            expected_leaf,
        }),
    )?)
}
