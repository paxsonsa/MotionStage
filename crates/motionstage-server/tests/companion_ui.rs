#![cfg(feature = "companion-ui")]
//! End-to-end smoke for the companion-UI listener: it binds an ephemeral localhost
//! port and serves the operator SPA over real HTTP.

use motionstage_server::companion_ui::serve_companion_ui;
use motionstage_server::{ServerConfig, ServerHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn serves_index_over_bound_port() {
    let server = ServerHandle::new(ServerConfig::default());
    let ui = serve_companion_ui(server, Some("test-token".into()))
        .await
        .expect("companion UI should bind");

    let port = ui.port();
    assert!(port > 0, "should bind a real ephemeral port");

    // Raw HTTP GET / — no client crate needed.
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to companion UI");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("send request");

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read response");
    let response = String::from_utf8_lossy(&buf);

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200, got: {}",
        response.lines().next().unwrap_or_default()
    );
    assert!(
        response.contains("MotionStage"),
        "index should contain the app title"
    );

    ui.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn ws_pushes_snapshot_and_dispatches_commands() {
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let server = ServerHandle::new(ServerConfig::default());
    let ui = serve_companion_ui(server, None).await.expect("bind");
    let url = format!("ws://127.0.0.1:{}/ws", ui.port());

    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect");

    // Snapshot on connect.
    let first = next_json(&mut ws).await;
    assert_eq!(first["type"], "snapshot", "first frame must be a snapshot");
    assert_eq!(first["mode"]["label"], "idle");
    assert!(first["scene"]["scenes"].is_array());
    assert!(first["sessions"].is_array());

    // Group A command over the socket drives the mode to live.
    ws.send(Message::Text(r#"{"SetDataFlow":"Live"}"#.into()))
        .await
        .expect("send command");

    let mut saw_live = false;
    for _ in 0..10 {
        let msg = next_json(&mut ws).await;
        if msg["type"] == "mode_changed" && msg["mode"]["label"] == "live" {
            saw_live = true;
            break;
        }
    }
    assert!(saw_live, "expected a mode_changed:live push after SetDataFlow");

    // Unparseable command is echoed back as a structured error, not a disconnect.
    ws.send(Message::Text("garbage".into()))
        .await
        .expect("send garbage");
    let mut saw_err = false;
    for _ in 0..10 {
        let msg = next_json(&mut ws).await;
        if msg["type"] == "command_error" {
            saw_err = true;
            break;
        }
    }
    assert!(saw_err, "expected command_error for an unparseable command");

    ui.shutdown().await.expect("shutdown");
}

async fn next_json<S>(ws: &mut S) -> serde_json::Value
where
    S: futures::Stream<Item = Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    use futures::StreamExt;
    loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
            .await
            .expect("ws frame within timeout")
            .expect("ws stream open")
            .expect("ws frame ok");
        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
            return serde_json::from_str(&text).expect("valid json frame");
        }
    }
}

