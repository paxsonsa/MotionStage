use crate::prelude::*;
use async_trait::async_trait;
use bevy_ecs::prelude::*;

#[async_trait]
pub trait EngineSystem {
    fn name(&self) -> String;
    async fn update(&mut self, world: &mut World) -> Result<()>;
}
