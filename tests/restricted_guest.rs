// SPDX-License-Identifier: AGPL-3.0-or-later
//! Ephemeral guest sessions (Restricted Mode, Slice 3): fail-closed minting and
//! complete isolation + wipe on teardown.

use std::path::Path;
use std::sync::Arc;

use mira::auth::local::LocalAuthService;
use mira::config::GuestConfig;
use mira::guest::{GuestManager, GuestMintError};
use mira::history::{HistoryStore, NewConversation};
use mira::memory::{Category, MemoryStorage, MemorySystem, Scope};
use mira::wiki::WikiRegistry;

struct Deps {
    mgr:      Arc<GuestManager>,
    auth:     Arc<LocalAuthService>,
    memory:   Arc<MemorySystem>,
    history:  Arc<HistoryStore>,
    mem_path: std::path::PathBuf,
}

fn build(enabled: bool, restricted_active: bool, seed_dir: Option<String>, dir: &Path) -> Deps {
    let data_dir = dir.to_path_buf();
    let mem_path = data_dir.join("memory.db");
    let auth = Arc::new(
        LocalAuthService::new(&data_dir.join("auth.db"), "test-secret".into(), 7).unwrap());
    let memory  = Arc::new(MemorySystem::new_keyword_only(mem_path.clone()).unwrap());
    let wiki    = Arc::new(WikiRegistry::new(data_dir.clone()));
    let history = Arc::new(HistoryStore::open(&data_dir.join("history.db")).unwrap());
    let cfg = GuestConfig {
        enabled,
        session_ttl_secs: 1800,
        max_active: 0,
        global_reset_secs: 0,
        seed_wiki_dir: seed_dir,
    };
    let mgr = Arc::new(GuestManager::new(
        cfg, restricted_active,
        Arc::clone(&auth), Arc::clone(&memory), wiki, Arc::clone(&history),
        data_dir, "TestServer".into(), None,
    ));
    Deps { mgr, auth, memory, history, mem_path }
}

#[tokio::test]
async fn guest_mint_is_fail_closed_when_disabled_or_unrestricted() {
    // guest.enabled = true but NO restriction profile active → refuse. This is
    // the key property: a guest is NEVER handed out on an unrestricted instance.
    let dir = tempfile::tempdir().unwrap();
    let d = build(true, false, None, dir.path());
    assert!(!d.mgr.is_enabled());
    assert!(matches!(d.mgr.mint().await, Err(GuestMintError::Disabled)));

    // profile active but guest.enabled = false → refuse.
    let dir2 = tempfile::tempdir().unwrap();
    let d2 = build(false, true, None, dir2.path());
    assert!(!d2.mgr.is_enabled());
    assert!(matches!(d2.mgr.mint().await, Err(GuestMintError::Disabled)));
}

#[tokio::test]
async fn guest_lifecycle_isolates_then_completely_wipes() {
    let dir = tempfile::tempdir().unwrap();

    // A baseline wiki with one page, to prove seeding.
    let seed = dir.path().join("seed-wiki");
    std::fs::create_dir_all(&seed).unwrap();
    std::fs::write(seed.join("welcome.md"), "# Welcome\nseeded context.").unwrap();

    let d = build(true, true, Some(seed.to_string_lossy().into_owned()), dir.path());
    assert!(d.mgr.is_enabled());

    let session = d.mgr.mint().await.expect("mint should succeed when enabled + restricted");
    let gid = session.user.id.clone();
    assert!(session.user.username.starts_with("guest_"));
    assert!(!session.access_token.is_empty(), "a scoped token was issued");

    // Account exists; seeded wiki page present.
    assert!(d.auth.get_user(&gid).unwrap().is_some());
    let wiki_dir = dir.path().join("wikis").join("users").join(&gid);
    assert!(wiki_dir.join("welcome.md").exists(), "guest wiki was seeded");

    // Write a memory + a conversation owned by the guest.
    d.memory.store_scoped(
        "guest-only secret".into(), Category::Fact, vec![], None,
        Scope::User, Some(&gid), &gid, &[], None, None, None,
    ).await.unwrap();
    let conv = d.history.create_conversation(NewConversation {
        user_id: gid.clone(), channel: "web".into(), title: Some("t".into()),
        model: None, provider: None, external_user_id: None, mode: None,
    }).unwrap();

    // Present before teardown.
    assert_eq!(guest_memory_count(&d.mem_path, &gid), 1);
    assert!(d.history.get_conversation(&conv.id).unwrap().is_some());

    // ── TEARDOWN — everything the guest touched must be gone ──
    d.mgr.teardown(&gid).await;

    assert!(d.auth.get_user(&gid).unwrap().is_none(),       "account deleted");
    assert!(!wiki_dir.exists(),                              "wiki wiped");
    assert_eq!(guest_memory_count(&d.mem_path, &gid), 0,     "memory wiped");
    assert!(d.history.get_conversation(&conv.id).unwrap().is_none(), "history wiped");
}

// Count a user's memory rows via an independent connection (proves the wipe hit
// the shared DB, not just an in-memory cache).
fn guest_memory_count(mem_path: &Path, user_id: &str) -> usize {
    MemoryStorage::new_for_user(mem_path, user_id).unwrap()
        .list_all(100, 0).unwrap().len()
}
