use std::collections::HashMap;

use crate::devices;
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
            protocol::client_message::Body::InitializeAck(model) => {
                let Some(spec) = model.device_spec else {
                    return Err(Error::InvalidValue("device spec is missing".to_string()));
                };

                let mut device = Device::new(client.into(), spec.name);
                let mut attributes = HashMap::<Name, Attribute>::new();
                for (name, value) in spec.attributes {
                    let Some(value) = value.value else {
                        return Err(Error::InvalidValue(format!(
                            "device spec attribute '{}' is missing a value",
                            name
                        )));
                    };
                    let value = match value {
                        protocol::attribute_value::Value::Float(v) => AttributeValue::Float(v),
                        protocol::attribute_value::Value::Vec3(v) => {
                            AttributeValue::Vec3((v.x, v.y, v.z).into())
                        }
                        protocol::attribute_value::Value::Vec4(v) => {
                            AttributeValue::Vec4((v.x, v.y, v.z, v.w).into())
                        }
                        protocol::attribute_value::Value::Matrix44(v) => {
                            if v.values.len() != 16 {
                                return Err(Error::InvalidValue(format!(
                                    "device spec attribute '{}' matrix44 value has invalid length",
                                    name
                                )));
                            }
                            AttributeValue::Matrix44(v.values.into())
                        }
                    };

                    let name: Name = name.into();
                    attributes.insert(name.clone(), Attribute::new(name, value));
                }
                device.attributes = attributes.into();
                devices::system::add_device(&mut self.world, device);
                Ok(())
            }
            protocol::client_message::Body::SceneCreateObject(_)
            | protocol::client_message::Body::SceneUpdateObject(_)
            | protocol::client_message::Body::SceneDeleteObject(_) => {
                scene::commands::procces(&mut self.world, message).map(|_| ())
            }
        }
    }

    pub async fn serialize(&mut self) -> Result<StateTree> {
        let state = StateTree::new();
        //
        // for device in self.world.query::<(&Device)>().iter() {
        //     state.devices.push(device)
        // }

        Ok(state)
    }
}
