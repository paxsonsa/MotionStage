use std::collections::HashMap;

use super::ClientHandle;
use crate::actor;
use crate::{Error, Result};
use async_trait::async_trait;

#[derive(Clone, Debug)]
pub enum Message {
    Register {
        client: ClientHandle,
        respond_to: actor::Responder<u32>,
    },
}

#[derive(Clone, Debug)]
pub struct ClientCoordinatorHandle {
    sender: actor::Sender<Message>,
}

#[async_trait]
impl actor::Handle for ClientCoordinatorHandle {
    type Message = Message;

    fn new(sender: actor::Sender<Message>) -> Self {
        Self { sender }
    }

    fn sender(&self) -> actor::Sender<Message> {
        self.sender.clone()
    }
}

impl ClientCoordinatorHandle {
    pub async fn register(&self, client: ClientHandle) -> Result<u32> {
        let (responder, response) = actor::Response::new();
        if let Err(err) = self.sender.send(Message::Register {
            client,
            respond_to: responder,
        }) {
            tracing::error!("failed to send message to client coordinator: {}", err);
            return Err(actor::Error::SendError.into());
        }
        response.await.map_err(Error::ActorError)
    }
}

pub fn spawn() -> ClientCoordinatorHandle {
    let actor = ClientCoordinator {
        clients: Default::default(),
    };
    actor::spawn(actor)
}
pub(super) struct ClientCoordinator {
    pub(super) clients: HashMap<u32, ClientHandle>,
}

impl ClientCoordinator {
    pub(super) fn new() -> Self {
        Self {
            clients: Default::default(),
        }
    }
}

#[async_trait]
impl actor::Actor for ClientCoordinator {
    type Message = Message;
    type Handle = ClientCoordinatorHandle;

    async fn handle_message(&mut self, message: Self::Message) -> Option<actor::Signal> {
        todo!()
    }
}
