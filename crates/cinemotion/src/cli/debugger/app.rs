use super::log_view;
use super::scene_tree;
use super::terminal;

use ratatui::{
    layout::{Constraint, Layout},
    widgets::{Block, Borders, Padding, Paragraph},
};
use std::sync::{Arc, Mutex};
use tui_tree_widget::{TreeItem, TreeState};

use cinemotion::protocol;
pub enum Action {
    AckInit(u32),
    UpdateState(cinemotion_core::state::GlobalState),
    Render,
    Quit,
}

pub struct App {
    address: String,
    device: protocol::DeviceSpec,
    should_quit: bool,
    scene_state: cinemotion_core::state::GlobalState,
    log_buffer: Arc<Mutex<log_view::RingBuffer<log_view::LogEvent>>>,
    scene_tree_state: TreeState<u32>,
}

impl App {
    pub fn new(
        address: String,
        device: protocol::DeviceSpec,
        log_buffer: Arc<Mutex<log_view::RingBuffer<log_view::LogEvent>>>,
    ) -> Self {
        Self {
            address,
            device,
            should_quit: false,
            scene_state: Default::default(),
            log_buffer,
            scene_tree_state: Default::default(),
        }
    }
    pub async fn run(&mut self) -> anyhow::Result<()> {
        tracing::info!("initializing runtime.");
        let connection = cinemotion::connect(self.address.clone()).await?;
        let config = cinemotion::Config::builder()
            .with_name("cinemotion-default".to_string())
            .with_connection(connection)
            .build();
        let mut runtime = cinemotion::runtime(config).start().await;
        let mut tui = terminal::Terminal::new();
        tui.show()?;

        loop {
            tokio::select! {
                Some(event) = tui.next() => {
                    let mut maybe_action = self.handle_event(&mut tui, event).await;
                    while let Some(action) = maybe_action {
                        maybe_action = self.update(&mut tui, &mut runtime, action).await;
                    }
                },
                Some(event) = runtime.next() => {
                    let mut maybe_action = self.handle_runtime_event(event).await;
                    while let Some(action) = maybe_action {
                        maybe_action = self.update(&mut tui, &mut runtime, action).await;
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

    pub async fn handle_event(
        &mut self,
        tui: &mut terminal::Terminal,
        event: terminal::Event,
    ) -> Option<Action> {
        match event {
            terminal::Event::Init => {
                tracing::info!("Initialized");
                None
            }
            terminal::Event::FocusGained => {
                tracing::info!("Focus Gained");
                None
            }
            terminal::Event::FocusLost => {
                tracing::info!("Focus Lost");
                None
            }
            terminal::Event::Key(key) => {
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
            terminal::Event::Mouse(mouse) => {
                tracing::info!("Mouse: {:?}", mouse);
                None
            }
            terminal::Event::Paste(p) => {
                tracing::info!("Paste: {:?}", p);
                None
            }
            terminal::Event::Resize(x, y) => {
                tracing::info!("Resize: {}, {}", x, y);
                None
            }
            terminal::Event::Tick => None,
            terminal::Event::Render => Some(Action::Render),
            terminal::Event::Error => {
                tracing::error!("Error");
                None
            }
        }
    }

    async fn handle_runtime_event(&mut self, event: cinemotion::RuntimeEvent) -> Option<Action> {
        match event {
            cinemotion::RuntimeEvent::DeviceInit { version: _, id } => Some(Action::AckInit(id)),
            cinemotion::RuntimeEvent::StateChange(state) => Some(Action::UpdateState(state)),
        }
    }

    fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        let [top_area, input_area] =
            Layout::vertical([Constraint::Min(10), Constraint::Length(3)]).areas(frame.area());

        let [log_area, scene_graph_area] =
            Layout::horizontal([Constraint::Percentage(75), Constraint::Min(10)]).areas(top_area);
        // Log window
        let log_widget = log_view::LogWidget {
            buffer: Arc::clone(&self.log_buffer),
        };

        frame.render_widget(log_widget, log_area);

        // Scene Graph Window
        let scene_graph_widget = scene_tree::SceneGraphWidget::new(&self.scene_state);
        frame.render_stateful_widget(
            scene_graph_widget,
            scene_graph_area,
            &mut self.scene_tree_state,
        );

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
        tui: &mut terminal::Terminal,
        runtime: &mut cinemotion::Runtime<cinemotion::Running>,
        action: Action,
    ) -> Option<Action> {
        match action {
            Action::AckInit(id) => {
                runtime.init(id, self.device.clone()).await;
                None
            }
            Action::UpdateState(state_tree) => {
                let mut index: u32 = 0;
                let mut scene_graph = TreeItem::new(index, "Scene State", vec![])
                    .expect("scene graph item failed to create");
                let mut opened = vec![index];
                self.scene_tree_state.open(opened.clone());
                index += 1;

                let mut devices = TreeItem::new(index, "devices".to_string(), vec![])
                    .expect("failed to create tree item");
                opened.push(index);
                self.scene_tree_state.open(opened.clone());
                index += 1;

                for (_id, device) in state_tree.devices.into_iter() {
                    let name = device.name;
                    let attributes = device.attributes;
                    let mut device_item = TreeItem::new(index, name.to_string(), vec![])
                        .expect("device item failed to create");
                    opened.push(index);
                    self.scene_tree_state.open(opened.clone());
                    index += 1;
                    for (name, attr) in attributes.iter() {
                        // TODO: Render Attribute.
                        let attribute_item = TreeItem::new_leaf(index, name.to_string());
                        device_item
                            .add_child(attribute_item)
                            .expect("failed to add attribute item to scene graph");
                        opened.push(index);
                        self.scene_tree_state.open(opened.to_vec());
                        index += 1;
                    }
                    devices
                        .add_child(device_item)
                        .expect("failed to add device to scene graph");
                }
                scene_graph.add_child(devices).expect("failed to add child");

                // tracing::info!(?opened, "opening");
                self.scene_graph = scene_graph;
                // self.scene_tree_state.open(opened);

                Some(Action::Render)
            }
            Action::Render => {
                tui.draw(|f| {
                    self.render(f);
                })
                .expect("render should not fail to process");
                None
            }
            Action::Quit => {
                self.should_quit = true;
                None
            }
        }
    }
}
