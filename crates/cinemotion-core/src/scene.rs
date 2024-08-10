use crate::prelude::*;
use std::{collections::HashMap, ops::Deref};

#[cfg(test)]
#[path = "scene_test.rs"]
mod scene_test;

#[derive(Clone)]
pub struct SceneInfo {
    pub name: Name,
}

#[derive(Debug, Clone)]
pub struct ObjectId(u32);

impl ObjectId {
    pub fn as_u32(&self) -> u32 {
        self.0
    }
}

impl Deref for ObjectId {
    type Target = u32;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<u32> for ObjectId {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

#[derive(Clone)]
pub struct SceneObject {
    pub name: Name,
    pub attributes: HashMap<Name, Attribute>,
    pub links: HashMap<Name, AttributeLink>,
}

impl SceneObject {
    pub fn new<N: Into<Name>>(name: N) -> Self {
        Self {
            name: name.into(),
            attributes: HashMap::new(),
            links: HashMap::new(),
        }
    }

    pub fn insert_link(&mut self, link: AttributeLink) -> Result<()> {
        let Some(_) = self.attributes.get(&link.attribute()) else {
            return Err(Error::NotFound(format!(
                "no attribute {} found on object.",
                link.attribute()
            )));
        };

        self.links.insert(link.attribute().clone(), link);
        Ok(())
    }

    pub fn insert_attribute(&mut self, attribute: Attribute) {
        self.attributes.insert(attribute.name().clone(), attribute);
    }
}

pub mod system {
    use std::sync::Arc;

    use crate::prelude::{Error, Result};
    use crate::world::World;
    use crate::{globals, name::*};

    use bevy_ecs::prelude::{Component, Entity};

    use super::{
        Attribute, AttributeLink, AttributeLinkMap, AttributeMap, AttributeValue, ObjectId,
        SceneObject,
    };

    #[derive(Component, Debug)]
    pub struct SceneObjectEntity;

    pub struct SceneObjectEntityRef {
        entity: Entity,
    }

    impl SceneObjectEntityRef {
        pub fn id(&self) -> ObjectId {
            self.entity.index().into()
        }

        pub fn as_scene_object(self, world: &World) -> SceneObject {
            let name = self.name(world);
            let attributes = self.attributes(world).clone();
            let links = self.links(world).clone();
            SceneObject {
                name,
                attributes: attributes.into(),
                links: links.into(),
            }
        }

        pub fn name<'w>(&self, world: &'w World) -> Name {
            world.get::<Name>(self.entity).unwrap().clone()
        }

        pub fn set_name<'w>(&mut self, world: &'w mut World, name: Name) {
            self._set(world, name)
        }

        pub fn attribute<'w, N: Into<Name>>(
            &self,
            world: &'w World,
            name: N,
        ) -> Option<&'w Attribute> {
            let name = name.into();
            self.attributes(world).get(&name)
        }

        pub fn update_attribute(
            &self,
            world: &mut World,
            name: &Name,
            value: Arc<AttributeValue>,
        ) -> Result<()> {
            let mut attr_map = world
                .get_mut::<AttributeMap>(self.entity)
                .expect("attribute map should exist for SceneObjectRef");

            let attr = attr_map
                .get_mut(&name)
                .expect("attribute with link should exist");
            attr.update_value(value)
        }

        pub fn attributes<'w>(&self, world: &'w World) -> &'w AttributeMap {
            world.get::<AttributeMap>(self.entity).unwrap()
        }

        pub fn set_attributes<'w>(&mut self, world: &'w mut World, attributes: AttributeMap) {
            self._set(world, attributes)
        }

        pub fn links<'w>(&self, world: &'w World) -> &'w AttributeLinkMap {
            world.get::<AttributeLinkMap>(self.entity).unwrap()
        }

        pub fn set_links(&mut self, world: &mut World, links: AttributeLinkMap) {
            self._set(world, links)
        }

        fn _set<'w, T: Component>(&mut self, world: &'w mut World, value: T) {
            world.get_entity_mut(self.entity).unwrap().insert(value);
        }
    }

    pub fn init(world: &mut World) {
        let mut object = SceneObject::new("default");
        object.insert_attribute(Attribute::new_matrix44("transform"));
        add_scene_object(world, object);
    }

    pub fn update(world: &mut World) -> Result<()> {
        for object in get_all(world) {
            let links = object.links(&world).clone();
            for (name, link) in links.iter() {
                let value = read_link_value(world, &link)?;
                println!("{:?} {:?} {:?}", name, link, value);
                object.update_attribute(world, &name, value)?;
            }
        }
        Ok(())
    }

    pub(crate) fn get_by_id<'a>(
        world: &'a mut World,
        id: &ObjectId,
    ) -> Option<SceneObjectEntityRef> {
        let entity = Entity::from_raw(**id);
        let Some(entity_ref) = world.get_entity(entity) else {
            return None;
        };

        if entity_ref.get::<SceneObjectEntity>().is_none() {
            return None;
        }

        Some(SceneObjectEntityRef { entity })
    }

    pub(crate) fn get_by_name<'a>(
        world: &'a mut World,
        name: &Name,
    ) -> Option<SceneObjectEntityRef> {
        let mut query = world.query::<(&SceneObjectEntity, &Name, Entity)>();
        for (_, other_name, entity) in query.iter(&world).collect::<Vec<_>>() {
            if name == other_name {
                return Some(SceneObjectEntityRef { entity });
            }
        }
        return None;
    }

    pub(crate) fn get_all<'a>(world: &'a mut World) -> Vec<SceneObjectEntityRef> {
        world
            .query::<(&SceneObjectEntity, Entity)>()
            .iter(&world)
            .map(|(_, entity)| SceneObjectEntityRef { entity })
            .collect::<Vec<_>>()
    }

    pub(super) fn try_add_scene_object(world: &mut World, object: SceneObject) -> Result<ObjectId> {
        if globals::system::is_motion_enabled(world) {
            return Err(Error::InvalidState(
                "cannot update the scene state while in motion".to_string(),
            ));
        }
        if let Some(_) = get_by_name(world, &object.name) {
            let reason = format!("object with name '{}' already exists.", object.name);
            return Err(Error::CommandFailed { reason });
        }
        Ok(add_scene_object(world, object).into())
    }

    pub(super) fn add_scene_object(world: &mut World, object: SceneObject) -> ObjectId {
        let attributes = AttributeMap::from(object.attributes);
        let links = AttributeLinkMap::from(object.links);
        let entity = world.spawn((SceneObjectEntity, object.name, attributes, links));
        entity.id().index().into()
    }

    pub(super) fn try_set_scene_object(
        world: &mut World,
        id: ObjectId,
        object: SceneObject,
    ) -> Result<ObjectId> {
        if let Some(id) = set_scene_object(world, id.clone(), object) {
            Ok(id)
        } else {
            let reason = format!("object with id '{:?}' does not exist.", id);
            Err(Error::NotFound(reason))
        }
    }

    pub(super) fn set_scene_object(
        world: &mut World,
        id: ObjectId,
        object: SceneObject,
    ) -> Option<ObjectId> {
        let Some(mut object_ref) = get_by_id(world, &id) else {
            return None;
        };

        object_ref.set_name(world, object.name);
        object_ref.set_attributes(world, object.attributes.into());
        object_ref.set_links(world, object.links.into());

        Some(object_ref.id())
    }
    pub(super) fn try_remove_scene_object_by_id(
        world: &mut World,
        id: ObjectId,
    ) -> Result<ObjectId> {
        if let Some(id) = remove_scene_object_by_id(world, id.clone()) {
            Ok(id)
        } else {
            let reason = format!("object with id '{:?}' does not exist.", id);
            Err(Error::CommandFailed { reason })
        }
    }

    pub(super) fn remove_scene_object_by_id(
        world: &mut World,
        device_id: ObjectId,
    ) -> Option<ObjectId> {
        let entity = Entity::from_raw(*device_id);

        let Some(_) = world.get_mut::<SceneObjectEntity>(entity) else {
            return None;
        };

        match world.despawn(entity) {
            true => Some(device_id),
            false => None,
        }
    }

    fn read_link_value<'a>(
        world: &mut World,
        link: &'a AttributeLink,
    ) -> Result<Arc<AttributeValue>> {
        let Some(device) = crate::devices::system::get(world, link.device()) else {
            return Err(Error::NotFound(format!(
                "no device by id '{:?}'",
                link.device()
            )));
        };
        let Some(attr) = device.attribute(world, link.device_attr()) else {
            return Err(Error::NotFound(format!(
                "no device attribute '{}.{}'",
                device.name(&world),
                link.device_attr()
            )));
        };

        Ok(attr.value())
    }
}

pub mod commands {
    use super::system;
    use crate::error::{Error, Result};
    use crate::protocol;
    use crate::world::World;

    pub fn procces(world: &mut World, message: protocol::client_message::Body) -> Result<bool> {
        // No matter what, we cannot update the scene while we are in motion or recording.
        if crate::globals::system::is_motion_enabled(world) {
            return Err(Error::InvalidState(
                "cannot update the scene state while in motion".to_string(),
            ));
        }

        match message {
            protocol::client_message::Body::SceneCreateObject(model) => {
                let Some(spec) = model.spec else {
                    return Err(Error::InvalidValue("device spec is missing".to_string()));
                };

                let object = spec.try_into()?;
                system::try_add_scene_object(world, object)?;
                Ok(true)
            }
            // Update an existing object in the world
            protocol::client_message::Body::SceneUpdateObject(model) => {
                let id = model.id;
                let Some(spec) = model.spec else {
                    return Err(Error::InvalidValue("device spec is missing".to_string()));
                };

                let object = spec.try_into()?;
                system::try_set_scene_object(world, id.into(), object)?;
                Ok(true)
            }
            protocol::client_message::Body::SceneDeleteObject(model) => {
                let id = model.id;
                system::try_remove_scene_object_by_id(world, id.into())?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}
