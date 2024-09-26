use crate::protocol;

#[cfg(test)]
#[path = "mod_test.rs"]
mod mod_test;

pub trait ConnectionHandle {
    fn id(&self) -> u32;
    async fn send(&mut self, message: protocol::ServerMessage);
}
