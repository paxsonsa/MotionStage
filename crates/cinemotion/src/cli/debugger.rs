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
        // TODO: Setup Log Messaging
        // TODO: Configure debugger and setup server task.
        // TODO: Button to start and top motion/recording
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
    }
}

pub enum Event {
    Init,
    FocusGained,
    FocusLost,
    Key(crossterm::event::KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Paste(String),
    Resize(u16, u16),
    Tick,
    Render,
    Error,
}

pub enum Action {
    Quit,
}

struct TerminalUI {
    task: tokio::task::JoinHandle<()>,
    terminal: ratatui::Terminal<Backend<std::io::Stderr>>,
    cancellation: tokio_util::sync::CancellationToken,
    event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
}

impl TerminalUI {
    pub fn new() -> Self {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            task: tokio::task::spawn(async {}),
            terminal: ratatui::Terminal::new(Backend::new(std::io::stderr())).unwrap(),
            cancellation: tokio_util::sync::CancellationToken::new(),
            event_tx,
            event_rx,
        }
    }

    pub fn show(&mut self) -> anyhow::Result<()> {
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

    fn start(&mut self) -> Result<()> {
        // Render 60 times per second
        let render_rate = tokio::time::Duration::from_secs_f64(1.0 / 60.0);
        // Process 200 times per second
        let process_rate = tokio::time::Duration::from_secs_f64(1.0 / 200.0);

        // Stop any existing tasks
        self.cancel();
        self.cancellation = tokio_util::sync::CancellationToken::new();
        let _cancellation_token = self.cancellation.clone();
        let _event_tx = self.event_tx.clone();

        self.task = tokio::spawn(async move {
            let mut event_stream = crossterm::event::EventStream::new();
            let mut process_interval = tokio::time::interval(process_rate);
            let mut render_interval = tokio::time::interval(render_rate);
            _event_tx.send(Event::Init).expect("failed to send init");

            loop {
                let process_tick = process_interval.tick();
                let render_tick = render_interval.tick();
                let crossterm_event = event_stream.next().fuse();

                tokio::select! {
                    _ = _cancellation_token.cancelled() => {
                        break;
                    }
                    maybe_event = crossterm_event => {
                        match maybe_event {
                            Some(Ok(e)) => {
                                match e {
                                    crossterm::event::Event::FocusGained => _event_tx.send(Event::FocusGained).unwrap(),
                                    crossterm::event::Event::FocusLost => _event_tx.send(Event::FocusLost).unwrap(),
                                    crossterm::event::Event::Key(key) => _event_tx.send(Event::Key(key)).unwrap(),
                                    crossterm::event::Event::Mouse(mouse) => _event_tx.send(Event::Mouse(mouse)).unwrap(),
                                    crossterm::event::Event::Paste(p) => _event_tx.send(Event::Paste(p)).unwrap(),
                                    crossterm::event::Event::Resize(x, y) => _event_tx.send(Event::Resize(x, y)).unwrap(),
                                }
                            }
                            Some(Err(e)) => {
                                _event_tx.send(Event::Error).unwrap();
                            }
                            None => {},
                        }
                    }
                    _ = process_tick => {
                        _event_tx.send(Event::Tick).unwrap();
                    },
                    _ = render_tick => {
                        _event_tx.send(Event::Render).unwrap();
                    },
                }
            }
        });

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
        self.event_rx.recv().await
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
        match event {
            Event::Init => {
                tracing::info!("Initialized");
                None
            }
            Event::FocusGained => {
                tracing::info!("Focus Gained");
                None
            }
            Event::FocusLost => {
                tracing::info!("Focus Lost");
                None
            }
            Event::Key(key) => {
                tracing::info!("Key: {:?}", key);
                match key.code {
                    crossterm::event::KeyCode::Char('c')
                        if key.modifiers == crossterm::event::KeyModifiers::CONTROL =>
                    {
                        Some(Action::Quit)
                    }
                    crossterm::event::KeyCode::Char(c) => {
                        tracing::info!("Char: {}", c);
                        None
                    }
                    crossterm::event::KeyCode::Backspace => None,
                    crossterm::event::KeyCode::Enter => None,
                    // Exit on ESC
                    crossterm::event::KeyCode::Esc => Some(Action::Quit),
                    // Exit on Ctrl+C
                    _ => None,
                }
            }
            Event::Mouse(mouse) => {
                tracing::info!("Mouse: {:?}", mouse);
                None
            }
            Event::Paste(p) => {
                tracing::info!("Paste: {:?}", p);
                None
            }
            Event::Resize(x, y) => {
                tracing::info!("Resize: {}, {}", x, y);
                None
            }
            Event::Tick => {
                tracing::info!("Tick");
                None
            }
            Event::Render => {
                tracing::info!("Render");
                None
            }
            Event::Error => {
                tracing::error!("Error");
                None
            }
        }
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
        match action {
            Action::Quit => {
                self.should_quit = true;
                None
            }
        }
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
