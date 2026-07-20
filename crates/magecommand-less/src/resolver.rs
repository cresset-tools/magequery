//! The import/Magento boundary (plan §7, §9.5).
//!
//! [`ImportResolver`] is the *entire* FS/Magento boundary — the compiler never
//! touches the filesystem or Magento layout. Orchestration implements it
//! (theme fallback, `Vendor_Module::path` resolution, `//@magento_import`
//! expansion); pure/non-Magento callers get the default no-op `magento_import`.

use std::sync::Arc;

/// Provenance for one source file (mirrors less.js `fileInfo`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileInfo {
    /// The file's path/name.
    pub filename: String,
    /// `rootpath` in effect for urls generated from this file.
    pub rootpath: String,
    /// Directory of `filename` (with trailing separator).
    pub current_directory: String,
    /// Directory of the entry point.
    pub entry_path: String,
    /// The entry-point filename.
    pub root_filename: String,
}

/// `@import` options parsed from the comma list (plan §2.9).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportOptions {
    /// `(reference)` — imported rules are invisible unless reached via extend/mixin.
    pub reference: bool,
    /// `(inline)` — splice the file's bytes verbatim, unparsed.
    pub inline: bool,
    /// `Some(true)` = force `css`, `Some(false)` = force `less`, `None` = by extension.
    pub css: Option<bool>,
    /// `(once)` (default) — dedup repeated imports of the same file.
    pub once: bool,
    /// `(multiple)` — allow repeats.
    pub multiple: bool,
    /// `(optional)` — a missing file is a diagnostic, not an error.
    pub optional: bool,
    /// `layer(name)` (4.x) — cascade layer for the import.
    pub layer: Option<String>,
}

/// A request to resolve one `@import` (plan §9.5).
#[derive(Debug, Clone)]
pub struct ImportRequest {
    /// The (interpolation-resolved) import path.
    pub path: String,
    /// The importing file.
    pub from: FileInfo,
    /// Parsed import options.
    pub options: ImportOptions,
}

/// What a resolver hands back for an import (plan §9.5). The `Ast` payload lets
/// the engine memoize `content_hash → Arc<Node>` so `lib/*` parses once.
pub enum ImportPayload {
    /// A pre-parsed, shareable AST.
    Ast(Arc<crate::ast::Node>),
    /// Raw LESS source to parse.
    Less(Arc<str>),
    /// A CSS file, re-emitted as a literal `@import` (with path rewrite).
    Css(Arc<str>),
    /// `(inline)` bytes to splice verbatim.
    Inline(Arc<str>),
}

impl std::fmt::Debug for ImportPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportPayload::Ast(_) => f.write_str("ImportPayload::Ast(..)"),
            ImportPayload::Less(_) => f.write_str("ImportPayload::Less(..)"),
            ImportPayload::Css(_) => f.write_str("ImportPayload::Css(..)"),
            ImportPayload::Inline(_) => f.write_str("ImportPayload::Inline(..)"),
        }
    }
}

/// A resolved import: where it lives + its payload (plan §9.5).
#[derive(Debug)]
pub struct ResolvedImport {
    /// Provenance for the resolved file.
    pub file: FileInfo,
    /// The content.
    pub payload: ImportPayload,
}

/// One concrete `@import` a `//@magento_import` directive expands to (plan §7.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagentoImportEntry {
    /// The concrete, already theme-fallback-resolved import path, e.g.
    /// `Vendor_Module::css/source/_module.less`.
    pub import_path: String,
    /// Whether it should be imported `(reference)`.
    pub reference: bool,
}

/// Why an import could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImportError {
    /// No file matched the request path.
    #[error("'{0}' wasn't found")]
    NotFound(String),
    /// I/O failure reading a resolved file.
    #[error("error reading '{path}': {message}")]
    Io { path: String, message: String },
    /// Any other resolver-defined failure.
    #[error("{0}")]
    Other(String),
}

/// The compiler's only filesystem/Magento boundary (plan §9.5).
pub trait ImportResolver: Sync {
    /// Resolve one `@import` to its content.
    fn resolve(&self, req: &ImportRequest) -> Result<ResolvedImport, ImportError>;

    /// Expand a `//@magento_import` directive to its concrete, load-order-sorted
    /// `@import` list (plan §7.1/§7.3). Called **only** when `magento_mode` is
    /// set and the lexer emitted a `MagentoImportDirective`. The default no-op
    /// keeps non-Magento callers pure.
    fn magento_import(
        &self,
        path: &str,
        reference: bool,
        from: &FileInfo,
    ) -> Result<Vec<MagentoImportEntry>, ImportError> {
        let _ = (path, reference, from);
        Ok(Vec::new())
    }

    /// Read an asset file's raw bytes for the resource functions (`data-uri`,
    /// `image-size` — plan §2.7/§C-assets). `path` is as written in the LESS
    /// source (possibly relative), `current_directory` the requesting file's
    /// directory. The default `None` makes those functions fall back to plain
    /// `url(path)` output, keeping non-filesystem callers pure.
    fn load_binary(&self, path: &str, current_directory: &str) -> Option<Vec<u8>> {
        let _ = (path, current_directory);
        None
    }
}

/// A resolver that fails every `resolve` — for pure passthrough / no-import
/// callers and tests.
pub struct NoopResolver;

impl ImportResolver for NoopResolver {
    fn resolve(&self, req: &ImportRequest) -> Result<ResolvedImport, ImportError> {
        Err(ImportError::NotFound(req.path.clone()))
    }
}
