use actor::Actor;

use super::*;

#[tokio::test]
async fn test_engine_add_client() {
    let engine = core::engine::Engine::new();
    let mut engine = EngineActor {
        clients: HashMap::new(),
        inner: engine,
    };

    let (client_sender, mut client_receiver) =
        tokio::sync::mpsc::unbounded_channel::<actor::Event<client::Message>>();
    let handle = tokio::spawn(async move {
        let message = client_receiver.recv().await.unwrap();
        let actor::Event::Message(res) = message else {
            panic!("Expected Message");
        };
        let client::Message::Init {
            response,
            assigned_id,
        } = res
        else {
            panic!("Expected Init");
        };

        assert!(assigned_id == 0);
        // Send Success.
        response.dispatch(Ok(())).await;
    });
    let (message, response) = Message::add_client(client::ClientHandle {
        sender: client_sender.into(),
    });
    assert!(engine.handle_message(message).await.is_none());
    response.await.unwrap().expect("should not error");

    println!("{:?}", engine.clients);
    assert!(engine.clients.contains_key(&0));
    handle.abort();
}

#[tokio::test]
async fn test_engine_remove_client() {
    let engine = core::engine::Engine::new();
    let mut engine = EngineActor {
        clients: HashMap::new(),
        inner: engine,
    };

    let (client_sender, _client_receiver) =
        tokio::sync::mpsc::unbounded_channel::<actor::Event<client::Message>>();
    let handle = client::ClientHandle {
        sender: client_sender.into(),
    };
    engine.clients.insert(1, handle);

    let (message, response) = Message::remove_client(1);
    assert!(engine.handle_message(message).await.is_none());
    response.await.unwrap().expect("should not error");
    assert!(!engine.clients.contains_key(&1));
}

#[tokio::test]
async fn test_engine_apply() {
    let engine = core::engine::Engine::new();
    let mut engine = EngineActor {
        clients: HashMap::new(),
        inner: engine,
    };

    let (message, response) =
        Message::apply(1, protocol::client_message::Body::Ping(protocol::Ping {}));
    assert!(engine.handle_message(message).await.is_none());
    response.await.unwrap().expect("response should succeed");
}
