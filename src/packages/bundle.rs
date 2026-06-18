// SPDX-License-Identifier: AGPL-3.0-or-later

//! Reading a `.mirapkg` bundle (tar.gz) into a parsed, validated manifest +
//! in-memory file list. Mirrors the skill `.miraskill` archive handling
//! (size caps, no symlinks, no path traversal, single top-level dir) so the
//! safety posture is identical.
//!
//! Parsing is side-effect-free — nothing touches disk — so preview/verify can
//! reject a hostile or malformed bundle before any install begins.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use tar::Archive;

use super::manifest::PackageManifest;

/// Caps matched to the skills archive handler.
const MAX_ARCHIVE_BYTES: usize = 10 * 1024 * 1024; // 10 MB compressed
const MAX_FILE_BYTES: u64 = 1 * 1024 * 1024; //        1 MB per file
const MAX_ENTRIES: usize = 200;

/// The manifest file inside a bundle's top-level directory.
pub const MANIFEST_NAME: &str = "package.json";

/// Parsed contents of a `.mirapkg`. Files are kept in memory until a caller
/// decides whether to write them out.
#[derive(Debug)]
pub struct ParsedBundle {
    pub manifest: PackageManifest,
    /// (relative path, bytes) for every regular file in the archive,
    /// including `<id>/package.json`.
    pub files: Vec<(PathBuf, Vec<u8>)>,
    pub total_bytes: u64,
    /// The single top-level directory (equals the manifest id).
    pub top_dir: String,
}

/// Decode the gzip+tar, validate path safety + caps, parse + validate the
/// manifest, and return the in-memory file list. Does NOT verify the
/// signature — callers run `verify_package` with a trust store separately.
pub fn parse_bundle(bytes: &[u8]) -> Result<ParsedBundle, String> {
    if bytes.len() > MAX_ARCHIVE_BYTES {
        return Err(format!(
            "bundle is {} bytes, max allowed is {MAX_ARCHIVE_BYTES}",
            bytes.len(),
        ));
    }

    let gz = GzDecoder::new(bytes);
    let mut tar = Archive::new(gz);

    let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut top_dir: Option<String> = None;
    let mut total_bytes: u64 = 0;

    for entry_res in tar.entries().map_err(|e| format!("not a valid tar.gz bundle: {e}"))? {
        if files.len() >= MAX_ENTRIES {
            return Err(format!("bundle exceeds entry limit of {MAX_ENTRIES}"));
        }
        let mut entry = entry_res.map_err(|e| format!("malformed tar entry: {e}"))?;

        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            return Err("symlinks and hard links are not allowed in package bundles".into());
        }

        let path = entry
            .path()
            .map_err(|e| format!("entry has invalid path: {e}"))?
            .into_owned();

        // Reject path traversal and absolute paths.
        if path.components().any(|c| {
            matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_))
        }) {
            return Err(format!("entry {path:?} contains an illegal path component (.. or absolute)"));
        }

        // Establish the single top-level directory.
        let mut comps = path.components();
        let first = match comps.next() {
            Some(Component::Normal(s)) => s.to_string_lossy().to_string(),
            None => continue,
            _ => return Err(format!("entry {path:?} has an illegal first segment")),
        };
        match top_dir.as_deref() {
            None => top_dir = Some(first.clone()),
            Some(known) if known == first => {}
            Some(known) => {
                return Err(format!(
                    "bundle must contain a single top-level directory; saw {known:?} and {first:?}",
                ))
            }
        }

        if entry_type.is_dir() {
            continue;
        }
        if !entry_type.is_file() {
            return Err(format!("entry {path:?} has unsupported type {entry_type:?}"));
        }

        let size = entry.header().size().map_err(|e| format!("can't read entry size: {e}"))?;
        if size > MAX_FILE_BYTES {
            return Err(format!(
                "entry {path:?} is {size} bytes, exceeds per-file cap of {MAX_FILE_BYTES}",
            ));
        }
        total_bytes = total_bytes.saturating_add(size);

        let mut buf = Vec::with_capacity(size as usize);
        entry.read_to_end(&mut buf).map_err(|e| format!("can't read {path:?}: {e}"))?;
        files.push((path, buf));
    }

    let top_dir = top_dir.ok_or_else(|| "bundle is empty".to_string())?;

    // Find package.json at the top level.
    let manifest_relpath = Path::new(&top_dir).join(MANIFEST_NAME);
    let manifest_bytes = files
        .iter()
        .find(|(p, _)| p == &manifest_relpath)
        .map(|(_, b)| b.clone())
        .ok_or_else(|| format!("bundle missing {top_dir}/{MANIFEST_NAME}"))?;

    let manifest_text = std::str::from_utf8(&manifest_bytes)
        .map_err(|e| format!("{MANIFEST_NAME} is not valid UTF-8: {e}"))?;
    let manifest = PackageManifest::parse_json(manifest_text).map_err(|e| e.to_string())?;
    manifest.validate().map_err(|e| e.to_string())?;

    // The top-level directory must equal the package id (keeps bundles
    // self-describing and collision-free on disk).
    if manifest.id != top_dir {
        return Err(format!(
            "bundle top-level directory {top_dir:?} doesn't match manifest id {:?}",
            manifest.id,
        ));
    }

    Ok(ParsedBundle { manifest, files, total_bytes, top_dir })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;

    /// Build a tar.gz with the given (path, bytes) entries.
    fn make_targz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
        for (path, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, path, &data[..]).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    const MANIFEST: &str = r#"{
        "format": "1",
        "id": "com.example.nextcloud-mcp",
        "name": "Nextcloud MCP",
        "version": "1.0.0",
        "components": [ { "type": "mcp_server", "spec": { "command": "python3" } } ]
    }"#;

    #[test]
    fn parses_a_valid_bundle() {
        let gz = make_targz(&[("com.example.nextcloud-mcp/package.json", MANIFEST.as_bytes())]);
        let parsed = parse_bundle(&gz).unwrap();
        assert_eq!(parsed.manifest.id, "com.example.nextcloud-mcp");
        assert_eq!(parsed.top_dir, "com.example.nextcloud-mcp");
    }

    #[test]
    fn rejects_top_dir_mismatch() {
        let gz = make_targz(&[("wrong-dir/package.json", MANIFEST.as_bytes())]);
        let err = parse_bundle(&gz).unwrap_err();
        assert!(err.contains("doesn't match manifest id"));
    }

    #[test]
    fn rejects_missing_manifest() {
        let gz = make_targz(&[("com.example.nextcloud-mcp/readme.txt", b"hi")]);
        let err = parse_bundle(&gz).unwrap_err();
        assert!(err.contains("missing"));
    }

    #[test]
    fn rejects_multiple_top_level_dirs() {
        // (Path traversal / symlinks are rejected at runtime too, but the safe
        // tar Builder won't emit `..` or absolute paths, so we can't forge one
        // here — this exercises the single-top-dir guard, which we can build.)
        let gz = make_targz(&[
            ("com.example.nextcloud-mcp/package.json", MANIFEST.as_bytes()),
            ("other-dir/file.txt", b"x"),
        ]);
        let err = parse_bundle(&gz).unwrap_err();
        assert!(err.contains("single top-level directory"));
    }

    #[test]
    fn rejects_non_targz() {
        let err = parse_bundle(b"not a gzip stream at all").unwrap_err();
        assert!(err.contains("tar.gz") || err.contains("valid"));
    }
}
