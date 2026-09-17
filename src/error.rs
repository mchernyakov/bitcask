use std::io;
use thiserror::Error;

/// Error type for kvs.
#[derive(Error, Debug)]
pub enum KvsError {
    /// IO error.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Serialization or deserialization error.
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    /// Removing non-existent key error.
    #[error("Key not found")]
    KeyNotFound,
    /// Unexpected command type error.
    /// It indicates a corrupted log or a program bug.
    #[error("Unexpected command type")]
    UnexpectedCommandType,
    /// Checksum mismatch — the record bytes don't match their stored CRC.
    #[error("Corrupted record")]
    Corruption,
    /// System clock error (current time is before the Unix epoch).
    #[error(transparent)]
    Clock(#[from] std::time::SystemTimeError),
    /// Stored value is not valid UTF-8.
    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),
    /// A data file referenced by the index is missing.
    #[error("Data file not found, id {0}")]
    MissingDataFile(u64),
}

/// Result type for kvs.
pub type Result<T> = std::result::Result<T, KvsError>;
