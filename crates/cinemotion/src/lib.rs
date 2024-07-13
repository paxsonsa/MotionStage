pub mod actor;
pub mod client;
pub mod engine;
pub mod error;
pub mod name;
pub(crate) mod protocol;
pub mod websocket;

pub static VERSION: &str = "0.1.0";
pub static DEFAULT_WEB_PORT: u16 = 7272;

pub use error::{Error, Result};
