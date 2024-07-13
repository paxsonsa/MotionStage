use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "kind", content = "data")]
pub enum Message {
    /// Initialize the client connection.
    ///
    /// This is the first message sent from the server to the client.
    Initialize(Initialize),

    /// Acknowledge the initialize the client connection.
    ///
    /// Sent by the client following the `Initialize` message. After this is sent the client
    /// can begin send other commands and messages.
    InitializeAck(InitializeAck),
}

impl Message {
    pub fn serialize(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    pub fn deserialize(data: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(data)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Initialize {
    pub id: u16,
}

impl Into<Message> for Initialize {
    fn into(self) -> Message {
        Message::Initialize(self)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct InitializeAck {}
