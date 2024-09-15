use anyhow::{anyhow, Result};
use cinemotion_core::protocol;
use clap::Args;
use crossterm::terminal;
use futures::SinkExt;
use futures_util::{future, pin_mut, StreamExt};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{self, Stylize},
    text::{self, Text, ToLine, ToText},
    widgets::{Block, Borders, List, ListItem, Padding, Paragraph},
    Terminal,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncBufReadExt;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Clone, clap::ValueEnum)]
enum Mode {
    Debugger,
    Observer,
}

/// Start the cinemotion broker services.
#[derive(Args)]
pub struct DebuggerCmd {
    #[clap(long = "address")]
    server_address: Option<String>,
    #[clap(long = "device")]
    device_spec_path: Option<PathBuf>,
    #[clap(long = "objects")]
    objects_spec_path: Option<PathBuf>,
    #[clap(long = "mode", default_value = "debugger")]
    mode: Mode,
}

static DEFAULT_ADDRESS: &str = "ws://0.0.0.0:7788";

impl DebuggerCmd {
    pub async fn run(&self) -> Result<i32> {
        terminal::enable_raw_mode()?;
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        )?;

        let mut stdout = std::io::stdout();
        let backend = ratatui::backend::CrosstermBackend::new(&mut stdout);
        let mut terminal = ratatui::Terminal::new(backend)?;

        // Preemptive clear of the window
        terminal.clear()?;

        // Create shared screen state and log holder
        let log = vec![];
        let log = Arc::new(Mutex::new(log));
        let log_for_tracing = Arc::clone(&log);
        let screen_state = Arc::new(Screen {
            app: Arc::new(Mutex::new(App::default())),
            log,
        });

        // Set up tracing subscriber with custom log writer
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new("info"))
            .with(fmt::Layer::new().with_writer(Box::new(move || LogWriter {
                log: Arc::clone(&log_for_tracing),
            })));
        tracing::subscriber::set_global_default(subscriber)?;

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

        // let conn = cinemotion::connect(address.clone()).await?;
        // let runtime = cinemotion::Runtime::<_>::builder()
        //     .name("cinemotion-debugger".to_string())
        //     .connection(conn)
        //     .runtime_fn(Box::new(|message| {
        //         Box::pin(async move {
        //             // TODO: Handle incoming messages.
        //             None
        //         })
        //             as std::pin::Pin<Box<dyn future::Future<Output = Option<()>> + Send>>
        //     }))
        //     .build();
        //
        // let runtime_handle = runtime.start().await;
        //
        let mut input_text = String::new();
        screen_state.message("Debugger started, awaiting command".into());

        // Render the log messages and input field to the terminal
        while screen_state.app.lock().unwrap().is_running {
            terminal.draw(|f| {
                let size = f.area();
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(10), Constraint::Length(3)].as_ref())
                    .split(size);

                // Log window

                // FIXME: Look into better state management flux/ELM
                let log_list = List::new(screen_state.log())
                    .block(Block::default().title("Log").borders(Borders::ALL));
                f.render_widget(log_list, chunks[0]);

                // Input field
                let input_paragraph = Paragraph::new(input_text.as_str()).block(
                    Block::default()
                        .title("Input")
                        .borders(Borders::ALL)
                        .padding(Padding::left(1)),
                );
                f.render_widget(input_paragraph, chunks[1]);
            })?;

            if crossterm::event::poll(std::time::Duration::from_millis(50))? {
                if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                    match key.code {
                        crossterm::event::KeyCode::Char('c')
                            if key.modifiers == crossterm::event::KeyModifiers::CONTROL =>
                        {
                            break;
                        }
                        crossterm::event::KeyCode::Char(c) => input_text.push(c),
                        crossterm::event::KeyCode::Backspace => {
                            input_text.pop();
                        }
                        crossterm::event::KeyCode::Enter => {
                            if let Ok(_) = screen_state.submit_command(&input_text) {}
                            input_text.clear();
                        }
                        // Exit on ESC
                        crossterm::event::KeyCode::Esc => break,
                        // Exit on Ctrl+C
                        _ => {}
                    }
                }
            }
        }
        terminal.clear()?;
        terminal::disable_raw_mode()?;
        Ok(0)
    }
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("invalid command: '{0}'")]
    InvalidCommand(String),
}

struct LogWriter {
    log: Arc<Mutex<Vec<text::Line<'static>>>>,
}

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let log_line = String::from_utf8_lossy(buf).into_owned();
        let mut logs = self.log.lock().unwrap();
        logs.push(log_line.into());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Screen {
    app: Arc<Mutex<App>>,
    log: Arc<Mutex<Vec<text::Line<'static>>>>,
}

impl Screen {
    fn present_error<T>(&self, err: Error) -> Result<T, Error> {
        let mut logs = self.log.lock().unwrap();
        logs.push(text::Line::styled(
            format!("ERROR: {err}"),
            style::Style::default().red(),
        ));
        Err(err)
    }
    fn message(&self, msg: String) {
        let mut logs = self.log.lock().unwrap();
        logs.push(format!("> {msg}").into());
    }

    fn submit_command<'a>(&self, command: &'a str) -> Result<(), Error> {
        let cmd = command.trim();
        let command_tokens: Vec<&str> = cmd.split_whitespace().collect();

        let main_command = match command_tokens.get(0) {
            Some(command) => command,
            None => {
                return Err(Error::InvalidCommand("".into()));
            }
        };
        let args = &command_tokens[1..];

        match *main_command {
            // Connect Command
            "help" => {
                // Better Help Output that is more readable.
                self.message("Commands: help, clear, exit".into());
            }
            "clear" => {
                self.log.lock().unwrap().clear();
            }
            "exit" => {
                // TODO: Prompt for confirmation
                self.app.lock().unwrap().is_running = false;
            }
            _ => {
                // FIXME: Why is error not colored in terminal?
                return self.present_error(Error::InvalidCommand(main_command.to_string()));
            }
        };
        Ok(())
    }
    fn log(&self) -> Vec<text::Line> {
        self.log.lock().unwrap().clone()
    }
}

struct App {
    is_running: bool,
}

impl Default for App {
    fn default() -> Self {
        Self { is_running: true }
    }
}

#[derive(Default)]
struct DebuggerState {
    init_acked: bool,
    motion_enabled: bool,
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
