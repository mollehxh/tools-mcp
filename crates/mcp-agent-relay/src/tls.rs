//! Mandatory mutual-TLS admission for the dedicated relay listener.

use crate::AuthenticatedPeer;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, RootCertStore, SignatureScheme,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use sha2::{Digest, Sha256};
use std::fmt::{Debug, Formatter, Write as _};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

/// Resolves an enrolled, non-revoked certificate fingerprint to relay authority.
pub trait PeerResolver: Send + Sync {
    fn resolve(&self, certificate_fingerprint: &str) -> Option<AuthenticatedPeer>;
}

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("relay TLS trust store is empty or invalid")]
    InvalidTrustStore(#[source] rustls::Error),
    #[error("relay TLS client verifier is invalid")]
    InvalidClientVerifier(#[source] rustls::server::VerifierBuilderError),
    #[error("relay TLS server identity is invalid")]
    InvalidServerIdentity(#[source] rustls::Error),
    #[error("relay mutual-TLS handshake failed")]
    Handshake(#[source] std::io::Error),
    #[error("relay client certificate is missing")]
    MissingClientCertificate,
    #[error("relay client certificate is unknown or revoked")]
    UnknownOrRevokedPeer,
}

/// TLS acceptor that requires a certificate issued by the configured device CA.
pub struct MtlsAcceptor<R> {
    acceptor: TlsAcceptor,
    resolver: Arc<R>,
}

struct EnrolledClientVerifier<R> {
    certificate_verifier: Arc<dyn ClientCertVerifier>,
    resolver: Arc<R>,
}

impl<R> Debug for EnrolledClientVerifier<R> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EnrolledClientVerifier")
            .finish_non_exhaustive()
    }
}

impl<R: PeerResolver> ClientCertVerifier for EnrolledClientVerifier<R> {
    fn offer_client_auth(&self) -> bool {
        self.certificate_verifier.offer_client_auth()
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.certificate_verifier.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let verified =
            self.certificate_verifier
                .verify_client_cert(end_entity, intermediates, now)?;
        let fingerprint = certificate_fingerprint(end_entity);
        if self
            .resolver
            .resolve(&fingerprint)
            .is_some_and(|peer| peer.certificate_fingerprint == fingerprint)
        {
            Ok(verified)
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.certificate_verifier
            .verify_tls12_signature(message, certificate, signature)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.certificate_verifier
            .verify_tls13_signature(message, certificate, signature)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.certificate_verifier.supported_verify_schemes()
    }
}

impl<R: PeerResolver + 'static> MtlsAcceptor<R> {
    /// Builds a mandatory-client-auth TLS acceptor from DER material.
    ///
    /// # Errors
    ///
    /// Returns an error when the CA or server certificate/key cannot form a safe config.
    pub fn new(
        device_ca: CertificateDer<'static>,
        server_chain: Vec<CertificateDer<'static>>,
        server_key: PrivateKeyDer<'static>,
        resolver: Arc<R>,
    ) -> Result<Self, TlsError> {
        let mut roots = RootCertStore::empty();
        roots.add(device_ca).map_err(TlsError::InvalidTrustStore)?;
        let certificate_verifier = WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(TlsError::InvalidClientVerifier)?;
        let verifier = Arc::new(EnrolledClientVerifier {
            certificate_verifier,
            resolver: Arc::clone(&resolver),
        });
        let config = rustls::ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(server_chain, server_key)
            .map_err(TlsError::InvalidServerIdentity)?;
        Ok(Self {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            resolver,
        })
    }

    /// Performs mTLS and resolves the leaf certificate to active device authority.
    ///
    /// # Errors
    ///
    /// Missing, untrusted, malformed, unknown, or revoked device certificates fail closed.
    pub async fn accept(
        &self,
        stream: TcpStream,
    ) -> Result<(TlsStream<TcpStream>, AuthenticatedPeer), TlsError> {
        let stream = self
            .acceptor
            .accept(stream)
            .await
            .map_err(TlsError::Handshake)?;
        let leaf = stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|certificates| certificates.first())
            .ok_or(TlsError::MissingClientCertificate)?;
        let fingerprint = hex_sha256(leaf.as_ref());
        let peer = self
            .resolver
            .resolve(&fingerprint)
            .filter(|peer| peer.certificate_fingerprint == fingerprint)
            .ok_or(TlsError::UnknownOrRevokedPeer)?;
        Ok((stream, peer))
    }
}

#[must_use]
pub fn certificate_fingerprint(certificate: &CertificateDer<'_>) -> String {
    hex_sha256(certificate.as_ref())
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a string cannot fail");
            output
        })
}
