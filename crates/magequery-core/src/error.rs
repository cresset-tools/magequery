//! Two kinds of failure, deliberately separated:
//!
//! * [`Error`] — the operation cannot produce a meaningful answer (no Magento root,
//!   unreadable `config.php`, unknown class). Returned as `Err`.
//! * [`Diagnostic`] — a non-fatal problem with one input (a malformed `di.xml` node in
//!   one of 700 modules). Collected on the index and surfaced via `Magento::diagnostics`,
//!   so a single broken file never blinds the tool to the other 699.
//!
//! This split is what lets magequery run usefully against the half-broken codebases
//! people most want to inspect.

use std::path::PathBuf;

use crate::ids::ClassName;
use crate::source::Source;

pub type Result<T> = std::result::Result<T, Error>;

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No `app/etc/config.php` (and friends) under the given root.
    #[error("no Magento 2 installation found at {path}")]
    NotMagentoRoot { path: PathBuf },

    /// A file that must be readable for the index to exist at all could not be read.
    #[error("reading {file}")]
    Io {
        file: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A required top-level file (e.g. `config.php`) was structurally unparseable.
    /// Per-module parse failures are [`Diagnostic`]s instead, never this.
    #[error("parsing {file}: {detail}")]
    Parse { file: PathBuf, detail: String },

    /// The requested class/interface is not declared anywhere we can see.
    #[error("class not found: {0}")]
    ClassNotFound(ClassName),

    /// The class exists in config but no PSR-4 autoload mapping resolves it to a file,
    /// so its inheritance chain (and thus inherited plugins) can't be determined.
    #[error("cannot resolve a source file for {class} (no PSR-4 autoload match)")]
    Unautoloadable { class: ClassName },

    /// A database operation failed (connection, query, or the `db` feature is off).
    #[error("database: {0}")]
    Db(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Error,
}

/// A non-fatal problem found while building the index.
#[non_exhaustive]
#[derive(Debug, Clone)]
#[derive(serde::Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    /// The offending file/line, when known.
    pub source: Option<Source>,
}

impl Diagnostic {
    pub fn warning(message: impl Into<String>, source: Option<Source>) -> Self {
        Self { severity: Severity::Warning, message: message.into(), source }
    }
    pub fn error(message: impl Into<String>, source: Option<Source>) -> Self {
        Self { severity: Severity::Error, message: message.into(), source }
    }
}
