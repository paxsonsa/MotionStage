use super::*;
use crate::attributes::AttributeValue;
use crate::globals;
use crate::prelude::name;

#[tokio::test]
async fn test_device_system_command_registration() {
    let mut world = crate::world::new();
    let mut device = Device::new(0, "deviceA");
    device
        .attributes
        .insert(Attribute::new("transform", AttributeValue::matrix44()));

    let reply = system::try_add_device(&mut world, device.clone()).expect("no device id returned");

    assert_eq!(DeviceId::from(0), reply,);
    let devices = system::get_all(&mut world);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name(&world), &name!("deviceA"));
    assert!(devices[0]
        .attributes(&world)
        .get(&name!("transform"))
        .is_some());
}

#[tokio::test]
async fn test_device_system_command_remove() {
    let mut world = crate::world::new();
    let mut device = Device::new(0, "deviceA");
    device
        .attributes
        .insert(Attribute::new_matrix44("transform"));

    let id = system::add_device(&mut world, device.clone());
    let reply = system::remove_device_by_id(&mut world, id.clone()).expect("no device id returned");
    assert_eq!(id, reply);

    let devices = system::get_all(&mut world);
    assert_eq!(devices.len(), 0);
}

#[tokio::test]
async fn test_device_system_motion_samples() {
    let mut world = crate::world::new();
    let mut device = Device::new(0, "deviceA");
    device
        .attributes
        .insert(Attribute::new_matrix44("transform"));
    let id = system::add_device(&mut world, device.clone());

    // Attempt to update the device transform when the motion state is Off
    assert!(!crate::globals::system::is_motion_enabled(&world));
    let mut value = AttributeValue::matrix44();
    value.as_matrix44_mut().unwrap().set(3, 0, 100.0);
    let sample = AttributeSample::new("transform", value.clone());
    let _ = system::try_apply_samples(&mut world, id.clone(), vec![sample.clone()])
        .expect("should not fail");

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

    system::try_apply_samples(&mut world, id.clone(), vec![sample.clone()])
        .expect("should not failed");

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
