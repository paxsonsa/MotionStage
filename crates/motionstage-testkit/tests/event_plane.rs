//! P1 exit-criterion integration test (docs/roadmap-v2.md, P1):
//! "A mutation from any session is observed by every other session (both
//! transports) without polling", joins deliver a real world snapshot before
//! activation, and reconnect is a rejoin (replay of the exact gap from the
//! last seen seq).
//!
//! The test boots a real server, connects two simulated QUIC players (the
//! same wire handshake the CLI simulator performs), and drives the event
//! plane end-to-end over the wire.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use motionstage_core::{AttributeValue, MappingRequest, Scene, SceneAttribute, SceneObject};
use motionstage_protocol::{
    BakeAttributeValue, ClientRole, DataFlowState, Feature, Mode, SessionState, StateEvent,
};
use motionstage_server::{ServerConfig, ServerHandle};
use motionstage_testkit::wire::WireClient;
use uuid::Uuid;

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_nanos() as u64)
        .unwrap_or_default()
}

async fn wait_for_session_closed(server: &ServerHandle, device_id: Uuid) -> Result<()> {
    for _ in 0..200 {
        if let Some(info) = server.session_info(device_id).await {
            if info.state == SessionState::Closed {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    bail!("session for device {device_id} did not close in time")
}

#[tokio::test(flavor = "multi_thread")]
async fn event_plane_replicates_mutations_snapshots_and_resync_across_quic_clients() -> Result<()> {
    // --- Boot an authoritative server with one host-authored scene. ---
    let config = ServerConfig {
        quic_bind_addr: "127.0.0.1:0".parse()?,
        enable_discovery: false,
        ..ServerConfig::default()
    };
    let server = ServerHandle::new(config);

    let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
        "position",
        AttributeValue::Vec3f([0.0, 0.0, 0.0]),
    ));
    let object_id = object.id;
    let scene = Scene::new("stage").with_object(object);
    let scene_id = scene.id;
    server.load_scene(scene).await;

    server.start().await?;
    let addr = server.quic_bind_addr().await;

    // --- Two simulated QUIC players join and reach Active. ---
    let device_a = Uuid::now_v7();
    let device_b = Uuid::now_v7();
    let mut client_a = WireClient::connect(
        addr,
        device_a,
        "sim-a",
        vec![ClientRole::MotionSource, ClientRole::Operator],
        vec![Feature::Motion, Feature::Mapping],
    )
    .await?;
    let mut client_b = WireClient::connect(
        addr,
        device_b,
        "sim-b",
        vec![ClientRole::Operator],
        vec![Feature::Mapping],
    )
    .await?;

    // Both handshakes delivered the authored world in the initial snapshot.
    for client in [&client_a, &client_b] {
        assert_eq!(client.initial_snapshot.scenes.len(), 1);
        assert_eq!(client.initial_snapshot.active_scene, Some(scene_id));
    }

    // Joining emits SessionJoined; once a client observes its own join it is
    // Active on the server without any polling in between.
    let join_a = client_a.wait_for_own_join().await?;
    assert_eq!(join_a.origin_session, Some(client_a.session_id));
    client_b.wait_for_own_join().await?;
    assert_eq!(
        server
            .session_info(device_a)
            .await
            .expect("session a")
            .state,
        SessionState::Active
    );

    // --- 1. Client A flips data_flow live; B observes it without pinging. ---
    // Retired ack (protocol 2.1): SetDataFlow success has no direct reply.
    // The single acknowledgement is A's own replicated ModeChanged echo (no
    // echo suppression), which the helper waits for and returns.
    let echo = client_a.set_data_flow(DataFlowState::Live).await?;
    assert_eq!(echo.origin_session, Some(client_a.session_id));
    assert!(matches!(echo.event, StateEvent::ModeChanged { mode } if mode == Mode::LIVE));

    // B sent nothing; the very next message it receives is the replicated
    // ModeChanged, stamped with A's session as origin.
    let observed = client_b.next_event().await?;
    assert_eq!(observed.origin_session, Some(client_a.session_id));
    assert!(matches!(observed.event, StateEvent::ModeChanged { mode } if mode == Mode::LIVE));

    // --- 2. The host API creates a mapping; both clients hear about it. ---
    let mapping_id = server
        .create_mapping(
            MappingRequest {
                source_device: device_a,
                source_output: "pose_pos".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: None,
            },
            now_ns(),
        )
        .await?;

    for (label, client) in [("a", &mut client_a), ("b", &mut client_b)] {
        let envelope = client.next_event().await?;
        assert_eq!(
            envelope.origin_session,
            Some(server.host_session_id()),
            "client {label}: host mutation must carry the host session origin"
        );
        match &envelope.event {
            StateEvent::MappingCreated { mapping } => {
                assert_eq!(mapping.mapping_id, mapping_id, "client {label}");
                assert_eq!(mapping.source_device, device_a, "client {label}");
                assert_eq!(mapping.target_scene, scene_id, "client {label}");
                assert_eq!(mapping.target_object, object_id, "client {label}");
                assert_eq!(mapping.target_attribute, "position", "client {label}");
                assert!(!mapping.lock, "client {label}");
            }
            other => bail!("client {label}: expected MappingCreated, got {other:?}"),
        }
    }

    // --- 3. B leaves; the world moves on; rejoin replays the exact gap. ---
    let b_last_seq = client_b.last_seq;
    let old_b_session = client_b.session_id;
    client_b
        .goodbye(Some("simulated disconnect".into()))
        .await?;
    // Keep the connection alive until the server has processed the goodbye,
    // then tear it down.
    wait_for_session_closed(&server, device_b).await?;
    drop(client_b);
    // The disconnect itself is a replicated mutation.
    assert_eq!(server.current_event_seq().await, b_last_seq + 1);

    server.set_mapping_lock(mapping_id, true).await?;
    server.set_data_flow(DataFlowState::Idle).await?;
    assert_eq!(server.current_event_seq().await, b_last_seq + 3);

    let mut client_b2 = WireClient::connect(
        addr,
        device_b,
        "sim-b",
        vec![ClientRole::Operator],
        vec![Feature::Mapping],
    )
    .await?;
    // The rejoin handshake snapshot is consistent with everything emitted
    // while B was away (its own SessionJoined comes after, as an event).
    assert_eq!(client_b2.initial_snapshot.seq, b_last_seq + 3);
    assert_eq!(client_b2.initial_snapshot.mode, Mode::IDLE);
    let rejoin = client_b2.wait_for_own_join().await?;
    assert_eq!(rejoin.seq, b_last_seq + 4);

    // Present the last seen seq; receive exactly the missed envelopes,
    // contiguous and in order.
    client_b2.send_resync_request(b_last_seq).await?;
    let mut replayed = Vec::new();
    for _ in 0..4 {
        replayed.push(client_b2.next_event().await?);
    }
    let seqs: Vec<u64> = replayed.iter().map(|envelope| envelope.seq).collect();
    assert_eq!(
        seqs,
        vec![
            b_last_seq + 1,
            b_last_seq + 2,
            b_last_seq + 3,
            b_last_seq + 4
        ]
    );
    assert!(matches!(
        &replayed[0].event,
        StateEvent::SessionLeft { session_id, reason }
            if *session_id == old_b_session
                && reason.as_deref() == Some("simulated disconnect")
    ));
    assert!(matches!(
        &replayed[1].event,
        StateEvent::MappingLockChanged { mapping_id: id, lock: true } if *id == mapping_id
    ));
    assert!(matches!(
        &replayed[2].event,
        StateEvent::ModeChanged { mode } if *mode == Mode::IDLE
    ));
    assert!(matches!(
        &replayed[3].event,
        StateEvent::SessionJoined { session_id, .. } if *session_id == client_b2.session_id
    ));
    // Nothing beyond the gap was replayed: the ordered control stream yields
    // the Pong for our ping as the very next message.
    client_b2.ping_expect_pong().await?;

    // --- 4. A fresh player's handshake: full world snapshot BEFORE activation. ---
    let device_c = Uuid::now_v7();
    let mut client_c = WireClient::connect(
        addr,
        device_c,
        "sim-c",
        vec![ClientRole::Operator],
        vec![Feature::Mapping],
    )
    .await?;
    let snapshot = &client_c.initial_snapshot;

    // Scene graph.
    assert_eq!(snapshot.scenes.len(), 1);
    let scene_snapshot = &snapshot.scenes[0];
    assert_eq!(scene_snapshot.scene_id, scene_id);
    assert_eq!(scene_snapshot.name, "stage");
    assert_eq!(scene_snapshot.objects.len(), 1);
    let object_snapshot = &scene_snapshot.objects[0];
    assert_eq!(object_snapshot.object_id, object_id);
    assert_eq!(object_snapshot.name, "camera");
    assert_eq!(object_snapshot.attributes.len(), 1);
    assert_eq!(object_snapshot.attributes[0].name, "position");
    assert!(matches!(
        object_snapshot.attributes[0].current_value,
        BakeAttributeValue::Vec3f(_)
    ));
    // Mappings and mode.
    assert_eq!(snapshot.mappings.len(), 1);
    assert_eq!(snapshot.mappings[0].mapping_id, mapping_id);
    assert!(snapshot.mappings[0].lock);
    assert_eq!(snapshot.mode, Mode::IDLE);
    assert_eq!(snapshot.active_scene, Some(scene_id));
    // Sessions, takes, playback: the domains replicated only via events are
    // recovered by the snapshot too. C sees the host, A, B's new session, and
    // itself (it registered before the snapshot); B's superseded session is
    // gone because its SessionLeft already fired.
    let session_ids: Vec<Uuid> = snapshot.sessions.iter().map(|s| s.session_id).collect();
    assert_eq!(
        snapshot.sessions.len(),
        4,
        "sessions: {:?}",
        snapshot.sessions
    );
    assert!(session_ids.contains(&server.host_session_id()));
    assert!(session_ids.contains(&client_a.session_id));
    assert!(session_ids.contains(&client_b2.session_id));
    assert!(session_ids.contains(&client_c.session_id));
    assert!(!session_ids.contains(&old_b_session));
    let hosts: Vec<_> = snapshot.sessions.iter().filter(|s| s.is_host).collect();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].device_name, "host");
    assert!(snapshot
        .sessions
        .iter()
        .any(|s| s.session_id == client_a.session_id && s.device_id == device_a && !s.is_host));
    // No takes were recorded and no playback is loaded in this scenario.
    assert!(snapshot.takes.is_empty());
    assert!(snapshot.playback.is_none());

    // The snapshot preceded activation: C's own SessionJoined is the next
    // event on the stream and carries the first seq after the snapshot.
    let snapshot_seq = snapshot.seq;
    let join_c = client_c.next_event().await?;
    assert_eq!(join_c.seq, snapshot_seq + 1);
    assert!(matches!(
        &join_c.event,
        StateEvent::SessionJoined { session_id, .. } if *session_id == client_c.session_id
    ));

    drop(client_a);
    drop(client_b2);
    drop(client_c);
    server.stop().await?;
    Ok(())
}
