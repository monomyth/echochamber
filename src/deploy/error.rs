use axum::http::StatusCode;

#[derive(Debug, thiserror::Error)]
pub enum DeployError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Failed(String),
}

impl DeployError {
    pub fn bad(message: impl Into<String>) -> Self {
        Self::BadRequest(message.into())
    }

    pub fn missing(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    pub fn fail(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Failed(_) => StatusCode::BAD_GATEWAY,
        }
    }
}
