// SPDX-License-Identifier: AGPL-3.0-or-later

// src/memory/vector_backend.rs

//! Pluggable vector storage backends for MIRA semantic memory.
//! Configured via `config.toml` → `[memory] vector_backend = "sqlite"`.

use crate::MiraError;

pub type RawVector = Vec<f32>;

/// Synchronous trait for persisting and retrieving embedding vectors.
/// All implementors must be Send + Sync so they can be held behind Arc.
pub trait VectorStoreBackend: Send + Sync {
    /// Persist or update the vector for the given memory id.
    fn upsert(&self, id: u64, vector: &RawVector, category: &str) -> Result<(), MiraError>;

    /// Return the top_k closest vectors to `query` by cosine similarity.
    /// Returns `(id, similarity_score)` pairs, highest-score first.
    fn search(&self, query: &RawVector, top_k: usize) -> Result<Vec<(u64, f32)>, MiraError>;

    /// Remove the vector for a memory that has been deleted.
    fn delete(&self, id: u64) -> Result<(), MiraError>;

    /// Load all stored vectors on startup to seed the in-memory VectorStore.
    fn load_all(&self) -> Result<Vec<(u64, RawVector, String)>, MiraError>;
}

/// SQLite-backed vector store. Vectors are stored as raw little-endian f32 BLOBs.
///
/// The DB is the source of truth; a full in-memory mirror (`cache`) is held so
/// `search` scores against RAM instead of re-`SELECT`ing + blob-deserialising
/// every vector on each query. The cache is seeded once at construction and kept
/// consistent on every `upsert`/`delete` (the sole write paths).
pub struct SqliteVectorBackend {
    conn:  std::sync::Mutex<rusqlite::Connection>,
    cache: std::sync::RwLock<std::collections::HashMap<u64, (RawVector, String)>>,
}

impl SqliteVectorBackend {
    pub fn new(db_path: &std::path::Path) -> Result<Self, MiraError> {
        let conn = rusqlite::Connection::open(db_path)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_vectors (
                id INTEGER PRIMARY KEY,
                category TEXT NOT NULL DEFAULT '',
                vector BLOB NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_vectors_id ON memory_vectors(id);",
        )
        .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let backend = Self {
            conn:  std::sync::Mutex::new(conn),
            cache: std::sync::RwLock::new(std::collections::HashMap::new()),
        };
        // Seed the cache from the DB (one read at startup).
        let initial = backend.load_all()?;
        let mut cache = backend.cache.write().unwrap();
        for (id, vec, cat) in initial {
            cache.insert(id, (vec, cat));
        }
        drop(cache);
        Ok(backend)
    }

    fn vec_to_blob(v: &RawVector) -> Vec<u8> {
        v.iter().flat_map(|f| f.to_le_bytes()).collect()
    }

    fn blob_to_vec(b: &[u8]) -> RawVector {
        b.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }
}

