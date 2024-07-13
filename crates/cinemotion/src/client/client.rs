use std::sync::atomic::{AtomicU16, Ordering};

use async_trait::async_trait;
use thiserror::Error;
use tokio::net::TcpStream;

use crate::actor::{self, HandleExt};
use crate::engine;
use crate::protocol;

static NEXT_ID: AtomicU16 = AtomicU16::new(0);

#[derive(Debug, Clone)]
pub enum Message {
    Init(actor::Responder<Result<(), ClientError>>),
}

impl Message {
    pub fn init() -> (Self, actor::Response<Result<(), ClientError>>) {
        let (responder, response) = actor::Response::new();
        (Self::Init(responder), response)
    }
}

use futures::sink::SinkExt;
use futures::stream::StreamExt;
use tokio_tungstenite::tungstenite;

/// `ClientError` is an enumeration that is intended to represent
/// different types of errors that can occur within the client.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum ClientError {
    #[error("failed to send message to actor")]
    SendError,

    #[error(transparent)]
    ActorError(#[from] actor::ActorError),
}

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
    id: u16,
    reader: R,
    writer: W,
    engine: engine::EngineHandle,
}

impl<R, W> Client<R, W>
where
    R: StreamExt<Item = tungstenite::Result<tungstenite::Message>> + Unpin,
    W: SinkExt<tungstenite::Message> + Unpin,
{
    pub fn new(reader: R, writer: W, engine: engine::EngineHandle) -> Self {
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::SeqCst),
            reader,
            writer,
            engine,
        }
    }

    pub fn id(&self) -> u16 {
        self.id
    }

    pub async fn disconnect(&self) -> Result<(), ClientError> {
        self.engine.remove_client(self.id);
        Ok(())
    }

    pub async fn send_message(&mut self, message: protocol::Message) -> Result<(), ClientError> {
        let message =
            tungstenite::Message::binary(message.serialize().map_err(|_| ClientError::SendError)?);
        self.writer
            .send(message)
            .await
            .map_err(|_| ClientError::SendError)?;
        Ok(())
    }

    pub async fn receive_message(&mut self, message: protocol::Message) -> Result<(), ClientError> {
        match message {
            protocol::Message::InitializeAck(m) => {
                /*
                 * self.status = ClientStatus::Active
                 * self.engine.process_message(self.id, m).await?;
                 * */
            }
            _ => {
                // self.engine.process_message(self.id, m).await?;
            }
        }
        Ok(())
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
        match message {
            Message::Init(respond_to) => {
                match self
                    .send_message(protocol::Initialize { id: self.id }.into())
                    .await
                {
                    Ok(_) => {
                        respond_to.dispatch(Ok(())).await;
                    }
                    Err(err) => {
                        tracing::error!(?err, "failed to send init message it client");
                        respond_to.dispatch(Err(ClientError::SendError)).await;
                    }
                }
            }
        };
        return None;
    }
}
#[derive(Debug, Clone)]
pub struct ClientHandle {
    pub(super) id: u16,
    pub(super) sender: actor::Sender<Message>,
}

impl ClientHandle {
    /// Returns the unique identifier for the client.
    ///
    /// This function is used to get the unique identifier (ID) for the client.
    pub fn id(&self) -> u16 {
        self.id
    }
    /// Initialize the client connection
    ///
    /// The client needs to initialize before it can respond to any messages
    /// and work with the engine runtime. This should be the first item after the client
    /// is created.
    pub async fn initialize(&self) -> Result<(), ClientError> {
        self.perform_send(|| Message::init()).await
    }
}

impl actor::Handle for ClientHandle {
    type Message = Message;

    fn sender(&self) -> actor::Sender<Message> {
        self.sender.clone()
    }
}

impl actor::HandleExt for ClientHandle {}

pub fn spawn_websocket_client(
    ws_stream: tokio_tungstenite::WebSocketStream<TcpStream>,
    engine: engine::EngineHandle,
) -> ClientHandle {
    use crate::actor::Actor;
    let (writer, reader) = ws_stream.split();
    let mut model = Client::new(reader, writer, engine);
    let id = model.id();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<_>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                // Handle when a new message is received via websocket
                Some(msg) = model.reader.next() => match msg {
                    Ok(msg) => {
                        tracing::debug!("received message: {:?}", msg);
                        if !msg.is_binary() {
                            tracing::warn!("received non-binary message, ignoring");
                            continue;
                        }

                        let data = msg.into_data();
                        let Ok(message) = protocol::Message::deserialize(&data) else {
                            tracing::error!(?data, "failed to deserialize message from websocket");
                            continue;
                        };
                        model.receive_message(message.into()).await.unwrap();

                    },
                    Err(err) => {
                        tracing::error!(?err, "failed to read message from websocket");
                        model.disconnect().await;
                        break;
                    }
                },

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
        id,
        sender: sender.into(),
    }
}
