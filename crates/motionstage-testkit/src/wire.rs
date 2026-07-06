//! Simulated QUIC wire clients for integration tests.
//!
//! [`WireClient`] mirrors the join handshake the `motionstage-cli` simulator
//! performs (`simulate.rs::connect_simulated_client`) against the protocol
//! 2.0 wire contract: connect → `ServerHello` → `ClientHello` →
//! `RegisterRequest` → `RegisterAccepted` → initial `SceneSnapshot` (sent
//! before the session is activated) → ordered `StateEventMsg` replication.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use motionstage_protocol::{
    AttributeDescriptor, AttributeKind, ClientHello, ClientRole, ControlMessage, DataFlowState,
    Feature, Mode, RegisterRequest, SceneSnapshotPayload, StateEvent, StateEventEnvelope,
    PROTOCOL_MAJOR, PROTOCOL_MINOR,
};
use motionstage_transport_quic::{ControlChannel, QuicClient, QuicPeer};
use uuid::Uuid;

/// Bound on every control-message receive so a missing event fails the test
/// instead of hanging it.
pub const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound on how many messages the skip-scanning helpers consume before
/// giving up, so a wrong predicate fails loudly instead of spinning.
const MAX_SKIPPED_MESSAGES: usize = 128;

/// A simulated remote player: a real QUIC client that completed the join
/// handshake and receives the replicated state-event stream.
pub struct WireClient {
    // Held so the client endpoint (and with it the connection) stays alive.
    _endpoint: QuicClient,
    pub peer: QuicPeer,
    pub control: ControlChannel,
    pub device_id: Uuid,
    pub session_id: Uuid,
    /// Initial world snapshot delivered during the handshake, before
    /// activation and before any state event.
    pub initial_snapshot: SceneSnapshotPayload,
    /// Highest state-event seq observed so far (starts at the snapshot seq).
    pub last_seq: u64,
}

impl WireClient {
    /// Connect and complete the full join handshake as a simulated player.
    pub async fn connect(
        addr: SocketAddr,
        device_id: Uuid,
        device_name: &str,
        roles: Vec<ClientRole>,
        features: Vec<Feature>,
    ) -> Result<Self> {
        let endpoint = QuicClient::new_insecure_for_local_dev()?;
        let peer = endpoint.connect(addr).await?;
        let mut control = peer.accept_control_stream().await?;

        match recv_with_timeout(&mut control).await? {
            ControlMessage::ServerHello(_) => {}
            other => bail!("expected ServerHello as first control message, got {other:?}"),
        }

        let advertised_attributes = if roles.contains(&ClientRole::MotionSource) {
            vec![AttributeDescriptor {
                path: "pose_pos".into(),
                value_type: AttributeKind::Vec3f,
            }]
        } else {
            Vec::new()
        };
        control
            .send(&ControlMessage::ClientHello(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                device_id,
                device_name: device_name.to_owned(),
                roles,
                features,
                advertised_attributes,
            }))
            .await?;
        control
            .send(&ControlMessage::RegisterRequest(RegisterRequest {
                pairing_token: None,
                api_key: None,
            }))
            .await?;

        let session_id = match recv_with_timeout(&mut control).await? {
            ControlMessage::RegisterAccepted(accepted) => accepted.session_id,
            ControlMessage::RegisterRejected(rejected) => {
                bail!(
                    "registration rejected: code={:?} reason={}",
                    rejected.code,
                    rejected.reason
                )
            }
            other => bail!("expected registration result, got {other:?}"),
        };

        // Wire contract: the initial world snapshot directly follows
        // registration, before activation and before any state event.
        let initial_snapshot = match recv_with_timeout(&mut control).await? {
            ControlMessage::SceneSnapshot(snapshot) => snapshot,
            other => bail!("expected initial SceneSnapshot after RegisterAccepted, got {other:?}"),
        };
        let last_seq = initial_snapshot.seq;

