use async_trait::async_trait;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use std::sync::atomic::{AtomicU32, Ordering};
use thiserror::Error;

use crate::actor::{self, ActorError, HandleExt};
use crate::engine;
use crate::perform_send_with_error_handling;
use cinemotion_core::protocol;

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone)]
pub enum Message {
    Init(actor::Responder<Result<(), ClientError>>),
    Send {
        message: protocol::ServerMessage,
        response: actor::Responder<Result<(), ClientError>>,
    },
    State(actor::Responder<Result<State, ClientError>>),
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

    pub fn state() -> (Self, actor::Response<Result<State, ClientError>>) {
        let (responder, response) = actor::Response::new();
        (Self::State(responder), response)
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

    #[error("client is disconnected")]
    Disconnected,

    #[error(transparent)]
    ActorError(#[from] actor::ActorError),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Status {
    /// The client is initializing and not ready to send or receive messages
    #[default]
    Initializing,
    /// The client is ready to send and receive messages
    Ready,
    /// the client is disconnected and dead.
    Disconnected,
}

/// Internal state of the client
#[derive(Clone, Debug, Default)]
pub struct State {
    pub status: Status,
}

#[derive(Debug, Clone)]
pub struct ClientHandle {
    pub(super) id: u32,
    pub(super) sender: actor::Sender<Message>,
}

impl ClientHandle {
    /// Returns the unique identifier for the client.
    ///
    /// This function is used to get the unique identifier (ID) for the client.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Initialize the client connection
    ///
    /// The client needs to initialize before it can respond to any messages
    /// and work with the engine runtime. This should be the first item after the client
    /// is created.
    pub async fn initialize(&self) -> Result<(), ClientError> {
        perform_send_with_error_handling!(self, Message::init())
    }

    /// Sends a message to the client.
    ///
    /// This function is responsible for transmitting messages to the client. If the message is successfully sent, it returns `Ok`, otherwise it returns an `Err`.
    ///
    pub async fn send(&self, message: protocol::ServerMessage) -> Result<(), ClientError> {
        tracing::debug!(?message, "sending message to client");
        perform_send_with_error_handling!(self, Message::send(message))
    }

    /// Returns the current state of the client.
    pub async fn state(&self) -> Result<State, ClientError> {
        perform_send_with_error_handling!(self, Message::state())
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
pub struct Client<R, W, T, S, Engine, FnSend, FnReceive>
where
    R: StreamExt<Item = T> + Unpin,
    W: SinkExt<S> + Unpin,
    Engine: engine::EngineResource,
    FnSend: FnMut(protocol::ServerMessage) -> S + Send + 'static,
    FnReceive: FnMut(T) -> Result<protocol::ClientMessage, ClientError> + Send + 'static,
{
    id: u32,
    reader: R,
    writer: W,
    engine: Engine,
    pub(super) state: State,
    send_fn: FnSend,
    receive_fn: FnReceive,
}

impl<R, W, T, S, Engine, FnTo, FnFrom> Client<R, W, T, S, Engine, FnTo, FnFrom>
where
    R: StreamExt<Item = T> + Unpin,
    W: SinkExt<S> + Unpin,
    Engine: engine::EngineResource,
    FnTo: FnMut(protocol::ServerMessage) -> S + Send + 'static,
    FnFrom: FnMut(T) -> Result<protocol::ClientMessage, ClientError> + Send + 'static,
{
    /// Create a new client with the given reader and writer for communicating with the network layer.
    pub fn new(reader: R, writer: W, engine: Engine, send_fn: FnTo, receive_fn: FnFrom) -> Self {
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

    /// Returns the unique identifier for the client.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Initialize the client connection
    ///
    /// This should be called once the client's connection has been established.
    pub async fn initialize(&mut self) -> Result<(), ClientError> {
        match self
            .send_message(
                protocol::DeviceInit {
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

    /// Set the client into a disconnected state.
    ///
    /// No messages will be passed through, even if the client is still connected at the network
    /// level.
    pub async fn disconnect(&mut self) -> Result<(), ClientError> {
        tracing::debug!("disconnecting client");
        if let Err(err) = self.engine.remove_client(self.id).await {
            tracing::error!(?err, "failed to remove client from engine");
        }
        self.state.status = Status::Disconnected;
        Ok(())
    }

    /// Send a message to the client.
    ///
    /// Note: The client needs to be initialized and not disconnected to send messages.
    pub async fn send_message(
        &mut self,
        message: protocol::ServerMessage,
    ) -> Result<(), ClientError> {
        let message = (self.send_fn)(message);
        self.writer
            .send(message)
            .await
            .map_err(|err| ClientError::SendError)?;
        Ok(())
    }

    /// Receive a message from the client.
    ///
    /// This function is responsible for receiving messages from the client and processing them.
    /// The message is then passed to the engine for processing.
    pub async fn receive_message(&mut self, message: T) -> Result<(), ClientError> {
        let message = (self.receive_fn)(message)?;
        let Some(message) = message.body else {
            return Err(ClientError::BadMessage(format!("missing message body")));
        };
        match message {
            protocol::client_message::Body::DeviceInitAck(_) => {
                self.state.status = Status::Ready;
            }
            protocol::client_message::Body::Ping(_) => {
                self.send_message(protocol::ServerMessage {
                    body: Some(protocol::server_message::Body::Pong(protocol::Pong {})),
                })
                .await
                .map_err(|err| ClientError::SendError)?;
                self.engine
                    .apply(self.id, message)
                    .await
                    .expect("engine should apply ping");
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
impl<R, W, T, S, Engine, FnTo, FnFrom> actor::Actor for Client<R, W, T, S, Engine, FnTo, FnFrom>
where
    T: Sync + Send,
    S: Sync + Send,
    R: StreamExt<Item = T> + Unpin + Send + Sync,
    W: SinkExt<S> + Unpin + Send + Sync,
    Engine: engine::EngineResource + Send + Send,
    FnTo: FnMut(protocol::ServerMessage) -> S + Sync + Send + 'static,
    FnFrom: FnMut(T) -> Result<protocol::ClientMessage, ClientError> + Sync + Send + 'static,
{
    type Message = Message;
    type Handle = ClientHandle;
    async fn handle_message(&mut self, message: Self::Message) -> Option<actor::Signal> {
        match message {
            // Requesting to initialize the client
            Message::Init(response) => response.dispatch(self.initialize().await).await,
            // Requesting to send a message to the client
            Message::Send { message, response } => match self.state.status {
                Status::Ready => response.dispatch(self.send_message(message).await).await,
                _ => response.dispatch(Ok(())).await,
            },
            // Requesting the current state of the client
            Message::State(response) => response.dispatch(Ok(self.state.clone())).await,
        }
        None
    }

    /// Handle a tick event.
    ///
    /// The tick event is responsible for reading the next message from the client.
    /// The messages are passed onto the receive_message function for processing.
    async fn tick(&mut self) -> Option<actor::Signal> {
        let Some(msg) = self.reader.next().await else {
            return Some(actor::Signal::Stop);
        };

        if let Err(err) = self.receive_message(msg).await {
            tracing::error!(?err, "failed to receive message from client reader");
            if let ClientError::Disconnected = err {
                if let Err(err) = self.disconnect().await {
                    tracing::error!(?err, "failed to disconnect client");
                }
                return Some(actor::Signal::Stop);
            }
        }
        None
    }
}

/// Spawn a new client with the given reader and writer for communicating with the network layer
pub fn spawn<R, W, T, S, Engine, FnSend, FnReceive>(
    reader: R,
    writer: W,
    engine: Engine,
    receive_fn: FnReceive,
    send_fn: FnSend,
) -> ClientHandle
where
    T: Sync + Send + 'static,
    S: Sync + Send + 'static,
    R: StreamExt<Item = T> + Unpin + Send + Sync + 'static,
    W: SinkExt<S> + Unpin + Send + Sync + 'static,
    Engine: engine::EngineResource + Send + 'static,
    FnSend: FnMut(protocol::ServerMessage) -> S + Sync + Send + 'static,
    FnReceive: FnMut(T) -> Result<protocol::ClientMessage, ClientError> + Sync + Send + 'static,
{
    let model = Client::new(reader, writer, engine, send_fn, receive_fn);
    let id = model.id();
    actor::spawn(model, move |sender| ClientHandle { id, sender })
}
