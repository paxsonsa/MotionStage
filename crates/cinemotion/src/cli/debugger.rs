use cinemotion_core::protocol;
use futures::SinkExt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::{anyhow, Result};
use clap::Args;
use futures_util::{future, pin_mut, StreamExt};
use tokio::io::AsyncBufReadExt;
use tokio::io::Lines;

#[derive(Clone, clap::ValueEnum)]
enum Mode {
    /// Run the debugger in interactive mode and simulate a device client.
    Debugger,
    /// Run the debugger in observer mode that will monitor the server and its state.
    Observer,
}

/// Start the cinemotion broker services.
#[derive(Args)]
pub struct DebuggerCmd {
    /// The server address to connect too.
    #[clap(long = "address")]
    server_address: Option<String>,

    /// A path to a JSON file that describes a device spec to use inplace of the default one.
    #[clap(long = "device")]
    device_spec_path: Option<PathBuf>,

    /// A path to a JSON file that descrives a object spec to use inplace of the default objects.
    #[clap(long = "objects")]
    objects_spec_path: Option<PathBuf>,

    /// The mode to run the debugger in, the default is debugger.
    #[clap(long = "mode", default_value = "debugger")]
    mode: Mode,
}

/*
* TODO: Implement the debugger system.
* - Interactive ability start/stop motion streaming and recording
* - Interactive ability to ability to reset the device.
*/

static DEFAULT_ADDRESS: &str = "ws://0.0.0.0:7788";

impl DebuggerCmd {
    pub async fn run(&self) -> Result<i32> {
        let address = self
            .server_address
            .clone()
            .unwrap_or(DEFAULT_ADDRESS.into());

        let device_spec = device_spec_from_path_or_default(self.device_spec_path.clone(), || {
            protocol::DeviceSpec {
                name: "Cinemotion Debugger".to_string(),
                attributes: [(
                    "transform".to_string(),
                    protocol::AttributeValue {
                        value: Some(protocol::attribute_value::Value::Matrix44(
                            protocol::Matrix44 {
                                values: vec![
                                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0,
                                    0.0, 0.0, 0.0, 1.0,
                                ],
                            },
                        )),
                    },
                )]
                .into(),
            }
        })?;

        let ws = match tokio_tungstenite::connect_async(address).await {
            Ok((ws, _)) => ws,
            Err(err) => return Err(anyhow!("failed to connect to server, aborting: {:?}", err)),
        };

        let (writer, reader) = ws.split();
        let writer = std::sync::Arc::new(tokio::sync::Mutex::new(writer));

        let mut stdin_reader = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        let stdin_task = async {
            while let Ok(Some(line)) = stdin_reader.next_line().await {
                tracing::info!("{:?}", line);
            }
        };

        let debugger_state = Arc::new(Mutex::new(DebuggerState {
            init_acked: false,
            initial_device_spec: device_spec,
            device_id: None,
        }));
        let receiver_task = {
            reader.for_each(move |msg| {
                let writer = writer.clone();
                let debugger_state = debugger_state.clone();
                async move {
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
                    if let Err(err) =
                        handle_server_message(debugger_state.clone(), msg, writer.clone()).await
                    {
                        tracing::error!(?err, "failed to handle server message");
                    }
                }
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

#[derive(Default)]
struct DebuggerState {
    init_acked: bool,
    initial_device_spec: protocol::DeviceSpec,
    device_id: Option<u32>,
}

fn device_spec_from_path_or_default<F>(
    path: Option<PathBuf>,
    default_fn: F,
) -> Result<cinemotion_proto::DeviceSpec>
where
    F: FnOnce() -> protocol::DeviceSpec,
{
    match path {
        Some(p) => {
            let spec = std::fs::read_to_string(p)?;
            Ok(serde_json::from_str(&spec)?)
        }
        None => Ok(default_fn()),
    }
}

async fn handle_server_message(
    state: Arc<Mutex<DebuggerState>>,
    msg: cinemotion_proto::ServerMessage,
    writer: std::sync::Arc<
        tokio::sync::Mutex<
            futures::stream::SplitSink<
                tokio_tungstenite::WebSocketStream<
                    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
                >,
                tokio_tungstenite::tungstenite::Message,
            >,
        >,
    >,
) -> Result<()> {
    let mut state = state.lock().unwrap();
    let mut writer = writer.lock().await;
    match msg.body {
        Some(cinemotion_proto::server_message::Body::DeviceInit(init)) => {
            state.device_id = Some(init.id);
            state.init_acked = true;

            let device_spec = state.initial_device_spec.clone();
            writer
                .send(convert_message(
                    cinemotion_proto::client_message::Body::DeviceInitAck(
                        cinemotion_proto::DeviceInitAck {
                            device_spec: Some(device_spec),
                        },
                    ),
                ))
                .await?;
        }
        _ => {}
    }
    Ok(())
}

fn convert_message(
    body: cinemotion_proto::client_message::Body,
) -> tokio_tungstenite::tungstenite::Message {
    let msg = cinemotion_proto::ClientMessage { body: Some(body) };
    let data: bytes::Bytes = msg
        .try_into()
        .expect("failed to generate bytes for protocol message");
    tokio_tungstenite::tungstenite::Message::binary(data)
}
