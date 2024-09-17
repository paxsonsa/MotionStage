use anyhow::{anyhow, Result};
use clap::Args;
use crossterm::{
    cursor,
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{FutureExt, SinkExt, StreamExt};
use ratatui::{
    backend::CrosstermBackend as Backend,
    layout::{Constraint, Direction, Layout},
    style::{self, Stylize},
    text::{self, Text, ToLine, ToText},
    widgets::{Block, Borders, List, ListItem, Padding, Paragraph},
    Terminal,
};
use std::{
    ops::{Deref, DerefMut},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use cinemotion_core::protocol;

#[derive(Args)]
pub struct DebuggerCmd {
    #[clap(long = "address")]
    server_address: Option<String>,
    #[clap(long = "device")]
    device_spec_path: Option<PathBuf>,
    #[clap(long = "objects")]
    objects_spec_path: Option<PathBuf>,
}

static DEFAULT_ADDRESS: &str = "ws://0.0.0.0:7788";

impl DebuggerCmd {
    pub async fn run(&self) -> Result<i32> {
        // TODO: Handle Keyboard input and setup basic terminal events
        // TODO: Configure debugger and setup server task.
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

        let mut app = App::default();
        app.run().await?;

        Ok(0)
        // Create shared screen state and log holder
        // let log = vec![];
        // let log = Arc::new(Mutex::new(log));
        // let log_for_tracing = Arc::clone(&log);
        // let screen_state = Arc::new(Screen {
        //     app: Arc::new(Mutex::new(App::default())),
        //     log,
        // });
        //
        // // Set up tracing subscriber with custom log writer
        // let subscriber = tracing_subscriber::registry()
        //     .with(tracing_subscriber::EnvFilter::new("info"))
        //     .with(fmt::Layer::new().with_writer(Box::new(move || LogWriter {
        //         log: Arc::clone(&log_for_tracing),
        //     })));
        // tracing::subscriber::set_global_default(subscriber)?;

        // let conn = cinemotion::connect(address.clone()).await?;
        // let runtime = cinemotion::Runtime::<_>::builder()
        //     .name("cinemotion-debugger".to_string())
        //     .connection(conn)
        //     .runtime_fn(Box::new(|message| {
        //         Box::pin(async move {
        //             None
        //         })
        //             as std::pin::Pin<Box<dyn future::Future<Output = Option<()>> + Send>>
        //     }))
        //     .build();
        //
        // let runtime_handle = runtime.start().await;
        //

        // if crossterm::event::poll(std::time::Duration::from_millis(50))? {
        //     if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
        //         match key.code {
        //             crossterm::event::KeyCode::Char('c')
        //                 if key.modifiers == crossterm::event::KeyModifiers::CONTROL =>
        //             {
        //                 break;
        //             }
        //             crossterm::event::KeyCode::Char(c) => input_text.push(c),
        //             crossterm::event::KeyCode::Backspace => {
        //                 input_text.pop();
        //             }
        //             crossterm::event::KeyCode::Enter => {
        //                 if let Ok(_) = screen_state.submit_command(&input_text) {}
        //                 input_text.clear();
        //             }
        //             // Exit on ESC
        //             crossterm::event::KeyCode::Esc => break,
        //             // Exit on Ctrl+C
        //             _ => {}
        //         }
        //     }
        // }
        // }
    }
}

pub enum Event {}

pub enum Action {}

struct TerminalUI {
    task: tokio::task::JoinHandle<()>,
    terminal: ratatui::Terminal<Backend<std::io::Stderr>>,
    cancellation: tokio_util::sync::CancellationToken,
}

impl TerminalUI {
    pub fn new() -> Self {
        Self {
            task: tokio::task::spawn(async {}),
            terminal: ratatui::Terminal::new(Backend::new(std::io::stderr())).unwrap(),
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub fn show(&self) -> anyhow::Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(std::io::stderr(), EnterAlternateScreen, cursor::Hide)?;
        crossterm::execute!(std::io::stderr(), EnableMouseCapture)?;
        crossterm::execute!(std::io::stderr(), EnableBracketedPaste)?;
        self.start()?;
        Ok(())
    }

    pub fn hide(&self) -> anyhow::Result<()> {
        self.stop()?;
        if crossterm::terminal::is_raw_mode_enabled()? {
            crossterm::execute!(std::io::stderr(), DisableBracketedPaste)?;
            crossterm::execute!(std::io::stderr(), DisableMouseCapture)?;
            crossterm::execute!(std::io::stderr(), LeaveAlternateScreen, cursor::Show)?;
            crossterm::terminal::disable_raw_mode()?;
        }
        Ok(())
    }

    fn start(&self) -> Result<()> {
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        self.cancel();
        let mut counter = 0;
        while !self.task.is_finished() {
            std::thread::sleep(std::time::Duration::from_millis(1));
            counter += 1;
            if counter > 50 {
                self.task.abort();
            }
            if counter > 100 {
                tracing::error!("Failed to abort task in 100 milliseconds for unknown reason");
                break;
            }
        }
        Ok(())
    }

    pub async fn next(&mut self) -> Option<Event> {
        None
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn suspend(&mut self) -> Result<()> {
        self.hide()?;
        #[cfg(not(windows))]
        signal_hook::low_level::raise(signal_hook::consts::signal::SIGTSTP)?;
        Ok(())
    }

    pub fn resume(&mut self) -> Result<()> {
        self.show()?;
        Ok(())
    }
}

impl Deref for TerminalUI {
    type Target = ratatui::Terminal<Backend<std::io::Stderr>>;

    fn deref(&self) -> &Self::Target {
        &self.terminal
    }
}

impl DerefMut for TerminalUI {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.terminal
    }
}

impl Drop for TerminalUI {
    fn drop(&mut self) {
        self.stop().unwrap();
    }
}

struct App {
    should_quit: bool,
}

impl Default for App {
    fn default() -> Self {
        Self { should_quit: false }
    }
}

impl App {
    async fn run(&mut self) -> anyhow::Result<()> {
        let mut tui = TerminalUI::new();
        tui.show()?;

        loop {
            tui.draw(|f| {
                self.render(f);
            })?;

            if let Some(event) = tui.next().await {
                let mut maybe_action = self.handle_event(event).await;
                while let Some(action) = maybe_action {
                    maybe_action = self.update(action).await;
                }
            };

            if self.should_quit {
                break;
            }
        }

        tui.hide()?;

        Ok(())
    }

    pub async fn handle_event(&mut self, event: Event) -> Option<Action> {
        None
    }

    pub fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        let size = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(10), Constraint::Length(3)].as_ref())
            .split(size);

        // Log window
        let log_list =
            List::new(["hello, world."]).block(Block::default().title("Log").borders(Borders::ALL));
        frame.render_widget(log_list, chunks[0]);

        // Input field
        let input_paragraph = Paragraph::new("").block(
            Block::default()
                .title("Input")
                .borders(Borders::ALL)
                .padding(Padding::left(1)),
        );
        frame.render_widget(input_paragraph, chunks[1]);
    }

    pub async fn update(&mut self, action: Action) -> Option<Action> {
        None
    }
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

fn convert_message(
    body: cinemotion_proto::client_message::Body,
) -> tokio_tungstenite::tungstenite::Message {
    let msg = cinemotion_proto::ClientMessage { body: Some(body) };
    let data: bytes::Bytes = msg
        .try_into()
        .expect("failed to generate bytes for protocol message");
    tokio_tungstenite::tungstenite::Message::binary(data)
}
