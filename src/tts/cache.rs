// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tts/cache.rs
//! Two-layer cache (LRU memory + disk) for synthesised audio.
//!
//! Keyed by `sha256(text || \0 || voice_id || \0 || backend_id || \0 || speed)`
//! so the same input always lands on the same disk path regardless of when or
//! how it was produced.
//!
//! only handles WAV output (Piper + eSpeak).  onward will need
//! to retain codec metadata across disk reads — for now we infer codec from
//! the cached file extension (and write `.wav` exclusively).

use lru::LruCache;
use sha2::{Digest, Sha256};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tokio::fs;
use tracing::{debug, warn};

use super::types::{AudioBuffer, AudioCodec};

// Cache key — pure data, no IO. Public so the service layer can compute
// keys before deciding whether to consult the cache at all.
pub fn cache_key(text: &str, voice_id: &str, backend_id: &str, speed: f32) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    h.update([0]);
    h.update(voice_id.as_bytes());
    h.update([0]);
    h.update(backend_id.as_bytes());
    h.update([0]);
    h.update(format!("{speed:.4}").as_bytes());
    hex::encode(h.finalize())
}

// Two-layer audio cache. Cheap to clone via `Arc`; methods take `&self`.
pub struct TtsCache {
    mem:            Mutex<LruCache<String, AudioBuffer>>,
    disk_dir:       PathBuf,
    // 0 = unbounded.
    max_disk_bytes: u64,
    // Zero = no TTL sweep.
    ttl:            Duration,
}

impl TtsCache {
    pub fn new(disk_dir: PathBuf, max_disk_mb: u64, ttl_days: u64) -> Self {
        let cap = NonZeroUsize::new(64).expect("constant > 0");
        Self {
            mem:            Mutex::new(LruCache::new(cap)),
            disk_dir,
            max_disk_bytes: max_disk_mb.saturating_mul(1024 * 1024),
            ttl:            Duration::from_secs(ttl_days.saturating_mul(86_400)),
        }
    }

    pub fn disk_dir(&self) -> &Path { &self.disk_dir }

    // Look up `key` in memory then on disk. On a disk hit the entry is
    // promoted into the in-memory LRU.
    pub async fn get(&self, key: &str) -> Option<AudioBuffer> {
        if let Some(buf) = self.mem.lock().ok().and_then(|mut g| g.get(key).cloned()) {
            return Some(buf);
        }
        let path = self.disk_path(key);
        let bytes = match fs::read(&path).await {
            Ok(b)  => b,
            Err(_) => return None,
        };
        let codec = AudioCodec::Wav { sample_rate: 22_050, channels: 1 };
        let buf = AudioBuffer { bytes, codec };
        if let Ok(mut g) = self.mem.lock() {
            g.put(key.to_string(), buf.clone());
        }
        debug!("tts cache: disk hit for {key}");
        Some(buf)
    }

