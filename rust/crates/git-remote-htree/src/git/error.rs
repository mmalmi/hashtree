//! Error types for git module

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Object not found: {0}")]
    ObjectNotFound(String),

    #[error("Invalid object type: {0}")]
    InvalidObjectType(String),

    #[error("Invalid object format: {0}")]
    InvalidObjectFormat(String),

    #[error("Invalid ref name: {0}")]
    InvalidRefName(String),

    #[error("Storage error: {0}")]
    StorageError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
