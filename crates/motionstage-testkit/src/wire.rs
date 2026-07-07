//! Simulated QUIC wire clients for integration tests.
//!
//! [`WireClient`] mirrors the join handshake the `motionstage-cli` simulator
//! performs (`simulate.rs::connect_simulated_client`) against the protocol
//! 2.1 wire contract: connect → `ServerHello` → `ClientHello` →
//! `RegisterRequest` → `RegisterAccepted` (carrying the negotiated protocol
//! minor) → initial `SceneSnapshot` (sent before the session is activated) →
//! ordered `StateEventMsg` replication, plus the operator-plane request/typed-
//! result ops (mappings, take control, on-demand snapshots).

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use motionstage_protocol::{
    AttributeDescriptor, AttributeKind, ClientHello, ClientRole, ControlMessage, DataFlowState,
    Feature, MappingSummary, RegisterRequest, SceneSnapshotPayload, StateEvent, StateEventEnvelope,
    TakeInfo, WireError, PROTOCOL_MAJOR, PROTOCOL_MINOR,
};
use motionstage_transport_quic::{
    AttributeUpdateFrame, AttributeValueFrame, ControlChannel, MotionDatagram, QuicClient, QuicPeer,
};
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
    /// The protocol minor the server speaks for this session
    /// ([`motionstage_protocol::RegisterAccepted::negotiated_protocol_minor`]).
    /// The server speaks exactly its own minor (no downgrade), so this is the
    /// server's minor for any accepted client within the same major.
    pub negotiated_protocol_minor: u16,
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
        Self::connect_with_protocol_minor(
            addr,
            device_id,
            device_name,
            roles,
            features,
            PROTOCOL_MINOR,
        )
        .await
    }

    /// Connect announcing a specific client protocol minor (same major), for
    /// version-negotiation interop tests. An older-minor client must register
    /// successfully and be told the server's own minor in `RegisterAccepted`
    /// (the server speaks exactly its own minor, no downgrade).
    pub async fn connect_with_protocol_minor(
        addr: SocketAddr,
        device_id: Uuid,
        device_name: &str,
        roles: Vec<ClientRole>,
        features: Vec<Feature>,
        protocol_minor: u16,
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
                protocol_minor,
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

        let (session_id, negotiated_protocol_minor) = match recv_with_timeout(&mut control).await? {
            ControlMessage::RegisterAccepted(accepted) => {
                (accepted.session_id, accepted.negotiated_protocol_minor)
            }
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
            negotiated_protocol_minor,
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

    /// Request the data-flow axis. Success has **no direct reply** (protocol
    /// 2.1 retired the `ModeState` ack): the single acknowledgement is the
    /// caller's own replicated `ModeChanged` echo, which this helper waits
    /// for and returns (skipping other sessions' interleaved events).
    /// Failure arrives as [`ControlMessage::Error`].
    pub async fn set_data_flow(&mut self, state: DataFlowState) -> Result<StateEventEnvelope> {
        self.control
            .send(&ControlMessage::SetDataFlow(state))
            .await?;
        let session_id = self.session_id;
        for _ in 0..MAX_SKIPPED_MESSAGES {
            match self.next_message().await? {
                ControlMessage::StateEventMsg(envelope) => {
                    if envelope.origin_session == Some(session_id)
                        && matches!(envelope.event, StateEvent::ModeChanged { .. })
                    {
                        return Ok(envelope);
                    }
                }
                ControlMessage::Error { code, reason } => {
                    bail!("SetDataFlow rejected: code={code:?} reason={reason}")
                }
                other => bail!("unexpected reply to SetDataFlow: {other:?}"),
            }
        }
        bail!("no own ModeChanged echo within {MAX_SKIPPED_MESSAGES} control messages")
    }

    /// Receive the next **direct reply** message, skipping replicated state
    /// events. Operator-plane direct replies and event echoes interleave in
    /// any order on the session loop, so typed replies must be matched by
    /// message type, not stream position.
    pub async fn next_reply(&mut self) -> Result<ControlMessage> {
        for _ in 0..MAX_SKIPPED_MESSAGES {
            match self.next_message().await? {
                ControlMessage::StateEventMsg(_) => continue,
                other => return Ok(other),
            }
        }
        bail!("no direct reply within {MAX_SKIPPED_MESSAGES} control messages")
    }

    /// Wire operator-plane mapping create ([`ControlMessage::CreateMapping`]).
    /// `source_device: None` resolves to this session's own device;
    /// `target_scene: None` resolves to the active scene. Returns the typed
    /// wire result carried by [`ControlMessage::MappingCreateResult`].
    #[allow(clippy::too_many_arguments)]
    pub async fn create_mapping(
        &mut self,
        source_device: Option<Uuid>,
        source_output: &str,
        target_scene: Option<Uuid>,
        target_object: Uuid,
        target_attribute: &str,
        component_mask: Option<Vec<usize>>,
    ) -> Result<Result<MappingSummary, WireError>> {
        self.control
            .send(&ControlMessage::CreateMapping {
                source_device,
                source_output: source_output.to_owned(),
                target_scene,
                target_object,
                target_attribute: target_attribute.to_owned(),
                component_mask,
            })
            .await?;
        match self.next_reply().await? {
            ControlMessage::MappingCreateResult { result } => Ok(result),
            other => bail!("expected MappingCreateResult for CreateMapping, got {other:?}"),
        }
    }

    /// Wire operator-plane mapping update ([`ControlMessage::UpdateMapping`],
    /// full replacement). Defaults resolve as in [`Self::create_mapping`].
    #[allow(clippy::too_many_arguments)]
    pub async fn update_mapping(
        &mut self,
        mapping_id: Uuid,
        source_device: Option<Uuid>,
        source_output: &str,
        target_scene: Option<Uuid>,
        target_object: Uuid,
        target_attribute: &str,
        component_mask: Option<Vec<usize>>,
    ) -> Result<Result<MappingSummary, WireError>> {
        self.control
            .send(&ControlMessage::UpdateMapping {
                mapping_id,
                source_device,
                source_output: source_output.to_owned(),
                target_scene,
                target_object,
                target_attribute: target_attribute.to_owned(),
                component_mask,
            })
            .await?;
        match self.next_reply().await? {
            ControlMessage::MappingCreateResult { result } => Ok(result),
            other => bail!("expected MappingCreateResult for UpdateMapping, got {other:?}"),
        }
    }

    /// Wire operator-plane mapping removal ([`ControlMessage::RemoveMapping`]).
    pub async fn remove_mapping(&mut self, mapping_id: Uuid) -> Result<Result<(), WireError>> {
        self.control
            .send(&ControlMessage::RemoveMapping { mapping_id })
            .await?;
        self.expect_mapping_op_result(mapping_id).await
    }

    /// Wire operator-plane lock toggle ([`ControlMessage::SetMappingLock`]).
    pub async fn set_mapping_lock(
        &mut self,
        mapping_id: Uuid,
        lock: bool,
    ) -> Result<Result<(), WireError>> {
        self.control
            .send(&ControlMessage::SetMappingLock { mapping_id, lock })
            .await?;
        self.expect_mapping_op_result(mapping_id).await
    }

    async fn expect_mapping_op_result(
        &mut self,
        expected_mapping_id: Uuid,
    ) -> Result<Result<(), WireError>> {
        match self.next_reply().await? {
            ControlMessage::MappingOpResult { mapping_id, result } => {
                if mapping_id != expected_mapping_id {
                    bail!(
                        "MappingOpResult for unexpected mapping: got {mapping_id}, \
                         expected {expected_mapping_id}"
                    );
                }
                Ok(result)
            }
            other => bail!("expected MappingOpResult, got {other:?}"),
        }
    }

    /// Wire take control: start recording ([`ControlMessage::StartTake`],
    /// Operator-gated, server-assigned identity). Returns the typed result
    /// carried by [`ControlMessage::TakeStartResult`].
    pub async fn start_take(&mut self) -> Result<Result<Uuid, WireError>> {
        self.control.send(&ControlMessage::StartTake).await?;
        match self.next_reply().await? {
            ControlMessage::TakeStartResult { result } => Ok(result),
            other => bail!("expected TakeStartResult, got {other:?}"),
        }
    }

    /// Wire take control: stop the active recording and register the take
    /// ([`ControlMessage::StopTake`], Operator-gated). Returns the typed
    /// result carried by [`ControlMessage::TakeStopResult`].
    pub async fn stop_take(&mut self) -> Result<Result<TakeInfo, WireError>> {
        self.control.send(&ControlMessage::StopTake).await?;
        match self.next_reply().await? {
            ControlMessage::TakeStopResult { result } => Ok(result),
            other => bail!("expected TakeStopResult, got {other:?}"),
        }
    }

    /// Request an on-demand full world snapshot
    /// ([`ControlMessage::GetSceneSnapshot`]).
    pub async fn request_scene_snapshot(&mut self) -> Result<SceneSnapshotPayload> {
        self.control.send(&ControlMessage::GetSceneSnapshot).await?;
        match self.next_reply().await? {
            ControlMessage::SceneSnapshot(snapshot) => Ok(snapshot),
            other => bail!("expected SceneSnapshot, got {other:?}"),
        }
    }

    /// Push one motion datagram carrying a single Vec3f attribute update for
    /// this device (the same wire path the CLI simulator streams on).
    pub fn send_motion_datagram(
        &self,
        output_attribute: &str,
        value: [f32; 3],
        timestamp_ns: u64,
    ) -> Result<()> {
        self.peer.send_motion_datagram(MotionDatagram {
            device_id: self.device_id,
            timestamp_ns,
            updates: vec![AttributeUpdateFrame {
                output_attribute: output_attribute.to_owned(),
                value: AttributeValueFrame::Vec3f(value),
            }],
        })?;
        Ok(())
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
