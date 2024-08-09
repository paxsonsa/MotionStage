use std::sync::Arc;

use async_trait::async_trait;
use cinemotion_core::protocol;
use derive_more::{Display, Error};
use futures::channel::mpsc;
use futures::stream::FusedStream;
use futures::SinkExt;
use tokio::sync::mpsc as tokio_mpsc;
use tokio::sync::Mutex;

use super::*;
use crate::actor;
use crate::engine;

/// Custom error for timeout
#[derive(Debug, Display, Error)]
struct TimeoutElapsed {}

/// Test actor struct to simulate actor behavior
struct TestActor<M>
where
    M: actor::Actor + 'static,
{
    pub model: M,
    receiver: tokio_mpsc::UnboundedReceiver<actor::Event<M::Message>>,
}

impl<M> TestActor<M>
where
    M: actor::Actor + 'static,
{
    /// Create a new TestActor
    pub fn new<F: FnOnce(actor::Sender<M::Message>) -> M::Handle>(
        model: M,
        handle_fn: F,
    ) -> (Self, M::Handle) {
        let (sender, receiver) = tokio_mpsc::unbounded_channel();
        let actor = Self { model, receiver };
        (actor, handle_fn(sender.into()))
    }

    /// Execute the actor loop and wait for the given future to complete or timeout.
    pub async fn wait_for<F, T>(
        &mut self,
        future: F,
        timeout_secs: Option<u64>,
    ) -> Result<T, TimeoutElapsed>
    where
        F: futures::Future<Output = T>,
    {
        let timeout_secs = timeout_secs.unwrap_or(3);
        let timeout = tokio::time::sleep(tokio::time::Duration::from_secs(timeout_secs));
        tokio::pin!(timeout);
        tokio::pin!(future);

        loop {
            tokio::select! {
                Some(event) = self.receiver.recv() => {
                    match event {
                        actor::Event::Stop { respond_to } => {
                            respond_to.send(()).unwrap();
                            break;
                        }
                        actor::Event::Message(message) => {
                            if let Some(actor::Signal::Stop) = self.model.handle_message(message).await {
                                break;
                            }
                        }
                    }
                }
                signal = self.model.tick() => {
                    if let Some(actor::Signal::Stop) = signal {
                        break;
                    }
                }
                _ = &mut timeout => {
                    panic!("Timeout");
                }
                result = &mut future => {
                    return Ok(result);
                }
            }
        }

        // Handle case where the loop breaks but we are still waiting on the future.
        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        match tokio::time::timeout(timeout, future).await {
            Ok(result) => Ok(result),
            Err(_) => Err(TimeoutElapsed {}),
        }
    }

    /// Step through the actor loop with a timeout
    pub async fn step(&mut self, timeout_secs: Option<u64>) -> Result<(), TimeoutElapsed> {
        let timeout_secs = timeout_secs.unwrap_or(3);
        let timeout = tokio::time::sleep(tokio::time::Duration::from_secs(timeout_secs));
        tokio::pin!(timeout);

        tokio::select! {
            Some(event) = self.receiver.recv() => {
                match event {
                    actor::Event::Stop { respond_to } => {
                        respond_to.send(()).unwrap();
                        Ok(())
                    }
                    actor::Event::Message(message) => {
                        if let Some(actor::Signal::Stop) = self.model.handle_message(message).await {
                            return Ok(());
                        }
                        return Ok(());
                    }
                }
            }
            signal = self.model.tick() => {
                if let Some(actor::Signal::Stop) = signal {
                    Ok(())
                } else {
                    Ok(())
                }
            }
            _ = &mut timeout => {
                panic!("Timeout");
            }
        }
    }
}

#[derive(Default)]
struct EngineSpy {
    pub apply_call_count: usize,
}

/// Fake engine for testing purposes
#[derive(Default)]
struct FakeEngine {
    pub spy: Arc<Mutex<EngineSpy>>,
}

