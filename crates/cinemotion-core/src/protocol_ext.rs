use std::collections::HashMap;

use crate::*;
use cinemotion_proto as protocol;
use name::Name;

/// Implements the `From` trait for converting a `state::StateTree`
/// into a `protocol::ServerMessage`.
impl From<state::StateTree> for protocol::ServerMessage {
    fn from(value: state::StateTree) -> Self {
        protocol::ServerMessage {
            body: Some(protocol::server_message::Body::State(value.into())),
        }
    }
}

/// Implements the `From` trait for converting a `state::StateTree`
/// into a `protocol::State`.
impl From<state::StateTree> for protocol::State {
    fn from(value: state::StateTree) -> Self {
        protocol::State {
            utime: value.utime as u64,
            session: Some(value.session.into()),
            devices: value
                .devices
                .into_iter()
                .map(|(id, device)| (id, device.into()))
                .collect(),
            scene: Some(value.scene.into()),
        }
    }
}

/// Implements the `Into` trait for converting a `state::SceneState`
/// into a `protocol::SceneState`.
impl Into<protocol::SceneState> for state::SceneState {
    fn into(self) -> protocol::SceneState {
        protocol::SceneState {
            objects: self.objects.into_iter().map(|obj| obj.into()).collect(),
        }
    }
}

/// Implements the `Into` trait for converting a `scene::SceneObject`
/// into a `protocol::SceneObjectState`.
impl Into<protocol::SceneObjectState> for scene::SceneObject {
    fn into(self) -> protocol::SceneObjectState {
        protocol::SceneObjectState {
            name: self.name.to_string(),
            attributes: self
                .attributes
                .into_values()
                .map(|attr| attr.into())
                .collect(),
            links: self.links.into_values().map(|link| link.into()).collect(),
        }
    }
}

/// Implements the `Into` trait for converting an `attributes::AttributeLink`
/// into a `protocol::AttributeLink`.
impl Into<protocol::AttributeLink> for attributes::AttributeLink {
    fn into(self) -> protocol::AttributeLink {
        protocol::AttributeLink {
            attribute: self.attribute().to_string(),
            device_id: self.device().as_u32(),
            device_attr: self.device_attr().to_string(),
        }
    }
}
/// Implements the conversion from `protocol::AttributeValue` to `attributes::AttributeValue`.
/// This conversion may fail, in which case an `error::Error` is returned.
impl TryFrom<protocol::AttributeValue> for attributes::AttributeValue {
    type Error = error::Error;

    /// Attempts to convert a `protocol::AttributeValue` into an `attributes::AttributeValue`.
    ///
    /// # Arguments
    ///
    /// * `value` - A `protocol::AttributeValue` instance containing the attribute value.
    ///
    /// # Returns
    ///
    /// * `Ok(attributes::AttributeValue)` - If the conversion is successful.
    /// * `Err(error::Error)` - If the conversion fails.
    fn try_from(value: protocol::AttributeValue) -> Result<Self, Self::Error> {
        match value.value {
            Some(protocol::attribute_value::Value::Float(v)) => {
                Ok(attributes::AttributeValue::Float(v))
            }
            Some(protocol::attribute_value::Value::Vec3(v)) => {
                Ok(attributes::AttributeValue::Vec3((v.x, v.y, v.z).into()))
            }
            Some(protocol::attribute_value::Value::Vec4(v)) => Ok(
                attributes::AttributeValue::Vec4((v.x, v.y, v.z, v.w).into()),
            ),
            Some(protocol::attribute_value::Value::Matrix44(v)) => {
                if v.values.len() != 16 {
                    Err(error::Error::InvalidValue(
                        "device spec attribute matrix44 value has invalid length".to_string(),
                    ))
                } else {
                    Ok(attributes::AttributeValue::Matrix44(v.values.into()))
                }
            }
            None => Err(error::Error::InvalidValue(
                "attribute value is missing a value".to_string(),
            )),
        }
    }
}

/// Implements the conversion from the given code into an `AttributeLink`.
/// This conversion may fail, in which case an `error::Error` is returned.
impl TryFrom<protocol::AttributeLink> for attributes::AttributeLink {
    type Error = error::Error;

