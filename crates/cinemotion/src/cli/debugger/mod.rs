mod app;
mod log_view;
mod scene_tree;
mod terminal;

use anyhow::Result;
use cinemotion::protocol;
use clap::Args;
use std::{path::PathBuf, sync::Arc};

#[derive(Args)]
pub struct DebuggerCmd {
    #[clap(long = "address")]
    server_address: Option<String>,
    #[clap(long = "device")]
    device_spec_path: Option<PathBuf>,
    #[clap(long = "objects")]
    objects_spec_path: Option<PathBuf>,
}

static DEFAULT_ADDRESS: &str = "ws://0.0.0.0:7878";

impl DebuggerCmd {
    pub async fn run(&self) -> Result<i32> {
        // TODO: Add another area for storing the scene graph.
        // TODO: Button to start and top motion/recording
        // TODO: Add Ping/Pong measurement for latency testing.
        // TODO: Investigate the next()/write() style of instead of actors for client layer, it
        // might reduce generic complexity.
        // TODO: Make sure that we are not leaking the 'core' into the cinemotion API.
        // TODO: Add Sin wave testing.
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        )?;

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

        let log_buffer = log_view::init_logging()?;
        let mut app = app::App::new(address, device_spec, Arc::clone(&log_buffer));
        app.run().await?;

        Ok(0)
    }
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
