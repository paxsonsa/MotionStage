use bevy_ecs::system::{SystemParam, SystemParamItem, SystemState};

use crate::error::*;
use crate::prelude::*;
use crate::protocol;
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

pub struct EngineState<'a, Param: SystemParam + 'static> {
    system_state: SystemState<Param>,
    param_item: SystemParamItem<'a, 'a, Param>,
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

    pub async fn apply(&mut self, client: i32, message: protocol::ClientMessage) -> Result<()> {
        let body = message
            .body
            .expect("message body should always be present, issue with serialization...");
        match body {
            protocol::client_message::Body::InitializeAck(_) => todo!(),
        };
        Ok(())
    }

    pub async fn serialize(&mut self) -> StateTree {
        let state = StateTree::new();
        //
        // for device in self.world.query::<(&Device)>().iter() {
        //     state.devices.push(device)
        // }

        state
    }
}
