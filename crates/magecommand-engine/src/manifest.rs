//! Output manifest for incremental compile (Win 1 of the CAS design — see
//! `docs/incremental-compile.md`).
//!
//! The compile is a pure function of the source tree, so its generated files are
//! content-addressable. After a compile we record every output file's content
//! hash in `generated/.mqcache/manifest.json`; on the next `--incremental` run
//! we hash the freshly-generated (in-memory) content and diff against the
//! manifest — writing only the files whose bytes actually changed, deleting the
//! ones that disappeared, and skipping the rest. On APFS, where a full write of
//! ~10k files costs ~1.8s, an unchanged re-compile then writes zero files.
//!
//! The manifest is TRUSTED as the record of on-disk state (we do not re-read the
//! output tree). `--force` is always the full, manifest-free reference path; a
//! missing/mismatched manifest falls back to a full write. Correctness rule: the
//! in-memory `generate_code` output is the single source of truth — incremental
//! only ever skips writing bytes already known to be identical, so its result is
//! byte-for-byte a full compile's.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use magequery_core::Magento;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Bumped when the manifest format changes — an older manifest is then ignored
/// (treated as absent) so a format change can never be mis-read.
const MANIFEST_VERSION: u32 = 1;

/// The compile tool version, baked in so a magecommand upgrade (whose generation
/// logic may differ) invalidates a stale manifest rather than trusting it.
const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `generated/.mqcache/manifest.json` — the recorded content hashes of the last
/// compile's `generated/code` tree, plus the guards that decide whether it may
/// be trusted for an incremental run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version ([`MANIFEST_VERSION`]).
    pub version: u32,
    /// magecommand version that wrote it.
    pub tool_version: String,
    /// Absolute Magento root (`BP`) it was built for — baked into some generated
    /// output, so a manifest from another root must not be trusted.
    pub bp: String,
    /// The stat-fingerprint digest of the compile INPUTS at the time this was
    /// written ([`FingerprintMode::Stat`]). `--incremental` recomputes it and,
    /// on a match, skips the whole compile — the output is already current.
    /// `None` on manifests written before Win 2 (treated as "unknown" → no
    /// short-circuit, a safe recompile).
    #[serde(default)]
    pub inputs_digest: Option<String>,
    /// `rel_path` (as written under `generated/code`, forward-slashed) → content
    /// hash. `BTreeMap` for a stable, diffable on-disk order.
    pub files: BTreeMap<String, String>,
}

/// How [`input_digest`] fingerprints each input file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FingerprintMode {
    /// `mtime` + `size` — no file reads, so it's fast (the interactive
    /// `--incremental` short-circuit). mtime is unreliable after a fresh
    /// `git checkout`, so this is for the local edit loop, not a CI key.
    Stat,
    /// blake3 of each file's bytes — checkout-independent, so it's the CI cache
    /// key, at the cost of reading every input.
    Content,
}

/// A digest over the whole compile INPUT set (see
/// [`crate::definitions::compile_input_files`]), plus `BP`, the tool version,
/// and the enabled-module set/order. Two inputs producing the same digest yield
/// the same `generated/` — so an unchanged digest means the last output is still
/// valid (skip the compile), and it is a sound CI cache key. Files are
/// fingerprinted in parallel; the sorted input list keeps the fold order stable.
pub fn input_digest(magento: &Magento, root: &Path, mode: FingerprintMode) -> String {
    use rayon::prelude::*;
    let files = crate::definitions::compile_input_files(magento, root);
    let per_file: Vec<[u8; 32]> = files
        .par_iter()
        .map(|path| {
            let mut h = blake3::Hasher::new();
            let rel = path.strip_prefix(root).unwrap_or(path);
            h.update(rel.to_string_lossy().as_bytes());
            h.update(b"\0");
            match mode {
                FingerprintMode::Stat => match std::fs::metadata(path) {
                    Ok(meta) => {
                        h.update(&meta.len().to_le_bytes());
                        let nanos = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_nanos())
                            .unwrap_or(0);
                        h.update(&nanos.to_le_bytes());
                    }
                    Err(_) => {
                        h.update(b"\x01missing");
                    }
                },
                FingerprintMode::Content => match std::fs::read(path) {
                    Ok(bytes) => {
                        h.update(&bytes);
                    }
                    Err(_) => {
                        h.update(b"\x01missing");
                    }
                },
            }
            *h.finalize().as_bytes()
        })
        .collect();

    let mut top = blake3::Hasher::new();
    top.update(TOOL_VERSION.as_bytes());
    top.update(b"\0");
    top.update(root.to_string_lossy().as_bytes());
    top.update(b"\0");
    for module in magento.modules() {
        if module.enabled {
            top.update(module.name.as_str().as_bytes());
            top.update(b"\0");
        }
    }
    for fp in &per_file {
        top.update(fp);
    }
    top.finalize().to_hex().to_string()
}

