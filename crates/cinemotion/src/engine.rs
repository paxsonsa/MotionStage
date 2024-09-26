use crate::backend;
use crate::client::ConnectionHandle;
use cinemotion_core as core;
use core::protocol;
use std::collections::HashMap;

#[cfg(test)]
#[path = "engine_test.rs"]
mod engine_test;

pub enum EngineEvent {
    Tick(core::state::StateTree),
    Error(EngineError),
}

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
    pub async fn apply<Client: ConnectionHandle>(
        &mut self,
        client: &mut Box<Client>,
        message: protocol::ClientMessage,
    ) -> Result<(), EngineError> {
        let Some(device_id) = self.device_id_table.get(&client.id()) else {
            tracing::warn!(
                "failed to apply client message, no enity id mapped, is the client initialized?"
            );
            return Ok(());
        };
        let body = message.body.expect("client body missing");

        {
            let span = tracing::span!(tracing::Level::INFO, "Engine::apply", device_id, ?body);
            let _guard = span.enter();
            tracing::trace!("applying message: {:?}", body);
            // TODO: Handle Errors.
            self.inner
                .apply(*device_id, body)
                .await
                .expect("engine apply() failed");
        }
        Ok(())
    }

    pub async fn next(&mut self) -> Option<EngineEvent> {
        tokio::select! {
            _ = self.interval.tick() => {
                let state = self.inner.update().await.expect("Engine update failed");
                Some(EngineEvent::Tick(state))
            }
        }
    }

    pub async fn register_client<Client: ConnectionHandle>(&mut self, client: &mut Box<Client>) {
        let client_id = client.id();
        let id = self.inner.reserve_device_id().await;
        self.device_id_table.insert(client.id(), id);
        let message = protocol::ServerMessage {
            body: Some(protocol::server_message::Body::DeviceInit(
                protocol::DeviceInit { version: 1, id },
            )),
        };
        client.send(message).await;
        tracing::info!(id, client_id, "registered connection");
    }

    pub async fn remove_client<Client: ConnectionHandle>(
        &mut self,
        client: &Box<Client>,
    ) -> Result<(), EngineError> {
        let Some(id) = self.device_id_table.remove(&client.id()) else {
            return Ok(());
        };
        self.inner.remove_client(id).await;
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        for id in self.device_id_table.values() {
            self.inner.remove_client(*id).await;
        }
    }
}

/// Errors that can occur in the Engine.
#[derive(Clone, Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine is fatally failed")]
    EngineFailed,
}

/// Spawns a new EngineActor and returns its handle.
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
