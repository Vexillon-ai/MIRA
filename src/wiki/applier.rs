// SPDX-License-Identifier: AGPL-3.0-or-later

// src/wiki/applier.rs
//! Deterministic applier — the only thing that mutates wiki files.
//!
//! Every variant of [`WikiOp`] maps to a small filesystem transformation
//! here. The applier reads the current page, applies the change in
//! memory, then writes the result back atomically. Writer-policy is
//! enforced before the write.

use chrono::Utc;

use crate::wiki::frontmatter::{PageFrontmatter, ProvenanceEntry, Writer};
use crate::wiki::ops::{Provenance, WikiOp};
use crate::wiki::page::{self, WikiPage};
use crate::wiki::paths::WikiPath;
use crate::wiki::store::WikiStore;
use crate::wiki::{Result, WikiError};

/// Stateless applier — borrows a `WikiStore` for path resolution. Each
/// `apply` call is its own short-lived unit.
pub struct WikiApplier<'a> {
    store: &'a WikiStore,
}

impl<'a> WikiApplier<'a> {
    pub fn new(store: &'a WikiStore) -> Self { Self { store } }

    /// Execute an op. The provenance is appended to the target page's
    /// frontmatter `provenance` list on every mutation; the caller
    /// provides it because the op object itself is conceptually
    /// stateless.
    pub fn apply(&self, op: &WikiOp, prov: &Provenance) -> Result<()> {
        match op {
            WikiOp::WritePage { path, frontmatter, body } => {
                self.write_page(path, frontmatter, body, prov, /* required = */ None)
            }
            WikiOp::UpdateSection { path, section, body } => {
                self.mutate_section(path, section, body, prov, SectionMode::Replace)
            }
            WikiOp::AppendSection { path, section, body } => {
                self.mutate_section(path, section, body, prov, SectionMode::Append)
            }
            WikiOp::LogEntry { kind, summary, page_refs } => {
                self.append_log(*kind, summary, page_refs)
            }
            WikiOp::Supersede { path, reason, replacement } => {
                self.supersede(path, reason, replacement.as_ref(), prov)
            }
            WikiOp::PromoteFact { memory_id, target, section } => {
                let body = format!(
                    "- Promoted from memory `{memory_id}` on {}\n",
                    Utc::now().format("%Y-%m-%d"),
                );
                self.mutate_section(target, section, &body, prov, SectionMode::Append)
            }
            WikiOp::DeletePage { path } => {
                self.delete_page(path, prov)
            }
        }
    }

    // ── Implementations ──────────────────────────────────────────────────────

    fn write_page(
        &self,
        path: &WikiPath,
        new_fm: &PageFrontmatter,
        body: &str,
        prov: &Provenance,
        required: Option<Writer>,
    ) -> Result<()> {
        let existing = self.store.try_read_page(path)?;
        let mut fm = match &existing {
            Some(p) => p.frontmatter.clone(),
            None    => PageFrontmatter::default(),
        };
        // Writer policy: if the page exists, enforce its declared writer.
        if let Some(existing) = &existing {
            enforce_writer(&existing.frontmatter.writer, required, prov)?;
        }
        // Take fields from `new_fm`, except keep accumulated provenance.
        fm.title       = new_fm.title.clone().or(fm.title);
        fm.writer      = new_fm.writer;
        fm.tags        = if new_fm.tags.is_empty() { fm.tags } else { new_fm.tags.clone() };
        fm.valid_from  = new_fm.valid_from.or(fm.valid_from);
        fm.valid_to    = new_fm.valid_to.or(fm.valid_to);
        fm.confidence  = new_fm.confidence.or(fm.confidence);
        fm.extra       = if new_fm.extra.is_empty() { fm.extra } else { new_fm.extra.clone() };
        append_provenance(&mut fm, prov);

        let page = WikiPage { path: path.clone(), frontmatter: fm, body: body.to_string() };
        page.write(self.store.root())
    }

    fn mutate_section(
        &self,
        path: &WikiPath,
        section: &str,
        body: &str,
        prov: &Provenance,
        mode: SectionMode,
    ) -> Result<()> {
        let mut page = self.store.try_read_page(path)?.unwrap_or_else(|| WikiPage {
            path: path.clone(),
            frontmatter: PageFrontmatter::default(),
            body: String::new(),
        });
        enforce_writer(&page.frontmatter.writer, None, prov)?;
        page.body = match mode {
            SectionMode::Replace => page::replace_section(&page.body, section, body),
            SectionMode::Append  => page::append_section(&page.body, section, body),
        };
        append_provenance(&mut page.frontmatter, prov);
        page.write(self.store.root())
    }

    fn append_log(
        &self,
        kind: crate::wiki::ops::LogKind,
        summary: &str,
        page_refs: &[WikiPath],
    ) -> Result<()> {
        let log_path = self.store.root().join("log.md");
        let existing = std::fs::read_to_string(&log_path).unwrap_or_default();
        let trimmed = existing.trim_end();
        let mut next = String::from(trimmed);
        if !next.is_empty() { next.push('\n'); }
        let date = Utc::now().format("%Y-%m-%d");
        next.push_str(&format!("\n## [{date}] {} | {summary}\n", kind.as_str()));
        if !page_refs.is_empty() {
            for p in page_refs {
                next.push_str(&format!("- [{p}]({p})\n", p = p.as_str()));
            }
        }
        page::write_raw(&log_path, &next)
    }

    fn supersede(
        &self,
        path: &WikiPath,
        reason: &str,
        replacement: Option<&WikiPath>,
        prov: &Provenance,
    ) -> Result<()> {
        let mut page = self.store.read_page(path)?;
        enforce_writer(&page.frontmatter.writer, None, prov)?;
        page.frontmatter.valid_to = Some(Utc::now().date_naive());
        if let Some(r) = replacement {
            page.frontmatter.extra.insert(
                "superseded_by".into(),
                serde_yaml::Value::String(r.as_str().to_string()),
            );
        }
        page.frontmatter.extra.insert(
            "supersede_reason".into(),
            serde_yaml::Value::String(reason.to_string()),
        );
        append_provenance(&mut page.frontmatter, prov);
        page.write(self.store.root())
    }

    fn delete_page(&self, path: &WikiPath, prov: &Provenance) -> Result<()> {
        let page = self.store.read_page(path)?;
        enforce_writer(&page.frontmatter.writer, None, prov)?;
        let abs = path.resolve(self.store.root());
        // Archive instead of unlink: rename to `<name>.deleted-<ts>.md` next
        // to the original so accidental deletes are recoverable until the
        // user clears them.
        let ts = Utc::now().format("%Y%m%d%H%M%S");
        let archived = abs.with_extension(format!("deleted-{ts}.md"));
        std::fs::rename(abs, archived)?;
        Ok(())
    }
}

#[derive(Copy, Clone)]
enum SectionMode { Replace, Append }

/// Append a provenance entry to a page's frontmatter so the file
/// itself carries the audit trail in addition to `wiki_audit`.
fn append_provenance(fm: &mut PageFrontmatter, prov: &Provenance) {
    fm.provenance.push(ProvenanceEntry {
        source: prov.source.clone(),
        turn_id: prov.turn_id.clone(),
        conversation_id: prov.conversation_id.clone(),
        extracted_at: Utc::now(),
    });
}

/// Writer-policy check. `required` is set when the caller knows which
/// writer the op corresponds to (the agent passes `Writer::Agent`, the
/// UI passes `Writer::User`); when `None`, we derive it from the
/// provenance source.
fn enforce_writer(
    page_writer: &Writer,
    required: Option<Writer>,
    prov: &Provenance,
) -> Result<()> {
    let acting = required.unwrap_or_else(|| match prov.source.as_str() {
        "user_ui"   | "user"  => Writer::User,
        "turn"      | "tool"  | "extractor" | "agent" => Writer::Agent,
        // Migration and imports are trusted system writes — allowed
        // regardless of page writer policy.
        "migration" | "import" => return_both_marker(),
        _ => Writer::Agent,
    });
    if matches!(acting, Writer::Both) { return Ok(()); }
    match (page_writer, acting) {
        (Writer::Both, _) => Ok(()),
        (Writer::Agent, Writer::Agent) => Ok(()),
        (Writer::User, Writer::User) => Ok(()),
        (page, acting) => Err(WikiError::WriterPolicy {
            required: acting.as_str().to_string(),
            actual: page.as_str().to_string(),
        }),
    }
}

/// Internal sentinel that the closure expression form can return — Rust
/// closures can't carry a control-flow `return` to the outer function, so
/// the match arm uses this helper to express "trusted system write".
fn return_both_marker() -> Writer { Writer::Both }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::frontmatter::Writer;
    use crate::wiki::ops::{LogKind, Provenance};
    use crate::wiki::page::write_raw;
    use tempfile::tempdir;