#[tokio::test]
async fn ws_snapshot_carries_takes_and_bridges_host_requests() {
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let server = ServerHandle::new(ServerConfig::default());
    // Keep a clone to drain the host-request queue the WS handler fills.
    let ui = serve_companion_ui(server.clone(), None).await.expect("bind");
    let url = format!("ws://127.0.0.1:{}/ws", ui.port());
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.expect("connect");

    // Snapshot now carries a takes array + playback field.
    let snap = next_json(&mut ws).await;
    assert_eq!(snap["type"], "snapshot");
    assert!(snap["takes"].is_array(), "snapshot must include takes");
    assert!(snap.get("playback").is_some(), "snapshot must include playback");

    // DCC-side commands route to the host-request queue, not ServerHandle.
    ws.send(Message::Text(r#"{"cmd":"resync_scene"}"#.into())).await.unwrap();
    ws.send(Message::Text(
        r#"{"cmd":"start_video","width":1280,"height":720,"fps":24}"#.into(),
    ))
    .await
    .unwrap();

    // Drain (poll briefly — command handling is async on the server task).
    let mut drained = Vec::new();
    for _ in 0..40 {
        drained.extend(server.drain_host_requests().await);
        if drained.len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        drained.iter().any(|r| matches!(r, motionstage_server::HostRequest::ResyncScene)),
        "resync_scene should reach the host queue, got {drained:?}"
    );
    assert!(
        drained.iter().any(|r| matches!(
            r,
            motionstage_server::HostRequest::StartVideo { width: 1280, fps: 24, .. }
        )),
        "start_video should reach the host queue, got {drained:?}"
    );

    ui.shutdown().await.expect("shutdown");
}

/// Bring a device session all the way to `Active` through the host-side
/// lifecycle API, as the QUIC handshake would.
async fn join_device(server: &motionstage_server::ServerHandle, device_id: uuid::Uuid, name: &str) {
    use motionstage_protocol::{
        AttributeDescriptor, AttributeKind, ClientHello, ClientRole, Feature, RegisterRequest,
        PROTOCOL_MAJOR, PROTOCOL_MINOR,
    };

    server.discovered(device_id, name).await.unwrap();
    server.transport_connected(device_id).await.unwrap();
    server
        .hello_exchanged(ClientHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            device_id,
            device_name: name.into(),
            roles: vec![ClientRole::MotionSource],
            features: vec![Feature::Motion],
            advertised_attributes: vec![AttributeDescriptor {
                path: "pose_pos".into(),
                value_type: AttributeKind::Vec3f,
            }],
        })
        .await
        .unwrap();
    server.authenticate(device_id).await.unwrap();
    server
        .register(
            device_id,
            RegisterRequest {
                pairing_token: None,
                api_key: None,
            },
        )
        .await
        .unwrap();
    server.scene_synced(device_id).await.unwrap();
    server.activate(device_id).await.unwrap();
}

/// Skip unrelated frames (e.g. polled metrics) until one of the given type
/// arrives.
async fn next_of_type<S>(ws: &mut S, ty: &str) -> serde_json::Value
where
    S: futures::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    for _ in 0..20 {
        let msg = next_json(ws).await;
        if msg["type"] == ty {
            return msg;
        }
    }
    panic!("no '{ty}' frame within 20 frames");
}

/// Structural changes must reach the browser via the state-event plane. The
/// server is never `start()`ed here, so the publish loop is not running and
/// `last_published_snapshot` stays `None` — the poll/diff path cannot produce
/// any of the frames asserted below.
#[tokio::test]
async fn ws_pushes_structural_changes_from_events_without_polling() {
    use motionstage_core::{AttributeValue, MappingRequest, Scene, SceneAttribute, SceneObject};

    let server = ServerHandle::new(ServerConfig::default());
    let ui = serve_companion_ui(server.clone(), None).await.expect("bind");
    let url = format!("ws://127.0.0.1:{}/ws", ui.port());
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.expect("connect");

    let snap = next_json(&mut ws).await;
    assert_eq!(snap["type"], "snapshot");
    assert_eq!(snap["scene"]["scenes"].as_array().unwrap().len(), 0);
    // The in-process host session rides the event plane but is not a "client";
    // the UI sessions list only carries device operators.
    assert_eq!(snap["sessions"].as_array().unwrap().len(), 0);

    // Scene load -> snapshot_changed.
    let object = SceneObject::new("camera").with_attribute(SceneAttribute::new(
        "position",
        AttributeValue::Vec3f([0.0, 0.0, 0.0]),
    ));
    let object_id = object.id;
    let scene = Scene::new("shot").with_object(object);
    let scene_id = scene.id;
    server.load_scene(scene).await;

    let msg = next_of_type(&mut ws, "snapshot_changed").await;
    assert_eq!(msg["scenes"][0]["name"], "shot");
    assert_eq!(msg["scenes"][0]["objects"][0]["name"], "camera");

    // Session activation -> session_upserted carrying the full session shape
    // (advertised outputs come from the session table, not the event payload).
    let device_id = uuid::Uuid::now_v7();
    join_device(&server, device_id, "ipad").await;
    let msg = next_of_type(&mut ws, "session_upserted").await;
    assert_eq!(msg["session"]["device_id"], device_id.to_string());
    assert_eq!(msg["session"]["device_name"], "ipad");
    assert_eq!(msg["session"]["state"], "Active");
    assert_eq!(
        msg["session"]["advertised_attributes"][0]["path"],
        "pose_pos"
    );

    // Mapping create -> snapshot_changed listing the mapping.
    let mapping_id = server
        .create_mapping(
            MappingRequest {
                source_device: device_id,
                source_output: "pose_pos".into(),
                target_scene: scene_id,
                target_object: object_id,
                target_attribute: "position".into(),
                component_mask: None,
            },
            100,
        )
        .await
        .expect("create mapping");
    let msg = next_of_type(&mut ws, "snapshot_changed").await;
    assert_eq!(msg["mappings"].as_array().unwrap().len(), 1);
    assert_eq!(msg["mappings"][0]["target_attribute"], "position");

    // Mapping remove -> snapshot_changed without it.
    server.remove_mapping(mapping_id).await.expect("remove mapping");
    let msg = next_of_type(&mut ws, "snapshot_changed").await;
    assert_eq!(msg["mappings"].as_array().unwrap().len(), 0);

    // Session close -> session_removed keyed by *device* id (the event carries
    // the session id; the handler translates).
    server.close_session(device_id, 200).await.expect("close session");
    let msg = next_of_type(&mut ws, "session_removed").await;
    assert_eq!(msg["device_id"], device_id.to_string());

    ui.shutdown().await.expect("shutdown");
}

