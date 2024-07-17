use std::net::TcpStream;

use anyhow::{anyhow, Result};
use cinemotion::{actor::Handle, client, engine, websocket, Error};
use clap::Args;
use futures_util::{future, pin_mut, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Start the cinemotion broker services.
#[derive(Args)]
pub struct DebuggerCmd {
    #[clap(long = "address")]
    server_address: Option<String>,
}

impl DebuggerCmd {
    pub async fn run(&self) -> Result<i32> {
        let address = self
            .server_address
            .clone()
            .unwrap_or("ws://0.0.0.0:7788".into());

        let ws = match tokio_tungstenite::connect_async(address).await {
            Ok((ws, _)) => ws,
            Err(err) => return Err(anyhow!("failed to connect to server, aborting: {:?}", err)),
        };

        let (writer, reader) = ws.split();

        let receiver_task = {
            reader.for_each(|msg| async {
                let msg = match msg {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::error!(?err, "failed to read message from server");
                        return;
                    }
                };
                let bytes = bytes::Bytes::from(msg.into_data());

                let msg = match cinemotion_proto::ServerMessage::try_from(bytes) {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::error!(?err, "failed to decode message from server");
                        return;
                    }
                };
                tracing::debug!("received message: {:?}", msg);
            })
        };

        let kill_switch = { tokio::signal::ctrl_c() };

        pin_mut!(receiver_task, kill_switch);
        future::select(receiver_task, kill_switch).await;
        Ok(0)
    }
}
