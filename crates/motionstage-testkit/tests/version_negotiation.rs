//! P3 protocol-hygiene interop test (docs/roadmap-v2.md, P3): "Older-minor
//! client interop test passes."
//!
//! Truth-in-docs contract (no 2.0 compatibility shim — wire compat is a clean
//! break): the server REQUIRES its own major and speaks EXACTLY its own minor.
//! A client announcing an older minor within the same major registers
//! successfully and is told the server's own minor in
//! `RegisterAccepted.negotiated_protocol_minor` (the server does not downgrade
//! its behaviour for a lesser client). A client announcing a *newer* major is
//! rejected with a typed `RegisterRejected{UnsupportedProtocol}`.

use anyhow::Result;
use motionstage_protocol::{
    ClientHello, ClientRole, ControlMessage, Feature, PROTOCOL_MAJOR, PROTOCOL_MINOR, RejectCode,
};
use motionstage_server::{ServerConfig, ServerHandle};
use motionstage_testkit::wire::WireClient;
use motionstage_transport_quic::QuicClient;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn older_minor_client_registers_and_is_told_the_server_minor() -> Result<()> {
    // An older minor must exist for this test to mean anything; revisit it
    // if the protocol minor ever resets to 0.
    const { assert!(PROTOCOL_MINOR > 0) };
    let config = ServerConfig {
        quic_bind_addr: "127.0.0.1:0".parse()?,
        enable_discovery: false,
        ..ServerConfig::default()
    };
    let server = ServerHandle::new(config);
    server.start().await?;
    let addr = server.quic_bind_addr().await;

    // An older-minor client (minor 0) completes the whole handshake — up to
    // and including the initial snapshot — and is told the server's own minor,
    // because the server speaks exactly its own minor (no downgrade).
    let older_device = Uuid::now_v7();
    let older = WireClient::connect_with_protocol_minor(
        addr,
        older_device,
        "sim-older-minor",
        vec![ClientRole::Operator],
        vec![Feature::Mapping],
        0,
    )
    .await?;
    assert_eq!(older.negotiated_protocol_minor, PROTOCOL_MINOR);
    // The server stores the minor it speaks (its own) per session.
    let session = server
        .session_info(older_device)
        .await
        .expect("older-minor session registered");
    assert_eq!(session.negotiated_protocol_minor, Some(PROTOCOL_MINOR));

    // A current client also negotiates the server's minor.
    let current = WireClient::connect(
        addr,
        Uuid::now_v7(),
        "sim-current-minor",
        vec![ClientRole::Operator],
        vec![Feature::Mapping],
    )
    .await?;
    assert_eq!(current.negotiated_protocol_minor, PROTOCOL_MINOR);

    drop(older);
    drop(current);
    server.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn foreign_major_client_is_rejected_with_typed_unsupported_protocol() -> Result<()> {
    let config = ServerConfig {
        quic_bind_addr: "127.0.0.1:0".parse()?,
        enable_discovery: false,
        ..ServerConfig::default()
    };
    let server = ServerHandle::new(config);
    server.start().await?;
    let addr = server.quic_bind_addr().await;

    // Drive the handshake by hand: a client on a different (newer) major must
    // be rejected with a typed RegisterRejected before the drop — the server
    // requires its own major, so this is the security-relevant "reject other
    // majors" branch of the contract.
    let endpoint = QuicClient::new_insecure_for_local_dev()?;
    let peer = endpoint.connect(addr).await?;
    let mut control = peer.accept_control_stream().await?;
    match control.recv().await? {
        ControlMessage::ServerHello(_) => {}
        other => panic!("expected ServerHello, got {other:?}"),
    }
    control
        .send(&ControlMessage::ClientHello(ClientHello {
            protocol_major: PROTOCOL_MAJOR + 1,
            protocol_minor: 0,
            device_id: Uuid::now_v7(),
            device_name: "sim-future-major".into(),
            roles: vec![ClientRole::Operator],
            features: vec![Feature::Mapping],
            advertised_attributes: Vec::new(),
        }))
        .await?;

    match control.recv().await? {
        ControlMessage::RegisterRejected(rejected) => {
            assert_eq!(rejected.code, RejectCode::UnsupportedProtocol);
            assert!(
                rejected.reason.contains("unsupported major"),
                "reason: {}",
                rejected.reason
            );
        }
        other => panic!("expected typed RegisterRejected for foreign major, got {other:?}"),
    }

    drop(control);
    drop(peer);
    drop(endpoint);
    server.stop().await?;
    Ok(())
}
