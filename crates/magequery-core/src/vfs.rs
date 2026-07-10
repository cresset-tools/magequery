//! The unsaved-buffer overlay: content reads go through here so an editor frontend can
//! analyze what's in its buffers rather than what's on disk.
//!
//! Scope is deliberate: the overlay overrides the **content** of files; discovery stays
//! on the real filesystem (directory walks, existence probes, PSR-4 candidate `stat`s).
//! A never-saved new file is therefore invisible until saved, and deleting a file from
//! the overlay is not a concept — both are editor states that resolve on save. Composer
//! metadata (`installed.json`, `composer.json` autoload) also stays on disk: its content
//! is only meaningful after `composer install`, so buffer edits to it are noise.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub(crate) struct Vfs {
    overlay: HashMap<PathBuf, String>,
}

impl Vfs {
    pub fn new(overlay: HashMap<PathBuf, String>) -> Self {
        Self { overlay }
    }

    /// The file's content: the overlay's version when the path is an open buffer, disk
    /// otherwise. Paths are compared verbatim — callers construct them from the same
    /// root the overlay's keys derive from (the LSP inserts each buffer under both its
    /// URI path and the canonicalized form to absorb symlink spellings).
    pub fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        if let Some(text) = self.overlay.get(path) {
            return Ok(text.clone());
        }
        std::fs::read_to_string(path)
    }
}
