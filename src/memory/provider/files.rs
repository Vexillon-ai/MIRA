// SPDX-License-Identifier: AGPL-3.0-or-later

//! `FilesMemoryProvider` — a plain-text/markdown memory store on disk.
//!
//! The honest baseline every benchmark needs: no embeddings, keyword recall over
//! per-user note files. Its real work is **path safety** — it applies the
//! memory-tool path rules (spec §7): percent-decode then reject traversal and
//! separators, sanitise incidental characters, and keep every resolved path
//! strictly under the configured root, with a per-note size cap.
//!
//! Layout: `<root>/<user>/<id>.md`, one note per file. A fact's `id` is
//! `"<user>/<id>"` so `forget` can round-trip without a separate scope arg; both
//! segments are re-sanitised on the way back in.

use std::path::{Path, PathBuf};
use async_trait::async_trait;
use chrono::Utc;

use super::{
    ForgetSelector, MemoryCapabilities, MemoryError, MemoryFact, MemoryHealth,
    MemoryLocality, MemoryProvider, MemoryQuery, MemoryRetrieval, Speaker, Trust, TurnRecord,
};

/// Max bytes stored for a single note (ingested content is truncated to this).
const MAX_NOTE_BYTES: usize = 64 * 1024;

pub struct FilesMemoryProvider {
    id: String,
    root: PathBuf,
}

impl FilesMemoryProvider {
    pub fn new(id: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self { id: id.into(), root: root.into() }
    }

    /// Resolve `<root>/<user>/<file>` safely, or error. Both segments are
    /// sanitised; the final path is asserted to stay under the root.
    fn resolve(&self, user: &str, file: &str) -> Result<PathBuf, MemoryError> {
        let user = safe_segment(user)?;
        let file = safe_segment(file)?;
        let p = self.root.join(&user).join(&file);
        // Belt-and-suspenders: the sanitised join can't escape, but assert it.
        if !p.starts_with(&self.root) {
            return Err(MemoryError::InvalidRequest("path escaped root".into()));
        }
        Ok(p)
    }

    fn user_dir(&self, user: &str) -> Result<PathBuf, MemoryError> {
        let user = safe_segment(user)?;
        Ok(self.root.join(user))
    }
}

/// Apply the path rules to one path segment: percent-decode, reject traversal
/// and separators outright, then sanitise any remaining non-`[A-Za-z0-9._-]`
/// character to `_`. Rejection (not silent remap) on explicit traversal so the
/// caller gets a clear error, per the spec.
fn safe_segment(raw: &str) -> Result<String, MemoryError> {
    let decoded = percent_decode(raw);
    let bad = decoded.is_empty()
        || decoded == "."
        || decoded == ".."
        || decoded.contains("..")
        || decoded.contains('/')
        || decoded.contains('\\')
        || decoded.contains('\0');
    if bad {
        return Err(MemoryError::InvalidRequest(format!("unsafe path segment: {raw:?}")));
    }
    let clean: String = decoded.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();
    // A segment that sanitised down to only dots (e.g. "...") is still unsafe.
    if clean.chars().all(|c| c == '.') {
        return Err(MemoryError::InvalidRequest(format!("unsafe path segment: {raw:?}")));
    }
    Ok(clean)
}

/// Minimal percent-decoder (handles `%XX` hex, case-insensitive). Enough to
/// unmask URL-encoded traversal like `%2e%2e` / `%2f` before the safety check.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn score_match(content: &str, query: &str) -> f32 {
    let hay = content.to_lowercase();
    let terms: Vec<&str> = query.split_whitespace()
        .filter(|t| t.chars().count() >= 3)
        .collect();
    if terms.is_empty() { return 0.0; }
    let hit = terms.iter().filter(|t| hay.contains(&t.to_lowercase())).count();
    hit as f32 / terms.len() as f32
}

#[async_trait]
impl MemoryProvider for FilesMemoryProvider {
    fn id(&self) -> &str { &self.id }

    fn capabilities(&self) -> MemoryCapabilities {
        MemoryCapabilities { read: true, write: true, forget: true, ..MemoryCapabilities::none() }
    }

    fn locality(&self) -> MemoryLocality { MemoryLocality::Loopback }

    async fn health(&self) -> MemoryHealth { MemoryHealth::Ready }

