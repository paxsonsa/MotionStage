use anyhow::Result;
use async_trait::async_trait;
use derive_more::Display;
use futures::Future;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::actor;
use crate::client;

#[derive(Clone, Debug)]
enum Message {
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

    #[doc = r" Creates a new handle with the given sender channel."]
    fn new(sender: actor::Sender<Self::Message>) -> Self {
        Self { sender }
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

    let handle = actor::spawn(actor);
    Ok(handle)
}
