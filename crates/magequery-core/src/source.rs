//! Provenance. Every fact magequery reports carries a [`Source`] so the answer is
//! always "...declared here: file:line", which is the whole point of the tool.

use std::path::PathBuf;

use crate::ids::{Area, ModuleName};

/// Where a declaration came from: which module, which file, which line, which area's
/// config namespace it was merged from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Source {
    pub module: ModuleName,
    pub file: PathBuf,
    /// 1-based line of the originating XML node. `0` means "known file, unknown line".
    pub line: u32,
    /// The config namespace this node was read from (`Global` for `etc/di.xml`,
    /// `Adminhtml` for `etc/adminhtml/di.xml`, etc.).
    pub area: Area,
}

impl Source {
    /// `path/to/file.xml:42` — the clickable form.
    pub fn location(&self) -> String {
        if self.line == 0 {
            self.file.display().to_string()
        } else {
            format!("{}:{}", self.file.display(), self.line)
        }
    }
}
