use crate::actor;
use crate::client::ClientError;
use crate::error;
use anyhow::Result;
use async_trait::async_trait;
use derive_more::Display;
use futures::stream::{SplitSink, SplitStream};
use futures::StreamExt;
use futures::{Future, SinkExt};
use tokio::net::TcpListener;
use tokio_tungstenite::{tungstenite, MaybeTlsStream, WebSocketStream};

use cinemotion_core::devices;
use cinemotion_core::protocol;

#[derive(Clone, Debug)]
pub enum Message {
    Stop {
        respond_to: tokio::sync::mpsc::Sender<()>,
    },
}

#[derive(Display, Debug)]
enum ActorSignal {
    Stop,
}

/// The websocket server actor.
#[derive(Debug)]
struct WebSocket<F, R>
where
    R: Future<Output = Result<()>> + Send + 'static,
    F: FnMut(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> R
        + Send
        + Sync
        + 'static,
{
    listener: TcpListener,
    on_connection: F,
}

#[async_trait]
impl<F, R> actor::Actor for WebSocket<F, R>
where
    R: Future<Output = Result<()>> + Send + 'static,
    F: FnMut(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> R
        + Send
        + Sync
        + 'static,
{
    type Message = Message;
    type Handle = WebSocketHandle;

    async fn tick(&mut self) -> Option<actor::Signal> {
        tracing::info!("starting websocket server actor.");
        loop {
            tokio::select! {
                result = self.listener.accept() => match result {
                    Ok((stream, addr)) => {
                        tracing::info!(?addr, "opened connection, attempting to establish websocket connection.");

                        match tokio_tungstenite::accept_async(stream).await {
                            Ok(ws_stream) => {
                                tracing::info!(?addr, "established websocket connection");
                                if let Err(err) = (self.on_connection)(ws_stream).await {
                                    tracing::error!(?addr, ?err, "failed to establish websocket connection, closing connection");
                                }
                            },
                            Err(err) => {
                                tracing::error!(?addr, ?err, "failed to establish websocket connection, closing connection");
                            },

                        }
                    },
                    Err(err) => tracing::error!("failed to accept connection: error={err}")
                },
            }
        }
    }
    async fn handle_message(&mut self, message: Message) -> Option<actor::Signal> {
        match message {
            Message::Stop { respond_to } => {
                respond_to.send(()).await.unwrap();
                return Some(actor::Signal::Stop);
            }
        }
    }
}

#[derive(Clone)]
pub struct WebSocketHandle {
    sender: actor::Sender<Message>,
}

#[async_trait]
impl actor::Handle for WebSocketHandle {
    type Message = Message;
    fn sender(&self) -> actor::Sender<Message> {
        self.sender.clone()
    }

    /// Stop the weovsocket server.
    ///
    /// This will stop the websocket server, all active connections will be closed.
    /// This will return once the server has been stopped.
    async fn stop(&mut self) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        self.sender.send(Message::Stop { respond_to: tx }).unwrap();
        rx.recv().await;
    }
}

/// An outgoing websocket connection to some server.
#[derive(Debug)]
pub struct Connection {
    pub(crate) reader: SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>,
    pub(crate) writer: futures::stream::SplitSink<
        WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        tungstenite::Message,
    >,
}

impl Connection {
    pub async fn next(&mut self) -> Option<protocol::ServerMessage> {
        self.reader
            .next()
            .await
            .and_then(|msg| -> Option<cinemotion_proto::ServerMessage> {
                let Ok(msg) = msg else {
                    tracing::error!("failed to read message from websocket");
                    return None;
                };
                tracing::info!("received message: {:?}", msg);
                if !msg.is_binary() {
                    tracing::warn!("received non-binary message, ignoring");
                    return None;
                }

                let data = bytes::Bytes::from(msg.into_data());
                let Ok(message) = data.try_into() else {
                    tracing::error!("failed to deserialize message from websocket");
                    return None;
                };
                Some(message)
            })
    }

    pub async fn write(&mut self, message: protocol::ClientMessage) -> Result<(), error::Error> {
        let message = convert_message(message);
        self.writer
            .send(message)
            .await
            .map_err(|err| crate::Error::ConnectionError(err.to_string()))?;
        Ok(())
    }
}

pub async fn connect(address: String) -> Result<Connection, crate::Error> {
    let (ws, _) = tokio_tungstenite::connect_async(address)
        .await
        .map_err(|err| {
            tracing::error!(?err, "Failed to connect to server.");
            crate::Error::ConnectionError(err.to_string())
        })?;
    let (ws_writer, ws_reader) = ws.split();
    Ok(Connection {
        reader: ws_reader,
        writer: ws_writer,
    })
}

/// Spawn a new websocket server and return a handler to it.
///
/// Once the last handle is dropped, the actor will be terminated.
pub fn server<F, R>(tcp_listener: TcpListener, on_connection: F) -> Result<WebSocketHandle>
where
    R: Future<Output = Result<()>> + Send + 'static,
    F: FnMut(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> R
        + Send
        + Sync
        + 'static,
{
    tracing::info!("spawning websocket server actor");
    let actor = WebSocket {
        listener: tcp_listener,
        on_connection,
    };

    let handle = actor::spawn(actor, |sender| WebSocketHandle { sender });
    Ok(handle)
}

pub fn receive_message<T>(
    msg: Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>,
) -> Result<T, error::Error>
where
    T: std::convert::TryFrom<bytes::Bytes>,
{
    match msg {
        Ok(msg) => {
            tracing::debug!("received message: {:?}", msg);
            if !msg.is_binary() {
                tracing::warn!("received non-binary message, ignoring");
                return Err(ClientError::BadMessage(format!("received non-binary message")).into());
            }

            let data = bytes::Bytes::from(msg.into_data());
            let Ok(message) = data.try_into() else {
                tracing::error!("failed to deserialize message from websocket");
                return Err(ClientError::BadMessage(format!(
                    "failed to convert message to local type"
                ))
                .into());
            };
            Ok(message)
        }
        Err(err) => {
            tracing::error!(
                ?err,
                "failed to read message from websocket, closing connection."
            );
            Err(ClientError::Disconnected.into())
        }
    }
}

pub fn convert_message<M>(msg: M) -> tokio_tungstenite::tungstenite::Message
where
    M: std::fmt::Debug + TryInto<bytes::Bytes>,
    <M as TryInto<bytes::Bytes>>::Error: std::fmt::Debug,
{
    let data: bytes::Bytes = msg
        .try_into()
        .expect("failed to generate bytes for protocol message");
    tokio_tungstenite::tungstenite::Message::binary(data)
}
