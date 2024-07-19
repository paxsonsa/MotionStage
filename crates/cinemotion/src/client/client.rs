use async_trait::async_trait;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use std::sync::atomic::{AtomicI32, Ordering};
use thiserror::Error;

use crate::actor::{self, Actor, HandleExt};
use crate::engine;
use cinemotion_core::protocol;

static NEXT_ID: AtomicI32 = AtomicI32::new(0);

#[derive(Debug, Clone)]
pub enum Message {
    Init(actor::Responder<Result<(), ClientError>>),
    Send {
        message: protocol::ServerMessage,
        response: actor::Responder<Result<(), ClientError>>,
    },
}

impl Message {
    pub fn init() -> (Self, actor::Response<Result<(), ClientError>>) {
        let (responder, response) = actor::Response::new();
        (Self::Init(responder), response)
    }

    pub fn send(
        message: protocol::ServerMessage,
    ) -> (Self, actor::Response<Result<(), ClientError>>) {
        let (responder, response) = actor::Response::new();
        (
            Self::Send {
                message,
                response: responder,
            },
            response,
        )
    }
}

/// `ClientError` is an enumeration that is intended to represent
/// different types of errors that can occur within the client.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum ClientError {
    #[error("failed to send message to actor")]
    SendError,

    #[error("bad message: {0}")]
    BadMessage(String),

    #[error(transparent)]
    ActorError(#[from] actor::ActorError),
}

#[derive(Default)]
enum Status {
    /// The client is initializing and not ready to send or receive messages
    #[default]
    Initializing,
    /// The client is ready to send and receive messages
    Ready,
    /// the client is disconnected and dead.
    Disconnected,
}

/// Internal state of the client
#[derive(Default)]
struct State {
    pub status: Status,
}

#[derive(Debug, Clone)]
pub struct ClientHandle {
    pub(super) id: i32,
    pub(super) sender: actor::Sender<Message>,
}

impl ClientHandle {
    /// Returns the unique identifier for the client.
    ///
    /// This function is used to get the unique identifier (ID) for the client.
    pub fn id(&self) -> i32 {
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

    /// Sends a message to the server.
    ///
    /// This function is responsible for transmitting messages to the client. If the message is successfully sent, it returns `Ok`, otherwise it returns an `Err`.
    ///
    pub async fn send(&self, message: protocol::ServerMessage) -> Result<(), ClientError> {
        self.perform_send(|| Message::send(message)).await
    }
}

impl actor::Handle for ClientHandle {
    type Message = Message;

    fn sender(&self) -> actor::Sender<Message> {
        self.sender.clone()
    }
}

impl actor::HandleExt for ClientHandle {}

/// `Client` is a generic struct that represents a client in the system.
/// It is parameterized over two types: `R` and `W`. a
/// `R` is a type that implements `StreamExt` with `Item` being a `Result` of `tungstenite::Message`.
/// `W` is a type that implements `SinkExt` with `Item` being a `tungstenite::Message`.
/// The `Client` struct also contains an `engine::EngineHandle` and a `coordinator::ClientCoordinatorHandle`.
///
/// The `new` function is used to create a new instance of `Client`.
///
/// The `Client` struct also implements the `Actor` trait from the `actor` module.
/// The `Message` associated type is set to `Message` and the `Handle` associated type is set to `ClientHandle`.
/// The `handle_message` function is an asynchronous function that handles incoming messages.
pub struct Client<R, W, T, S, FnSend, FnReceive>
where
    R: StreamExt<Item = T> + Unpin,
    W: SinkExt<S> + Unpin,
    FnSend: FnMut(protocol::ServerMessage) -> S + Send + 'static,
    FnReceive: FnMut(T) -> Result<protocol::ClientMessage, ClientError> + Send + 'static,
{
    id: i32,
    reader: R,
    writer: W,
    engine: engine::EngineHandle,
    state: State,
    send_fn: FnSend,
    receive_fn: FnReceive,
}

impl<R, W, T, S, FnTo, FnFrom> Client<R, W, T, S, FnTo, FnFrom>
where
    R: StreamExt<Item = T> + Unpin,
    W: SinkExt<S> + Unpin,
    FnTo: FnMut(protocol::ServerMessage) -> S + Send + 'static,
    FnFrom: FnMut(T) -> Result<protocol::ClientMessage, ClientError> + Send + 'static,
{
    pub fn new(
        reader: R,
        writer: W,
        engine: engine::EngineHandle,
        send_fn: FnTo,
        receive_fn: FnFrom,
    ) -> Self {
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::SeqCst),
            reader,
            writer,
            engine,
            state: State::default(),
            send_fn,
            receive_fn,
        }
    }

    pub fn id(&self) -> i32 {
        self.id
    }

    pub async fn initialize(&mut self) -> Result<(), ClientError> {
        match self
            .send_message(
                protocol::Initialize {
                    version: 1,
                    id: self.id,
                }
                .into(),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => {
                tracing::error!(?err, "failed to send init message it client");
                Err(ClientError::SendError)
            }
        }
    }

    pub async fn disconnect(&mut self) -> Result<(), ClientError> {
        if let Err(err) = self.engine.remove_client(self.id).await {
            tracing::error!(?err, "failed to remove client from engine");
        }
        self.state.status = Status::Disconnected;
        Ok(())
    }

    pub async fn send_message(
        &mut self,
        message: protocol::ServerMessage,
    ) -> Result<(), ClientError> {
        let message = (self.send_fn)(message);
        // let bytes: bytes::Bytes = message.try_into().map_err(|_| ClientError::SendError)?;
        // let message = tungstenite::Message::binary(bytes.to_vec());
        self.writer
            .send(message)
            .await
            .map_err(|_| ClientError::SendError)?;
        Ok(())
    }

    pub async fn receive_message(&mut self, message: T) -> Result<(), ClientError> {
        let message = (self.receive_fn)(message)?;
        let Some(message) = message.body else {
            return Err(ClientError::BadMessage(format!("missing message body")));
        };
        match message {
            protocol::client_message::Body::InitializeAck(_) => {
                self.state.status = Status::Ready;
            }
            m => {
                self.engine
                    .apply(self.id, m)
                    .await
                    .expect("engine apply should not fail");
            }
        }
        Ok(())
    }
}

