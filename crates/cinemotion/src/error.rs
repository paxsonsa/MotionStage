use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("actor error occured: {0}")]
    ActorError(#[from] crate::actor::ActorError),

    #[error("client error occured: {0}")]
    ClientError(#[from] crate::client::ClientError),

    #[error("connection error occured: {0}")]
    ConnectionError(#[from] tokio_tungstenite::tungstenite::Error),
}

pub type Result<T> = std::result::Result<T, self::Error>;
