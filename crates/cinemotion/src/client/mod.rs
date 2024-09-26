use crate::protocol;

#[cfg(test)]
#[path = "mod_test.rs"]
mod mod_test;

#[allow(async_fn_in_trait)]
pub trait ConnectionHandle {
    fn id(&self) -> u32;
    async fn send(&mut self, message: protocol::ServerMessage);
}
