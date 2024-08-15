use super::*;
use crate::name;

#[tokio::test]
async fn test_motion_mode() {
    let mut engine = Engine::new();
    let message =
        protocol::client_message::Body::MotionSetMode(protocol::MotionSetMode { enabled: true });
    engine.apply(1, message).await.unwrap();
    assert!(globals::system::is_motion_enabled(&engine.world));
    let message =
        protocol::client_message::Body::MotionSetMode(protocol::MotionSetMode { enabled: false });
    engine.apply(1, message).await.unwrap();
    assert!(!globals::system::is_motion_enabled(&engine.world));
}

#[tokio::test]
async fn test_device_init() {
    let mut engine = Engine::new();
    let id = engine.reserve_client().await;
    let message = protocol::client_message::Body::DeviceInitAck(protocol::DeviceInitAck {
        device_spec: Some(protocol::DeviceSpec {
            name: "test".to_string(),
            attributes: HashMap::new(),
        }),
    });
    engine.apply(id, message).await.unwrap();
    assert!(devices::system::get(&mut engine.world, &devices::DeviceId::new(id)).is_some())
}

#[tokio::test]
async fn test_device_init_with_empty_spec() {
    let mut engine = Engine::new();
    let message = protocol::client_message::Body::DeviceInitAck(protocol::DeviceInitAck {
        device_spec: None,
    });
    assert!(engine.apply(1, message).await.is_err());
}

#[tokio::test]
async fn test_scene_creation_object() {
    let mut engine = Engine::new();
    let message = protocol::client_message::Body::SceneCreateObject(protocol::SceneCreateObject {
        spec: Some(protocol::SceneObjectSpec {
            name: "test".to_string(),
            links: vec![],
            attributes: vec![],
        }),
    });

    globals::system::set_motion_mode(&mut engine.world, true);
    assert!(engine.apply(1, message.clone()).await.is_err());
    globals::system::set_motion_mode(&mut engine.world, false);

    assert!(engine.apply(1, message).await.is_ok());
    let obj =
        scene::system::get_by_name(&mut engine.world, &name!("test")).expect("object not found");

    // Update the scene object
    let message = protocol::client_message::Body::SceneUpdateObject(protocol::SceneUpdateObject {
        id: obj.id().as_u32(),
        spec: Some(protocol::SceneObjectSpec {
            name: "test".to_string(),
            links: vec![],
            attributes: vec![protocol::Attribute {
                name: "position".to_string(),
                default_value: Some(protocol::AttributeValue {
                    value: Some(protocol::attribute_value::Value::Vec3(protocol::Vec3 {
                        x: 0.0,
                        y: 0.0,
                        z: 0.0,
                    })),
                }),
                value: None,
            }],
        }),
    });

    globals::system::set_motion_mode(&mut engine.world, true);
    assert!(engine.apply(1, message.clone()).await.is_err());
    globals::system::set_motion_mode(&mut engine.world, false);

    engine.apply(1, message).await.expect("should not error");
    let obj = scene::system::get_by_name(&mut engine.world, &name!("test")).unwrap();
    assert!(obj.attribute(&engine.world, name!("position")).is_some());

    // Delete the scene object
    let message = protocol::client_message::Body::SceneDeleteObject(protocol::SceneDeleteObject {
        id: obj.id().as_u32(),
    });
    globals::system::set_motion_mode(&mut engine.world, true);
    assert!(engine.apply(1, message.clone()).await.is_err());
    globals::system::set_motion_mode(&mut engine.world, false);

    engine.apply(1, message).await.expect("should not error");
    assert!(scene::system::get_by_id(&mut engine.world, &obj.id()).is_none());
}

#[tokio::test]
async fn test_standard_integration() {
    let mut engine = Engine::new();
    let id = engine.reserve_client().await;
    let message = protocol::client_message::Body::DeviceInitAck(protocol::DeviceInitAck {
        device_spec: Some(protocol::DeviceSpec {
            name: "DeviceA".to_string(),
            attributes: [(
                "transform".to_string(),
                protocol::AttributeValue {
                    value: Some(protocol::attribute_value::Value::Vec4(protocol::Vec4 {
                        x: 0.0,
                        y: 0.0,
                        z: 0.0,
                        w: 0.0,
                    })),
                },
            )]
            .into(),
        }),
    });
    engine
        .apply(id, message)
        .await
        .expect("device init should not fail");

    // Create a scene objects
    let message = protocol::client_message::Body::SceneCreateObject(protocol::SceneCreateObject {
        spec: Some(protocol::SceneObjectSpec {
            name: "test".to_string(),
            links: vec![protocol::AttributeLink {
                device_id: id,
                device_attr: "transform".to_string(),
                attribute: "position".to_string(),
            }],
            attributes: vec![protocol::Attribute {
                name: "position".to_string(),
                default_value: Some(protocol::AttributeValue {
                    value: Some(protocol::attribute_value::Value::Vec4(protocol::Vec4 {
                        x: 0.0,
                        y: 0.0,
                        z: 0.0,
                        w: 0.0,
                    })),
                }),
                value: None,
            }],
        }),
    });
    engine.apply(id, message).await.expect("should not error");

    // Send a device sample
    let sample = protocol::client_message::Body::DeviceSample(protocol::DeviceSample {
        attributes: [(
            "transform".to_string(),
            protocol::AttributeValue {
                value: Some(protocol::attribute_value::Value::Vec4(protocol::Vec4 {
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                    w: 4.0,
                })),
            },
        )]
        .into(),
    });
    engine
        .apply(id, sample.clone())
        .await
        .expect("should not error when updating sample despite motion disabled.");

    // Enable motion and send sample.
    let message =
        protocol::client_message::Body::MotionSetMode(protocol::MotionSetMode { enabled: true });
    engine
        .apply(id, message)
        .await
        .expect("setting motion mode should not fail.");
    engine
        .apply(id, sample)
        .await
        .expect("should not error when updating samples.");

    // Check that the scene object has updated
    let state = engine.update().await.expect("should not error");

    // Retrieve the device with ID
    let device = state.devices.get(&id).expect("device not found");

    // Retrieve the 'transform' attribute from the device
    let transform_attribute = device
        .attributes
        .get(&name!("transform"))
        .expect("attribute not found")
        .clone();

    // Extract the value of the 'transform' attribute as a vec4
    let transform_value = transform_attribute
        .value()
        .as_vec4()
        .expect("value not a vec4");

    // Assert that the transform value is as expected
    assert_eq!(transform_value, (1.0f64, 2.0f64, 3.0f64, 4.0f64));

    // Retrieve the first object in the scene
    let first_object = &state.scene.objects[0];

    // Retrieve the 'position' attribute from the first object
    let position_attribute = first_object
        .attributes
        .get(&name!("position"))
        .expect("object attribute not found");

    // Extract the value of the 'position' attribute as a vec4
    let position_value = position_attribute
        .value()
        .as_vec4()
        .expect("value not a vec4");

    // Assert that the position value is the same as the projected attribute
    assert_eq!(position_value, (1.0f64, 2.0f64, 3.0f64, 4.0f64));
    // TODO: Test Linking Errors
}
