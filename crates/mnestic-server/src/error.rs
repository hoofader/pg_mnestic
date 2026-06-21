// SPDX-License-Identifier: AGPL-3.0-only

//! HTTP error mapping. An internal cause stays server-side; the public body is generic
//! so a 500 never leaks schema, SQL, or upstream provider text.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    BadRequest(String),
    NotFound,
    TooManyRequests,
    Internal(String),
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Internal(e.to_string())
    }
}

impl From<mnestic_engine::Error> for ApiError {
    fn from(e: mnestic_engine::Error) -> Self {
        ApiError::Internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            ApiError::TooManyRequests => {
                (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded".to_string())
            }
            ApiError::Internal(detail) => {
                // Keep the cause out of the response; a public 500 must not reveal it. The
                // detail goes to the logs so an operator can still diagnose it.
                tracing::error!(error = %detail, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn internal_error_body_does_not_leak_detail() {
        let resp = ApiError::Internal("relation mnestic_secret does not exist".into()).into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(!body.contains("mnestic_secret"), "internal detail leaked: {body}");
        assert!(body.contains("internal error"));
    }
}
