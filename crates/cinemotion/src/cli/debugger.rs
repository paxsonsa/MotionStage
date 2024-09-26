use anyhow::{anyhow, Result};
use chrono::{DateTime, Local};
use clap::Args;
use crossterm::{
    cursor,
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{FutureExt, SinkExt, StreamExt};
use ratatui::{
    backend::CrosstermBackend as Backend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout},
    style::{self, Style, Stylize},
    text::{self, Text, ToLine, ToText},
    widgets::{Block, Borders, List, ListItem, Padding, Paragraph, Widget},
    Terminal,
};
use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tracing::{
    field::{Field, Visit},
    Subscriber,
};
use tracing_subscriber::{fmt, prelude::*, registry::LookupSpan, EnvFilter};

use cinemotion::protocol;

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
        // TODO: Button to start and top motion/recording
        // TODO: Add Ping/Pong measurement for latency testing.
        // TODO: Add another area for storing the scene graph.
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

        let log_buffer = init_logging()?;
        let mut app = App::new(address, device_spec, Arc::clone(&log_buffer));
        app.run().await?;

        Ok(0)
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

fn init_logging() -> Result<Arc<Mutex<RingBuffer<LogEvent>>>, anyhow::Error> {
    const LOG_CAPACITY: usize = 10000;
    let log_buffer = Arc::new(Mutex::new(RingBuffer::new(LOG_CAPACITY)));
    let collector_layer = LogCollector {
        buffer: Arc::clone(&log_buffer),
    };
    let subscriber = tracing_subscriber::registry()
        .with(EnvFilter::new("info"))
        .with(collector_layer);
    tracing::subscriber::set_global_default(subscriber)?;
    Ok(log_buffer)
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
    AckInit(u32),
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
        init_panic_hook();
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
        // Render 5 times per second
        let render_rate = tokio::time::Duration::from_secs_f64(1.0 / 5.0);
        // Process 200 times per second
        let process_rate = tokio::time::Duration::from_secs_f64(1.0 / 5.0);

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
    address: String,
    device: protocol::DeviceSpec,
    should_quit: bool,
    log_buffer: Arc<Mutex<RingBuffer<LogEvent>>>,
}

impl App {
    fn new(
        address: String,
        device: protocol::DeviceSpec,
        log_buffer: Arc<Mutex<RingBuffer<LogEvent>>>,
    ) -> Self {
        Self {
            address,
            device,
            should_quit: false,
            log_buffer,
        }
    }
    async fn run(&mut self) -> anyhow::Result<()> {
        tracing::info!("initializing runtime.");
        let connection = cinemotion::connect(self.address.clone()).await?;
        let config = cinemotion::Config::builder()
            .with_name("cinemotion-default".to_string())
            .with_connection(connection)
            .build();
        let mut runtime = cinemotion::runtime(config).start().await;
        let mut tui = TerminalUI::new();
        tui.show()?;

        loop {
            tokio::select! {
                Some(event) = tui.next() => {
                    let mut maybe_action = self.handle_event(&mut tui, event).await;
                    while let Some(action) = maybe_action {
                        maybe_action = self.update(&mut runtime, action).await;
                    }
                },
                Some(event) = runtime.next() => {
                    tracing::info!("runtime message: {:?}", event);
                    let mut maybe_action = self.handle_runtime_event(event).await;
                    while let Some(action) = maybe_action {
                        maybe_action = self.update(&mut runtime, action).await;
                    }
                },
            }

            if self.should_quit {
                break;
            }
        }

        tui.hide()?;

        Ok(())
    }

    pub async fn handle_event(&mut self, tui: &mut TerminalUI, event: Event) -> Option<Action> {
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
                match key.code {
                    // Exit on Ctrl+C
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
            Event::Tick => None,
            Event::Render => {
                tui.draw(|f| {
                    self.render(f);
                })
                .expect("render should not fail to process");

                None
            }
            Event::Error => {
                tracing::error!("Error");
                None
            }
        }
    }

    async fn handle_runtime_event(&mut self, event: cinemotion::RuntimeEvent) -> Option<Action> {
        match event {
            cinemotion::RuntimeEvent::DeviceInit { version, id } => Some(Action::AckInit(id)),
            cinemotion::RuntimeEvent::StateChange(state) => {
                tracing::info!(?state, "state update");
                None
            }
        }
    }

    fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        let [log_area, input_area] =
            Layout::vertical([Constraint::Min(10), Constraint::Length(3)]).areas(frame.area());
        // Log window
        let log_widget = LogWidget {
            buffer: Arc::clone(&self.log_buffer),
        };
        frame.render_widget(log_widget, log_area);

