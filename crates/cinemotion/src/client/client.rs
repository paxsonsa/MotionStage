use async_trait::async_trait;
use tokio::net::TcpStream;

use super::coordinator;
use crate::actor;
use crate::engine;

#[derive(Debug, Clone)]
pub enum Message {}

use futures::sink::SinkExt;
use futures::stream::StreamExt;
use tokio_tungstenite::tungstenite;

/// `Client` is a generic struct that represents a client in the system.
/// It is parameterized over two types: `R` and `W`.
/// `R` is a type that implements `StreamExt` with `Item` being a `Result` of `tungstenite::Message`.
/// `W` is a type that implements `SinkExt` with `Item` being a `tungstenite::Message`.
/// The `Client` struct also contains an `engine::EngineHandle` and a `coordinator::ClientCoordinatorHandle`.
///
/// The `new` function is used to create a new instance of `Client`.
///
/// The `Client` struct also implements the `Actor` trait from the `actor` module.
/// The `Message` associated type is set to `Message` and the `Handle` associated type is set to `ClientHandle`.
/// The `handle_message` function is an asynchronous function that handles incoming messages.
pub struct Client<R, W> {
    reader: R,
    writer: W,
    engine: engine::EngineHandle,
    coordinator: coordinator::ClientCoordinatorHandle,
}

impl<R, W> Client<R, W>
where
    R: StreamExt<Item = tungstenite::Result<tungstenite::Message>> + Unpin,
    W: SinkExt<tungstenite::Message> + Unpin,
{
    pub fn new(
        reader: R,
        writer: W,
        engine: engine::EngineHandle,
        coordinator: coordinator::ClientCoordinatorHandle,
    ) -> Self {
        Self {
            reader,
            writer,
            engine,
            coordinator,
        }
    }
}

#[async_trait]
impl<R, W> actor::Actor for Client<R, W>
where
    R: StreamExt<Item = tungstenite::Result<tungstenite::Message>> + Unpin + Send + Sync,
    W: SinkExt<tungstenite::Message> + Unpin + Send + Sync,
{
    type Message = Message;
    type Handle = ClientHandle;
    async fn handle_message(&mut self, message: Self::Message) -> Option<actor::Signal> {
        return None;
    }
}
#[derive(Debug, Clone)]
pub struct ClientHandle {
    sender: actor::Sender<Message>,
}

impl actor::Handle for ClientHandle {
    type Message = Message;
    fn new(sender: actor::Sender<Message>) -> Self {
        Self { sender }
    }
    fn sender(&self) -> actor::Sender<Message> {
        self.sender.clone()
    }
}

pub fn spawn_websocket_client(
    ws_stream: tokio_tungstenite::WebSocketStream<TcpStream>,
    coordinator: coordinator::ClientCoordinatorHandle,
    engine: engine::EngineHandle,
) -> ClientHandle {
    use crate::actor::Actor;
    let (writer, reader) = ws_stream.split();
    let mut model = Client::new(reader, writer, engine, coordinator);
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<_>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                // Handle when a new message is received via websocket
                recv = model.reader.next() => {},

                // Handle when a new message is received via the actor system
                recv = receiver.recv() => match recv {
                    Some(event) => match event {
                        actor::Event::Stop { respond_to } => {
                            respond_to.send(()).unwrap();
                            break;
                        }
                        actor::Event::Message(message) => {
                            if let Some(signal) = model.handle_message(message).await {
                                match signal {
                                    actor::Signal::Stop => break,
                                }
                            }
                        }
                    },
                    None => break,
                }
            }
        }
    });
    ClientHandle {
        sender: sender.into(),
    }
}
