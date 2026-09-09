use futures_util::{SinkExt, StreamExt};
use mcp_agent_relay::{
    AuthenticatedPeer, ERROR_CONTRACT_VERSION, GatewaySocketConfig, MtlsAcceptor, PeerResolver,
    Platform, RELAY_PROTOCOL, RELAY_SUBPROTOCOL, RESULT_CONTRACT_VERSION, Register, RelayFrame,
    RelayPayload, TlsError, accept_gateway_authenticated, certificate_fingerprint, decode_frame,
    encode_frame, new_connection_id, tool_schema_digest,
};
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, date_time_ymd,
};
use rustls::RootCertStore;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Resolver {
    peers: Mutex<HashMap<String, AuthenticatedPeer>>,
}

impl Resolver {
    fn enroll(&self, certificate: &CertificateDer<'_>) {
        let fingerprint = certificate_fingerprint(certificate);
        self.peers.lock().unwrap().insert(
            fingerprint.clone(),
            AuthenticatedPeer {
                owner_id: "owner".to_owned(),
                device_id: "device".to_owned(),
                certificate_fingerprint: fingerprint,
                platform: Platform::Macos,
            },
        );
    }

    fn revoke(&self, certificate: &CertificateDer<'_>) {
        self.peers
            .lock()
            .unwrap()
            .remove(&certificate_fingerprint(certificate));
    }
}

impl PeerResolver for Resolver {
    fn resolve(&self, certificate_fingerprint: &str) -> Option<AuthenticatedPeer> {
        self.peers
            .lock()
            .unwrap()
            .get(certificate_fingerprint)
            .cloned()
    }
}

struct Identity {
    certificate: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
}

impl Identity {
    fn clone_key(&self) -> PrivateKeyDer<'static> {
        self.key.clone_key()
    }
}

struct Fixture {
    ca: CertificateDer<'static>,
    server: Identity,
    client: Identity,
    wrong_ca_client: Identity,
    wrong_purpose_client: Identity,
    expired_client: Identity,
}

fn fixture() -> Fixture {
    let (ca, issuer) = certificate_authority("device-ca");
    let server = leaf(
        &issuer,
        "localhost",
        ExtendedKeyUsagePurpose::ServerAuth,
        false,
    );
    let client = leaf(
        &issuer,
        "device",
        ExtendedKeyUsagePurpose::ClientAuth,
        false,
    );
    let wrong_purpose_client = leaf(
        &issuer,
        "wrong-purpose",
        ExtendedKeyUsagePurpose::ServerAuth,
        false,
    );
    let expired_client = leaf(
        &issuer,
        "expired",
        ExtendedKeyUsagePurpose::ClientAuth,
        true,
    );
    let (_, wrong_issuer) = certificate_authority("wrong-ca");
    let wrong_ca_client = leaf(
        &wrong_issuer,
        "wrong-ca-device",
        ExtendedKeyUsagePurpose::ClientAuth,
        false,
    );
    Fixture {
        ca,
        server,
        client,
        wrong_ca_client,
        wrong_purpose_client,
        expired_client,
    }
}

fn certificate_authority(name: &str) -> (CertificateDer<'static>, Issuer<'static, KeyPair>) {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, name);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let key = KeyPair::generate().unwrap();
    let certificate = params.self_signed(&key).unwrap().der().clone();
    (certificate, Issuer::new(params, key))
}

fn leaf(
    issuer: &Issuer<'_, KeyPair>,
    name: &str,
    purpose: ExtendedKeyUsagePurpose,
    expired: bool,
) -> Identity {
    let mut params = CertificateParams::new(vec![name.to_owned()]).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, name);
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![purpose];
    if expired {
        params.not_before = date_time_ymd(2019, 1, 1);
        params.not_after = date_time_ymd(2020, 1, 1);
    }
    let key = KeyPair::generate().unwrap();
    let certificate = params.signed_by(&key, issuer).unwrap().der().clone();
    Identity {
        certificate,
        key: PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
    }
}

async fn handshake(
    fixture: &Fixture,
    resolver: Arc<Resolver>,
    client: Option<&Identity>,
) -> Result<AuthenticatedPeer, TlsError> {
    let acceptor = MtlsAcceptor::new(
        fixture.ca.clone(),
        vec![fixture.server.certificate.clone()],
        fixture.server.clone_key(),
        resolver,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        acceptor.accept(stream).await.map(|(_, peer)| peer)
    });

    let mut roots = RootCertStore::empty();
    roots.add(fixture.ca.clone()).unwrap();
    let builder = rustls::ClientConfig::builder().with_root_certificates(roots);
    let config = match client {
        Some(client) => builder
            .with_client_auth_cert(vec![client.certificate.clone()], client.clone_key())
            .unwrap(),
        None => builder.with_no_client_auth(),
    };
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let connector = TlsConnector::from(Arc::new(config));
    let _ = connector
        .connect(ServerName::try_from("localhost").unwrap(), stream)
        .await;
    server.await.unwrap()
}

