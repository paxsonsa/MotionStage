use anyhow::Result;
use derive_more::Display;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

///
#[derive(Debug)]
enum ActorMessage {
    Stop {
        respond_to: tokio::sync::oneshot::Sender<()>,
    },
}

#[derive(Display, Debug)]
enum ActorSignal {
    Stopped,
}

/// The websocket server actor.
struct Actor {
    listener: TcpListener,
    receiver: mpsc::UnboundedReceiver<ActorMessage>,
}

impl Actor {
    async fn handle_message(&mut self, message: ActorMessage) -> Option<ActorSignal> {
        match message {
            ActorMessage::Stop { respond_to } => {
                respond_to.send(()).unwrap();
                return Some(ActorSignal::Stopped);
            }
        }
    }
}

#[derive(Clone)]
pub struct WebSocketServerHandle {
    sender: mpsc::UnboundedSender<ActorMessage>,
}

impl WebSocketServerHandle {
    fn new(sender: mpsc::UnboundedSender<ActorMessage>) -> Self {
        Self { sender }
    }

    /// Stop the weovsocket server.
    ///
    /// This will stop the websocket server, all active connections will be closed.
    /// This will return once the server has been stopped.
    pub async fn stop(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(ActorMessage::Stop { respond_to: tx })
            .unwrap();
        rx.await.unwrap();
    }
}

/// Spawn a new websocket server and return a handler to it.
///
/// Once the last handle is dropped, the actor will be terminated.
pub fn server(tcp_listener: TcpListener) -> Result<WebSocketServerHandle> {
    let (sender, receiver) = mpsc::unbounded_channel::<_>();
    let mut actor = Actor {
        listener: tcp_listener,
        receiver,
    };

    tokio::spawn(async move {
        tracing::info!("starting websocket server...");
        loop {
            tokio::select! {
                result = actor.receiver.recv() => {
                    match result {
                        Some(message) => {
                            if let Some(signal) = actor.handle_message(message).await {
                                tracing::info!(?signal, "websocket server actor received signal, terminating actor.");
                            }
                        },
                        None => {
                            tracing::debug!("websocket server actor command channel closed. terminating actor.");
                            return;
                        }
                    }
                },
                result = actor.listener.accept() => match result {
                    Ok((stream, addr)) => {
                        tracing::info!(?addr, "opened connection, attempting to establish websocket connection.");

                        match tokio_tungstenite::accept_async(stream).await {
                            Ok(ws_stream) => {
                                tracing::info!(?addr, "established websocket connection");
                                // TODO: Spawn a new connection actor and register with connection
                                // manager.
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
    });

    Ok(WebSocketServerHandle::new(sender))
}
