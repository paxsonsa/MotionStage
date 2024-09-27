use crate::backend;
use crate::client::ConnectionHandle;
use cinemotion_core as core;
use core::protocol;
use std::collections::HashMap;

#[cfg(test)]
#[path = "engine_test.rs"]
mod engine_test;

/// Represents events that can occur in the engine.
pub enum EngineEvent {
    /// Event that occurs on each tick, containing the current state tree.
    Tick(core::state::StateTree),
    /// Event that occurs when an error happens in the engine.
    Error(EngineError),
}

/// The main engine struct that manages the core logic and state.
pub struct Engine<Backend>
where
    Backend: backend::Backend,
{
    interval: tokio::time::Interval,
    inner: Box<Backend>,
    device_id_table: HashMap<u32, u32>,
}

impl<Backend> Engine<Backend>
where
    Backend: backend::Backend,
{
    /// Applies a connection message to the engine.
    ///
    /// # Arguments
    ///
    /// * `connection` - The connection sending the message.
    /// * `message` - The message to apply.
    ///
    /// # Returns
    ///
    /// A `Result` indicating success or failure.
    pub async fn apply<Connection: ConnectionHandle>(
        &mut self,
        connection: &mut Box<Connection>,
        message: protocol::ClientMessage,
    ) -> Result<(), EngineError> {
        let Some(device_id) = self.device_id_table.get(&connection.id()) else {
            tracing::warn!(
                "failed to apply conn message, no entity id mapped, is the conn initialized?"
            );
            return Ok(());
        };
        let body = message.body.expect("conn body missing");

        {
            let span = tracing::span!(tracing::Level::INFO, "Engine::apply", device_id, ?body);
            let _guard = span.enter();
            tracing::trace!("applying message: {:?}", body);
            self.inner
                .apply(*device_id, body)
                .await
                .expect("engine apply() failed");
        }
        Ok(())
    }

    /// Retrieves the next event from the engine.
    ///
    /// # Returns
    ///
    /// An `Option` containing the next `EngineEvent`.
    pub async fn next(&mut self) -> Option<EngineEvent> {
        tokio::select! {
            _ = self.interval.tick() => {
                let state = self.inner.update().await.expect("Engine update failed");
                Some(EngineEvent::Tick(state))
            }
        }
    }

    /// Registers a new connection with the engine.
    ///
    /// # Arguments
    ///
    /// * `connection` - The connection to register.
    pub async fn registered_connection<Connection: ConnectionHandle>(
        &mut self,
        conn: &mut Box<Connection>,
    ) {
        let conn_id = conn.id();
        let id = self.inner.reserve_device_id().await;
        self.device_id_table.insert(conn.id(), id);
        let message = protocol::ServerMessage {
            body: Some(protocol::server_message::Body::DeviceInit(
                protocol::DeviceInit { version: 1, id },
            )),
        };
        conn.send(message).await;
        tracing::info!(id, conn_id, "registered connection");
    }

    /// Notifies the engine the connection was closed.
    ///
    /// # Arguments
    ///
    /// * `connection` - The connection to the was closed.
    ///
    /// # Returns
    ///
    /// A `Result` indicating success or failure.
    pub async fn closed_connection<Conn: ConnectionHandle>(
        &mut self,
        conn: &Box<Conn>,
    ) -> Result<(), EngineError> {
        let Some(id) = self.device_id_table.remove(&conn.id()) else {
            return Ok(());
        };
        self.inner
            .despawn_device_by_id(id)
            .await
            .expect("should not fail to remove conn from backend.");
        Ok(())
    }

    /// Shuts down the engine, removing all conns.
    pub async fn shutdown(&mut self) {
        for id in self.device_id_table.values() {
            if let Err(err) = self.inner.despawn_device_by_id(*id).await {
                tracing::error!("failed to remove connection while shutting down: {:?}", err);
            }
        }
    }
}

/// Errors that can occur in the Engine.
#[derive(Clone, Debug, thiserror::Error)]
pub enum EngineError {
    /// Error indicating that the engine has failed fatally.
    #[error("engine is fatally failed")]
    EngineFailed,
}

/// Spawns a new Engine and returns its handle.
///
/// # Arguments
///
/// * `backend` - The backend to use for the engine.
///
/// # Returns
///
/// A new `Engine` instance.
pub fn spawn<Backend>(backend: Backend) -> Engine<Backend>
where
    Backend: backend::Backend,
{
    Engine {
        inner: Box::new(backend),
        interval: tokio::time::interval(tokio::time::Duration::from_secs_f64(1.0 / 120.0)),
        device_id_table: Default::default(),
    }
}