    // Insert `(key, buf)` into both layers. Disk write failures are logged
    // and swallowed — the memory hit is still useful and a missed write just
    // means a future call re-synthesises.
    pub async fn put(&self, key: &str, buf: &AudioBuffer) {
        if let Ok(mut g) = self.mem.lock() {
            g.put(key.to_string(), buf.clone());
        }
        let path = self.disk_path(key);
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent).await {
                warn!("tts cache: cannot create dir {}: {e}", parent.display());
                return;
            }
        }
        if let Err(e) = fs::write(&path, &buf.bytes).await {
            warn!("tts cache: disk write {} failed: {e}", path.display());
        }
    }

    // Drop both layers. Called by the `mira tts cache clear` CLI.
    pub async fn clear(&self) -> std::io::Result<()> {
        if let Ok(mut g) = self.mem.lock() { g.clear(); }
        if self.disk_dir.exists() {
            fs::remove_dir_all(&self.disk_dir).await?;
            fs::create_dir_all(&self.disk_dir).await?;
        }
        Ok(())
    }

    // Best-effort sweep:
    // 1. delete entries older than `ttl` (when `ttl > 0`),
    // 2. delete oldest entries until on-disk size ≤ `max_disk_bytes`
    // (when `max_disk_bytes > 0`).
    pub async fn sweep(&self) -> std::io::Result<CacheStats> {
        let mut entries: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
        if !self.disk_dir.exists() {
            return Ok(CacheStats::default());
        }
        let mut rd = fs::read_dir(&self.disk_dir).await?;
        let now = SystemTime::now();
        while let Some(ent) = rd.next_entry().await? {
            let path = ent.path();
            let meta = match ent.metadata().await { Ok(m) => m, Err(_) => continue };
            if !meta.is_file() { continue; }
            let mtime = meta.modified().unwrap_or(now);
            let len   = meta.len();
            let mut keep = true;
            if !self.ttl.is_zero() {
                if let Ok(age) = now.duration_since(mtime) {
                    if age > self.ttl {
                        let _ = fs::remove_file(&path).await;
                        keep = false;
                    }
                }
            }
            if keep { entries.push((path, mtime, len)); }
        }

        if self.max_disk_bytes > 0 {
            // Oldest first — drop until we fit.
            entries.sort_by_key(|(_, m, _)| *m);
            let mut total: u64 = entries.iter().map(|(_, _, l)| *l).sum();
            while total > self.max_disk_bytes {
                let Some((p, _, l)) = entries.first().cloned() else { break };
                let _ = fs::remove_file(&p).await;
                total = total.saturating_sub(l);
                entries.remove(0);
            }
        }

        let total_bytes = entries.iter().map(|(_, _, l)| *l).sum();
        Ok(CacheStats { entries: entries.len(), total_bytes })
    }

    pub async fn stats(&self) -> CacheStats {
        if !self.disk_dir.exists() {
            return CacheStats::default();
        }
        let mut rd = match fs::read_dir(&self.disk_dir).await {
            Ok(r) => r,
            Err(_) => return CacheStats::default(),
        };
        let mut entries = 0;
        let mut bytes   = 0;
        while let Ok(Some(ent)) = rd.next_entry().await {
            if let Ok(meta) = ent.metadata().await {
                if meta.is_file() {
                    entries += 1;
                    bytes   += meta.len();
                }
            }
        }
        CacheStats { entries, total_bytes: bytes }
    }

    fn disk_path(&self, key: &str) -> PathBuf {
        // Two-char fan-out so a single dir doesn't accumulate millions of
        // entries before the sweep can run.
        let (a, b) = key.split_at(2.min(key.len()));
        self.disk_dir.join(a).join(format!("{b}.wav"))
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CacheStats {
    pub entries:     usize,
    pub total_bytes: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn buf(bytes: &[u8]) -> AudioBuffer {
        AudioBuffer {
            bytes: bytes.to_vec(),
            codec: AudioCodec::Wav { sample_rate: 22_050, channels: 1 },
        }
    }

    #[test]
    fn cache_key_deterministic_and_input_sensitive() {
        let a = cache_key("hello", "amy", "piper", 1.0);
        let b = cache_key("hello", "amy", "piper", 1.0);
        let c = cache_key("hello", "amy", "piper", 1.5);
        let d = cache_key("hi",    "amy", "piper", 1.0);
        let e = cache_key("hello", "ryan","piper", 1.0);
        let f = cache_key("hello", "amy", "espeak", 1.0);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_ne!(a, e);
        assert_ne!(a, f);
        assert_eq!(a.len(), 64, "hex sha256 = 64 chars");
    }

    #[tokio::test]
    async fn round_trip_through_memory_and_disk() {
        let dir = tempdir().unwrap();
        let cache = TtsCache::new(dir.path().to_path_buf(), 10, 30);

        assert!(cache.get("k1").await.is_none());
        cache.put("k1abcdef", &buf(b"audio")).await;

        // Memory hit.
        let hit = cache.get("k1abcdef").await.unwrap();
        assert_eq!(hit.bytes, b"audio");

        // Disk persistence — drop the LRU and read back.
        let cache2 = TtsCache::new(dir.path().to_path_buf(), 10, 30);
        let hit2 = cache2.get("k1abcdef").await.expect("disk persistence");
        assert_eq!(hit2.bytes, b"audio");
    }

    #[tokio::test]
    async fn clear_drops_disk_and_memory() {
        let dir = tempdir().unwrap();
        let cache = TtsCache::new(dir.path().to_path_buf(), 10, 30);
        cache.put("aaaa", &buf(b"x")).await;
        cache.clear().await.unwrap();
        assert!(cache.get("aaaa").await.is_none());
        let stats = cache.stats().await;
        assert_eq!(stats.entries, 0);
    }

    #[tokio::test]
    async fn sweep_evicts_when_over_disk_cap() {
        let dir   = tempdir().unwrap();
        // 1 MB cap; we'll plant 3× 600 KB entries.
        let cache = TtsCache::new(dir.path().to_path_buf(), 1, 0);
        for k in ["aaaa", "bbbb", "cccc"] {
            cache.put(k, &buf(&vec![0u8; 600 * 1024])).await;
            // Spread mtimes so eviction is deterministic.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let stats = cache.sweep().await.unwrap();
        assert!(stats.entries < 3, "sweep should have evicted at least one entry, got {stats:?}");
        assert!(stats.total_bytes <= 1024 * 1024,
            "post-sweep size {} > cap {}", stats.total_bytes, 1024 * 1024);
    }

    #[tokio::test]
    async fn fan_out_uses_two_char_prefix() {
        let dir = tempdir().unwrap();
        let cache = TtsCache::new(dir.path().to_path_buf(), 10, 30);
        cache.put("ab1234", &buf(b"x")).await;
        let prefix = dir.path().join("ab");
        assert!(prefix.exists(), "fan-out dir should exist");
    }
}