impl Manifest {
    /// Path of the manifest file under `root`.
    fn path(root: &Path) -> PathBuf {
        root.join("generated/.mqcache/manifest.json")
    }

    /// Hash one generated file's bytes → lowercase hex. blake3: fast and
    /// collision-resistant (the hash gates whether a write is skipped, so a
    /// collision would mean a stale file — cryptographic strength earns its
    /// keep here).
    pub fn hash(content: &[u8]) -> String {
        blake3::hash(content).to_hex().to_string()
    }

    /// Load the manifest for `root`, returning `None` (never an error) when it
    /// is absent, unreadable, malformed, a different format version, from
    /// another tool version, or built for a different `BP`. Any of those means
    /// "cannot trust it" → the caller does a full write. `bp` is the current
    /// absolute root.
    pub fn load(root: &Path, bp: &str) -> Option<Manifest> {
        let bytes = std::fs::read(Self::path(root)).ok()?;
        let manifest: Manifest = serde_json::from_slice(&bytes).ok()?;
        if manifest.version != MANIFEST_VERSION
            || manifest.tool_version != TOOL_VERSION
            || manifest.bp != bp
        {
            return None;
        }
        Some(manifest)
    }

    /// Build a fresh manifest from the just-written file set (`rel_path` → hash).
    /// `inputs_digest` starts empty; the caller sets it (it needs the `Magento`
    /// handle to fingerprint the inputs) before [`save`](Self::save).
    pub fn from_hashes(bp: &str, files: BTreeMap<String, String>) -> Manifest {
        Manifest {
            version: MANIFEST_VERSION,
            tool_version: TOOL_VERSION.to_owned(),
            bp: bp.to_owned(),
            inputs_digest: None,
            files,
        }
    }

    /// Persist the manifest under `root/generated/.mqcache/`, creating the dir.
    /// Atomic (temp + rename) so a crash can't leave a half-written manifest
    /// that would later parse as garbage.
    pub fn save(&self, root: &Path) -> Result<()> {
        let path = Self::path(root);
        let dir = path.parent().expect("manifest path has a parent");
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::io(&path, std::io::Error::other(e)))?;
        let tmp = dir.join(".manifest.json.tmp");
        std::fs::write(&tmp, &json).map_err(|e| Error::io(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::io(&path, e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_guards() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut files = BTreeMap::new();
        files.insert("Foo/Bar.php".to_owned(), Manifest::hash(b"<?php // a"));
        let m = Manifest::from_hashes("/srv/shop", files);
        m.save(root).unwrap();

        // Same bp → loads.
        let loaded = Manifest::load(root, "/srv/shop").expect("loads");
        assert_eq!(loaded.files.len(), 1);
        assert_eq!(
            loaded.files.get("Foo/Bar.php").unwrap(),
            &Manifest::hash(b"<?php // a")
        );

        // Different bp → not trusted.
        assert!(Manifest::load(root, "/other/root").is_none());
        // Missing → None, not an error.
        assert!(Manifest::load(dir.path().join("nope").as_path(), "/srv/shop").is_none());
    }

    #[test]
    fn hash_is_content_sensitive() {
        assert_ne!(Manifest::hash(b"a"), Manifest::hash(b"b"));
        assert_eq!(Manifest::hash(b"same"), Manifest::hash(b"same"));
    }
}
