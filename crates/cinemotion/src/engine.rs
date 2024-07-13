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
        id: u16,
        responder: actor::Responder<Result<(), EngineError>>,
    },
}

impl Message {
    pub fn add_client(
        client: client::ClientHandle,
    ) -> (Self, actor::Response<Result<(), EngineError>>) {
        let (responder, response) = actor::Response::new();
        (Self::AddClient { client, responder }, response)
    }

    pub fn remove_client(id: u16) -> (Self, actor::Response<Result<(), EngineError>>) {
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

    pub async fn add_client(&self, client: client::ClientHandle) -> Result<(), EngineError> {
        self.perform_send(|| Message::add_client(client)).await
    }

    pub async fn remove_client(&self, id: u16) -> Result<(), EngineError> {
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
    clients: std::collections::HashMap<u16, client::ClientHandle>,
    inner: core::engine::Engine,
}

#[async_trait]
impl actor::Actor for EngineActor {
    type Message = Message;
    type Handle = EngineHandle;

    async fn handle_message(&mut self, message: Self::Message) -> Option<actor::Signal> {
        match message {
            Message::AddClient { client, responder } => {
                self.clients.insert(client.id(), client.clone());

                match client.initialize().await {
                    Ok(_) => {
                        responder
                            .dispatch(Ok(()))
                            .await
                            .expect("response receiver should not close.");
                    }
                    Err(e) => {
                        responder
                            .dispatch(Err(EngineError::MessageFailed(format!("{e:?}"))))
                            .await
                            .expect("response receiver should not close.");
                    }
                }
            }
            Message::RemoveClient { id, responder } => {
                // TODO: Remove client device from engine as well.
                self.clients.remove(&id);
                responder
                    .dispatch(Ok(()))
                    .await
                    .expect("response receiver should not close.");
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
