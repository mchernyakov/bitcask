use crate::KvsError;
use std::fmt::Display;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    KeyNotFound = 1,
    Internal = 2,
    InvalidRequest = 3,
}

impl Display for ErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            ErrorCode::KeyNotFound => "Key not found",
            ErrorCode::Internal => "Internal server error",
            ErrorCode::InvalidRequest => "Invalid request",
        })
    }
}

impl From<&KvsError> for ErrorCode {
    fn from(err: &KvsError) -> Self {
        match err {
            KvsError::KeyNotFound => ErrorCode::KeyNotFound,
            KvsError::InvalidData(_) => ErrorCode::InvalidRequest,
            // io, corruption, locks, ...
            _ => ErrorCode::Internal,
        }
    }
}

impl From<ErrorCode> for KvsError {
    fn from(code: ErrorCode) -> Self {
        match code {
            ErrorCode::KeyNotFound => KvsError::KeyNotFound,
            ErrorCode::InvalidRequest => KvsError::InvalidData("rejected by server".into()),
            ErrorCode::Internal => KvsError::ServerInternal,
        }
    }
}

impl TryFrom<u8> for ErrorCode {
    type Error = KvsError;
    fn try_from(b: u8) -> crate::Result<Self> {
        Ok(match b {
            1 => ErrorCode::KeyNotFound,
            2 => ErrorCode::Internal,
            3 => ErrorCode::InvalidRequest,
            _ => return Err(KvsError::UnexpectedCommandType),
        })
    }
}