fn client_config(fixture: &Fixture, client: &Identity) -> rustls::ClientConfig {
    let mut roots = RootCertStore::empty();
    roots.add(fixture.ca.clone()).unwrap();
    rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(vec![client.certificate.clone()], client.clone_key())
        .unwrap()
}

#[tokio::test]
async fn valid_enrolled_device_completes_mtls() {
    let fixture = fixture();
    let resolver = Arc::new(Resolver::default());
    resolver.enroll(&fixture.client.certificate);
    let peer = handshake(&fixture, resolver, Some(&fixture.client))
        .await
        .unwrap();
    assert_eq!(peer.device_id, "device");
}

#[tokio::test]
async fn missing_wrong_ca_wrong_purpose_and_expired_certificates_fail_handshake() {
    let fixture = fixture();
    for client in [
        None,
        Some(&fixture.wrong_ca_client),
        Some(&fixture.wrong_purpose_client),
        Some(&fixture.expired_client),
    ] {
        assert!(matches!(
            handshake(&fixture, Arc::new(Resolver::default()), client).await,
            Err(TlsError::Handshake(_))
        ));
    }
}

#[tokio::test]
async fn unknown_and_revoked_certificates_fail_inside_the_tls_handshake() {
    let fixture = fixture();
    let unknown = handshake(
        &fixture,
        Arc::new(Resolver::default()),
        Some(&fixture.client),
    )
    .await;
    assert!(matches!(unknown, Err(TlsError::Handshake(_))));

    let resolver = Arc::new(Resolver::default());
    resolver.enroll(&fixture.client.certificate);
    resolver.revoke(&fixture.client.certificate);
    let revoked = handshake(&fixture, resolver, Some(&fixture.client)).await;
    assert!(matches!(revoked, Err(TlsError::Handshake(_))));
}

#[tokio::test]
async fn real_mtls_websocket_listener_negotiates_bounded_relay_protocol() {
    let fixture = fixture();
    let resolver = Arc::new(Resolver::default());
    resolver.enroll(&fixture.client.certificate);
    let acceptor = MtlsAcceptor::new(
        fixture.ca.clone(),
        vec![fixture.server.certificate.clone()],
        fixture.server.clone_key(),
        resolver,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cancellation = CancellationToken::new();
    let server_cancellation = cancellation.clone();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let (tls, peer) = acceptor.accept(tcp).await.unwrap();
        let (registration, backend) = accept_gateway_authenticated(
            tls,
            peer,
            7,
            GatewaySocketConfig::new(["0".repeat(64)]).unwrap(),
            server_cancellation.clone(),
        )
        .await
        .unwrap();
        assert_eq!(registration.platform, Platform::Macos);
        server_cancellation.cancel();
        backend.wait_closed().await;
    });

    let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
    let connector = TlsConnector::from(Arc::new(client_config(&fixture, &fixture.client)));
    let tls = connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    let mut request = format!("wss://localhost:{}/relay", address.port())
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", RELAY_SUBPROTOCOL.parse().unwrap());
    let (mut socket, response) = tokio_tungstenite::client_async(request, tls).await.unwrap();
    assert_eq!(
        response.headers().get("Sec-WebSocket-Protocol").unwrap(),
        RELAY_SUBPROTOCOL
    );
    let frame = RelayFrame {
        protocol: RELAY_PROTOCOL,
        connection_id: new_connection_id(),
        generation: 0,
        sequence: 0,
        payload: RelayPayload::Register {
            registration: Register {
                min_protocol: RELAY_PROTOCOL,
                max_protocol: RELAY_PROTOCOL,
                tool_schema_digest: tool_schema_digest(),
                result_contract_version: RESULT_CONTRACT_VERSION,
                error_contract_version: ERROR_CONTRACT_VERSION,
                system_skill_manifest_digest: "0".repeat(64),
                platform: Platform::Macos,
                workspace_id: "workspace".to_owned(),
                containment_posture: "macos-seatbelt-verified".to_owned(),
                launch_instance_id: "launch".to_owned(),
                connection_epoch: 1,
            },
        },
    };
    socket
        .send(Message::Binary(encode_frame(&frame).unwrap().into()))
        .await
        .unwrap();
    let response = socket.next().await.unwrap().unwrap().into_data();
    assert!(matches!(
        decode_frame(&response).unwrap().payload,
        RelayPayload::Registered
    ));
    cancellation.cancel();
    server.await.unwrap();
}

#[test]
fn malformed_certificate_material_is_rejected_before_listener_activation() {
    let fixture = fixture();
    let malformed = CertificateDer::from(vec![1, 2, 3, 4]);
    assert!(matches!(
        MtlsAcceptor::new(
            malformed,
            vec![fixture.server.certificate],
            fixture.server.key,
            Arc::new(Resolver::default()),
        ),
        Err(TlsError::InvalidTrustStore(_))
    ));
}