    /// Attempts to convert a `super::Link` into an `AttributeLink`.
    ///
    /// # Arguments
    ///
    /// * `link` - A `super::Link` instance containing the link information.
    ///
    /// # Returns
    ///
    /// * `Ok(AttributeLink)` - If the conversion is successful.
    /// * `Err(error::Error)` - If the conversion fails.
    fn try_from(link: protocol::AttributeLink) -> Result<Self, Self::Error> {
        let attribute = link.attribute;
        let device = link.device_id;
        let device_attr = link.device_attr;
        Ok(attributes::AttributeLink::new(
            device.into(),
            device_attr,
            attribute,
        ))
    }
}

/// Implements the conversion from `SceneObjectSpec` to `SceneObject`.
/// This conversion may fail, in which case an `error::Error` is returned.
impl TryFrom<protocol::SceneObjectSpec> for scene::SceneObject {
    type Error = error::Error;

    /// Attempts to convert a `SceneObjectSpec` into a `SceneObject`.
    ///
    /// # Arguments
    ///
    /// * `spec` - A `SceneObjectSpec` instance containing the scene object specification.
    ///
    /// # Returns
    ///
    /// * `Ok(SceneObject)` - If the conversion is successful.
    /// * `Err(error::Error)` - If the conversion fails.
    fn try_from(spec: protocol::SceneObjectSpec) -> Result<Self, Self::Error> {
        let mut object = scene::SceneObject::new(spec.name);

        for attr in spec.attributes {
            object.insert_attribute(attr.try_into()?);
        }

        for link in spec.links {
            let attribute = link.attribute;
            let device = link.device_id;
            let device_attr = link.device_attr;
            let link = attributes::AttributeLink::new(device.into(), device_attr, attribute);
            if let Err(err) = object.insert_link(link) {
                tracing::warn!("failed to add link: {}", err);
                continue;
            }
        }

        Ok(object)
    }
}
/// Implements the conversion from `Option<protocol::AttributeValue>` to `attributes::AttributeValue`.
/// This conversion may fail, in which case an `error::Error` is returned.
impl TryFrom<Option<protocol::AttributeValue>> for attributes::AttributeValue {
    type Error = error::Error;

    /// Attempts to convert an `Option<protocol::AttributeValue>` into an `attributes::AttributeValue`.
    ///
    /// # Arguments
    ///
    /// * `value` - An `Option<protocol::AttributeValue>` instance containing the attribute value.
    ///
    /// # Returns
    ///
    /// * `Ok(attributes::AttributeValue)` - If the conversion is successful.
    /// * `Err(error::Error)` - If the conversion fails.
    fn try_from(value: Option<protocol::AttributeValue>) -> Result<Self, Self::Error> {
        match value {
            Some(attr_value) => attr_value.try_into(),
            None => Err(error::Error::InvalidValue(
                "attribute value is missing".to_string(),
            )),
        }
    }
}

impl Into<protocol::AttributeValue> for attributes::AttributeValue {
    fn into(self) -> protocol::AttributeValue {
        todo!()
    }
}

/// Implements the conversion from `protocol::Attribute` to `attributes::Attribute`.
/// This conversion may fail, in which case an `error::Error` is returned.
impl TryFrom<protocol::Attribute> for attributes::Attribute {
    type Error = error::Error;

    /// Attempts to convert a `protocol::Attribute` into an `attributes::Attribute`.
    ///
    /// # Arguments
    ///
    /// * `attr` - A `protocol::Attribute` instance containing the attribute.
    ///
    /// # Returns
    ///
    /// * `Ok(attributes::Attribute)` - If the conversion is successful.
    /// * `Err(error::Error)` - If the conversion fails.
    fn try_from(attr: protocol::Attribute) -> Result<Self, Self::Error> {
        // Convert the attribute name.
        let name: Name = attr.name.into();
        // Try to convert the default attribute value.
        let value = attr.default_value.try_into()?;
        // Return the constructed `attributes::Attribute` instance.
        Ok(attributes::Attribute::new(name, value))
    }
}

impl Into<protocol::Attribute> for attributes::Attribute {
    fn into(self) -> protocol::Attribute {
        protocol::Attribute {
            name: self.name().to_string(),
            value: Some((*(self.value())).clone().into()),
            default_value: Some((*(self.default_value())).clone().into()),
        }
    }
}

/// Implements the conversion from `protocol::DeviceSpec` to `devices::Device`.
/// This conversion may fail, in which case an `error::Error` is returned.
impl TryFrom<protocol::DeviceSpec> for devices::Device {
    type Error = error::Error;