#[async_trait]
impl<R, W, T, S, FnTo, FnFrom> actor::Actor for Client<R, W, T, S, FnTo, FnFrom>
where
    T: Sync + Send,
    S: Sync + Send,
    R: StreamExt<Item = T> + Unpin + Send + Sync,
    W: SinkExt<S> + Unpin + Send + Sync,
    FnTo: FnMut(protocol::ServerMessage) -> S + Sync + Send + 'static,
    FnFrom: FnMut(T) -> Result<protocol::ClientMessage, ClientError> + Sync + Send + 'static,
{
    type Message = Message;
    type Handle = ClientHandle;
    async fn handle_message(&mut self, message: Self::Message) -> Option<actor::Signal> {
        match message {
            Message::Init(response) => response.dispatch(self.initialize().await).await,
            Message::Send { message, response } => {
                response.dispatch(self.send_message(message).await).await;
            }
        }
        None
    }
}

/// Spawn a new client with the given reader and writer for communicating with the network layer
pub fn spawn<R, W, T, S, FnSend, FnReceive>(
    reader: R,
    writer: W,
    engine: engine::EngineHandle,
    receive_fn: FnReceive,
    send_fn: FnSend,
) -> ClientHandle
where
    T: Sync + Send + 'static,
    S: Sync + Send + 'static,
    R: StreamExt<Item = T> + Unpin + Send + Sync + 'static,
    W: SinkExt<S> + Unpin + Send + Sync + 'static,
    FnSend: FnMut(protocol::ServerMessage) -> S + Sync + Send + 'static,
    FnReceive: FnMut(T) -> Result<protocol::ClientMessage, ClientError> + Sync + Send + 'static,
{
    let mut model = Client::new(reader, writer, engine, send_fn, receive_fn);
    let id = model.id();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<_>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                // Handle when a new message is received via websocket
                Some(msg) = model.reader.next() => {
                    if let Err(err) = model.receive_message(msg).await {
                        tracing::error!(?err, "failed to receive message from client reader");
                    }
                },
                // Handle when a new message is received via the actor system
                recv = receiver.recv() => {
                    handle_actor_message(&mut model, recv).await;
                }
            }
        }
    });

    ClientHandle {
        id,
        sender: sender.into(),
    }
}

// async fn handle_receive<R, W, T>(client: &mut Client<R, W, T, FnSend, FnReceive>, msg: T)
// where
//     R: StreamExt<Item = T> + Unpin,
//     W: SinkExt<T> + Unpin,
// {
//     match msg {
//         Ok(msg) => {
//             tracing::debug!("received message: {:?}", msg);
//             if !msg.is_binary() {
//                 tracing::warn!("received non-binary message, ignoring");
//                 return;
//             }
//
//             let data = bytes::Bytes::from(msg.into_data());
//             let Ok(message) = data.try_into() else {
//                 tracing::error!("failed to deserialize message from websocket");
//                 return;
//             };
//
//             client.receive_message(message).await.unwrap();
//         }
//         Err(err) => {
//             tracing::error!(?err, "failed to read message from websocket");
//             if let Err(err) = client.disconnect().await {
//                 tracing::error!(
//                     ?err,
//                     "while failing to read message from websocket, failed to disconnect client "
//                 );
//             }
//         }
//     }
// }

async fn handle_actor_message<T, S, R, W, FnSend, FnReceive>(
    client: &mut Client<R, W, T, S, FnSend, FnReceive>,
    recv: Option<actor::Event<Message>>,
) where
    T: Sync + Send,
    S: Sync + Send,
    R: StreamExt<Item = T> + Unpin + Send + Sync,
    W: SinkExt<S> + Unpin + Send + Sync,
    FnSend: FnMut(protocol::ServerMessage) -> S + Sync + Send + 'static,
    FnReceive: FnMut(T) -> Result<protocol::ClientMessage, ClientError> + Sync + Send + 'static,
{
    match recv {
        Some(event) => match event {
            actor::Event::Stop { respond_to } => {
                respond_to.send(()).unwrap();
            }
            actor::Event::Message(message) => {
                if let Some(signal) = client.handle_message(message).await {
                    match signal {
                        actor::Signal::Stop => {}
                    }
                }
            }
        },
        None => {}
    }
}
