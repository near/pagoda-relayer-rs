use axum::http::StatusCode;
use utoipa::ToSchema;

use std::fmt;

// Define a custom error type for process_signed_delegate_action
#[derive(Debug, ToSchema)]
pub struct RelayError {
    // NOTE: imported StatusCode itself doesn't have a corresponding schema in the OpenAPI document
    #[schema(example = "400")]
    pub(crate) status_code: StatusCode,
    #[schema(example = "AccountId example.near does not have enough remaining gas allowance.")]
    pub(crate) message: String,
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.status_code, self.message)
    }
}

impl std::error::Error for RelayError {}
