use crate::name;

use super::*;
//
// #[tokio::test]
// async fn test_device_registration() {
//     let mut engine = Engine::new();
//
//     let mut command = device::Device::new("deviceA");
//     command.insert_attribute(Attribute::new(
//         name!("transform"),
//         AttributeValue::matrix44(),
//     ));
//     let command = device::Command::Register(command);
//     let (command, mut receiver) = CommandInfo::with(command.into());
//     engine
//         .process(command)
//         .await
//         .expect("device registration to complete");
//
//     let Ok(Some(reply)) = receiver.try_recv().unwrap() else {
//         panic!("should receive reply with device id.")
//     };
//
//     assert!(matches!(
//         CommandReply::EntityId(0),
//         CommandReply::EntityId(_reply)
//     ));
//
//     engine.tick().await.expect("tick should not failed");
//
//     let world = engine.get_world_mut();
//     for item in world.query::<&Device>().iter(&world) {
//         assert_eq!(item.name(), name!("deviceA"));
//         assert_eq!(item.attributes().len(), 1);
//     }
// }
//
// #[tokio::test]
// async fn test_device_update() {
//     let mut engine = Engine::new();
//     let device_id = device::system::create(engine.get_world_mut(), name!("deviceA"));
//     device::system::add_attribute(
//         engine.get_world_mut(),
//         device_id,
//         Attribute::new(name!("transform"), AttributeValue::matrix44()),
//     )
//     .unwrap();
//
//     let mut command = device::commands::DeviceUpdate::new()
//
// }
