#[cfg(test)]
#[path = "./lib_test.rs"]
mod lib_test;
use prost::Message;

// Include the `items` module, which is generated from items.proto.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/cinemotion.rs"));
}
pub use proto::*;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("decoding error occurred: {0}")]
    DecodingError(#[from] prost::DecodeError),

    #[error("encoding error occurred: {0}")]
    EncodingError(#[from] prost::EncodeError),
}

impl TryInto<bytes::Bytes> for ServerMessage {
    type Error = self::ProtocolError;
    fn try_into(self) -> Result<bytes::Bytes, Self::Error> {
        let mut buf = bytes::BytesMut::new();
        self.encode(&mut buf)?;
        Ok(buf.freeze())
    }
}

impl TryFrom<bytes::Bytes> for ClientMessage {
    type Error = self::ProtocolError;

    fn try_from(value: bytes::Bytes) -> Result<Self, Self::Error> {
        match prost::Message::decode(value) {
            Ok(msg) => Ok(msg),
            Err(err) => Err(err.into()),
        }
    }
}

// This is a convenience macro to implement the From trait for our
// generated protobuf types.
macro_rules! impl_from_client_body {
    ($type:ident) => {
        impl From<$type> for client_message::Body {
            fn from(message: $type) -> Self {
                client_message::Body::$type(message)
            }
        }
    };
}

macro_rules! impl_into_server_message {
    ($type:ident) => {
        impl From<$type> for ServerMessage {
            fn from(body: $type) -> Self {
                ServerMessage {
                    body: Some(server_message::Body::$type(body)),
                }
            }
        }
    };
}
impl_into_server_message!(Error);
impl_into_server_message!(Ping);
impl_into_server_message!(Pong);
impl_into_server_message!(Initialize);

impl_from_client_body!(InitializeAck);
