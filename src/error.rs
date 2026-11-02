use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{code} at {path}: {message}")]
pub struct Error {
    pub code: String,
    pub path: String,
    pub message: String,
    pub retryable: bool,
}
pub type Result<T> = std::result::Result<T, Error>;
impl Error {
    pub fn new(code: &str, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            path: path.into(),
            message: message.into(),
            retryable: false,
        }
    }
    pub(crate) fn io(e: impl std::fmt::Display) -> Self {
        Self {
            code: "storage_error".into(),
            path: String::new(),
            message: e.to_string(),
            retryable: true,
        }
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::io(e)
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::new(
            "input_invalid",
            format!("line {} column {}", e.line(), e.column()),
            e.to_string(),
        )
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::io(e)
    }
}
