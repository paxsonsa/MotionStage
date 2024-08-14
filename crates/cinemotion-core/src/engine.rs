use std::collections::HashMap;

use crate::devices;
use crate::error::*;
use crate::globals;
use crate::prelude::*;
use crate::protocol;
use crate::protocol::client_message::Body as ClientBody;
use crate::state::*;

#[cfg(test)]
#[path = "engine_test.rs"]
mod engine_test;

pub struct Engine {
    world: World,
}

impl Engine {
    pub fn new() -> Self {
        let mut world = world::new();
        scene::system::init(&mut world);

        Engine {
            world: world::new(),
        }
    }

    pub fn get_world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// Reserve a engine entity and return the ID.
    pub async fn reserve_entity(&mut self) -> u32 {
        world::reserve_entity(&mut self.world)
    }

    pub async fn apply(
        &mut self,
        client: u32,
        message: protocol::client_message::Body,
    ) -> Result<()> {
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
