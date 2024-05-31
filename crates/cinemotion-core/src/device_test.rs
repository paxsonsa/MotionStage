use super::*;
use crate::attributes::AttributeValue;
use crate::commands::CommandReply;
use crate::globals;
use crate::prelude::name;

#[tokio::test]
async fn test_device_system_command_registration() {
    let mut world = crate::world::new();
    let mut device = Device::new("deviceA");
    device
        .attributes
        .insert(Attribute::new("transform", AttributeValue::matrix44()));
    let command = Command::Register(device);

    let _reply = commands::process(&mut world, command)
        .unwrap()
        .expect("no device id returned");

    assert!(matches!(
        CommandReply::EntityId(0),
        CommandReply::EntityId(_reply),
    ));
    let devices = system::get_all(&mut world);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name(&world), &name!("deviceA"));
    assert!(devices[0]
        .attributes(&world)
        .get(&name!("transform"))
        .is_some());
}

#[tokio::test]
async fn test_device_system_command_update() {
    let mut world = crate::world::new();

    let mut device = Device::new("deviceA");
    device
        .attributes
        .insert(Attribute::new_matrix44("transform"));

    let id = system::add_device(&mut world, device.clone());

    // Add a new attribute
    device.attributes.insert(Attribute::new_vec3("vel"));

    let command = Command::Update((id, device));
    let _reply = commands::process(&mut world, command)
        .unwrap()
        .expect("no device id returned");

    assert!(matches!(
        CommandReply::EntityId(0),
        CommandReply::EntityId(_reply),
    ));
    let devices = system::get_all(&mut world);

    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name(&world), &name!("deviceA"));
    assert!(devices[0]
        .attributes(&world)
        .get(&name!("transform"))
        .is_some());
    assert!(devices[0].attributes(&world).get(&name!("vel")).is_some());
}

#[tokio::test]
async fn test_device_system_command_remove() {
    let mut world = crate::world::new();
    let mut device = Device::new("deviceA");
    device
        .attributes
        .insert(Attribute::new_matrix44("transform"));

    let id = system::add_device(&mut world, device.clone());
    let command = Command::Remove(id);
    let _reply = commands::process(&mut world, command)
        .unwrap()
        .expect("no device id returned");

    assert!(matches!(
        CommandReply::EntityId(0),
        CommandReply::EntityId(_reply),
    ));
    let devices = system::get_all(&mut world);
    assert_eq!(devices.len(), 0);
}

#[tokio::test]
async fn test_device_system_motion_samples() {
    let mut world = crate::world::new();
    let mut device = Device::new("deviceA");
    device
        .attributes
        .insert(Attribute::new_matrix44("transform"));
    let id = system::add_device(&mut world, device.clone());

    // Attempt to update the device transform when the motion state is Off
    assert!(!crate::globals::system::is_motion_enabled(&world));
    let mut value = AttributeValue::matrix44();
    value.as_matrix44_mut().unwrap().set(3, 0, 100.0);
    let sample = AttributeSample::new("transform", value.clone());
    let command = Command::Sample((id.clone(), vec![sample.clone()]));
    let _ = commands::process(&mut world, command).expect("should not fail");

    let devices = system::get_all(&mut world);

    // Device should not update its attributes when motion state is Off.
    assert_eq!(
        devices[0]
            .attributes(&world)
            .get(&name!("transform"))
            .unwrap()
            .value()
            .as_matrix44()
            .unwrap()
            .get(3, 0)
            .unwrap(),
        0.0
    );

    // Enable Motion and Try again
    globals::system::enable_motion(&mut world);

    let command = Command::Sample((id.clone(), vec![sample.clone()]));
    let _ = commands::process(&mut world, command).expect("should not failed");

    let devices = system::get_all(&mut world);

    // Device should not update its attributes when motion state is Off.
    assert_eq!(
        devices[0]
            .attributes(&world)
            .get(&name!("transform"))
            .unwrap()
            .value(),
        value.into()
    );
}
