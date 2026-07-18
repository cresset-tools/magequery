//! Compile engine for magecommand.
//!
//! Computes and returns owned, structured data; never prints, never exits,
//! never reads ambient state (magequery-core's discipline). The binary crate
//! renders.
//!
//! M0 scope: the archive-compare harness — the oracle every later milestone
//! lands green against. Ground truth is a real `bin/magento setup:di:compile`
//! output archived as `generated/_code` + `generated/_metadata`.

use std::path::PathBuf;

pub mod areaconfig;
pub mod arguments;
pub mod build;
pub mod classify;
pub mod codegen;
pub mod compare;
pub mod definitions;
pub mod fused;
pub mod interception;
pub mod interceptor;
pub mod laminas;
pub mod manifest;
pub mod metadata;
pub mod proxy;
pub mod reflect;
pub mod pluginlist;
pub mod phpexport;

pub use classify::{classify, obfuscation_blocked_classes, residual_report, Classified, ClassifyCtx, KnownGroup, KnownKind};
pub use compare::{compare_dirs, CompareReport};

/// Errors the engine can return.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The archive (ground-truth) directory does not exist.
    #[error("archive directory not found: {0}")]
    ArchiveMissing(PathBuf),
    /// Writing would clobber an existing file and `force` wasn't given.
    #[error("refusing to overwrite {0} (pass --force)")]
    WouldOverwrite(PathBuf),
    /// An I/O failure, tagged with the path it happened on.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
