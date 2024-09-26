use cinemotion_core::devices;
use cinemotion_core::error::*;
use cinemotion_core::globals;
use cinemotion_core::prelude::*;
use cinemotion_core::protocol;
use cinemotion_core::protocol::client_message::Body as ClientBody;
use cinemotion_core::state::*;

#[cfg(test)]
#[path = "backend_test.rs"]
mod backend_test;

#[allow(async_fn_in_trait)]
pub trait Backend {
    async fn reserve_device_id(&mut self) -> u32;
    async fn apply(&mut self, client: u32, message: protocol::client_message::Body) -> Result<()>;
    async fn update(&mut self) -> Result<StateTree>;
    async fn remove_client(&mut self, client: u32) -> Result<()>;
}

pub struct DefaultBackend {
    world: World,
}

impl DefaultBackend {
    pub fn new() -> Self {
        let mut world = world::new();
        scene::system::init(&mut world);

        DefaultBackend {
            world: world::new(),
        }
    }

    pub fn get_world_mut(&mut self) -> &mut World {
        &mut self.world
    }
}

impl Default for DefaultBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for DefaultBackend {
    /// Remove the client entity
    async fn remove_client(&mut self, client: u32) -> Result<()> {
        scene::system::remove_device_links(&mut self.world, client.clone())?;
        devices::system::remove_device_by_id(&mut self.world, client.into());
        Ok(())
    }

    /// Reserve a engine entity and return the ID.
    async fn reserve_device_id(&mut self) -> u32 {
        world::reserve_entity(&mut self.world)
    }

    async fn apply(&mut self, client: u32, message: protocol::client_message::Body) -> Result<()> {
        tracing::trace!(client, ?message, "applying message to session");
        match message {
            ClientBody::MotionSetMode(model) => {
                globals::system::set_motion_mode(&mut self.world, model.enabled);
                Ok(())
            }
            ClientBody::DeviceInitAck(_) | ClientBody::DeviceSample(_) => {
                devices::commands::process(client, &mut self.world, message).map(|_| ())
            }
            ClientBody::SceneCreateObject(_)
            | ClientBody::SceneUpdateObject(_)
            | ClientBody::SceneDeleteObject(_) => {
                scene::commands::procces(&mut self.world, message).map(|_| ())
            }

            // Do nothing
            ClientBody::Ping(_) | ClientBody::Pong(_) => Ok(()),
        }
    }

    async fn update(&mut self) -> Result<StateTree> {
        let world = &mut self.world;
        scene::system::update(world)?;
        let state = StateTree::new(&mut self.world);
        Ok(state)
    }
}