impl VectorStoreBackend for SqliteVectorBackend {
    fn upsert(&self, id: u64, vector: &RawVector, category: &str) -> Result<(), MiraError> {
        let blob = Self::vec_to_blob(vector);
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO memory_vectors (id, category, vector) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET vector = excluded.vector, category = excluded.category",
                rusqlite::params![id, category, blob],
            )
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        }
        // Mirror into the cache only after the DB write succeeds.
        self.cache.write().unwrap().insert(id, (vector.clone(), category.to_string()));
        Ok(())
    }

    fn search(&self, query: &RawVector, top_k: usize) -> Result<Vec<(u64, f32)>, MiraError> {
        // Brute-force cosine over the in-memory mirror — no per-query DB read.
        // (Still O(n) scoring; an ANN index would be the next step at large scale.)
        let cache = self.cache.read().unwrap();
        let mut results: Vec<(u64, f32)> = cache
            .iter()
            .map(|(id, (vec, _))| (*id, crate::memory::semantic::cosine_similarity(query, vec)))
            .collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_k);
        Ok(results)
    }

    fn delete(&self, id: u64) -> Result<(), MiraError> {
        {
            let conn = self.conn.lock().unwrap();
            conn.execute("DELETE FROM memory_vectors WHERE id = ?1", rusqlite::params![id])
                .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        }
        self.cache.write().unwrap().remove(&id);
        Ok(())
    }

    fn load_all(&self) -> Result<Vec<(u64, RawVector, String)>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, category, vector FROM memory_vectors")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                let id: u64 = row.get(0)?;
                let cat: String = row.get(1)?;
                let blob: Vec<u8> = row.get(2)?;
                Ok((id, blob, cat))
            })
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?
            .filter_map(|r| r.ok())
            .map(|(id, blob, cat)| (id, Self::blob_to_vec(&blob), cat))
            .collect();
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_backend() -> SqliteVectorBackend {
        SqliteVectorBackend::new(std::path::Path::new(":memory:")).unwrap()
    }

    #[test]
    fn test_upsert_and_load() {
        let b = make_backend();
        b.upsert(1, &vec![1.0, 0.0, 0.0], "fact").unwrap();
        b.upsert(2, &vec![0.0, 1.0, 0.0], "preference").unwrap();
        let all = b.load_all().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_upsert_overwrite() {
        let b = make_backend();
        b.upsert(1, &vec![1.0, 0.0, 0.0], "fact").unwrap();
        b.upsert(1, &vec![0.5, 0.5, 0.0], "preference").unwrap();
        let all = b.load_all().unwrap();
        assert_eq!(all.len(), 1);
        assert!((all[0].1[0] - 0.5).abs() < 1e-6);
        assert_eq!(all[0].2, "preference");
    }

    #[test]
    fn test_delete() {
        let b = make_backend();
        b.upsert(1, &vec![1.0, 0.0, 0.0], "fact").unwrap();
        b.delete(1).unwrap();
        assert!(b.load_all().unwrap().is_empty());
    }

    // The in-memory cache stays consistent with mutations — search reflects an
    // overwrite and a delete (would fail if search read a stale cache).
    #[test]
    fn search_reflects_overwrite_and_delete() {
        let b = make_backend();
        b.upsert(1, &vec![1.0, 0.0, 0.0], "fact").unwrap();
        b.upsert(2, &vec![0.0, 1.0, 0.0], "fact").unwrap();
        // Overwrite id 1 to point along the y-axis.
        b.upsert(1, &vec![0.0, 1.0, 0.0], "fact").unwrap();
        let top = b.search(&vec![0.0, 1.0, 0.0], 1).unwrap();
        assert_eq!(top.len(), 1);
        assert!(top[0].1 > 0.99, "overwritten vector should score ~1.0");
        // Delete id 2 → it must vanish from search results.
        b.delete(2).unwrap();
        let ids: Vec<u64> = b.search(&vec![0.0, 1.0, 0.0], 5).unwrap()
            .into_iter().map(|(id, _)| id).collect();
        assert!(!ids.contains(&2), "deleted id must not appear in search");
        assert!(ids.contains(&1));
    }

    // A fresh backend over an existing DB seeds its cache from disk, so search
    // works immediately without any upsert this process.
    #[test]
    fn cache_seeds_from_existing_db_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vec.db");
        {
            let b1 = SqliteVectorBackend::new(&path).unwrap();
            b1.upsert(7, &vec![1.0, 0.0], "fact").unwrap();
            b1.upsert(8, &vec![0.0, 1.0], "fact").unwrap();
        }
        let b2 = SqliteVectorBackend::new(&path).unwrap();
        let results = b2.search(&vec![1.0, 0.0], 1).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 7, "cache should be seeded from the DB");
    }

    #[test]
    fn test_search_returns_closest() {
        let b = make_backend();
        b.upsert(1, &vec![1.0, 0.0, 0.0], "fact").unwrap();
        b.upsert(2, &vec![0.0, 1.0, 0.0], "preference").unwrap();
        let results = b.search(&vec![1.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1);
        assert!((results[0].1 - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_vec_blob_roundtrip() {
        let v = vec![0.1f32, 0.2, 0.3, -0.4];
        let blob = SqliteVectorBackend::vec_to_blob(&v);
        let back = SqliteVectorBackend::blob_to_vec(&blob);
        for (a, b) in v.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1e-7);
        }
    }
}
