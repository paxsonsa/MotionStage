use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use actix::prelude::*;
use actix_broker::BrokerSubscribe;
use actix_telepathy::prelude::*;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::client::RemoteClientHandle;
use crate::engine::Engine;

#[cfg(test)]
#[path = "./runtime_test.rs"]
mod runtime_test;

pub struct Runtime {
    addr: std::net::SocketAddr,
    actor: RuntimeActor,
    cancellation: CancellationToken,
}

impl Runtime {
    pub fn new(addr: std::net::SocketAddr, engine: Box<dyn Engine>) -> Self {
        let cancellation = CancellationToken::new();
        let actor = RuntimeActor::new(engine, cancellation.clone());
        Self {
            addr,
            actor,
            cancellation,
        }
    }

    pub async fn start(self) -> anyhow::Result<()> {
        let _addr = self.actor.start();
        let _cluster = Cluster::new(self.addr, vec![]);

        tokio::select! {
            _ = self.cancellation.cancelled() => {
            }
            _ = tokio::signal::ctrl_c() => {
            }
        }
        System::current().stop();
        Ok(())
    }
}

#[derive(RemoteMessage, Serialize, Deserialize)]
#[with_source(source)]
pub struct RegisterMessage {
    source: RemoteAddr,
}

#[derive(RemoteMessage, Serialize, Deserialize)]
pub struct PingMessage {}

#[derive(RemoteActor)]
#[remote_messages(RegisterMessage)]
pub struct RuntimeActor {
    engine: Box<dyn Engine>,
    cancellation: CancellationToken,
    clients: ClientRegistry,
}

impl RuntimeActor {
    /// Create a new runtime instance.
    ///
    /// Establish a new runtime instance that will listen to the given address
    /// and use the given engine to manage scene state.
    ///
    /// * `addr` - The address to bind the runtime to.
    /// * `engine` - The engine to use for managing scene state.
    ///
    pub fn new(engine: Box<dyn Engine>, cancellation: CancellationToken) -> Self {
        Self {
            engine,
            cancellation,
            clients: ClientRegistry::new(),
        }
    }
}

impl Actor for RuntimeActor {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.register(ctx.address().recipient());
        self.subscribe_system_async::<ClusterLog>(ctx);
    }

    fn stopping(&mut self, ctx: &mut Self::Context) -> Running {
        self.clients.shutdown("runtime was stopped");
        Running::Stop
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        self.cancellation.cancel();
    }
}

impl Handler<PingMessage> for RuntimeActor {
    type Result = ();

    fn handle(&mut self, msg: PingMessage, _ctx: &mut Self::Context) -> Self::Result {
        println!("Received ping message");
    }
}

impl Handler<RegisterMessage> for RuntimeActor {
    type Result = ();

    fn handle(&mut self, msg: RegisterMessage, _ctx: &mut Self::Context) -> Self::Result {
        // Register the client.
        let client = self.clients.register(msg.source);

        // Acknowledge the client's registration.
        client.registered();
    }
}

impl Handler<ClusterLog> for RuntimeActor {
    type Result = ();

    fn handle(&mut self, msg: ClusterLog, _ctx: &mut Self::Context) -> Self::Result {
        match msg {
            ClusterLog::NewMember(node) => {
                tracing::info!("New member joined: {:?}", node);
            }
            ClusterLog::MemberLeft(addr) => {
                self.clients.deregister_node_by_addr(addr);
            }
        }
    }
}

/// A handle to interact with the runtime.
///
/// This provides a more ergonomic way to interact with the runtime actor.
/// The runtime handle can be used as a local interface to a remote actor.
pub struct RuntimeHandle {
    pub addr: RemoteAddr,
}

impl RuntimeHandle {
    pub fn register_client(&self, addr: RemoteAddr) {
        self.addr.do_send(RegisterMessage { source: addr });
    }

    pub fn ping(&self) {
        self.addr.do_send(PingMessage {});
    }
}

/// A registry to manage the clients connected
struct ClientRegistry {
    /// A set of remote addresses that are connected to the server.
    client_addrs: HashMap<RemoteAddr, Arc<RemoteClientHandle>>,
    /// The next client ID to assign to a new client.
    next_client_id: AtomicUsize,
}

impl ClientRegistry {
    pub fn new() -> Self {
        Self {
            client_addrs: HashMap::new(),
            next_client_id: AtomicUsize::new(0),
        }
    }

    /// Register a new client with the registry.
    ///
    /// * `addr` - The address of the client to register.
    pub fn register(&mut self, addr: RemoteAddr) -> Arc<RemoteClientHandle> {
        let id = self.next_client_id.fetch_add(1, Ordering::Relaxed);
        let handle = Arc::new(RemoteClientHandle {
            id,
            addr: addr.clone(),
        });
        self.client_addrs.insert(addr.clone(), handle.clone());
        handle
    }

    /// Deregister all clients that are connected from the given socket addr.
    ///
    /// The nature of disconnects means we can know the socket address that diconnected and
    /// left the cluster. This method will deregister all clients that are associated with the
    /// given socket address.
    ///
    /// * `socket_addr` - The socket address to deregister client associated too.
    pub fn deregister_node_by_addr(&mut self, socket_addr: SocketAddr) {
        self.client_addrs
            .retain(|addr, _| addr.node.socket_addr != socket_addr);
    }

    pub fn shutdown(&self, reason: &str) {
        for client in self.client_addrs.values() {
            client.close(reason);
        }
    }
}
