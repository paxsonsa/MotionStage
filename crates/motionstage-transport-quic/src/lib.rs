use std::{
    fmt::Debug,
    net::SocketAddr,
    sync::{Arc, Once},
    time::Duration,
};

use motionstage_core::{AttributeUpdate, AttributeValue};
use motionstage_protocol::ControlMessage;
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, UnixTime};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct QuicServer {
    endpoint: Endpoint,
    /// SHA-256 fingerprint of the server's DER certificate.
    cert_fingerprint: [u8; 32],
}

impl QuicServer {
    pub fn bind(bind_addr: SocketAddr) -> Result<Self, QuicTransportError> {
        install_rustls_provider();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .map_err(|err| QuicTransportError::Cert(err.to_string()))?;

        let cert_der = CertificateDer::from(cert.cert.der().to_vec());
        let cert_fingerprint: [u8; 32] = Sha256::digest(&cert_der).into();
        let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
        let mut server_config =
            quinn::ServerConfig::with_single_cert(vec![cert_der], key_der.into())
                .map_err(|err| QuicTransportError::Tls(err.to_string()))?;

        let transport = Arc::get_mut(&mut server_config.transport)
            .expect("server transport config must be uniquely owned at construction");
        configure_transport(transport);

        let endpoint = Endpoint::server(server_config, bind_addr)?;
        Ok(Self {
            endpoint,
            cert_fingerprint,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, QuicTransportError> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Raw SHA-256 fingerprint of the server's DER certificate.
    pub fn cert_fingerprint(&self) -> &[u8; 32] {
        &self.cert_fingerprint
    }

    /// Hex-encoded SHA-256 fingerprint of the server's DER certificate.
    pub fn cert_fingerprint_hex(&self) -> String {
        hex_encode(&self.cert_fingerprint)
    }

    pub async fn accept(&self) -> Result<QuicPeer, QuicTransportError> {
        let Some(incoming) = self.endpoint.accept().await else {
            return Err(QuicTransportError::Handshake(
                "accept stream closed".to_string(),
            ));
        };

        let connection = incoming
            .await
            .map_err(|err| QuicTransportError::Connection(err.to_string()))?;

        Ok(QuicPeer {
            connection: connection.clone(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct QuicClient {
    endpoint: Endpoint,
}

impl QuicClient {
    /// Create a client that skips server certificate verification.
    /// Only available when the `insecure` feature is enabled.
    #[cfg(feature = "insecure")]
    pub fn new_insecure_for_local_dev() -> Result<Self, QuicTransportError> {
        install_rustls_provider();
        let mut endpoint = Endpoint::client("0.0.0.0:0".parse().expect("static address parses"))?;

        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
            .with_no_client_auth();

        let mut client_cfg = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
                .map_err(|err| QuicTransportError::Crypto(err.to_string()))?,
        ));
        client_cfg.transport_config(default_transport_config());
        endpoint.set_default_client_config(client_cfg);

        Ok(Self { endpoint })
    }

    /// Create a client that pins to a specific server certificate fingerprint (TOFU).
    /// The `expected_fingerprint` is the SHA-256 hash of the server's DER certificate.
    pub fn new_with_pinned_cert(
        expected_fingerprint: [u8; 32],
    ) -> Result<Self, QuicTransportError> {
        install_rustls_provider();
        let mut endpoint = Endpoint::client("0.0.0.0:0".parse().expect("static address parses"))?;

        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier {
                expected_fingerprint,
            }))
            .with_no_client_auth();

        let mut client_cfg = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
                .map_err(|err| QuicTransportError::Crypto(err.to_string()))?,
        ));
        client_cfg.transport_config(default_transport_config());
        endpoint.set_default_client_config(client_cfg);

        Ok(Self { endpoint })
    }

    pub async fn connect(&self, addr: SocketAddr) -> Result<QuicPeer, QuicTransportError> {
        let connecting = self
            .endpoint
            .connect(addr, "localhost")
            .map_err(|err| QuicTransportError::Connect(err.to_string()))?;
        let connection = connecting
            .await
            .map_err(|err| QuicTransportError::Connection(err.to_string()))?;

        Ok(QuicPeer {
            connection: connection.clone(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct QuicPeer {
    connection: Connection,
}

impl QuicPeer {
    pub async fn open_control_stream(&self) -> Result<ControlChannel, QuicTransportError> {
        let (send, recv) = self
            .connection
            .open_bi()
            .await
            .map_err(|err| QuicTransportError::Connection(err.to_string()))?;
        Ok(ControlChannel { send, recv })
    }

    pub async fn accept_control_stream(&self) -> Result<ControlChannel, QuicTransportError> {
        let (send, recv) = self
            .connection
            .accept_bi()
            .await
            .map_err(|err| QuicTransportError::Connection(err.to_string()))?;
        Ok(ControlChannel { send, recv })
    }

    pub fn send_motion_datagram(&self, datagram: MotionDatagram) -> Result<(), QuicTransportError> {
        let bytes = bincode::serialize(&datagram)
            .map_err(|err| QuicTransportError::Serialization(err.to_string()))?;
        self.connection
            .send_datagram(bytes.into())
            .map_err(|err| QuicTransportError::Datagram(err.to_string()))?;
        Ok(())
    }

    pub async fn recv_motion_datagram(&self) -> Result<MotionDatagram, QuicTransportError> {
        let bytes = self
            .connection
            .read_datagram()
            .await
            .map_err(|err| QuicTransportError::Connection(err.to_string()))?;
        let datagram: MotionDatagram = bincode::deserialize(&bytes)
            .map_err(|err| QuicTransportError::Serialization(err.to_string()))?;
        Ok(datagram)
    }
}

pub struct ControlChannel {
    send: SendStream,
    recv: RecvStream,
}

impl ControlChannel {
    pub async fn send(&mut self, message: &ControlMessage) -> Result<(), QuicTransportError> {
        let bytes = bincode::serialize(message)
            .map_err(|err| QuicTransportError::Serialization(err.to_string()))?;
        let len = bytes.len() as u32;
        self.send
            .write_all(&len.to_le_bytes())
            .await
            .map_err(|err| QuicTransportError::Write(err.to_string()))?;
        self.send
            .write_all(&bytes)
            .await
            .map_err(|err| QuicTransportError::Write(err.to_string()))?;
        Ok(())
    }

    pub async fn recv(&mut self) -> Result<ControlMessage, QuicTransportError> {
        read_control_message(&mut self.recv).await
    }

    pub fn finish(&mut self) -> Result<(), QuicTransportError> {
        self.send
            .finish()
            .map_err(|err| QuicTransportError::Write(err.to_string()))
    }
}

async fn read_control_message(recv: &mut RecvStream) -> Result<ControlMessage, QuicTransportError> {
    let mut len_bytes = [0_u8; 4];
    recv.read_exact(&mut len_bytes)
        .await
        .map_err(|err| QuicTransportError::Read(err.to_string()))?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    let mut bytes = vec![0_u8; len];
    recv.read_exact(&mut bytes)
        .await
        .map_err(|err| QuicTransportError::Read(err.to_string()))?;
    let message: ControlMessage = bincode::deserialize(&bytes)
        .map_err(|err| QuicTransportError::Serialization(err.to_string()))?;
    Ok(message)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct MotionDatagram {
    pub device_id: Uuid,
    pub timestamp_ns: u64,
    pub updates: Vec<AttributeUpdateFrame>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct AttributeUpdateFrame {
    pub output_attribute: String,
    pub value: AttributeValueFrame,
}

impl From<AttributeUpdateFrame> for AttributeUpdate {
    fn from(value: AttributeUpdateFrame) -> Self {
        Self {
            output_attribute: value.output_attribute,
            value: value.value.into(),
        }
    }
}

impl From<AttributeUpdate> for AttributeUpdateFrame {
    fn from(value: AttributeUpdate) -> Self {
        Self {
            output_attribute: value.output_attribute,
            value: value.value.into(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub enum AttributeValueFrame {
    Bool(bool),
    Int32(i32),
    Float32(f32),
    Float64(f64),
    Vec2f([f32; 2]),
    Vec3f([f32; 3]),
    Vec4f([f32; 4]),
    Quatf([f32; 4]),
    Mat4f([[f32; 4]; 4]),
    Trigger(bool),
}

impl From<AttributeValueFrame> for AttributeValue {
    fn from(value: AttributeValueFrame) -> Self {
        match value {
            AttributeValueFrame::Bool(v) => Self::Bool(v),
            AttributeValueFrame::Int32(v) => Self::Int32(v),
            AttributeValueFrame::Float32(v) => Self::Float32(v),
            AttributeValueFrame::Float64(v) => Self::Float64(v),
            AttributeValueFrame::Vec2f(v) => Self::Vec2f(v),
            AttributeValueFrame::Vec3f(v) => Self::Vec3f(v),
            AttributeValueFrame::Vec4f(v) => Self::Vec4f(v),
            AttributeValueFrame::Quatf(v) => Self::Quatf(v),
            AttributeValueFrame::Mat4f(v) => Self::Mat4f(v),
            AttributeValueFrame::Trigger(v) => Self::Trigger(v),
        }
    }
}

impl From<AttributeValue> for AttributeValueFrame {
    fn from(value: AttributeValue) -> Self {
        match value {
            AttributeValue::Bool(v) => Self::Bool(v),
            AttributeValue::Int32(v) => Self::Int32(v),
            AttributeValue::Float32(v) => Self::Float32(v),
            AttributeValue::Float64(v) => Self::Float64(v),
            AttributeValue::Vec2f(v) => Self::Vec2f(v),
            AttributeValue::Vec3f(v) => Self::Vec3f(v),
            AttributeValue::Vec4f(v) => Self::Vec4f(v),
            AttributeValue::Quatf(v) => Self::Quatf(v),
            AttributeValue::Mat4f(v) => Self::Mat4f(v),
            AttributeValue::Trigger(v) => Self::Trigger(v),
        }
    }
}

#[derive(Debug, Error)]
pub enum QuicTransportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tls: {0}")]
    Tls(String),
    #[error("cert: {0}")]
    Cert(String),
    #[error("connect: {0}")]
    Connect(String),
    #[error("connection: {0}")]
    Connection(String),
    #[error("read: {0}")]
    Read(String),
    #[error("write: {0}")]
    Write(String),
    #[error("serialization: {0}")]
    Serialization(String),
    #[error("datagram: {0}")]
    Datagram(String),
    #[error("crypto: {0}")]
    Crypto(String),
    #[error("handshake: {0}")]
    Handshake(String),
}

// ---------------------------------------------------------------------------
// Certificate verifiers
// ---------------------------------------------------------------------------

/// Verifier that pins to a specific SHA-256 certificate fingerprint (TOFU).
#[derive(Debug)]
struct PinnedCertVerifier {
    expected_fingerprint: [u8; 32],
}

impl rustls::client::danger::ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let actual: [u8; 32] = Sha256::digest(end_entity).into();
        if actual == self.expected_fingerprint {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "certificate fingerprint mismatch: expected {}, got {}",
                hex_encode(&self.expected_fingerprint),
                hex_encode(&actual),
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

/// Verifier that accepts any server certificate without validation.
/// Only available when the `insecure` feature is enabled.
#[cfg(feature = "insecure")]
#[derive(Debug)]
struct SkipServerVerification;

#[cfg(feature = "insecure")]
impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

// ---------------------------------------------------------------------------
// Hex helpers
// ---------------------------------------------------------------------------

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn hex_decode_fingerprint(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

fn install_rustls_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn configure_transport(transport: &mut quinn::TransportConfig) {
    transport.max_concurrent_uni_streams(32_u8.into());
    transport.datagram_receive_buffer_size(Some(64 * 1024));
    // Prevent idle control-only sessions from expiring during interactive pauses.
    transport.keep_alive_interval(Some(Duration::from_secs(2)));
}

fn default_transport_config() -> Arc<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    configure_transport(&mut transport);
    Arc::new(transport)
}

#[cfg(test)]
mod tests {
    use motionstage_core::AttributeValue;
    use motionstage_protocol::ControlMessage;
    use uuid::Uuid;

    use crate::{AttributeUpdateFrame, MotionDatagram, QuicClient, QuicServer};

    #[tokio::test]
    async fn control_stream_roundtrip() {
        let server = QuicServer::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let fingerprint = *server.cert_fingerprint();

        let server_task = tokio::spawn(async move {
            let peer = server.accept().await.unwrap();
            let mut stream = peer.accept_control_stream().await.unwrap();
            let message = stream.recv().await.unwrap();
            assert_eq!(message, ControlMessage::Ping);
            stream.send(&ControlMessage::Pong).await.unwrap();
            stream.finish().unwrap();
            tokio::time::sleep(tokio::time::Duration::from_millis(25)).await;
        });

        let client = QuicClient::new_with_pinned_cert(fingerprint).unwrap();
        let peer = client.connect(addr).await.unwrap();
        let mut stream = peer.open_control_stream().await.unwrap();
        stream.send(&ControlMessage::Ping).await.unwrap();
        stream.finish().unwrap();
        let response = stream.recv().await.unwrap();
        assert_eq!(response, ControlMessage::Pong);

        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn motion_datagram_roundtrip() {
        let server = QuicServer::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let fingerprint = *server.cert_fingerprint();

        let expected_device = Uuid::now_v7();
        let server_task = tokio::spawn(async move {
            let peer = server.accept().await.unwrap();
            let datagram = peer.recv_motion_datagram().await.unwrap();
            assert_eq!(datagram.device_id, expected_device);
            assert_eq!(datagram.timestamp_ns, 42);
            assert_eq!(datagram.updates.len(), 1);
        });

        let client = QuicClient::new_with_pinned_cert(fingerprint).unwrap();
        let peer = client.connect(addr).await.unwrap();
        peer.send_motion_datagram(MotionDatagram {
            device_id: expected_device,
            timestamp_ns: 42,
            updates: vec![AttributeUpdateFrame {
                output_attribute: "pose_pos".into(),
                value: AttributeValue::Vec3f([1.0, 2.0, 3.0]).into(),
            }],
        })
        .unwrap();

        server_task.await.unwrap();
    }

}
