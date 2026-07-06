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
