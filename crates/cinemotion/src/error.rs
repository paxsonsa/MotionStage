#[derive(Clone, Debug, thiserror::Error)]
pub enum Error {
    #[error("connection error occured: {0}")]
    ConnectionError(String),
}

pub type Result<T> = std::result::Result<T, self::Error>;
