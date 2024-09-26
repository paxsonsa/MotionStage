use std::sync::{Arc, Mutex};

use super::*;

struct DummyBackend {}

impl backend::Backend for DummyBackend {
    async fn reserve_device_id(&mut self) -> u32 {
        return 33;
    }

    async fn apply(
        &mut self,
        client: u32,
        message: protocol::client_message::Body,
    ) -> core::prelude::Result<()> {
        Ok(())
    }

    async fn update(&mut self) -> core::prelude::Result<core::state::StateTree> {
        todo!()
    }

    async fn remove_client(&mut self, client: u32) -> core::prelude::Result<()> {
        todo!()
    }
}

struct MockConnection {
    id: u32,
    messages: Arc<Mutex<Vec<protocol::ServerMessage>>>,
}

impl ConnectionHandle for MockConnection {
    fn id(&self) -> u32 {
        self.id
    }
    async fn send(&mut self, message: protocol::ServerMessage) {
        self.messages.lock().unwrap().push(message)
    }
}
#[tokio::test]
async fn test_new_connection_creation() {
    let dummy_backend = DummyBackend {};
    let mut engine = spawn(dummy_backend);
    let messages = Arc::new(Mutex::new(vec![]));
    engine
        .register_client(&mut Box::new(MockConnection {
            id: 22,
            messages: messages.clone(),
        }))
        .await;

    let msgs = messages.lock().unwrap();
    assert_eq!(msgs.len(), 1, "expected one message to be sent");

    let device_id = msgs[0]
        .body
        .as_ref()
        .and_then(|b| match b {
            protocol::server_message::Body::DeviceInit(init) => Some(init),
            _ => None,
        })
        .expect("expected DeviceInit message")
        .id;
    assert_eq!(device_id, 33, "expected device id to be 33");
}
