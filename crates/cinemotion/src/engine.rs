use crate::actor;
use crate::actor::{ActorError, HandleExt};
use crate::client;
use crate::perform_send_with_error_handling;
use async_trait::async_trait;
use cinemotion_core as core;
use core::protocol;
use std::collections::HashMap;

#[cfg(test)]
#[path = "engine_test.rs"]
mod engine_test;

/// Errors that can occur in the Engine.
#[derive(Clone, Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine is fatally failed")]
    EngineFailed,
    #[error("failed while processing message: {0}")]
    MessageFailed(String),
    #[error(transparent)]
    ActorError(#[from] actor::ActorError),
}

/// Messages that can be sent to the Engine.
#[derive(Debug, Clone)]
pub enum Message {
    AddClient {
        client: client::ClientHandle,
        responder: actor::Responder<Result<(), EngineError>>,
    },
    RemoveClient {
        id: u32,
        responder: actor::Responder<Result<(), EngineError>>,
    },
    Apply {
        client_id: u32,
        message: core::protocol::client_message::Body,
        responder: actor::Responder<Result<(), EngineError>>,
    },
}

impl Message {
    pub fn apply(
        client_id: u32,
        message: core::protocol::client_message::Body,
    ) -> (Self, actor::Response<Result<(), EngineError>>) {
        let (responder, response) = actor::Response::new();
        (
            Self::Apply {
                client_id,
                message,
                responder,
            },
            response,
        )
    }

    pub fn add_client(
        client: client::ClientHandle,
    ) -> (Self, actor::Response<Result<(), EngineError>>) {
        let (responder, response) = actor::Response::new();
        (Self::AddClient { client, responder }, response)
    }

    pub fn remove_client(id: u32) -> (Self, actor::Response<Result<(), EngineError>>) {
        let (responder, response) = actor::Response::new();
        (Self::RemoveClient { id, responder }, response)
    }
}

/// Trait defining the operations that can be performed on the Engine.
#[async_trait]
pub trait EngineResource: Send + Sync {
    async fn apply(
        &mut self,
        client_id: u32,
        message: protocol::client_message::Body,
    ) -> Result<(), EngineError>;

    async fn add_client(&self, client: client::ClientHandle) -> Result<(), EngineError>;

    async fn remove_client(&self, id: u32) -> Result<(), EngineError>;
}

/// Handle for interacting with the Engine.
#[derive(Clone, Debug)]
pub struct EngineHandle {
    sender: actor::Sender<Message>,
}

impl EngineHandle {
    pub(crate) fn new(sender: actor::Sender<Message>) -> Self {
        Self { sender }
    }
}

#[async_trait]
impl EngineResource for EngineHandle {
    async fn apply(
        &mut self,
        client_id: u32,
        message: core::protocol::client_message::Body,
    ) -> Result<(), EngineError> {
        perform_send_with_error_handling!(self, Message::apply(client_id, message))
    }

    async fn add_client(&self, client: client::ClientHandle) -> Result<(), EngineError> {
        perform_send_with_error_handling!(self, Message::add_client(client))
    }

    async fn remove_client(&self, id: u32) -> Result<(), EngineError> {
        perform_send_with_error_handling!(self, Message::remove_client(id))
    }
}

impl actor::Handle for EngineHandle {
    type Message = Message;

    fn sender(&self) -> actor::Sender<Self::Message> {
        self.sender.clone()
    }
}

impl actor::HandleExt for EngineHandle {}

/// Actor representing the Engine.
pub struct EngineActor {
    clients: HashMap<u32, client::ClientHandle>,
    inner: core::engine::Engine,
}

#[async_trait]
impl actor::Actor for EngineActor {
    type Message = Message;
    type Handle = EngineHandle;

    async fn handle_message(&mut self, message: Self::Message) -> Option<actor::Signal> {
        match message {
            Message::Apply {
                client_id,
                message,
                responder,
            } => {
                if let Err(err) = self.inner.apply(client_id, message).await {
                    tracing::error!(?err, "failed to apply message");
                } else {
                    responder.dispatch(Ok(())).await;
                }
            }
            Message::AddClient { client, responder } => {
                let id = self.inner.reserve_client().await;
                self.clients.insert(id, client.clone());
                if let Err(e) = client.initialize(id).await {
                    responder
                        .dispatch(Err(EngineError::MessageFailed(format!("{e:?}"))))
                        .await;
                } else {
                    let _ = responder.try_dispatch(Ok(())).await;
                }
            }
            Message::RemoveClient { id, responder } => {
                tracing::debug!(id, "removing disconnected client");
                if let Err(err) = self.inner.remove_client(id).await {
                    tracing::error!(id, ?err, "failed to remove the client.");
                }
                self.clients.remove(&id);
                responder.dispatch(Ok(())).await;
            }
        };
        None
    }

    async fn tick(&mut self) -> Option<actor::Signal> {
        match self.inner.update().await {
            Ok(state) => {
                if let Err(err) = broadcast(&self.clients, state).await {
                    tracing::error!(?err, "failed to broadcast state to clients");
                }
                None
            }
            Err(err) => {
                tracing::error!(?err, "failed during engine runtime tick");
                Some(actor::Signal::Stop)
            }
        }
    }
}

/// Spawns a new EngineActor and returns its handle.
pub fn spawn() -> EngineHandle {
    let engine = EngineActor {
        clients: HashMap::new(),
        inner: core::engine::Engine::new(),
    };
    actor::spawn(engine, EngineHandle::new)
}

/// Broadcasts the state to all clients.
async fn broadcast(
    clients: &HashMap<u32, client::ClientHandle>,
    state: core::state::StateTree,
) -> Result<(), EngineError> {
    for client in clients.values() {
        client.send(state.clone().into()).await.map_err(|err| {
            EngineError::MessageFailed(format!(
                "failed to broadcast state to client: err={:?}",
                err
            ))
        })?;
    }
    Ok(())
}