    async fn retrieve(&self, q: &MemoryQuery) -> Result<MemoryRetrieval, MemoryError> {
        if q.user_id.trim().is_empty() {
            return Err(MemoryError::InvalidRequest("user_id is required".into()));
        }
        let dir = self.user_dir(&q.user_id)?;
        let mut facts: Vec<MemoryFact> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") { continue; }
                let content = match std::fs::read_to_string(&path) { Ok(c) => c, Err(_) => continue };
                let score = score_match(&content, &q.query);
                if score <= 0.0 { continue; }
                let recorded_at = entry.metadata().ok()
                    .and_then(|m| m.modified().ok())
                    .map(chrono::DateTime::<Utc>::from);
                let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
                let safe_user = safe_segment(&q.user_id).unwrap_or_default();
                facts.push(MemoryFact {
                    id: Some(format!("{safe_user}/{stem}")),
                    content: content.trim().to_string(),
                    score: Some(score),
                    recorded_at,
                    valid_at: None,
                    source_id: self.id.clone(),
                    provenance: Some(format!("file:{stem}")),
                    confidence: None,
                    speaker: Speaker::User,
                    trust: Trust::Unverified,
                    complete_set: false,
                    provider_meta: serde_json::Value::Null,
                });
            }
        }
        if facts.is_empty() {
            return Ok(MemoryRetrieval::Absent { reason: None });
        }
        facts.sort_by(|a, b| b.score.unwrap_or(0.0).partial_cmp(&a.score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal));
        facts.truncate(q.k.max(1));
        Ok(MemoryRetrieval::Facts(facts))
    }

    async fn ingest(&self, turn: &TurnRecord) -> Result<usize, MemoryError> {
        let body = turn.user_msg.trim();
        if body.is_empty() { return Ok(0); }
        let dir = self.user_dir(&turn.user_id)?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| MemoryError::Backend(format!("files: mkdir: {e}")))?;
        let id = uuid::Uuid::now_v7().to_string();
        let path = self.resolve(&turn.user_id, &format!("{id}.md"))?;
        let mut text = body.to_string();
        if text.len() > MAX_NOTE_BYTES {
            text.truncate(floor_char_boundary(&text, MAX_NOTE_BYTES));
        }
        std::fs::write(&path, text)
            .map_err(|e| MemoryError::Backend(format!("files: write: {e}")))?;
        set_owner_only(&path);
        Ok(1)
    }

    async fn forget(&self, sel: &ForgetSelector) -> Result<usize, MemoryError> {
        match sel {
            ForgetSelector::Id { caller_user_id, item, .. } => {
                let (user, file) = item.split_once('/')
                    .ok_or_else(|| MemoryError::InvalidRequest(format!("bad files id '{item}'")))?;
                // Cross-user guard: a caller may only forget their OWN notes. The
                // user segment in the id is NOT trusted as authorization.
                if user != caller_user_id {
                    return Err(MemoryError::InvalidRequest("cross-user forget refused".into()));
                }
                let path = self.resolve(user, file)?;
                match std::fs::remove_file(&path) {
                    Ok(()) => Ok(1),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
                    Err(e) => Err(MemoryError::Backend(format!("files: remove: {e}"))),
                }
            }
            ForgetSelector::AllForUser(user) => {
                let dir = self.user_dir(user)?;
                match std::fs::remove_dir_all(&dir) {
                    Ok(()) => Ok(1),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
                    Err(e) => Err(MemoryError::Backend(format!("files: purge: {e}"))),
                }
            }
        }
    }
}

fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() { return s.len(); }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) { i -= 1; }
    i
}

