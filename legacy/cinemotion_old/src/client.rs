use actix::prelude::*;
use actix_telepathy::prelude::*;
use serde::{Deserialize, Serialize};

/// A handle for interacting with a client.
#[derive(Debug, Clone)]
pub struct RemoteClientHandle {
    /// The ID of the client
    pub id: usize,
    /// The address of the client
    pub addr: RemoteAddr,
}

impl RemoteClientHandle {
    pub fn registered(&self) {
        self.addr.do_send(ClientMessage::Registered {});
    }

    pub fn close(&self, reason: &str) {
        self.addr.do_send(ClientMessage::Close {
            reason: reason.to_string(),
        });
    }
}

#[derive(RemoteMessage, Serialize, Deserialize)]
pub enum ClientMessage {
    Registered,
    Register { source: RemoteAddr },
    Ping,
    Pong,
    Close { reason: String },
}

/// A delegate for the client actor.
pub trait ClientObserver: Send {
    fn registered(&self);
    fn close(&self, reason: &str);
}

#[derive(RemoteActor)]
#[remote_messages(ClientMessage)]
pub struct ClientActor {
    observer: Box<dyn ClientObserver>,
}

impl ClientActor {
    pub fn new(observer: Box<dyn ClientObserver>) -> Self {
        Self { observer }
    }
}

impl Actor for ClientActor {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.register(ctx.address().recipient());
    }
}

impl Handler<ClientMessage> for ClientActor {
    type Result = ();

    fn handle(&mut self, msg: ClientMessage, _ctx: &mut Self::Context) -> Self::Result {
        match msg {
            ClientMessage::Registered => {
                println!("ClientActor::Registered");
                self.observer.registered();
            }
            ClientMessage::Close { reason } => {
                println!("ClientActor::Close: {}", reason);
                self.observer.close(&reason);
            }
            _ => {
                unimplemented!();
            }
        }
    }
}
