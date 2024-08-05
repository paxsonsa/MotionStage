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

macro_rules! invoke {
    ($option:expr, $method:ident $(, $args:expr)*) => {
        if let Some(ref value) = $option {
            value.$method($($args),*).await?;
        }
    };
}

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
        }
    }

    pub async fn update(&mut self) -> Result<StateTree> {
        let state = StateTree::new(&mut self.world);
        Ok(state)
    }
}
