//! P3 exit-criterion integration test (docs/roadmap-v2.md, P3):
//! "A device with no DCC-side help can: join, browse the scene, create a
//! mapping to a target, go live, record a take, stop, and see the take
//! identity — exercised by an integration test using the simulator client."
//!
//! The test boots a real server whose only host-side involvement is
//! authoring the scene graph, then drives everything else from a simulated
//! QUIC device over the wire operator plane: handshake (initial
//! `SceneSnapshot`), target picking from the snapshot, `CreateMapping`
//! (own-device source), `SetDataFlow` live, `StartTake`, motion datagrams,
//! `StopTake`, and the on-demand `GetSceneSnapshot`. A second observing
//! client must receive the corresponding `StateEvent`s in seq order.
//!
//! A second test pins the permission model: a non-Operator session's attempt
//! to remove another device's mapping is refused with the typed error inside
//! `MappingOpResult`, mutates nothing, and emits no `StateEvent`.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use motionstage_core::{AttributeValue, Scene, SceneAttribute, SceneObject};
use motionstage_protocol::{
    ClientRole, DataFlowState, Feature, Mode, RejectCode, StateEvent, StateEventEnvelope,
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

/// Boot a server with a host-authored one-object scene and a take catalog
/// rooted in `catalog_dir`, and start its QUIC runtime.
async fn boot_stage_server(catalog_dir: &Path) -> Result<(ServerHandle, StageWorld)> {
    let config = ServerConfig {
        quic_bind_addr: "127.0.0.1:0".parse()?,
        enable_discovery: false,
        take_catalog_path: catalog_dir.join("takes_catalog.json"),
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
    // The only host-side help the device gets: an authored world to map into.
    server.load_scene(scene).await;

    server.start().await?;
    Ok((
        server,
        StageWorld {
            scene_id,
            object_id,
        },
    ))
}

struct StageWorld {
    scene_id: Uuid,
    object_id: Uuid,
}

#[tokio::test(flavor = "multi_thread")]
async fn wire_device_runs_mapping_and_take_lifecycle_without_host_help() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (server, world) = boot_stage_server(temp.path()).await?;
    let addr = server.quic_bind_addr().await;

    // --- The device joins: handshake delivers the world snapshot. ---
    let mut device = WireClient::connect(
        addr,
        Uuid::now_v7(),
        "sim-device",
        vec![ClientRole::MotionSource, ClientRole::Operator],
        vec![Feature::Motion, Feature::Mapping, Feature::Recording],
    )
    .await?;
    device.wait_for_own_join().await?;

    // A second observing client joins before any mutation, so every
    // subsequent StateEvent must reach it via replication.
    let mut observer = WireClient::connect(
        addr,
        Uuid::now_v7(),
        "sim-observer",
        vec![ClientRole::Operator],
        vec![Feature::Mapping],
    )
    .await?;
    observer.wait_for_own_join().await?;
    let observer_baseline_seq = observer.last_seq;

    // --- Pick a target object from the handshake snapshot (no host help). ---
    let snapshot = &device.initial_snapshot;
    let active_scene = snapshot
        .active_scene
        .ok_or_else(|| anyhow!("handshake snapshot has no active scene"))?;
    assert_eq!(active_scene, world.scene_id);
    let scene = snapshot
        .scenes
        .iter()
        .find(|scene| scene.scene_id == active_scene)
        .ok_or_else(|| anyhow!("active scene missing from handshake snapshot"))?;
    let target_object = scene
        .objects
        .first()
        .ok_or_else(|| anyhow!("snapshot scene has no objects"))?;
    assert_eq!(target_object.object_id, world.object_id);
    let target_attribute = target_object
        .attributes
        .first()
        .ok_or_else(|| anyhow!("snapshot object has no attributes"))?
        .name
        .clone();
    assert_eq!(target_attribute, "position");

    // --- CreateMapping to the picked target, own-device source (defaults:
    //     source_device = own device, target_scene = active scene). ---
    let mapping = device
        .create_mapping(
            None,
            "pose_pos",
            None,
            target_object.object_id,
            &target_attribute,
            None,
        )
        .await?
        .map_err(|err| anyhow!("CreateMapping refused: {err:?}"))?;
    assert_eq!(mapping.source_device, device.device_id);
    assert_eq!(mapping.source_output, "pose_pos");
    assert_eq!(mapping.target_scene, world.scene_id);
    assert_eq!(mapping.target_object, world.object_id);
    assert_eq!(mapping.target_attribute, "position");
    assert!(!mapping.lock);

    // --- Go live, then start a take (server-assigned identity). ---
    let mode_echo = device.set_data_flow(DataFlowState::Live).await?;
    assert!(matches!(mode_echo.event, StateEvent::ModeChanged { mode } if mode == Mode::LIVE));

    let take_id = device
        .start_take()
        .await?
        .map_err(|err| anyhow!("StartTake refused: {err:?}"))?;

    // --- Push motion datagrams until the server has recorded some frames.
    //     (QUIC datagrams are fire-and-forget; poll the server's ingest
    //     metric purely as test synchronization.) ---
    let mut recorded = 0;
    for i in 0..200_u32 {
        device.send_motion_datagram("pose_pos", [i as f32, 0.0, 0.0], now_ns())?;
        recorded = server.metrics().await.motion_updates;
        if recorded >= 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        recorded >= 5,
        "server ingested only {recorded} motion updates"
    );

    // --- StopTake returns the registered take identity. ---
    let take = device
        .stop_take()
        .await?
        .map_err(|err| anyhow!("StopTake refused: {err:?}"))?;
    assert_eq!(take.take_id, take_id);
    assert_eq!(take.scene_id, world.scene_id);
    assert_eq!(take.name, "Take 001");
    assert!(
        take.frame_count >= 1,
        "registered take recorded no frames: {take:?}"
    );
    // Server-assigned identity: the path lives in the take-catalog directory
    // and was never supplied by the device.
    let take_path = Path::new(&take.path);
    assert_eq!(take_path.parent(), Some(temp.path()));
    let file_name = take_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    assert!(
        file_name.starts_with("take-") && file_name.ends_with(".cmtrk"),
        "unexpected take file name {file_name:?}"
    );
    assert!(
        take_path.exists(),
        "recording file written at {take_path:?}"
    );

    // --- The device sees the take identity in an on-demand snapshot. ---
    let refreshed = device.request_scene_snapshot().await?;
    assert!(
        refreshed.takes.iter().any(|entry| entry.take_id == take_id),
        "take {take_id} missing from on-demand snapshot: {:?}",
        refreshed.takes
    );
    assert_eq!(refreshed.mode, Mode::LIVE);
    assert_eq!(refreshed.mappings.len(), 1);
    assert_eq!(refreshed.mappings[0].mapping_id, mapping.mapping_id);

    // --- The observer received every mutation as replicated StateEvents,
    //     contiguous and in seq order, all stamped with the device session. ---
    let mut observed: Vec<StateEventEnvelope> = Vec::new();
    for _ in 0..7 {
        observed.push(observer.next_event().await?);
    }
    let seqs: Vec<u64> = observed.iter().map(|envelope| envelope.seq).collect();
    let expected_seqs: Vec<u64> = (1..=7)
        .map(|offset| observer_baseline_seq + offset)
        .collect();
    assert_eq!(seqs, expected_seqs, "events not contiguous in seq order");
    for envelope in &observed {
        assert_eq!(
            envelope.origin_session,
            Some(device.session_id),
            "expected device-originated event, got {envelope:?}"
        );
    }
    assert!(matches!(
        &observed[0].event,
        StateEvent::MappingCreated { mapping: created } if created.mapping_id == mapping.mapping_id
    ));
    assert!(matches!(
        &observed[1].event,
        StateEvent::ModeChanged { mode } if *mode == Mode::LIVE
    ));
    assert!(matches!(
        &observed[2].event,
        StateEvent::ModeChanged { mode } if *mode == Mode::RECORDING
    ));
    assert!(matches!(
        &observed[3].event,
        StateEvent::RecordingStarted { take_id: started, scene_id }
            if *started == take_id && *scene_id == world.scene_id
    ));
    assert!(matches!(
        &observed[4].event,
        StateEvent::RecordingStopped { take_id: stopped, frame_count }
            if *stopped == take_id && *frame_count == take.frame_count
    ));
    assert!(matches!(
        &observed[5].event,
        StateEvent::TakeRegistered { take: registered } if registered.take_id == take_id
    ));
    assert!(matches!(
        &observed[6].event,
        StateEvent::ModeChanged { mode } if *mode == Mode::LIVE
    ));

    drop(device);
    drop(observer);
    server.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn foreign_mapping_removal_without_operator_is_refused_with_typed_error() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (server, world) = boot_stage_server(temp.path()).await?;
    let addr = server.quic_bind_addr().await;

    // The mapping owner is deliberately NOT an operator: own-device mapping
    // management must not require the role.
    let mut owner = WireClient::connect(
        addr,
        Uuid::now_v7(),
        "sim-owner",
        vec![ClientRole::MotionSource],
        vec![Feature::Motion, Feature::Mapping],
    )
    .await?;
    owner.wait_for_own_join().await?;

    let mapping = owner
        .create_mapping(None, "pose_pos", None, world.object_id, "position", None)
        .await?
        .map_err(|err| anyhow!("owner CreateMapping refused: {err:?}"))?;

    // A different device, also without the Operator role.
    let mut intruder = WireClient::connect(
        addr,
        Uuid::now_v7(),
        "sim-intruder",
        vec![ClientRole::MotionSource],
        vec![Feature::Motion, Feature::Mapping],
    )
    .await?;
    intruder.wait_for_own_join().await?;
    let seq_before_denial = intruder.last_seq;

    // --- The foreign removal is refused with the typed error... ---
    let denial = intruder
        .remove_mapping(mapping.mapping_id)
        .await?
        .expect_err("foreign RemoveMapping by a non-operator must be refused");
    assert_eq!(denial.code, RejectCode::RoleDenied);
    assert!(
        denial.reason.contains("own device"),
        "unexpected denial reason: {}",
        denial.reason
    );

    // --- ...mutates nothing... ---
    assert_eq!(server.current_event_seq().await, seq_before_denial);
    let snapshot = intruder.request_scene_snapshot().await?;
    assert_eq!(snapshot.mappings.len(), 1);
    assert_eq!(snapshot.mappings[0].mapping_id, mapping.mapping_id);

    // --- ...and emitted no StateEvent: when the owner legitimately removes
    //     its own mapping, the intruder's very next event is that removal,
    //     seq-contiguous with its own join. ---
    owner
        .remove_mapping(mapping.mapping_id)
        .await?
        .map_err(|err| anyhow!("owner RemoveMapping refused: {err:?}"))?;
    let removal = intruder.next_event().await?;
    assert_eq!(removal.seq, seq_before_denial + 1);
    assert_eq!(removal.origin_session, Some(owner.session_id));
    assert!(matches!(
        &removal.event,
        StateEvent::MappingRemoved { mapping_id } if *mapping_id == mapping.mapping_id
    ));

    drop(owner);
    drop(intruder);
    server.stop().await?;
    Ok(())
}

/// Finding (security): permission checks bind to the SERVER's session record,
/// not to identity re-sent on the wire. A non-Operator session cannot manage
/// another device's mapping even if it NAMES the victim's `device_id` as the
/// request's `source_device` — the actor's device and Operator status are read
/// from `SessionInfo`, so a request-supplied `source_device` is only usable
/// when it equals the session's own device (or the session is Operator).
#[tokio::test(flavor = "multi_thread")]
async fn non_operator_cannot_manage_foreign_device_by_naming_it_in_the_request() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (server, world) = boot_stage_server(temp.path()).await?;
    let addr = server.quic_bind_addr().await;

    // The victim owns a mapping on its own device; it is NOT an operator.
    let mut victim = WireClient::connect(
        addr,
        Uuid::now_v7(),
        "sim-victim",
        vec![ClientRole::MotionSource],
        vec![Feature::Motion, Feature::Mapping],
    )
    .await?;
    victim.wait_for_own_join().await?;
    let victim_mapping = victim
        .create_mapping(None, "pose_pos", None, world.object_id, "position", None)
        .await?
        .map_err(|err| anyhow!("victim CreateMapping refused: {err:?}"))?;

    // The intruder is also a non-Operator session on a different device.
    let mut intruder = WireClient::connect(
        addr,
        Uuid::now_v7(),
        "sim-intruder",
        vec![ClientRole::MotionSource],
        vec![Feature::Motion, Feature::Mapping],
    )
    .await?;
    intruder.wait_for_own_join().await?;
    let seq_before = server.current_event_seq().await;

    // (1) CreateMapping naming the VICTIM's device as source_device: refused,
    //     because the intruder's identity (non-operator, own device) comes from
    //     its session record — not from the source_device it puts on the wire.
    let create_denial = intruder
        .create_mapping(
            Some(victim.device_id),
            "pose_pos",
            None,
            world.object_id,
            "position",
            None,
        )
        .await?
        .expect_err("naming the victim's device must not let a non-operator create for it");
    assert_eq!(create_denial.code, RejectCode::RoleDenied);
    assert!(
        create_denial.reason.contains("own device"),
        "unexpected denial reason: {}",
        create_denial.reason
    );

    // (2) UpdateMapping of the victim's mapping, naming the victim as source:
    //     also refused (own-source rule reads the session record).
    let update_denial = intruder
        .update_mapping(
            victim_mapping.mapping_id,
            Some(victim.device_id),
            "pose_pos",
            None,
            world.object_id,
            "position",
            None,
        )
        .await?
        .expect_err("naming the victim's device must not let a non-operator update its mapping");
    assert_eq!(update_denial.code, RejectCode::RoleDenied);

    // Nothing mutated: no event was emitted by either refused request, and the
    // victim's mapping is unchanged.
    assert_eq!(server.current_event_seq().await, seq_before);
    let snapshot = intruder.request_scene_snapshot().await?;
    assert_eq!(snapshot.mappings.len(), 1);
    assert_eq!(snapshot.mappings[0].mapping_id, victim_mapping.mapping_id);
    assert_eq!(snapshot.mappings[0].source_device, victim.device_id);

    drop(victim);
    drop(intruder);
    server.stop().await?;
    Ok(())
}
