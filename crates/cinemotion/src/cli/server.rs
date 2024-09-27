use std::collections::HashMap;

use anyhow::Result;
use cinemotion::{
    backend,
    client::ConnectionHandle,
    engine::{self, EngineEvent},
    websocket::{self, ServerEvent},
};
use clap::Args;
use tokio::net::TcpListener;

/// Start the cinemotion broker services.
#[derive(Args)]
pub struct ServerCmd {
    #[clap(long = "address")]
    server_bind_address: Option<std::net::SocketAddr>,
}

impl ServerCmd {
    pub async fn run(&self) -> Result<i32> {
        let address = self
            .server_bind_address
            .unwrap_or("0.0.0.0:7878".parse().unwrap());

        let mut engine = engine::spawn(backend::DefaultBackend::new());
        let listener = TcpListener::bind(address).await?;

        // Need to clone the handles to move into the closure
        let mut server = websocket::serve(listener);
        let mut clients: HashMap<u32, Box<websocket::WebsocketHandle>> = Default::default();

        loop {
            tokio::select! {
                Some(event) = server.next() => match event {
                    ServerEvent::ConnectionEstablished(client) => {
                        let client_id = client.id();
                        clients.insert(client_id, client);
                        engine.registered_connection(clients.get_mut(&client_id).unwrap()).await;
                    },
                    ServerEvent::ConnectionFailed => {
                        tracing::error!("Connection failed");
                    },
                    ServerEvent::ConnectionClosed{ connection_id } => {
                        let client = clients.get_mut(&connection_id).expect("missing client for closing the connection");
                        engine.closed_connection(&client).await?;
                    },
                    ServerEvent::Message { connection_id, message } => {
                        let client = clients.get_mut(&connection_id).expect("missing client for closing the connection");
                        engine.apply(client, message).await?;
                    },
                },
                Some(event) = engine.next() => match event {
                    EngineEvent::Tick(state) => {
                        server.broadcast(state.into()).await?;
                    },
                    EngineEvent::Error(err) => {
                        tracing::error!("Engine error: {:?}", err);
                    },
                },
                _ = tokio::signal::ctrl_c() => {
                    engine.shutdown().await;
                    server.shutdown().await;
                    break;
                }
            }
        }
        Ok(0)
    }
}
