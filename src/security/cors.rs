// SPDX-License-Identifier: AGPL-3.0-or-later

// src/security/cors.rs
//! CORS layer configuration.
//!
//! Wraps `tower_http::cors::CorsLayer` with MIRA-specific defaults:
//! - Allowed methods: GET, POST, OPTIONS
//! - Allowed headers: Authorization, Content-Type, X-Telegram-Bot-Api-Secret-Token
//! - Max-age: 3 600 s (1 hour)
//! - Credentials: false (Bearer token is explicit, not cookie-based)
//!
//! Allowed origins come from [`super::SecurityConfig::cors_origins`]:
//! - Empty list → deny all cross-origin requests
//! - `["*"]`    → allow any origin
//! - Otherwise  → allow only the listed origins

use axum::http::{HeaderName, HeaderValue, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Build a configured [`CorsLayer`] from the allowed-origins list.
pub fn build_cors_layer(allowed_origins: &[String]) -> CorsLayer {
    let origin = if allowed_origins.is_empty() {
        AllowOrigin::list(vec![])
    } else if allowed_origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        let values: Vec<HeaderValue> = allowed_origins
            .iter()
            .filter_map(|o| HeaderValue::from_str(o).ok())
            .collect();
        AllowOrigin::list(values)
    };

    CorsLayer::new()
        .allow_origin(origin)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
            HeaderName::from_static("x-telegram-bot-api-secret-token"),
        ])
        .max_age(std::time::Duration::from_secs(3600))
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_with_empty_origins() {
        let _ = build_cors_layer(&[]);
    }

    #[test]
    fn builds_with_wildcard() {
        let _ = build_cors_layer(&["*".to_string()]);
    }

    #[test]
    fn builds_with_specific_origins() {
        let _ = build_cors_layer(&["https://example.com".to_string()]);
    }
}
