use anyhow::Result;
use cinemotion::engine::EngineResource;
use clap::Args;
use futures::SinkExt;
use futures::StreamExt;
use tokio::net::TcpListener;

use cinemotion::{actor::Handle, client, engine, websocket, Error};

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

        let mut engine = engine::spawn();
        let listener = TcpListener::bind(address).await?;

        // Need to clone the handles to move into the closure
        let engine_handle = engine.clone();

        let mut websocket_server = websocket::server(listener, move |ws_stream| {
            // Need to clone the engine handle and client coordinator handle to move into the async
            // block
            let engine_handle = engine_handle.clone();
            async move {
                let (writer, reader) = ws_stream.split();
                let mut client = client::spawn(
                    reader,
                    writer,
                    engine_handle.clone(),
                    websocket::receive_message,
                    websocket::convert_message,
                );

                if let Err(err) = engine_handle.add_client(client.clone()).await {
                    tracing::error!(?err, "failed to add new client.");
                    client.stop().await;
                    return Err(err.into());
                }
                Ok(())
            }
        })?;

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    websocket_server.stop().await;
                    engine.stop().await;
                    break;
                }
            }
        }
        Ok(0)
    }
}
