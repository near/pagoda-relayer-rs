use axum::http::StatusCode;

use std::fmt;

// Define a custom error type for process_signed_delegate_action
#[derive(Debug)]
pub struct RelayError {
    pub(crate) status_code: StatusCode,
    pub(crate) message: String,
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.status_code, self.message)
    }
}

impl std::error::Error for RelayError {}
