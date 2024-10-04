use anyhow::Result;
use crossterm::{
    cursor,
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{FutureExt, StreamExt};
use ratatui::backend::CrosstermBackend as Backend;
use std::ops::{Deref, DerefMut};

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

pub struct Terminal {
    task: tokio::task::JoinHandle<()>,
    terminal: ratatui::Terminal<Backend<std::io::Stderr>>,
    cancellation: tokio_util::sync::CancellationToken,
    event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
}

impl Terminal {
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
                            Some(Err(_)) => {
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

impl Deref for Terminal {
    type Target = ratatui::Terminal<Backend<std::io::Stderr>>;

    fn deref(&self) -> &Self::Target {
        &self.terminal
    }
}

impl DerefMut for Terminal {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.terminal
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.stop().unwrap();
    }
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
