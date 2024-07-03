use anyhow::Result;
use clap::Args;
use tokio::net::TcpListener;

use cinemotion::{actor::Handle, client, engine, websocket};

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

        let mut client_coordinator = client::coordinator::spawn();
        let engine = engine::spawn(client_coordinator.clone());
        let listener = TcpListener::bind(address).await?;

        // Need to clone the handles to move into the closure
        let engine_handle = engine.clone();
        let client_coordinator_handle = client_coordinator.clone();

        let websocket_server = websocket::server(listener, move |ws_stream| {
            // Need to clone the engine handle and client coordinator handle to move into the async
            // block
            let engine_handle = engine_handle.clone();
            let client_coordinator_handle = client_coordinator_handle.clone();
            async move {
                let client = client::spawn_websocket_client(
                    ws_stream,
                    client_coordinator_handle.clone(),
                    engine_handle.clone(),
                );
                let _ = client_coordinator_handle.register(client).await;
                Ok(())
            }
        })?;

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    websocket_server.stop().await;
                    client_coordinator.stop().await;
                    engine.stop().await;
                    break;
                }
            }
        }
        Ok(0)
    }
}
