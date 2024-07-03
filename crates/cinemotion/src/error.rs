use thiserror::Error;

#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    #[error("actor error occured: {0}")]
    ActorError(#[from] crate::actor::Error),
}

pub type Result<T> = std::result::Result<T, self::Error>;
