use crate::protocol;

#[derive(Clone)]
pub struct StateTree {}

impl StateTree {
    pub fn new() -> Self {
        StateTree {}
    }
}

impl From<StateTree> for protocol::ServerMessage {
    fn from(value: StateTree) -> Self {
        protocol::ServerMessage {
            body: Some(protocol::server_message::Body::State(value.into())),
        }
    }
}

impl From<StateTree> for protocol::State {
    fn from(value: StateTree) -> Self {
        protocol::State {}
    }
}
