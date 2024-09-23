use async_trait::async_trait;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use thiserror::Error;

use crate::actor::{self, ActorError, HandleExt};
use crate::engine;
use crate::error::Error as CrateError;
use crate::perform_send_with_error_handling;
use cinemotion_core::protocol;

#[derive(Debug, Clone)]
pub enum Message {
    Id(actor::Responder<Result<u32, CrateError>>),
    Init {
        assigned_id: u32,
        response: actor::Responder<Result<(), CrateError>>,
    },
    Send {
        message: protocol::ServerMessage,
        response: actor::Responder<Result<(), CrateError>>,
    },
    State(actor::Responder<Result<State, CrateError>>),
}

impl Message {
    pub fn id() -> (Self, actor::Response<Result<u32, CrateError>>) {
        let (responder, response) = actor::Response::new();
        (Self::Id(responder), response)
    }
    pub fn init(id: u32) -> (Self, actor::Response<Result<(), CrateError>>) {
        let (responder, response) = actor::Response::new();
        (
            Self::Init {
                assigned_id: id,
                response: responder,
            },
            response,
        )
    }

    pub fn send(
        message: protocol::ServerMessage,
    ) -> (Self, actor::Response<Result<(), CrateError>>) {
        let (responder, response) = actor::Response::new();
        (
            Self::Send {
                message,
                response: responder,
            },
            response,
        )
    }

