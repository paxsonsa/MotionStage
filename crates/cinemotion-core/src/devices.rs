use std::ops::Deref;

use bevy_ecs::prelude::Component;

use crate::name::*;
use crate::prelude::{Attribute, AttributeMap, AttributeSample, Error, Result};

#[cfg(test)]
#[path = "device_test.rs"]
mod device_test;

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DeviceId(u32);

impl DeviceId {
    pub fn as_u32(&self) -> u32 {
        self.0
    }
}

impl Deref for DeviceId {
    type Target = u32;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<u32> for DeviceId {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

#[derive(Component, Clone)]
pub struct Device {
    pub id: DeviceId,
    pub name: Name,
    pub attributes: AttributeMap,
}

impl Device {
    pub fn new<N: Into<Name>, I: Into<DeviceId>>(id: I, name: N) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            attributes: AttributeMap::new(),
        }
    }
}

pub enum Command {
    Set((DeviceId, Device)),
    Remove(DeviceId),
    Sample((DeviceId, Vec<AttributeSample>)),
    Reset(DeviceId),
}

pub mod commands {
    use super::{system, Device};
    use crate::attributes;
    use crate::error::{Error, Result};
    use crate::{protocol, world::World};

    pub fn process<I: Into<super::DeviceId>>(
        client: I,
        world: &mut World,
        message: protocol::client_message::Body,
    ) -> Result<bool> {
        let client = client.into();
        match message {
            protocol::client_message::Body::DeviceInitAck(model) => {
                let Some(spec) = model.device_spec else {
                    return Err(Error::InvalidValue("device spec is missing".to_string()));
                };

                let mut device: Device = spec.try_into()?;
                device.id = client;
                system::add_device(world, device);
                Ok(true)
            }
            protocol::client_message::Body::DeviceSample(model) => {
                let mut samples = vec![];
                for (name, value) in model.attributes.into_iter() {
                    samples.push(attributes::AttributeSample::new(name, value.try_into()?));
                }
                system::try_apply_samples(world, client, samples)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

pub mod system {

    use bevy_ecs::world::Mut;

    use super::*;
    use crate::{
        globals,
        world::{Entity, World},
    };

    #[derive(Component)]
    struct DeviceEntity;

    pub struct DeviceEntityRef {
        pub entity: Entity,
    }

    impl DeviceEntityRef {
        pub fn name<'w>(&self, world: &'w World) -> &'w Name {
            world
                .get::<Name>(self.entity)
                .expect("expect device entity to have a name")
        }

        pub fn set_name(&mut self, world: &mut World, name: Name) {
            self._set(world, name)
        }

        pub fn apply_sample(&mut self, world: &mut World, sample: AttributeSample) -> Result<()> {
            let mut attribute_map = world
                .get_mut::<AttributeMap>(self.entity)
                .expect("device entity should have attribute map");

            let Some(attr) = attribute_map.get_mut(&sample.name) else {
                return Err(Error::NotFound(format!(
                    "no attribute named {} no device {}",
                    sample.name,
                    self.entity.index()
                )));
            };
            attr.update_value(sample.value.into())?;
            Ok(())
        }

        pub fn attribute<'w>(&self, world: &'w World, name: &Name) -> Option<&'w Attribute> {
            self.attributes(&world).get(name)
        }

        pub fn set_attributes(&mut self, world: &mut World, attrs: AttributeMap) {
            self._set(world, attrs)
        }

        pub fn attributes<'w>(&self, world: &'w World) -> &'w AttributeMap {
            world
                .get::<AttributeMap>(self.entity)
                .expect("device entity should have attribute map")
        }

        pub fn attributes_mut<'w>(&mut self, world: &'w mut World) -> Mut<'w, AttributeMap> {
            world
                .get_mut::<AttributeMap>(self.entity)
                .expect("device entity should have attribute map")
        }

        pub fn insert_attribute(&mut self, world: &mut World, attribute: Attribute) {
            world
                .get_mut::<AttributeMap>(self.entity)
                .expect("device entity should have attribute map")
                .insert(attribute)
        }

        pub fn reset(&mut self, world: &mut World) {
            for (_, attr) in self.attributes_mut(world).iter_mut() {
                attr.reset();
            }
        }

        fn _set<'w, T: Component>(&mut self, world: &'w mut World, value: T) {
            world.get_entity_mut(self.entity).unwrap().insert(value);
        }
    }

    pub(crate) fn get<'a>(world: &'a mut World, id: &DeviceId) -> Option<DeviceEntityRef> {
        let entity = Entity::from_raw(**id);
        let Some(entity_ref) = world.get_entity_mut(entity) else {
            return None;
        };
        if entity_ref.get::<DeviceEntity>().is_none() {
            return None;
        }

        Some(DeviceEntityRef { entity })
    }

    pub(super) fn get_all<'a>(world: &'a mut World) -> Vec<DeviceEntityRef> {
        world
            .query::<(&DeviceEntity, Entity)>()
            .iter(&world)
            .map(|(_, entity)| DeviceEntityRef { entity })
            .collect::<Vec<_>>()
    }

    pub(crate) fn try_add_device(world: &mut World, device: Device) -> Result<DeviceId> {
        if globals::system::is_motion_enabled(world) {
            return Err(Error::InvalidState(
                "cannot add device while in motion".into(),
            ));
        }

        if let Some(_) = get(world, &device.id) {
            return Err(Error::NotFound(
                "no device with matching id found for update".into(),
            ));
        };

        Ok(add_device(world, device))
    }

    pub(crate) fn add_device(world: &mut World, device: Device) -> DeviceId {
        let entity = world.spawn((DeviceEntity, device.id, device.name, device.attributes));
        entity.id().index().into()
    }

    pub(crate) fn try_set_device(world: &mut World, device: Device) -> Result<DeviceId> {
        // We cannot update device instances while in motion
        if globals::system::is_motion_enabled(world) {
            return Err(Error::InvalidState(
                "cannot set device while in motion".into(),
            ));
        }

        let device_id = device.id;
        let Some(mut device_ref) = get(world, &device_id) else {
            return Err(Error::NotFound(
                "no device with matching id found for update".into(),
            ));
        };

        device_ref.set_name(world, device.name);
        device_ref.set_attributes(world, device.attributes);

        Ok(device_id)
    }

    pub(crate) fn remove_device_by_id(world: &mut World, device_id: DeviceId) -> Option<DeviceId> {
        let entity = Entity::from_raw(*device_id);
        match world.despawn(entity) {
            true => Some(device_id),
            false => None,
        }
    }

    pub(crate) fn try_apply_samples(
        world: &mut World,
        device_id: DeviceId,
        samples: Vec<AttributeSample>,
    ) -> Result<()> {
        if !globals::system::is_motion_enabled(world) {
            return Ok(());
        }

        let Some(mut device) = get(world, &device_id) else {
            return Err(Error::NotFound(
                "no device with matching id found for samples".into(),
            ));
        };

        for sample in samples {
            if let Err(err) = device.apply_sample(world, sample) {
                println!(
                    "failed to apply samples to device {}: {}",
                    device_id.as_u32(),
                    err
                );
            }
        }

        return Ok(());
    }
}
