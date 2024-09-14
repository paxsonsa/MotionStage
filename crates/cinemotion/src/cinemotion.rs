use crate::actor::{spawn, Actor, ActorError, Event, Handle, Sender, Signal};
use async_trait::async_trait;
use futures::stream::{SplitSink, SplitStream};
use futures::StreamExt;
use tokio_tungstenite::{tungstenite, MaybeTlsStream, WebSocketStream};
use tracing;

#[derive(Debug)]
pub struct Connection {
    pub(crate) reader: SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>,
    pub(crate) writer: futures::stream::SplitSink<
        WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        tungstenite::Message,
    >,
}

pub async fn connect(address: String) -> Result<Connection, crate::Error> {
    let (ws, _) = tokio_tungstenite::connect_async(address)
        .await
        .map_err(|err| {
            tracing::error!(?err, "Failed to connect to server.");
            crate::Error::ConnectionError(err)
        })?;
    let (ws_writer, ws_reader) = ws.split();
    Ok(Connection {
        reader: ws_reader,
        writer: ws_writer,
    })
}

#[derive(typed_builder::TypedBuilder)]
pub struct Runtime<Handler>
where
    Handler: FnMut(Message) -> std::pin::Pin<Box<dyn futures::Future<Output = Option<()>> + Send>>
        + Send
        + 'static,
{
    name: String,
    connection: Connection,
    runtime_fn: Box<Handler>,
}

impl<Handler> Runtime<Handler>
where
    Handler: FnMut(Message) -> std::pin::Pin<Box<dyn futures::Future<Output = Option<()>> + Send>>
        + Send
        + 'static,
{
    pub async fn start(mut self) -> RuntimeHandle {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Receive messages from the network connection
                    msg = self.connection.reader.next() => {

                    },
                    // Receive messages from the runtime handle.
                    msg = rx.recv() => {
                        match msg {
                            Some(msg) => {}
                            None => {
                                // TODO: Shutdown the service
                                return;
                            }
                        }

                    }
                }
            }
        });

        RuntimeHandle { tx }
    }
}

#[derive(Clone)]
pub struct RuntimeHandle {
    tx: tokio::sync::mpsc::UnboundedSender<Message>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Log(String),
    Command(String),
    // Add more message types as needed
}

struct CinemotionActor {
    sender: Sender<Message>,
}

impl CinemotionActor {
    pub fn new(sender: Sender<Message>) -> Self {
        Self { sender }
    }
}

#[async_trait]
impl Actor for CinemotionActor {
    type Handle = Cinemotion;
    type Message = Message;

    async fn handle_message(&mut self, message: Self::Message) -> Option<Signal> {
        match message {
            Message::Log(log) => {
                tracing::info!("Log: {}", log);
            }
            Message::Command(cmd) => {
                tracing::info!("Command: {}", cmd);
                // Handle command
            }
        }
        None
    }
}

/// TODO: The cinemotion client needs to have an interface that matches the basic actions we expect
/// to take for the cinemotion interface for Python.

#[derive(Clone)]
pub struct Cinemotion {
    sender: Sender<Message>,
}

#[async_trait]
impl Handle for Cinemotion {
    type Message = Message;

    fn sender(&self) -> Sender<Self::Message> {
        self.sender.clone()
    }
}

impl Cinemotion {
    pub fn new(sender: Sender<Message>) -> Self {
        Self { sender }
    }
}
