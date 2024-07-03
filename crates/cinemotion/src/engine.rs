use crate::actor;
use crate::client::coordinator::ClientCoordinatorHandle;

use async_trait::async_trait;

#[derive(Clone, Debug)]
pub struct EngineHandle {
    sender: actor::Sender<String>,
}

impl EngineHandle {
    pub async fn stop(&self) {}
}

impl actor::Handle for EngineHandle {
    type Message = String;

    fn new(sender: actor::Sender<Self::Message>) -> Self {
        Self { sender }
    }

    fn sender(&self) -> actor::Sender<Self::Message> {
        self.sender.clone()
    }
}

pub struct EngineActor {}

#[async_trait]
impl actor::Actor for EngineActor {
    type Message = String;
    type Handle = EngineHandle;
    async fn handle_message(&mut self, message: Self::Message) -> Option<actor::Signal> {
        todo!()
    }
}

pub fn spawn(client_coordinator: ClientCoordinatorHandle) -> EngineHandle {
    let engine = EngineActor {};
    actor::spawn(engine)
}
