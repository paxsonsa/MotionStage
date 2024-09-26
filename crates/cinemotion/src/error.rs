use thiserror::Error;

#[derive(Clone, Debug, thiserror::Error)]
pub enum Error {
    #[error("actor error occured: {0}")]
    ActorError(#[from] crate::actor::ActorError),

    #[error("connection error occured: {0}")]
    ConnectionError(String),
}

pub type Result<T> = std::result::Result<T, self::Error>;
