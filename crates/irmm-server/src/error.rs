use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn not_found() -> Self {
        Self(StatusCode::NOT_FOUND, "not found".into())
    }

    pub fn forbidden() -> Self {
        Self(StatusCode::FORBIDDEN, "forbidden".into())
    }

    pub fn unauthorized() -> Self {
        Self(StatusCode::UNAUTHORIZED, "unauthorized".into())
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, msg.into())
    }

    pub fn conflict(msg: impl Into<String>) -> Self {
        Self(StatusCode::CONFLICT, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        if let sqlx::Error::Database(db) = &e {
            if db.is_unique_violation() {
                return Self::conflict("already exists");
            }
            if db.is_foreign_key_violation() {
                return Self::not_found();
            }
        }
        tracing::error!(error = %e, "database error");
        Self(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!(error = %format!("{e:#}"), "internal error");
        Self(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
    }
}
