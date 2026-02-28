use actix::prelude::*;

use crate::session;

#[derive(Clone)]
pub struct RuntimeHandle {
    endpoint: Addr<Runtime>,
}

impl RuntimeHandle {
    /// Establish a new client session to the runtime
    ///
    /// Establisha new client session with the runtime that
    /// provides easy access to managing the client session from
    /// from the conneciton side. The returned session handle provides
    /// access to the outgoing message stream.
    pub async fn new_session(&self) -> session::SessionHandle {
        self.endpoint.send(NewSessionMsg).await.unwrap()
    }

    pub async fn remove_session(&self, id: usize) {
        self.endpoint.do_send(RemoveSessionMsg { id: id });
    }
}

pub struct Runtime {}

impl Runtime {
    pub fn new() -> RuntimeHandle {
        let endpoint = SyncArbiter::start(1, || Runtime {});
        RuntimeHandle { endpoint }
    }
}

impl Actor for Runtime {
    type Context = SyncContext<Self>;
}

/// New Session Message
///
/// Establish a new client session with the runtime.
#[derive(Message)]
#[rtype(result = "session::SessionHandle")]
struct NewSessionMsg;

impl Handler<NewSessionMsg> for Runtime {
    type Result = MessageResult<NewSessionMsg>;

    fn handle(&mut self, _: NewSessionMsg, ctx: &mut Self::Context) -> Self::Result {
        let session = session::Session::new(
            0,
            RuntimeHandle {
                endpoint: ctx.address(),
            },
        );
        MessageResult(session)
    }
}

/// Remove Session Message
///
/// Remove a client session from the runtime.
#[derive(Message)]
#[rtype(result = "()")]
struct RemoveSessionMsg {
    id: usize,
}

impl Handler<RemoveSessionMsg> for Runtime {
    type Result = ();

    fn handle(&mut self, msg: RemoveSessionMsg, ctx: &mut Self::Context) {
        // TODO: Implement session removal
    }
}
