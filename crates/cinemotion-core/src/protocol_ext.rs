use std::collections::HashMap;

use crate::*;
use cinemotion_proto as protocol;
use name::Name;

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
