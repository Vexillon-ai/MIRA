// SPDX-License-Identifier: AGPL-3.0-or-later

// src/log_rotate.rs
//! Size-based log-file rotation with a fixed active path.
//!
//! The active file always lives at the configured `logging.file` path (e.g.
//! `~/.mira/logs/mira.log`) so `/api/logs/stream` can tail a stable name
//! rather than chasing a date-suffixed one. When the active file grows past
//! `max_file_size_mb`, it is rolled to `mira.log.1`, existing archives shift up
//! (`.1`→`.2`, …), and any archive beyond `max_files - 1` is dropped — keeping
//! at most `max_files` files total (active + archives).
//!
//! Rotation is size-triggered at the *start* of a write. `tracing_appender`'s
//! non-blocking worker hands us one formatted event per write, so rotating
//! before the write means a log line is never split across two files.
//!
//! This writer lives behind `tracing_appender::non_blocking`, whose dedicated
//! worker thread is the sole caller, so it needs no internal locking.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub struct RotatingWriter {
    path:      PathBuf,
    file:      File,
    written:   u64,
    /// Rotate when the active file would exceed this. `0` disables rotation
    /// (a single unbounded file — the legacy `rolling::never` behaviour).
    max_bytes: u64,
    /// Total files retained *including* the active one. Clamped to `>= 1`.
    max_files: u32,
}

impl RotatingWriter {
    pub fn new(path: &Path, max_file_size_mb: u32, max_files: u32) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).ok();
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        // Seed `written` from the file already on disk so an existing near-full
        // log rolls promptly on the next write rather than only bounding growth
        // that happens after this process started.
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            file,
            written,
            max_bytes: (max_file_size_mb as u64).saturating_mul(1024 * 1024),
            max_files: max_files.max(1),
        })
    }

    /// `mira.log` → `mira.log.<n>` (preserves the full base name incl. its
    /// extension, so `mira.log.1` not `mira.1`).
    fn archive_path(&self, n: u32) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    }

    /// Roll the active file out and open a fresh one. Best-effort: on any fs
    /// error we report to stderr and keep the current handle, so logging never
    /// breaks — at worst the active file exceeds the cap until the next write.
    fn rotate(&mut self) {
        let _ = self.file.flush();

        // No archives retained (`max_files == 1`): just truncate in place.
        if self.max_files <= 1 {
            match OpenOptions::new().create(true).write(true).truncate(true).open(&self.path) {
                Ok(f)  => { self.file = f; self.written = 0; }
                Err(e) => eprintln!("log_rotate: truncate {:?} failed: {e}", self.path),
            }
            return;
        }

        // Drop the oldest archive, then shift the rest up by one.
        let oldest = self.max_files - 1; // highest archive index we keep
        let _ = fs::remove_file(self.archive_path(oldest));
        for n in (1..oldest).rev() {
            let _ = fs::rename(self.archive_path(n), self.archive_path(n + 1));
        }
        // active → .1
        if let Err(e) = fs::rename(&self.path, self.archive_path(1)) {
            eprintln!("log_rotate: roll {:?} -> .1 failed: {e}", self.path);
            return; // keep the already-open handle rather than losing logs
        }
        match OpenOptions::new().create(true).append(true).open(&self.path) {
            Ok(f)  => { self.file = f; self.written = 0; }
            Err(e) => eprintln!("log_rotate: reopen {:?} failed: {e}", self.path),
        }
    }
}

impl Write for RotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Rotate *before* writing this event so lines stay whole. Guard on
        // `written > 0` so a single event larger than the cap can't spin
        // rotating an empty file forever.
        if self.max_bytes > 0
            && self.written > 0
            && self.written.saturating_add(buf.len() as u64) > self.max_bytes
        {
            self.rotate();
        }
        // Write the whole event so the non-blocking worker never re-calls with a
        // remainder (which could otherwise straddle a rotation).
        self.file.write_all(buf)?;
        self.written += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(p: &Path) -> String {
        fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn rolls_active_to_dot_one_when_over_cap() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("mira.log");
        // 1 byte cap so the second write triggers a roll (first write is exempt
        // by the `written > 0` guard).
        let mut w = RotatingWriter::new(&path, 0, 3).unwrap();
        w.max_bytes = 1;
        w.write_all(b"first\n").unwrap();
        w.write_all(b"second\n").unwrap();
        w.flush().unwrap();

        assert_eq!(read(&path), "second\n", "active holds the newest line");
        assert_eq!(read(&path.with_extension("log.1")), "first\n", ".1 holds the rolled line");
    }

    #[test]
    fn retains_at_most_max_files_total() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("mira.log");
        let mut w = RotatingWriter::new(&path, 0, 3).unwrap(); // active + .1 + .2
        w.max_bytes = 1;
        for i in 0..5 {
            w.write_all(format!("line{i}\n").as_bytes()).unwrap();
        }
        w.flush().unwrap();

        // 3 files kept: active(line4) .1(line3) .2(line2); line0/line1 dropped.
        assert_eq!(read(&path),                          "line4\n");
        assert_eq!(read(&path.with_extension("log.1")),  "line3\n");
        assert_eq!(read(&path.with_extension("log.2")),  "line2\n");
        assert!(!path.with_extension("log.3").exists(), "no fourth file when max_files=3");
    }

    #[test]
    fn max_files_one_truncates_in_place() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("mira.log");
        let mut w = RotatingWriter::new(&path, 0, 1).unwrap();
        w.max_bytes = 1;
        w.write_all(b"old\n").unwrap();
        w.write_all(b"new\n").unwrap();
        w.flush().unwrap();

        assert_eq!(read(&path), "new\n", "single-file mode keeps only the latest");
        assert!(!path.with_extension("log.1").exists(), "no archive when max_files=1");
    }

    #[test]
    fn zero_max_bytes_never_rotates() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("mira.log");
        let mut w = RotatingWriter::new(&path, 0, 5).unwrap(); // max_bytes stays 0
        for i in 0..100 {
            w.write_all(format!("l{i}\n").as_bytes()).unwrap();
        }
        w.flush().unwrap();
        assert!(!path.with_extension("log.1").exists(), "rotation disabled → single file");
        assert!(read(&path).contains("l99"));
    }

    #[test]
    fn seeds_written_from_existing_file() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("mira.log");
        fs::write(&path, "already here\n").unwrap(); // 13 bytes on disk
        let mut w = RotatingWriter::new(&path, 0, 3).unwrap();
        w.max_bytes = 5; // below the pre-existing size
        // First write sees written=13 > 0 and 13+.. > 5 → rotates immediately,
        // preserving the pre-existing content in .1.
        w.write_all(b"new\n").unwrap();
        w.flush().unwrap();
        assert_eq!(read(&path.with_extension("log.1")), "already here\n");
        assert_eq!(read(&path), "new\n");
    }
}