    /// Attempts to convert a `protocol::DeviceSpec` into a `devices::Device`.
    ///
    /// # Arguments
    ///
    /// * `spec` - A `protocol::DeviceSpec` instance containing the device specification.
    ///
    /// # Returns
    ///
    /// * `Ok(devices::Device)` - If the conversion is successful.
    /// * `Err(error::Error)` - If the conversion fails.
    fn try_from(spec: protocol::DeviceSpec) -> Result<Self, Self::Error> {
        let mut attributes = HashMap::<Name, attributes::Attribute>::new();

        // Iterate over each attribute in the device specification.
        for (name, value) in spec.attributes {
            // Try to convert the attribute value.
            let value = value.try_into()?;
            // Convert the attribute name.
            let name: Name = name.into();
            // Insert the attribute into the attributes map.
            attributes.insert(name.clone(), attributes::Attribute::new(name, value));
        }

        // Return the constructed `devices::Device` instance.
        Ok(devices::Device {
            id: 0.into(),
            name: spec.name.into(),
            attributes: attributes.into(),
        })
    }
}

/// Implements the `Into` trait for converting a `devices::Device`
/// into a `protocol::DeviceState`.
impl Into<protocol::DeviceState> for devices::Device {
    fn into(self) -> protocol::DeviceState {
        let mut attributes: Vec<protocol::Attribute> = vec![];
        let attribute_map: HashMap<_, _> = self.attributes.into();
        for attr in attribute_map.into_values().into_iter() {
            attributes.push(attr.into());
        }
        protocol::DeviceState {
            name: self.name.to_string(),
            attributes,
        }
    }
}

impl From<protocol::State> for state::StateTree {
    fn from(proto_state: protocol::State) -> Self {
        state::StateTree {
            utime: proto_state.utime as u128,
            session: proto_state
                .session
                .map(|s| s.into())
                .expect("session state is missing."),
            devices: proto_state
                .devices
                .into_iter()
                .map(|(id, device)| {
                    (
                        id,
                        devices::Device {
                            id: id.clone().into(),
                            name: device.name.into(),
                            attributes: device
                                .attributes
                                .into_iter()
                                .map(|attr| -> attributes::Attribute {
                                    attr.try_into().expect("attribute conversion failed")
                                })
                                .collect(),
                        },
                    )
                })
                .collect(),
            scene: proto_state
                .scene
                .map(|s| s.into())
                .expect("scene state is missing."),
        }
    }
}

impl Into<protocol::SessionState> for state::SessionState {
    fn into(self) -> protocol::SessionState {
        protocol::SessionState {
            motion_enabled: self.motion_enabled,
        }
    }
}

impl From<protocol::SessionState> for state::SessionState {
    fn from(proto_session: protocol::SessionState) -> Self {
        state::SessionState {
            motion_enabled: proto_session.motion_enabled,
        }
    }
}

impl From<protocol::SceneState> for state::SceneState {
    fn from(proto_scene: protocol::SceneState) -> Self {
        state::SceneState {
            // TODO: Implement SceneInfo conversion
            info: scene::SceneInfo {
                name: "Default".into(),
            },
            objects: proto_scene
                .objects
                .into_iter()
                .map(|obj| obj.into())
                .collect(),
        }
    }
}

impl From<protocol::SceneObjectState> for scene::SceneObject {
    fn from(proto_obj: protocol::SceneObjectState) -> Self {
        scene::SceneObject {
            name: proto_obj.name.into(),
            attributes: proto_obj
                .attributes
                .into_iter()
                .map(|attr| -> Result<attributes::Attribute, error::Error> { attr.try_into() })
                .take_while(Result::is_ok)
                .map(Result::unwrap)
                .map(|attr| (attr.name().clone(), attr))
                .collect(),
            links: proto_obj
                .links
                .into_iter()
                .map(|link| -> Result<attributes::AttributeLink, error::Error> { link.try_into() })
                .take_while(Result::is_ok)
                .map(Result::unwrap)
                .map(|link| (link.attribute().clone().into(), link.into()))
                .collect::<HashMap<name::Name, attributes::AttributeLink>>(),
        }
    }
}

impl From<protocol::ServerMessage> for state::StateTree {
    fn from(proto_msg: protocol::ServerMessage) -> Self {
        match proto_msg.body {
            Some(protocol::server_message::Body::State(state)) => state.into(),
            _ => panic!("Invalid ServerMessage body"),
        }
    }
}
