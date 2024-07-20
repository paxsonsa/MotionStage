use derive_more::{Display, Error};
use futures::SinkExt;

use super::*;
use crate::actor::{self, Handle};
use crate::engine;
use cinemotion_core::protocol;

#[derive(Debug, Display, Error)]
struct TimeoutElapsed {}

struct TestActor<M>
where
    M: actor::Actor + 'static,
{
    pub model: M,
    receiver: tokio::sync::mpsc::UnboundedReceiver<actor::Event<M::Message>>,
}

impl<M> TestActor<M>
where
    M: actor::Actor + 'static,
{
    pub fn new<F: FnOnce(actor::Sender<M::Message>) -> M::Handle>(
        model: M,
        handle_fn: F,
    ) -> (Self, M::Handle) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
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
                    match signal {
                        Some(actor::Signal::Stop) => break,
                        _ => {}
                    }
                }
                _ = &mut timeout => {
                    panic!("Timeout");
                }
                result = &mut future => { return Ok(result) },
            }
        }
        // Handle case where the loop breaks but we are still waiting on the future.
        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        match tokio::time::timeout(timeout, future).await {
            Ok(result) => Ok(result),
            Err(_) => Err(TimeoutElapsed {}),
        }
    }

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
                match signal {
                    Some(actor::Signal::Stop) => Ok(()),
                    _ => {Ok(())}
                }
            }
            _ = &mut timeout => {
                panic!("Timeout");
            }
        }
    }
}

/// Test that the client initializes successfully
#[tokio::test]
async fn test_client_initialization() {
    // Create a pair of channels for sending/receiver messages form the client itself.
    let (client_sender, mut client_receiver) = futures::channel::mpsc::unbounded();

    // Crate a pair of channels for sending/receiving messages from the websocket.
    let (mut ws_sender, ws_receiver) = futures::channel::mpsc::unbounded();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let engine_ = engine::EngineHandle::new(tx.into());

    let receive_fn = move |msg: protocol::ClientMessage| Ok(msg);
    let send_fn = move |msg| msg;

    let mut client = client::Client::new(ws_receiver, client_sender, engine_, send_fn, receive_fn);
    let id = client.id();
    let (mut test_actor, mut handle) = TestActor::new(client, |sender| ClientHandle { id, sender });

    test_actor
        .wait_for(handle.initialize(), None)
        .await
        .unwrap()
        .expect("client should initialize successfully.");

    let message = client_receiver
        .try_next()
        .expect("message should be present")
        .unwrap();

    assert!(matches!(
        message.body.unwrap(),
        protocol::server_message::Body::Initialize(protocol::Initialize { .. })
    ));

    let init_ack = protocol::ClientMessage {
        body: Some(protocol::client_message::Body::InitializeAck(
            protocol::InitializeAck {},
        )),
    };
    ws_sender.send(init_ack).await.unwrap();
    test_actor.step(None).await.unwrap();

    let state = test_actor
        .wait_for(handle.state(), None)
        .await
        .unwrap()
        .expect("client should return state");
    assert!(matches!(state.status, Status::Ready));
}
