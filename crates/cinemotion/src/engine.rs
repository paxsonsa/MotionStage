use async_trait::async_trait;

use cinemotion_core as core;

use crate::actor;
use crate::actor::HandleExt;
use crate::client;

#[derive(Clone, Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine is fatally failed")]
    EngineFailed,
    #[error("failed while processing message: {0}")]
    MessageFailed(String),
}

#[derive(Debug, Clone)]
pub enum Message {
    AddClient {
        client: client::ClientHandle,
        responder: actor::Responder<Result<(), EngineError>>,
    },
    RemoveClient {
        id: i32,
        responder: actor::Responder<Result<(), EngineError>>,
    },
    Apply {
        client_id: i32,
        message: core::protocol::client_message::Body,
        responder: actor::Responder<Result<(), EngineError>>,
    },
}

impl Message {
    pub fn apply(
        client_id: i32,
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

    pub fn remove_client(id: i32) -> (Self, actor::Response<Result<(), EngineError>>) {
        let (responder, response) = actor::Response::new();
        (Self::RemoveClient { id, responder }, response)
    }
}

#[derive(Clone, Debug)]
pub struct EngineHandle {
    sender: actor::Sender<Message>,
}

impl EngineHandle {
    pub(crate) fn new(sender: actor::Sender<Message>) -> Self {
        Self { sender }
    }

    pub async fn apply(
        &self,
        client_id: i32,
        message: core::protocol::client_message::Body,
    ) -> Result<(), EngineError> {
        self.perform_send(|| Message::apply(client_id, message))
            .await
    }

    pub async fn add_client(&self, client: client::ClientHandle) -> Result<(), EngineError> {
        self.perform_send(|| Message::add_client(client)).await
    }

    pub async fn remove_client(&self, id: i32) -> Result<(), EngineError> {
        self.perform_send(|| Message::remove_client(id)).await
    }
}

impl actor::Handle for EngineHandle {
    type Message = Message;

    fn sender(&self) -> actor::Sender<Self::Message> {
        self.sender.clone()
    }
}

impl actor::HandleExt for EngineHandle {}

pub struct EngineActor {
    clients: std::collections::HashMap<i32, client::ClientHandle>,
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
            } => match self.inner.apply(client_id, message).await {
                Ok(_) => {
                    responder.dispatch(Ok(())).await;
                    return None;
                }
                Err(err) => {
                    tracing::error!(?err, "failed to apply message");
                }
            },
            Message::AddClient { client, responder } => {
                self.clients.insert(client.id(), client.clone());

                match client.initialize().await {
                    Ok(_) => {
                        let _ = responder.try_dispatch(Ok(())).await;
                    }
                    Err(e) => {
                        responder
                            .dispatch(Err(EngineError::MessageFailed(format!("{e:?}"))))
                            .await;
                    }
                }
            }
            Message::RemoveClient { id, responder } => {
                // TODO: Remove client device from engine as well.
                self.clients.remove(&id);
                responder.dispatch(Ok(())).await;
            }
        };
        None
    }
}

pub fn spawn() -> EngineHandle {
    let engine = EngineActor {
        clients: std::collections::HashMap::new(),
        inner: core::engine::Engine::new(),
    };
    actor::spawn(engine, EngineHandle::new)
}
