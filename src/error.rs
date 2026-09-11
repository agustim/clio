use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("http client error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("invalid input: {0}")]
    BadRequest(String),

    #[error("blocked url: {0}")]
    Blocked(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("forbidden")]
    Forbidden,

    #[error("not found")]
    NotFound,

    #[error("pipeline error: {0}")]
    Pipeline(String),

    /// Fallades del proveïdor LLM (timeout, cos no desxifrable, resposta buida,
    /// circuit obert / cooldown...). Es distingeixen de les fallades de *link*
    /// perquè NO han de comptar com a fallada de la URL (no auto-blocklist) ni
    /// enganxar l'admin per cada link: són problemes transitoris del model.
    #[error("llm error: {0}")]
    Llm(String),

    #[error("git error: {0}")]
    Git(String),
}

pub type Result<T> = std::result::Result<T, AppError>;

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Blocked(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::Forbidden => StatusCode::FORBIDDEN,
            AppError::NotFound => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        // Log internals, never leak details on 500.
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "request failed");
        }
        let msg = match status {
            StatusCode::INTERNAL_SERVER_ERROR => "internal server error".to_string(),
            _ => self.to_string(),
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}