    fn wiki() -> (tempfile::TempDir, WikiStore) {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("pages")).unwrap();
        write_raw(&root.join("log.md"), "# Log\n").unwrap();
        let store = WikiStore::new(root);
        (dir, store)
    }

    #[test]
    fn write_page_creates_then_updates() {
        let (_dir, store) = wiki();
        let applier = WikiApplier::new(&store);
        let path = WikiPath::parse("pages/foo.md").unwrap();
        let mut fm = PageFrontmatter::default();
        fm.title = Some("Foo".into());
        fm.writer = Writer::Agent;
        let op = WikiOp::WritePage { path: path.clone(), frontmatter: fm.clone(), body: "# v1\n".into() };
        let prov = Provenance::user_ui("u1");
        applier.apply(&op, &prov).unwrap();
        let p = store.read_page(&path).unwrap();
        assert!(p.body.contains("# v1"));
        assert_eq!(p.frontmatter.title.as_deref(), Some("Foo"));
        assert_eq!(p.frontmatter.provenance.len(), 1);
    }

    #[test]
    fn replace_section_changes_body() {
        let (_dir, store) = wiki();
        let applier = WikiApplier::new(&store);
        let path = WikiPath::parse("pages/foo.md").unwrap();
        applier.apply(
            &WikiOp::WritePage {
                path: path.clone(),
                frontmatter: PageFrontmatter::default(),
                body: "## A\nold\n\n## B\nkeep\n".into(),
            },
            &Provenance::user_ui("u1"),
        ).unwrap();
        applier.apply(
            &WikiOp::UpdateSection { path: path.clone(), section: "A".into(), body: "new".into() },
            &Provenance::user_ui("u1"),
        ).unwrap();
        let p = store.read_page(&path).unwrap();
        assert!(p.body.contains("new"));
        assert!(!p.body.contains("old\n"));
        assert!(p.body.contains("## B"));
    }

    #[test]
    fn append_section_keeps_existing_content() {
        let (_dir, store) = wiki();
        let applier = WikiApplier::new(&store);
        let path = WikiPath::parse("pages/foo.md").unwrap();
        applier.apply(
            &WikiOp::WritePage {
                path: path.clone(),
                frontmatter: PageFrontmatter::default(),
                body: "## A\nfirst\n".into(),
            },
            &Provenance::user_ui("u1"),
        ).unwrap();
        applier.apply(
            &WikiOp::AppendSection { path: path.clone(), section: "A".into(), body: "second".into() },
            &Provenance::user_ui("u1"),
        ).unwrap();
        let p = store.read_page(&path).unwrap();
        assert!(p.body.contains("first"));
        assert!(p.body.contains("second"));
    }

    #[test]
    fn log_entry_appends_to_log_md() {
        let (_dir, store) = wiki();
        let applier = WikiApplier::new(&store);
        let op = WikiOp::LogEntry {
            kind: LogKind::Note,
            summary: "test-entry".into(),
            page_refs: vec![],
        };
        applier.apply(&op, &Provenance::user_ui("u1")).unwrap();
        let log = store.read_log_raw().unwrap();
        assert!(log.contains("test-entry"));
        assert!(log.contains("note"));
    }

    #[test]
    fn supersede_sets_valid_to() {
        let (_dir, store) = wiki();
        let applier = WikiApplier::new(&store);
        let path = WikiPath::parse("pages/foo.md").unwrap();
        applier.apply(
            &WikiOp::WritePage {
                path: path.clone(),
                frontmatter: PageFrontmatter::default(),
                body: "body\n".into(),
            },
            &Provenance::user_ui("u1"),
        ).unwrap();
        applier.apply(
            &WikiOp::Supersede {
                path: path.clone(),
                reason: "newer info".into(),
                replacement: None,
            },
            &Provenance::user_ui("u1"),
        ).unwrap();
        let p = store.read_page(&path).unwrap();
        assert!(p.frontmatter.valid_to.is_some());
        assert!(p.frontmatter.extra.contains_key("supersede_reason"));
    }

    #[test]
    fn writer_policy_rejects_agent_writing_user_page() {
        let (_dir, store) = wiki();
        let applier = WikiApplier::new(&store);
        let path = WikiPath::parse("pages/foo.md").unwrap();
        // Create a user-writer page via the UI.
        applier.apply(
            &WikiOp::WritePage {
                path: path.clone(),
                frontmatter: PageFrontmatter {
                    writer: Writer::User,
                    ..Default::default()
                },
                body: "user\n".into(),
            },
            &Provenance::user_ui("u1"),
        ).unwrap();
        // Now the extractor tries to mutate it — must be rejected.
        let res = applier.apply(
            &WikiOp::UpdateSection { path, section: "Notes".into(), body: "agent edit".into() },
            &Provenance {
                source: "extractor".into(),
                turn_id: Some("t1".into()),
                conversation_id: Some("c1".into()),
                actor: "extractor".into(),
            },
        );
        assert!(matches!(res, Err(WikiError::WriterPolicy { .. })));
    }

    #[test]
    fn delete_page_archives_not_unlinks() {
        let (_dir, store) = wiki();
        let applier = WikiApplier::new(&store);
        let path = WikiPath::parse("pages/foo.md").unwrap();
        applier.apply(
            &WikiOp::WritePage {
                path: path.clone(),
                frontmatter: PageFrontmatter::default(),
                body: "x\n".into(),
            },
            &Provenance::user_ui("u1"),
        ).unwrap();
        applier.apply(&WikiOp::DeletePage { path: path.clone() }, &Provenance::user_ui("u1")).unwrap();
        // Original gone, archived file remains in the same directory.
        assert!(!path.resolve(store.root()).exists());
        let pages_dir = store.root().join("pages");
        let archived: Vec<_> = std::fs::read_dir(&pages_dir).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".deleted-"))
            .collect();
        assert_eq!(archived.len(), 1);
    }
}