#[async_trait]
impl engine::EngineResource for FakeEngine {
    async fn apply(
        &mut self,
        _client_id: u32,
        _message: protocol::client_message::Body,
    ) -> Result<(), engine::EngineError> {
        self.spy.lock().await.apply_call_count += 1;
        Ok(())
    }

    async fn add_client(&self, _client: client::ClientHandle) -> Result<(), engine::EngineError> {
        Ok(())
    }

    async fn remove_client(&self, _id: u32) -> Result<(), engine::EngineError> {
        Ok(())
    }
}

/// Struct to hold channel handles
struct ChannelHandles {
    server_rx: mpsc::UnboundedReceiver<protocol::ServerMessage>,
    websocket_tx: mpsc::UnboundedSender<protocol::ClientMessage>,
}

/// Initialize channels for testing
fn initialize_channels() -> (
    ChannelHandles,
    mpsc::UnboundedSender<protocol::ServerMessage>,
    mpsc::UnboundedReceiver<protocol::ClientMessage>,
    FakeEngine,
) {
    // Create a pair of channels for receiving messages from a fake network client.
    let (client_sender, server_rx) = mpsc::unbounded();

    // Create a pair of channels for sending messages to the websocket.
    let (websocket_tx, websocket_rx) = mpsc::unbounded();

    let engine_handle = FakeEngine::default();

    // Create the struct with the channels.
    let handles = ChannelHandles {
        server_rx,
        websocket_tx,
    };

    (handles, client_sender, websocket_rx, engine_handle)
}

/// Test that the client initializes successfully
#[tokio::test]
async fn test_client_initialization() {
    let (mut handles, client_sender, ws_receiver, engine_handle) = initialize_channels();
    let receive_fn = move |msg: protocol::ClientMessage| Ok(msg);
    let send_fn = move |msg| msg;

    let client = client::Client::new(
        ws_receiver,
        client_sender,
        engine_handle,
        send_fn,
        receive_fn,
    );
    let id = client.id();
    let (mut test_actor, handle) = TestActor::new(client, |sender| ClientHandle { id, sender });

    test_actor
        .wait_for(handle.initialize(), None)
        .await
        .unwrap()
        .expect("client should initialize successfully.");

    let message = handles
        .server_rx
        .try_next()
        .expect("message should be present")
        .unwrap();

    assert!(matches!(
        message.body.unwrap(),
        protocol::server_message::Body::DeviceInit(protocol::DeviceInit { .. })
    ));

    let init_ack = protocol::ClientMessage {
        body: Some(protocol::client_message::Body::DeviceInitAck(
            protocol::DeviceInitAck {
                device_spec: Some(protocol::DeviceSpec {
                    name: "deviceA".to_string(),
                    attributes: Default::default(),
                }),
            },
        )),
    };
    handles.websocket_tx.send(init_ack).await.unwrap();
    test_actor.step(None).await.unwrap();

    let state = test_actor
        .wait_for(handle.state(), None)
        .await
        .unwrap()
        .expect("client should return state");
    assert!(matches!(state.status, Status::Ready));
}

/// Test that the client actor initializes successfully
#[tokio::test]
async fn test_client_actor_initialization() {
    let (mut handles, client_sender, ws_receiver, engine_handle) = initialize_channels();
    let receive_fn = move |msg: protocol::ClientMessage| Ok(msg);
    let send_fn = move |msg| msg;

    let mut client = client::Client::new(
        ws_receiver,
        client_sender,
        engine_handle,
        send_fn,
        receive_fn,
    );
    client
        .initialize()
        .await
        .expect("client should initialize successfully.");
    let msg = handles
        .server_rx
        .try_next()
        .expect("message should be present")
        .unwrap();
    let body = msg.body.expect("body should be present");
    assert!(
        matches!(body, protocol::server_message::Body::DeviceInit(_)),
        "expected DeviceInit message"
    );
}

