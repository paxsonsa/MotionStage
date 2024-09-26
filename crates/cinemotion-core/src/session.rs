use std::collections::HashMap;

use crate::devices;
use crate::error::*;
use crate::globals;
use crate::prelude::*;
use crate::protocol;
use crate::protocol::client_message::Body as ClientBody;
use crate::state::*;

#[cfg(test)]
#[path = "session_test.rs"]
mod session_test;

pub struct Session {
    world: World,
}

impl Session {
    pub fn new() -> Self {
        let mut world = world::new();
        scene::system::init(&mut world);

        Session {
            world: world::new(),
        }
    }

    pub fn get_world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// Remove the client entity
    pub async fn remove_client(&mut self, client: u32) -> Result<()> {
        scene::system::remove_device_links(&mut self.world, client.clone())?;
        devices::system::remove_device_by_id(&mut self.world, client.into());
        Ok(())
    }

    /// Reserve a engine entity and return the ID.
    pub async fn reserve_device_id(&mut self) -> u32 {
        world::reserve_entity(&mut self.world)
    }

    pub async fn apply(
        &mut self,
        client: u32,
        message: protocol::client_message::Body,
    ) -> Result<()> {
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

    pub async fn update(&mut self) -> Result<StateTree> {
        let world = &mut self.world;
        scene::system::update(world)?;
        let state = StateTree::new(&mut self.world);
        Ok(state)
    }
}
