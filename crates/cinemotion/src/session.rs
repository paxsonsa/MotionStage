use actix::prelude::*;
use std::time::Instant;
use tokio::sync::mpsc;

use crate::rt;

/// Session Event
pub enum SessionEvent {
    Message(String),
}

pub struct SessionHandle {
    pub id: usize,
    endpoint: Addr<Session>,
    pub event_stream: mpsc::UnboundedReceiver<SessionEvent>,
}

impl SessionHandle {
    pub async fn close(&self) {}

    pub async fn ping(&self) {}

    pub async fn pong(&self) {}
}

pub struct Session {
    /// unique client session id
    pub id: usize,
    pub last_heartbeat: Instant,
    pub runtime: rt::RuntimeHandle,
    pub event_stream: mpsc::UnboundedSender<SessionEvent>,
}

impl Session {
    pub fn new(id: usize, runtime: rt::RuntimeHandle) -> SessionHandle {
        let (tx, rx) = mpsc::unbounded_channel();
        let endpoint = SyncArbiter::start(1, move || Session {
            id,
            last_heartbeat: Instant::now(),
            runtime: runtime.clone(),
            event_stream: tx.clone(),
        });

        SessionHandle {
            id,
            endpoint,
            event_stream: rx,
        }
    }
}

impl Actor for Session {
    type Context = SyncContext<Self>;

    fn stopped(&mut self, ctx: &mut Self::Context) {
        self.runtime.remove_session(self.id);
    }
}

/// Close Session Command
#[derive(Message)]
#[rtype(result = "()")]
struct CloseSession;

impl Handler<CloseSession> for Session {
    type Result = ();

    fn handle(&mut self, _: CloseSession, ctx: &mut Self::Context) -> Self::Result {
        ctx.stop();
    }
}
