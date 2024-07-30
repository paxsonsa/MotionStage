use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::Args;
use futures_util::{future, pin_mut, StreamExt};
use tokio::io::AsyncBufReadExt;
use tokio::io::Lines;
/// Start the cinemotion broker services.
#[derive(Args)]
pub struct DebuggerCmd {
    #[clap(long = "address")]
    server_address: Option<String>,

    #[clap(long = "file")]
    file_path: Option<PathBuf>,
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

        let mut stdin_reader = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        let stdin_task = async {
            while let Ok(Some(line)) = stdin_reader.next_line().await {
                tracing::info!("{:?}", line);
            }
        };

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
                tracing::debug!("{:?}", serde_json::to_string(&msg).unwrap());
            })
        };

        let kill_switch = { tokio::signal::ctrl_c() };

        pin_mut!(receiver_task, stdin_task, kill_switch);
        tokio::select! {
            _ = receiver_task => {}
            _ = stdin_task => {}
            _ = kill_switch => {}
        }

        Ok(0)
    }
}
