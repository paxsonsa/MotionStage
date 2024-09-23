pub use crate::websocket::{connect, Connection};
pub use cinemotion_core::protocol;

pub struct Running;
pub struct Stopped;

#[derive(typed_builder::TypedBuilder)]
#[builder(field_defaults(setter(prefix = "with_")))]
pub struct Config {
    pub name: String,
    pub connection: Connection,
}

#[derive(Debug)]
pub enum RuntimeEvent {
    DeviceInit { version: u32, id: u32 },
    StateChange(cinemotion_core::state::StateTree),
}

pub struct Runtime<State> {
    id: u32,
    config: Config,
    handle: tokio::task::JoinHandle<()>,
    _state: std::marker::PhantomData<State>,
}

pub fn runtime(config: Config) -> Runtime<Stopped> {
    Runtime::<Stopped> {
        id: 0,
        config,
        handle: tokio::task::spawn(async {}),
        _state: std::marker::PhantomData,
    }
}

impl<Stopped> Runtime<Stopped> {
    pub async fn start(self) -> Runtime<Running> {
        let handle = tokio::task::spawn(async move {});
        Runtime::<Running> {
            id: 0,
            config: self.config,
            handle,
            _state: std::marker::PhantomData,
        }
    }
}

impl<Running> Runtime<Running> {
    pub async fn init(&mut self, device_spec: protocol::DeviceSpec) {
        self.config
            .connection
            .write(
                protocol::DeviceInitAck {
                    device_spec: Some(device_spec),
                }
                .into(),
            )
            .await
            .expect("failed to send message");
    }

    pub async fn next(&mut self) -> Option<RuntimeEvent> {
        self.config.connection.next().await.and_then(|msg| {
            let Some(body) = msg.body else {
                return None;
            };
            match body {
                protocol::server_message::Body::Error(err) => None,
                protocol::server_message::Body::Ping(_) => None,
                protocol::server_message::Body::Pong(_) => None,
                protocol::server_message::Body::DeviceInit(init) => {
                    Some(RuntimeEvent::DeviceInit {
                        version: init.version,
                        id: init.id,
                    })
                }
                protocol::server_message::Body::State(state) => {
                    Some(RuntimeEvent::StateChange(state.into()))
                }
            }
        })
    }
}

#[derive(Clone)]
pub struct RuntimeHandle {
    tx: tokio::sync::mpsc::UnboundedSender<Message>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Log(String),
    Command(String),
    // Add more message types as needed
}