/// Test that the client actor disconnects successfully
#[tokio::test]
async fn test_client_actor_disconnection() {
    let (_handles, client_sender, ws_receiver, engine_handle) = initialize_channels();
    let receive_fn = move |msg: protocol::ClientMessage| Ok(msg);
    let send_fn = move |msg| msg;

    let mut client = client::Client::new(
        ws_receiver,
        client_sender,
        engine_handle,
        send_fn,
        receive_fn,
    );
    client.state.status = Status::Ready;

    client
        .disconnect()
        .await
        .expect("client should disconnect successfully.");

    assert_eq!(client.state.status, Status::Disconnected);
}

/// Test that the client actor sends a message successfully
#[tokio::test]
async fn test_client_actor_send_message() {
    let (mut handles, client_sender, ws_receiver, engine_handle) = initialize_channels();
    let receive_fn = move |msg: protocol::ClientMessage| Ok(msg);
    let send_fn = move |msg| msg;
    let mut client = client::Client::new(
        ws_receiver,
        client_sender,
        engine_handle,
        send_fn,
        receive_fn,
    );
    client.state.status = Status::Ready;
    let message = protocol::ServerMessage {
        body: Some(protocol::server_message::Body::Ping(protocol::Ping {})),
    };
    client
        .send_message(message.clone())
        .await
        .expect("client should send message successfully.");
    let msg = handles
        .server_rx
        .try_next()
        .expect("message should be present")
        .unwrap();
    assert_eq!(msg, message);
}

/// Test the client actor receive_message() with a bad message raises an error.
#[tokio::test]
async fn test_client_actor_receive_message_with_bad_message() {
    let (_, client_sender, ws_receiver, engine_handle) = initialize_channels();
    let receive_fn = move |msg: protocol::ClientMessage| Ok(msg);
    let send_fn = move |msg| msg;
    let mut client = client::Client::new(
        ws_receiver,
        client_sender,
        engine_handle,
        send_fn,
        receive_fn,
    );
    client.state.status = Status::Ready;
    let message = protocol::ClientMessage { body: None };
    let result = client.receive_message(message.clone()).await;
    assert!(result.is_err());
}

/// Test the client actor receive_message() invokes the engine resource apply.
#[tokio::test]
async fn test_client_actor_receive_message_calls_engine_apply() {
    let (handlers, client_sender, ws_receiver, engine_handle) = initialize_channels();
    let receive_fn = move |msg: protocol::ClientMessage| Ok(msg);
    let send_fn = move |msg| msg;
    let spy = engine_handle.spy.clone();
    let mut client = client::Client::new(
        ws_receiver,
        client_sender,
        engine_handle,
        send_fn,
        receive_fn,
    );
    client.state.status = Status::Ready;
    let message = protocol::ClientMessage {
        body: Some(protocol::client_message::Body::Ping(protocol::Ping {})),
    };
    let _ = client
        .receive_message(message.clone())
        .await
        .expect("should not fail");
    assert!(spy.lock().await.apply_call_count == 1);
    assert!(!handlers.server_rx.is_terminated());
}

/// Test the client actor receive_message() ping message handler sends pong.
#[tokio::test]
async fn test_client_actor_receive_message_ping() {
    let (mut handles, client_sender, ws_receiver, engine_handle) = initialize_channels();
    let receive_fn = move |msg: protocol::ClientMessage| Ok(msg);
    let send_fn = move |msg| msg;
    let mut client = client::Client::new(
        ws_receiver,
        client_sender,
        engine_handle,
        send_fn,
        receive_fn,
    );
    client.state.status = Status::Ready;
    let message = protocol::ClientMessage {
        body: Some(protocol::client_message::Body::Ping(protocol::Ping {})),
    };
    let _ = client
        .receive_message(message)
        .await
        .expect("should not fail");
    let msg = handles.server_rx.try_next().unwrap().unwrap().body.unwrap();
    assert!(matches!(msg, protocol::server_message::Body::Pong(_)));
}