    pub fn state() -> (Self, actor::Response<Result<State, CrateError>>) {
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

    #[error("client is in undefined state")]
    UndefinedState,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Status {
    #[default]
    Undefined,
    /// The client is initializing and not ready to send or receive messages
    Initializing(u32),
    /// The client is ready to send and receive messages
    Ready(u32),
    /// the client is disconnected and dead.
    Disconnected(u32),
}

/// Internal state of the client
#[derive(Clone, Debug, Default)]
pub struct State {
    pub status: Status,
}

#[derive(Debug, Clone)]
pub struct ClientHandle {
    pub sender: actor::Sender<Message>,
}

impl ClientHandle {
    /// Returns the unique identifier for the client.
    ///
    /// This function is used to get the unique identifier (ID) for the client.
    pub async fn id(&self) -> Result<u32, CrateError> {
        perform_send_with_error_handling!(self, Message::id())
    }

    /// Initialize the client connection
    ///
    /// The client needs to initialize before it can respond to any messages
    /// and work with the engine runtime. This should be the first item after the client
    /// is created.
    pub async fn initialize(&self, id: u32) -> Result<(), CrateError> {
        perform_send_with_error_handling!(self, Message::init(id))
    }

    /// Sends a message to the client.
    ///
    /// This function is responsible for transmitting messages to the client. If the message is successfully sent, it returns `Ok`, otherwise it returns an `Err`.
    ///
    pub async fn send_message(&self, message: protocol::ServerMessage) -> Result<(), CrateError> {
        tracing::debug!(?message, "sending message to client");
        perform_send_with_error_handling!(self, Message::send(message))
    }

    /// Returns the current state of the client.
    pub async fn state(&self) -> Result<State, CrateError> {
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
pub struct Client<R, W, T, S, E, Engine, FnSend, FnReceive>
where
    R: StreamExt<Item = T> + Unpin,
    W: SinkExt<S> + Unpin,
    E: Into<CrateError> + 'static,
    Engine: engine::EngineResource,
    FnSend: FnMut(protocol::ServerMessage) -> S + Send + 'static,
    FnReceive: FnMut(T) -> Result<protocol::ClientMessage, E> + Send + 'static,
{
    reader: R,
    writer: W,
    engine: Engine,
    pub(super) state: State,
    send_fn: FnSend,
    receive_fn: FnReceive,
}

impl<R, W, T, S, E, Engine, FnTo, FnFrom> Client<R, W, T, S, E, Engine, FnTo, FnFrom>
where
    R: StreamExt<Item = T> + Unpin,
    W: SinkExt<S> + Unpin,
    E: Into<CrateError> + 'static,
    Engine: engine::EngineResource,
    FnTo: FnMut(protocol::ServerMessage) -> S + Send + 'static,
    FnFrom: FnMut(T) -> Result<protocol::ClientMessage, E> + Send + 'static,
{
    /// Create a new client with the given reader and writer for communicating with the network layer.
    pub fn new(reader: R, writer: W, engine: Engine, send_fn: FnTo, receive_fn: FnFrom) -> Self {
        Self {
            reader,
            writer,
            engine,
            state: State::default(),
            send_fn,
            receive_fn,
        }
    }

    /// Returns the unique identifier for the client.
    pub fn id(&self) -> Option<u32> {
        match self.state.status {
            Status::Undefined => None,
            Status::Initializing(id) | Status::Ready(id) | Status::Disconnected(id) => Some(id),
        }
    }

    /// Initialize the client connection
    ///
    /// This should be called once the client's connection has been established.
    pub async fn initialize(&mut self, id: u32) -> Result<(), CrateError> {
        self.state.status = Status::Initializing(id);
        match self
            .send_message(
                protocol::DeviceInit {
                    version: 1,
                    id: self.id().unwrap(),
                }
                .into(),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => {
                tracing::error!(?err, "failed to send init message it client");
                Err(ClientError::SendError.into())
            }
        }
    }

    /// Set the client into a disconnected state.
    ///
    /// No messages will be passed through, even if the client is still connected at the network
    /// level.
    pub async fn disconnect(&mut self) -> Result<(), CrateError> {
        tracing::debug!("disconnecting client");
        if let Err(err) = self.engine.remove_client(self.id().unwrap()).await {
            tracing::error!(?err, "failed to remove client from engine");
        }
        self.state.status = Status::Disconnected(self.id().unwrap());
        Ok(())
    }

    /// Send a message to the client.
    ///
    /// Note: The client needs to be initialized and not disconnected to send messages.
    pub async fn send_message(
        &mut self,
        message: protocol::ServerMessage,
    ) -> Result<(), CrateError> {
        let message = (self.send_fn)(message);
        self.writer
            .send(message)
            .await
            .map_err(|_| -> CrateError { ClientError::SendError.into() })?;
        Ok(())
    }

    /// Receive a message from the client.
    ///
    /// This function is responsible for receiving messages from the client and processing them.
    /// The message is then passed to the engine for processing.
    pub async fn receive_message(&mut self, message: T) -> Result<(), CrateError> {
        let Some(id) = self.id() else {
            return Err(ClientError::UndefinedState.into());
        };
        let message = (self.receive_fn)(message).map_err(Into::<CrateError>::into)?;
        let Some(message) = message.body else {
            return Err(ClientError::BadMessage(format!("missing message body")).into());
        };
        match message {
            protocol::client_message::Body::DeviceInitAck(_) => {
                self.state.status = Status::Ready(id);
            }
            protocol::client_message::Body::Ping(_) => {
                self.send_message(protocol::ServerMessage {
                    body: Some(protocol::server_message::Body::Pong(protocol::Pong {})),
                })
                .await
                .map_err(|_| CrateError::ClientError(ClientError::SendError.into()))?;
                self.engine
                    .apply(id, message)
                    .await
                    .expect("engine should apply ping");
            }
            m => {
                self.engine
                    .apply(id, m)
                    .await
                    .expect("engine apply should not fail");
            }
        }
        Ok(())
    }
}

#[async_trait]
impl<R, W, T, S, E, Engine, FnTo, FnFrom> actor::Actor
    for Client<R, W, T, S, E, Engine, FnTo, FnFrom>
where
    T: Sync + Send,
    S: Sync + Send,
    R: StreamExt<Item = T> + Unpin + Send + Sync,
    W: SinkExt<S> + Unpin + Send + Sync,
    E: Into<CrateError> + 'static,
    Engine: engine::EngineResource + Send + Send,
    FnTo: FnMut(protocol::ServerMessage) -> S + Sync + Send + 'static,
    FnFrom: FnMut(T) -> Result<protocol::ClientMessage, E> + Sync + Send + 'static,
{
    type Message = Message;
    type Handle = ClientHandle;
    async fn handle_message(&mut self, message: Self::Message) -> Option<actor::Signal> {
        match message {
            Message::Id(response) => match self.id() {
                Some(id) => response.dispatch(Ok(id)).await,
                None => {
                    response
                        .dispatch(Err(ClientError::UndefinedState.into()))
                        .await
                }
            },
            // Requesting to initialize the client
            Message::Init {
                assigned_id,
                response,
            } => {
                response.dispatch(self.initialize(assigned_id).await).await;
            }
            // Requesting to send a message to the client
            Message::Send { message, response } => match self.state.status {
                Status::Ready(_) => response.dispatch(self.send_message(message).await).await,
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
            if let CrateError::ClientError(ClientError::Disconnected) = err {
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
pub fn spawn<R, W, T, S, E, Engine, FnSend, FnReceive>(
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
    E: Into<CrateError> + 'static,
    Engine: engine::EngineResource + Send + 'static,
    FnSend: FnMut(protocol::ServerMessage) -> S + Sync + Send + 'static,
    FnReceive: FnMut(T) -> Result<protocol::ClientMessage, E> + Sync + Send + 'static,
{
    let model = Client::new(reader, writer, engine, send_fn, receive_fn);
    actor::spawn(model, move |sender| ClientHandle { sender })
}
