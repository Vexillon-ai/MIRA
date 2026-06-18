// SPDX-License-Identifier: AGPL-3.0-or-later

// src/server/handlers/artifacts.rs
//! GET /api/artifacts/{id} — serves files saved by [`ArtifactStore`].
//!
//! Mounted on the **public** router (no bearer auth) for the same reason
//! `/avatars` is: `<img src>` requests don't carry Authorization headers, so
//! locking these behind auth would force the web client into a fetch +
//! blob-URL round trip per image. The URL itself is the capability — it
//! contains the artifact's SHA-256, which is unguessable without already
//! having the bytes.
//!
//! Defence in depth even given that:
//! - The `id` path segment is validated against [`is_valid_filename`]
//!   before we touch the filesystem (rejects `..`, slashes, uppercase hex,
//!   disallowed extensions).
//! - The store resolves the path under its own root only — the validator
//!   guarantees no separator survives the check, so directory traversal is
//!   impossible regardless of how the filename is constructed.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::artifacts::{is_valid_filename, ArtifactStore};

/// Map a known-good artifact extension to its MIME type. The validator has
/// already restricted us to this set, so the fallback is never hit in
/// practice — kept as a safety net.
fn content_type_for(ext: &str) -> &'static str {
    match ext {
        "png"          => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif"          => "image/gif",
        "svg"          => "image/svg+xml",
        "webp"         => "image/webp",
        // Audio (e.g. MCP audio content blocks: TTS / recordings).
        "mp3"          => "audio/mpeg",
        "wav"          => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "m4a"          => "audio/mp4",
        "flac"         => "audio/flac",
        // Video (artifacts referenced by tools / generated elsewhere).
        "mp4"          => "video/mp4",
        "webm"         => "video/webm",
        "mov"          => "video/quicktime",
        _              => "application/octet-stream",
    }
}

pub async fn get_artifact(
    State(store):  State<Arc<ArtifactStore>>,
    Path(id):      Path<String>,
) -> Response {
    if !is_valid_filename(&id) {
        return (StatusCode::BAD_REQUEST, "invalid artifact id").into_response();
    }

    let Some(path) = store.path_for(&id) else {
        return (StatusCode::NOT_FOUND, "artifact not found").into_response();
    };

    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::NOT_FOUND, "artifact not found").into_response(),
    };

    // Safe: `is_valid_filename` already confirmed there's a `.` and the
    // extension is in the allowlist.
    let ext = id.rsplit_once('.').map(|(_, e)| e).unwrap_or("");

    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type_for(ext)),
    );
    // Artifacts are content-addressed and never mutated, so callers can
    // cache aggressively.
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, State};

    fn fresh_store() -> (Arc<ArtifactStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s   = ArtifactStore::new(dir.path()).unwrap();
        (Arc::new(s), dir)
    }

    #[tokio::test]
    async fn serves_saved_artifact_with_correct_content_type() {
        let (store, _dir) = fresh_store();
        let id = store.save_bytes(b"\x89PNG\r\n\x1a\nfake", "png").unwrap();

        let resp = get_artifact(State(Arc::clone(&store)), Path(id.filename())).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/png",
        );
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let (store, _dir) = fresh_store();
        let resp = get_artifact(
            State(Arc::clone(&store)),
            Path("../etc/passwd".to_string()),
        ).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rejects_disallowed_extension_in_url() {
        let (store, _dir) = fresh_store();
        let bad = format!("{}.exe", "a".repeat(64));
        let resp = get_artifact(State(Arc::clone(&store)), Path(bad)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn returns_404_for_unknown_sha() {
        let (store, _dir) = fresh_store();
        let unknown = format!("{}.png", "a".repeat(64));
        let resp = get_artifact(State(Arc::clone(&store)), Path(unknown)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
