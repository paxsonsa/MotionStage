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

    let message = protocol::client_message::Body::DeviceInitAck(protocol::DeviceInitAck {
        device_spec: Some(protocol::DeviceSpec {
            name: "test".to_string(),
            attributes: HashMap::new(),
        }),
    });
    engine.apply(1, message).await.unwrap();
    assert!(devices::system::get(&mut engine.world, &devices::DeviceId::new(1)).is_some())
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
async fn test_device_sampling() {}
