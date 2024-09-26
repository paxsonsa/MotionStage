use crate::client::ConnectionHandle;
use crate::error;
use anyhow::Result;
use futures::stream::{SplitSink, SplitStream};
use futures::SinkExt;
use futures::StreamExt;
use std::collections::HashMap;
use tokio::net::TcpListener;
use tokio_tungstenite::{tungstenite, MaybeTlsStream, WebSocketStream};

use cinemotion_core::protocol;

pub enum ServerEvent {
    ConnectionEstablished(Box<WebsocketHandle>),
    ConnectionFailed,
    ConnectionClosed {
        connection_id: u32,
    },
    Message {
        connection_id: u32,
        message: protocol::ClientMessage,
    },
}

pub enum ClientEvent {
    ConnectionFailed {
        connection_id: u32,
    },
    ClientMessage {
        connection_id: u32,
        message: tungstenite::Message,
    },
    ServerMessage {
        target_id: u32,
        message: protocol::ServerMessage,
    },
}

pub struct WebsocketServer {
    next_connection_id: u32,
    connections: HashMap<u32, Websocket>,
    listener: TcpListener,
    sender: tokio::sync::mpsc::Sender<ClientEvent>,
    receiver: tokio::sync::mpsc::Receiver<ClientEvent>,
}

impl WebsocketServer {
    pub async fn next(&mut self) -> Option<ServerEvent> {
        tokio::select! {
            result = self.listener.accept() => match result {
                Ok((stream, addr)) => {
                    tracing::info!(?addr, "opened connection, attempting to establish websocket connection.");

                    match tokio_tungstenite::accept_async(stream).await {
                        Ok(ws_stream) => {
                            tracing::info!(?addr, "established websocket connection");
                            Some(ServerEvent::ConnectionEstablished(self.new_connection(ws_stream)))
                        },
                        Err(err) => {
                            tracing::error!(?addr, ?err, "failed to establish websocket connection, closing connection");
                            Some(ServerEvent::ConnectionFailed)
                        },

                    }
                },
                Err(err) => {
                    tracing::error!("failed to accept connection: error={err}");
                    None
                }
            },
            Some(event) = self.receiver.recv() => {
                match event {
                    ClientEvent::ConnectionFailed { connection_id } => {
                        tracing::error!(?connection_id, "connection failed");
                        self.connections.remove(&connection_id);
                        Some(ServerEvent::ConnectionClosed { connection_id })
                    },
                    ClientEvent::ClientMessage{ connection_id, message } => {
                        if !message.is_binary() {
                            tracing::warn!("received non-binary message, ignoring");
                            return None;
                        }

                        let data = bytes::Bytes::from(message.into_data());
                        let Ok(message) = data.try_into() else {
                            tracing::error!("failed to deserialize message from websocket");
                            return None
                        };
                        Some(ServerEvent::Message{ connection_id , message })
                    },
                    ClientEvent::ServerMessage{ target_id, message } => {
                        let Some(connection) = self.connections.get_mut(&target_id) else {
                            tracing::error!(?target_id, "failed to find connection for message");
                            return None;
                        };

                        if let Err(err) = connection.send(message).await {
                            tracing::error!(?err, "failed to send message to connection");
                            return None;
                        }
                        None
                    },
                }
            }
        }
    }

    pub async fn broadcast(&mut self, message: protocol::ServerMessage) -> Result<()> {
        for connection in self.connections.values_mut() {
            if let Err(err) = connection.send(message.clone()).await {
                tracing::error!(?err, "failed to send message to connection");
            }
        }
        Ok(())
    }

    pub async fn shutdown(self) {
        for (_, connection) in self.connections {
            connection.stop().await;
        }
    }

    fn new_connection(
        &mut self,
        stream: WebSocketStream<tokio::net::TcpStream>,
    ) -> Box<WebsocketHandle> {
        let (writer, mut reader) = stream.split();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let id = self.next_connection_id;

        let message_channel = self.sender.clone();
        let connection_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            let cancellation = connection_cancellation;
            loop {
                tokio::select! {
                    Some(message) = reader.next() => {
                        tracing::info!(?id, ?message, "received message");

                        match message {
                            Ok(message) => {
                                message_channel.send(ClientEvent::ClientMessage{ connection_id: id, message}).await.unwrap();
                            },
                            Err(err) => {
                                tracing::error!(?id, ?err, "failed to read message from connection");
                                message_channel.send(ClientEvent::ConnectionFailed{ connection_id: id}).await.unwrap();
                                break;
                            }
                        }
                    },
                    _ = cancellation.cancelled() => {
                        break;
                    }

                }
            }
        });

        let connection = Websocket {
            id: self.next_connection_id,
            task,
            cancellation,
            writer,
        };
        let handle = WebsocketHandle {
            id: self.next_connection_id,
            sender: self.sender.clone(),
        };
        self.connections.insert(self.next_connection_id, connection);
        self.next_connection_id += 1;
        Box::new(handle)
    }
}

pub struct Websocket {
    id: u32,
    task: tokio::task::JoinHandle<()>,
    cancellation: tokio_util::sync::CancellationToken,
    writer: SplitSink<WebSocketStream<tokio::net::TcpStream>, tungstenite::Message>,
}

impl Websocket {
    pub async fn stop(self) {
        self.cancellation.cancel();
        self.task.await.unwrap();
    }

    pub async fn send(&mut self, message: protocol::ServerMessage) -> Result<(), error::Error> {
        let message = convert_message(message);
        self.writer
            .send(message)
            .await
            .map_err(|err| crate::Error::ConnectionError(err.to_string()))?;
        Ok(())
    }
}

pub struct WebsocketHandle {
    id: u32,
    sender: tokio::sync::mpsc::Sender<ClientEvent>,
}

impl ConnectionHandle for WebsocketHandle {
    fn id(&self) -> u32 {
        self.id
    }

    async fn send(&mut self, message: protocol::ServerMessage) {
        self.sender
            .send(ClientEvent::ServerMessage {
                target_id: self.id,
                message,
            })
            .await
            .expect("failed to queue initialization message");
    }
}

pub fn serve(listener: TcpListener) -> WebsocketServer {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    WebsocketServer {
        next_connection_id: 1,
        connections: HashMap::new(),
        listener,
        sender: tx,
        receiver: rx,
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

    pub async fn write<M>(&mut self, message: M) -> Result<(), error::Error>
    where
        M: std::fmt::Debug + TryInto<bytes::Bytes>,
        <M as TryInto<bytes::Bytes>>::Error: std::fmt::Debug,
    {
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