#[cfg(unix)]
fn set_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn set_owner_only(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn provider() -> (FilesMemoryProvider, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        (FilesMemoryProvider::new("files", dir.path().to_path_buf()), dir)
    }

    #[test]
    fn rejects_dot_dot_traversal() {
        assert!(safe_segment("../etc").is_err());
        assert!(safe_segment("..").is_err());
        assert!(safe_segment("a/../b").is_err());
    }

    #[test]
    fn rejects_url_encoded_traversal() {
        assert!(safe_segment("%2e%2e").is_err(), "%2e%2e decodes to ..");
        assert!(safe_segment("%2e%2e%2fetc").is_err(), "%2e%2e%2f decodes to ../");
        assert!(safe_segment("foo%2fbar").is_err(), "%2f decodes to /");
    }

    #[test]
    fn rejects_absolute_and_separators() {
        assert!(safe_segment("/etc/passwd").is_err());
        assert!(safe_segment("a\\b").is_err());
        assert!(safe_segment("a\0b").is_err());
        assert!(safe_segment("...").is_err());
    }

    #[test]
    fn sanitises_incidental_chars_and_keeps_safe_ones() {
        assert_eq!(safe_segment("Alice_1.md").unwrap(), "Alice_1.md");
        assert_eq!(safe_segment("a b:c").unwrap(), "a_b_c");
    }

    #[test]
    fn resolve_stays_under_root_and_traversal_errors() {
        let (p, _d) = provider();
        assert!(p.resolve("alice", "n.md").unwrap().starts_with(&p.root));
        assert!(p.resolve("../../etc", "passwd").is_err());
        assert!(p.resolve("alice", "..%2f..%2fpasswd").is_err());
    }

    #[tokio::test]
    async fn ingest_retrieve_forget_round_trip() {
        let (p, _d) = provider();
        let turn = TurnRecord {
            user_id: "alice".into(), agent_id: None, thread_id: None,
            channel: "web".into(), user_msg: "I love hiking in the mountains".into(),
            assistant_msg: String::new(), conversation_id: None,
        };
        assert_eq!(p.ingest(&turn).await.unwrap(), 1);

        let got = p.retrieve(&MemoryQuery::new("alice", "hiking mountains", 5)).await.unwrap();
        let id = match got {
            MemoryRetrieval::Facts(f) => {
                assert_eq!(f.len(), 1);
                assert!(f[0].content.contains("hiking"));
                f[0].id.clone().unwrap()
            }
            other => panic!("expected facts, got {other:?}"),
        };
        // A query that doesn't match → Absent, not error.
        assert!(matches!(
            p.retrieve(&MemoryQuery::new("alice", "quantum chromodynamics", 5)).await.unwrap(),
            MemoryRetrieval::Absent { .. }
        ));
        assert_eq!(p.forget(&ForgetSelector::Id { caller_user_id: "alice".into(), item: id, caller_username: None, caller_email: None }).await.unwrap(), 1);
        assert!(matches!(
            p.retrieve(&MemoryQuery::new("alice", "hiking mountains", 5)).await.unwrap(),
            MemoryRetrieval::Absent { .. }
        ));
    }

    #[tokio::test]
    async fn users_are_isolated() {
        let (p, _d) = provider();
        let turn = |u: &str, m: &str| TurnRecord {
            user_id: u.into(), agent_id: None, thread_id: None, channel: "web".into(),
            user_msg: m.into(), assistant_msg: String::new(), conversation_id: None,
        };
        p.ingest(&turn("alice", "alice keeps bees")).await.unwrap();
        p.ingest(&turn("bob", "bob keeps bees")).await.unwrap();
        // Bob's query only sees bob's note.
        match p.retrieve(&MemoryQuery::new("bob", "keeps bees", 5)).await.unwrap() {
            MemoryRetrieval::Facts(f) => {
                assert_eq!(f.len(), 1);
                assert!(f[0].content.contains("bob") && !f[0].content.contains("alice"));
            }
            other => panic!("expected facts, got {other:?}"),
        }
    }

    // H1 regression: a caller must NOT be able to forget another user's note by
    // crafting an id with a different user segment.
    #[tokio::test]
    async fn cross_user_forget_is_refused() {
        let (p, _d) = provider();
        let turn = TurnRecord {
            user_id: "alice".into(), agent_id: None, thread_id: None, channel: "web".into(),
            user_msg: "alice secret note".into(), assistant_msg: String::new(), conversation_id: None,
        };
        p.ingest(&turn).await.unwrap();
        let id = match p.retrieve(&MemoryQuery::new("alice", "secret", 5)).await.unwrap() {
            MemoryRetrieval::Facts(f) => f[0].id.clone().unwrap(),
            other => panic!("expected facts, got {other:?}"),
        };
        // Bob tries to forget alice's note via the crafted id → refused.
        let r = p.forget(&ForgetSelector::Id { caller_user_id: "bob".into(), item: id.clone(), caller_username: None, caller_email: None }).await;
        assert!(matches!(r, Err(MemoryError::InvalidRequest(_))), "cross-user forget must be refused");
        // Alice's note still there.
        assert!(matches!(
            p.retrieve(&MemoryQuery::new("alice", "secret", 5)).await.unwrap(),
            MemoryRetrieval::Facts(_)
        ), "the note must survive the refused cross-user forget");
        // Alice herself can forget it.
        assert_eq!(p.forget(&ForgetSelector::Id { caller_user_id: "alice".into(), item: id, caller_username: None, caller_email: None }).await.unwrap(), 1);
    }
}