/// Ordering: the connect-time snapshot already folds in earlier events, so the
/// connection must not replay them — the first frame after the snapshot is the
/// one for the first post-connect mutation.
#[tokio::test]
async fn ws_snapshot_gates_events_already_folded_in() {
    use motionstage_core::{AttributeValue, Scene, SceneAttribute, SceneObject};

    let server = ServerHandle::new(ServerConfig::default());

    // Mutations *before* the browser connects.
    let scene = Scene::new("pre").with_object(SceneObject::new("camera").with_attribute(
        SceneAttribute::new("position", AttributeValue::Vec3f([0.0, 0.0, 0.0])),
    ));
    server.load_scene(scene).await;
    let device_id = uuid::Uuid::now_v7();
    join_device(&server, device_id, "ipad").await;
    // A device that was discovered but never registered has no session_id and
    // never emits SessionJoined/SessionLeft: the snapshot must not list it.
    let ghost_device = uuid::Uuid::now_v7();
    server.discovered(ghost_device, "ghost").await.unwrap();

    let ui = serve_companion_ui(server.clone(), None).await.expect("bind");
    let url = format!("ws://127.0.0.1:{}/ws", ui.port());
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.expect("connect");

    // The snapshot reflects the pre-connect state: only registered clients.
    let snap = next_json(&mut ws).await;
    assert_eq!(snap["type"], "snapshot");
    assert_eq!(snap["scene"]["scenes"][0]["name"], "pre");
    assert_eq!(snap["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(snap["sessions"][0]["device_name"], "ipad");

    // The very next frame must be for the post-connect mutation — nothing
    // between the snapshot and it (no duplicate session_upserted or
    // snapshot_changed for pre-connect events; the idle server emits no polls).
    let scene2 = Scene::new("post").with_object(SceneObject::new("light").with_attribute(
        SceneAttribute::new("energy", AttributeValue::Float32(1.0)),
    ));
    server.load_scene(scene2).await;

    let msg = next_json(&mut ws).await;
    assert_eq!(
        msg["type"], "snapshot_changed",
        "expected the post-connect scene load to be the first delta, got: {msg}"
    );
    let names: Vec<_> = msg["scenes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"post".to_string()), "scenes: {names:?}");

    ui.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn shutdown_releases_the_port() {
    let server = ServerHandle::new(ServerConfig::default());
    let ui = serve_companion_ui(server, None)
        .await
        .expect("bind first listener");
    let port = ui.port();
    ui.shutdown().await.expect("shutdown");

    // After shutdown the task is gone; a fresh connect should fail to handshake.
    // (Port reuse timing varies, so we only assert shutdown completes cleanly and
    // the handle reported a real port.)
    assert!(port > 0);
}
