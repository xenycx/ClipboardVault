use axum::{
    Json,
    extract::multipart::MultipartError,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use thiserror::Error;
use tracing::error;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("authentication is required")]
    Unauthorized,
    #[error("your account is waiting for approval")]
    PendingApproval,
    #[error("you do not have permission for this action")]
    Forbidden,
    #[error("the requested record was not found")]
    NotFound,
    #[error("{0}")]
    Validation(String),
    #[error("upload exceeds the allowed size")]
    PayloadTooLarge { received: u64, limit: u64 },
    #[error("insufficient storage space for this upload")]
    InsufficientStorage { available: u64, required: u64 },
    #[error("the upload offset does not match the server offset")]
    OffsetConflict { expected: u64 },
    #[error("this file cannot be previewed safely")]
    PreviewUnavailable,
    #[error("the requested byte range is not satisfiable")]
    RangeNotSatisfiable { size: u64 },
    #[error("authentication service is unavailable")]
    AuthUnavailable,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Multipart(#[from] MultipartError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, detail): (StatusCode, &str, Value) = match &self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "AUTH_REQUIRED", json!({})),
            Self::PendingApproval => (StatusCode::FORBIDDEN, "ACCOUNT_PENDING", json!({})),
            Self::Forbidden => (StatusCode::FORBIDDEN, "FORBIDDEN", json!({})),
            Self::NotFound => (StatusCode::NOT_FOUND, "NOT_FOUND", json!({})),
            Self::Validation(_) => (StatusCode::BAD_REQUEST, "VALIDATION_ERROR", json!({})),
            Self::PayloadTooLarge { received, limit } => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "PAYLOAD_TOO_LARGE",
                json!({"received": received, "limit": limit}),
            ),
            Self::InsufficientStorage {
                available,
                required,
            } => (
                StatusCode::INSUFFICIENT_STORAGE,
                "INSUFFICIENT_STORAGE",
                json!({"available": available, "required": required}),
            ),
            Self::OffsetConflict { expected } => (
                StatusCode::CONFLICT,
                "UPLOAD_OFFSET_CONFLICT",
                json!({"expectedOffset": expected}),
            ),
            Self::PreviewUnavailable => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "PREVIEW_UNAVAILABLE",
                json!({}),
            ),
            Self::RangeNotSatisfiable { size } => (
                StatusCode::RANGE_NOT_SATISFIABLE,
                "RANGE_NOT_SATISFIABLE",
                json!({"size": size}),
            ),
            Self::AuthUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "AUTH_UNAVAILABLE",
                json!({}),
            ),
            Self::Multipart(error) => (error.status(), "MULTIPART_ERROR", json!({})),
            Self::Database(_) | Self::Io(_) | Self::Other(_) => {
                error!(error = %self, "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    json!({}),
                )
            }
        };
        let message = match code {
            "INTERNAL_ERROR" => "An internal error occurred".to_owned(),
            _ => self.to_string(),
        };
        let mut response = (
            status,
            Json(json!({"error": {"code": code, "message": message, "detail": detail}})),
        )
            .into_response();
        if let Self::RangeNotSatisfiable { size } = self {
            response.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{size}")).unwrap(),
            );
        }
        response
    }
}