        // Input field
        let input_paragraph = Paragraph::new("").block(
            Block::default()
                .title("Input")
                .borders(Borders::ALL)
                .padding(Padding::left(1)),
        );
        frame.render_widget(input_paragraph, input_area);
    }

    async fn update(
        &mut self,
        runtime: &mut cinemotion::Runtime<cinemotion::Running>,
        action: Action,
    ) -> Option<Action> {
        match action {
            Action::AckInit(_) => {
                runtime.init(self.device.clone()).await;
                None
            }
            Action::Quit => {
                self.should_quit = true;
                None
            }
        }
    }
}

struct LogWidget {
    buffer: Arc<Mutex<RingBuffer<LogEvent>>>,
}

impl Widget for LogWidget {
    fn render(self, area: ratatui::layout::Rect, buf: &mut Buffer) {
        let buffer = self.buffer.lock().unwrap();
        let lines_to_render = area.height as usize;
        let total_logs = buffer.size;
        let mut logs_iter = buffer.iter();

        if total_logs > lines_to_render {
            logs_iter.nth(total_logs - lines_to_render);
        }

        for (i, event_data) in logs_iter.take(lines_to_render).enumerate() {
            let fields_str = event_data
                .fields
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join(", ");

            let line = format!(
                "[{}] [{}] {} {}",
                event_data.timestamp.format("%Y-%m-%d %H:%M:%S"),
                event_data.level,
                event_data.message.as_deref().unwrap_or(""),
                if fields_str.is_empty() {
                    "".to_string()
                } else {
                    format!("{{{}}}", fields_str)
                }
            );

            buf.set_string(area.left(), area.top() + i as u16, line, Style::default());
        }
    }
}

struct RingBuffer<T> {
    buffer: Vec<Option<T>>,
    capacity: usize,
    start: usize,
    size: usize,
}

impl<T> RingBuffer<T> {
    fn new(capacity: usize) -> Self {
        let mut buffer: Vec<Option<T>> = Vec::with_capacity(1000);
        for _ in 0..capacity {
            buffer.push(None);
        }
        Self {
            buffer,
            capacity,
            start: 0,
            size: 0,
        }
    }

    fn push(&mut self, item: T) {
        if self.size < self.capacity {
            let idx = (self.start + self.size) % self.capacity;
            self.buffer[idx] = Some(item);
            self.size += 1;
        } else {
            self.buffer[self.start] = Some(item);
            self.start = (self.start + 1) % self.capacity;
        }
    }
    fn iter(&self) -> RingBufferIter<T> {
        RingBufferIter {
            buffer: &self.buffer,
            capacity: self.capacity,
            start: self.start,
            size: self.size,
            index: 0,
        }
    }
}

struct RingBufferIter<'a, T> {
    buffer: &'a [Option<T>],
    capacity: usize,
    start: usize,
    size: usize,
    index: usize,
}

impl<'a, T> Iterator for RingBufferIter<'a, T>
where
    T: 'a,
{
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.size {
            return None;
        }
        let pos = (self.start + self.index) % self.capacity;
        self.index += 1;
        self.buffer[pos].as_ref()
    }
}

impl<'a, T> DoubleEndedIterator for RingBufferIter<'a, T>
where
    T: 'a,
{
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.index >= self.size {
            return None;
        }
        let pos = (self.start + self.size - 1 - self.index) % self.capacity;
        self.index += 1;
        self.buffer[pos].as_ref()
    }
}

#[derive(Default)]
struct EventVisitor {
    fields: HashMap<String, String>,
    message: Option<String>,
}

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name().is_empty() {
            self.message = Some(format!("{:?}", value));
        } else {
            self.fields
                .insert(field.name().to_string(), format!("{:?}", value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name().is_empty() {
            self.message = Some(value.to_string());
        } else {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }
}
struct LogEvent {
    timestamp: DateTime<Local>,
    level: tracing::Level,
    message: Option<String>,
    fields: HashMap<String, String>,
}

struct LogCollector {
    buffer: Arc<Mutex<RingBuffer<LogEvent>>>,
}

impl<S> tracing_subscriber::Layer<S> for LogCollector
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        let mut buffer = self.buffer.lock().unwrap();
        buffer.push(LogEvent {
            timestamp: Local::now(),
            level: *event.metadata().level(),
            message: visitor.message,
            fields: visitor.fields,
        });
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

pub fn init_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // intentionally ignore errors here since we're already in a panic
        let _ = restore_tui();
        original_hook(panic_info);
    }));
}

pub fn restore_tui() -> std::io::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}