        Ok(Self {
            _endpoint: endpoint,
            peer,
            control,
            device_id,
            session_id,
            initial_snapshot,
            last_seq,
        })
    }

    /// Receive the next control message (bounded by [`RECV_TIMEOUT`]),
    /// tracking the highest observed state-event seq.
    pub async fn next_message(&mut self) -> Result<ControlMessage> {
        let message = recv_with_timeout(&mut self.control).await?;
        if let ControlMessage::StateEventMsg(envelope) = &message {
            self.last_seq = self.last_seq.max(envelope.seq);
        }
        Ok(message)
    }

    /// Receive the next control message and require it to be a state event.
    pub async fn next_event(&mut self) -> Result<StateEventEnvelope> {
        match self.next_message().await? {
            ControlMessage::StateEventMsg(envelope) => Ok(envelope),
            other => bail!("expected StateEventMsg, got {other:?}"),
        }
    }

    /// Skip messages until a state event matching `predicate` arrives.
    pub async fn wait_for_event(
        &mut self,
        predicate: impl Fn(&StateEventEnvelope) -> bool,
    ) -> Result<StateEventEnvelope> {
        for _ in 0..MAX_SKIPPED_MESSAGES {
            if let ControlMessage::StateEventMsg(envelope) = self.next_message().await? {
                if predicate(&envelope) {
                    return Ok(envelope);
                }
            }
        }
        bail!("no matching state event within {MAX_SKIPPED_MESSAGES} control messages")
    }

    /// Wait for this session's own `SessionJoined` replication event (the
    /// server emits it at activation, after the initial snapshot).
    pub async fn wait_for_own_join(&mut self) -> Result<StateEventEnvelope> {
        let session_id = self.session_id;
        self.wait_for_event(|envelope| {
            matches!(
                &envelope.event,
                StateEvent::SessionJoined { session_id: joined, .. } if *joined == session_id
            )
        })
        .await
    }

    /// Request the data-flow axis and wait for the direct `ModeState` reply,
    /// skipping replicated state events (callers assert on those separately).
    pub async fn set_data_flow(&mut self, state: DataFlowState) -> Result<Mode> {
        self.control
            .send(&ControlMessage::SetDataFlow(state))
            .await?;
        for _ in 0..MAX_SKIPPED_MESSAGES {
            match self.next_message().await? {
                ControlMessage::ModeState(mode) => return Ok(mode),
                ControlMessage::StateEventMsg(_) => continue,
                ControlMessage::Error { code, reason } => {
                    bail!("SetDataFlow rejected: code={code:?} reason={reason}")
                }
                other => bail!("unexpected reply to SetDataFlow: {other:?}"),
            }
        }
        bail!("no ModeState reply within {MAX_SKIPPED_MESSAGES} control messages")
    }

    /// Ask the server to replay every event after `last_seq` (rejoin path).
    pub async fn send_resync_request(&mut self, last_seq: u64) -> Result<()> {
        self.control
            .send(&ControlMessage::ResyncRequest { last_seq })
            .await?;
        Ok(())
    }

    /// Round-trip a ping and require the *very next* inbound messages to be
    /// the `Pong` + `ModeState` heartbeat reply. Because the control stream
    /// is ordered, this proves nothing else was in flight ahead of the reply.
    pub async fn ping_expect_pong(&mut self) -> Result<()> {
        self.control.send(&ControlMessage::Ping).await?;
        match self.next_message().await? {
            ControlMessage::Pong => {}
            other => bail!("expected Pong immediately after Ping, got {other:?}"),
        }
        match self.next_message().await? {
            ControlMessage::ModeState(_) => {}
            other => bail!("expected ModeState heartbeat after Pong, got {other:?}"),
        }
        Ok(())
    }

    /// Gracefully leave the session (`ClientGoodbye`), so the server closes
    /// it deterministically instead of waiting for an idle timeout.
    pub async fn goodbye(&mut self, reason: Option<String>) -> Result<()> {
        self.control
            .send(&ControlMessage::ClientGoodbye { reason })
            .await?;
        Ok(())
    }
}

async fn recv_with_timeout(control: &mut ControlChannel) -> Result<ControlMessage> {
    tokio::time::timeout(RECV_TIMEOUT, control.recv())
        .await
        .map_err(|_| anyhow!("timed out waiting for control message"))?
        .map_err(|err| anyhow!("control channel receive failed: {err}"))
}
