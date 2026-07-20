//! Theme LESS orchestration (plan §7.2/§7.3/§7.8, §10 ownership table): the
//! Magento side of the compiler/orchestration split. The compiler
//! (`magecommand-less`) never touches the filesystem or Magento layout; this
//! module owns everything Magento-shaped:
//!
//! - the **theme fallback chain** from `theme.xml` `<parent>` (generalized —
//!   any depth, third-party themes inheriting Luma inheriting blank; §E3);
//! - **`Vendor_Module::path`** resolution through the fallback (§7.2);
//! - the **`//@magento_import` expansion** (via the compiler's
//!   `ImportResolver::magento_import` callback, §7.1), ordered exactly like
//!   Magento's `Css\PreProcessor\File\Collector\Aggregated` — pinned by a
//!   live probe against a real install (see `magento_import` below);
//! - **entry-point assembly** for a `(theme, locale)` pair;
//! - **fault surfacing** naming the offending module/file (§7.5) — loud-fail
//!   by default, with an opt-in `skip_broken_modules` that drops a broken
//!   module's partial and re-splices.
//!
//! Everything is pure file inspection over a plain Magento source tree
//! (`lib/web`, theme dirs, module `view/` dirs) — **no PHP, no DB, no
//! bootstrap**. Module load order comes from `config.php` via
//! `magequery-core`'s module index; theme discovery reuses
//! `magequery_core::Magento::themes()`.
//!
//! ## The logical-path model (replaces Magento's materialization)
//!
//! Magento copies every source into `var/view_preprocessed` so relative
//! imports resolve in one merged tree. We keep the same semantics without
//! copying: every file is addressed by its **logical path** in that merged
//! tree — `css/source/_theme.less` (theme scope) or
//! `Vendor_Module/css/source/_module.less` (module scope) — and the resolver
//! maps logical → physical through the overlay at import time:
//!
//! - theme scope: child theme `web/`, …ancestors' `web/`, then `lib/web`;
//! - module scope: child theme `<Module>/web/`, …ancestors', then the
//!   module's `view/<area>/web/`, then `view/base/web/`.
//!
//! This is what makes cross-file fallback correct: blank's `_styles.less`
//! imports `source/_theme.less`, and when compiling Luma that must resolve to
//! **Luma's** `_theme.less` override even though the importing file is
//! blank's — logical resolution gives exactly that (physical-relative
//! resolution would not).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use magecommand_less::{
    FileInfo, ImportError, ImportPayload, ImportRequest, ImportResolver, LessOptions,
    MagentoImportEntry, ResolvedImport,
};

/// The standard theme entry points. The six LESS entries are blank/luma's
/// compiled set; `critical` is Luma's SEVENTH entry (verified against real
/// SCD output) — a pre-minified plain `css/critical.css` copied VERBATIM by
/// the deploy (no compile). Themes lacking an entry skip it in the default
/// run.
pub const ENTRY_POINTS: [&str; 7] = [
    "styles-m", "styles-l", "print", "email", "email-inline", "email-fonts", "critical",
];

/// One theme in the fallback chain.
#[derive(Debug, Clone)]
pub struct ThemeRef {
    /// Full id, e.g. `frontend/Magento/luma`.
    pub id: String,
    /// The theme's root directory (holds `theme.xml`, `web/`, module contexts).
    pub dir: PathBuf,
}

/// One enabled module, in `config.php` load order.
#[derive(Debug, Clone)]
pub struct ModuleRef {
    /// `Vendor_Module`.
    pub name: String,
    /// The module's root directory (holds `view/`).
    pub dir: PathBuf,
}

/// A LESS deploy failure, attributed as §7.5 requires: the offending module
/// and physical file when they can be determined.
#[derive(Debug)]
pub struct LessDeployError {
    /// The entry point being compiled (`css/styles-m.less`), when known.
    pub entry: Option<String>,
    /// The owning module of the failing file (a `Vendor_Module::` partial).
    pub module: Option<String>,
    /// The physical path of the failing file.
    pub file: Option<PathBuf>,
    /// The underlying message (the compiler's rendered error, or an
    /// orchestration-level description).
    pub message: String,
}

impl std::fmt::Display for LessDeployError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(entry) = &self.entry {
            write!(f, "entry {entry}: ")?;
        }
        if let Some(module) = &self.module {
            write!(f, "module {module}: ")?;
        }
        if let Some(file) = &self.file {
            write!(f, "file {}: ", file.display())?;
        }
        f.write_str(&self.message)
    }
}

impl std::error::Error for LessDeployError {}

/// Options for one deploy run.
#[derive(Debug, Clone, Default)]
pub struct LessDeployOptions {
    /// Drop a broken module's `//@magento_import` partial and re-splice
    /// instead of failing the entry point (§7.5). Default OFF: fail loudly.
    pub skip_broken_modules: bool,
}

/// A compiled entry point.
#[derive(Debug)]
pub struct CompiledEntry {
    /// Logical entry path (`css/styles-m.less`).
    pub entry: String,
    /// The physical file the entry resolved to (theme or ancestor).
    pub entry_file: PathBuf,
    /// The compiled CSS.
    pub css: String,
    /// Compiler warnings.
    pub warnings: Vec<String>,
    /// Modules dropped under `skip_broken_modules`, with the error that got
    /// each dropped. Empty on a clean compile.
    pub skipped_modules: Vec<(String, String)>,
}

/// Build the child-first theme fallback chain for `theme_id` from `theme.xml`
/// `<parent>` declarations (§7.8/§E3 — generalized, arbitrary depth).
/// `themes` is the discovered `(id, dir)` set (`magequery_core::Magento::
/// themes()`); `theme_id` accepts `Vendor/name` or `<area>/Vendor/name`.
pub fn theme_chain(
    area: &str,
    theme_id: &str,
    themes: &[(String, PathBuf)],
) -> Result<Vec<ThemeRef>, LessDeployError> {
    let full = |id: &str| {
        if id.starts_with(&format!("{area}/")) {
            id.to_string()
        } else {
            format!("{area}/{id}")
        }
    };
    let by_id: BTreeMap<&str, &PathBuf> =
        themes.iter().map(|(id, dir)| (id.as_str(), dir)).collect();
    let mut chain = Vec::new();
    let mut cur = full(theme_id);
    let mut seen = std::collections::BTreeSet::new();
    loop {
        if !seen.insert(cur.clone()) {
            return Err(LessDeployError {
                entry: None,
                module: None,
                file: None,
                message: format!("theme parent cycle at '{cur}'"),
            });
        }
        let Some(dir) = by_id.get(cur.as_str()) else {
            return Err(LessDeployError {
                entry: None,
                module: None,
                file: chain.last().map(|t: &ThemeRef| t.dir.join("theme.xml")),
                message: if chain.is_empty() {
                    format!("theme '{cur}' not found on disk")
                } else {
                    format!("parent theme '{cur}' not found on disk")
                },
            });
        };
        chain.push(ThemeRef {
            id: cur.clone(),
            dir: (*dir).clone(),
        });
        match theme_parent(dir) {
            Some(parent) => cur = full(&parent),
            None => break,
        }
    }
    Ok(chain)
}

/// Extract `<parent>Vendor/name</parent>` from a theme dir's `theme.xml`.
/// Tolerant text scan (XML comments stripped first) — `theme.xml` is tiny and
/// schema-fixed.
fn theme_parent(dir: &Path) -> Option<String> {
    let xml = std::fs::read_to_string(dir.join("theme.xml")).ok()?;
    let xml = strip_xml_comments(&xml);
    let start = xml.find("<parent>")? + "<parent>".len();
    let end = xml[start..].find("</parent>")? + start;
    let parent = xml[start..end].trim();
    (!parent.is_empty()).then(|| parent.to_string())
}

fn strip_xml_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("<!--") {
        out.push_str(&rest[..i]);
        match rest[i..].find("-->") {
            Some(j) => rest = &rest[i + j + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// The orchestrator for one `(area, theme)` pair: owns the fallback chain and
/// module set, resolves logical paths, and compiles entry points.
pub struct LessOrchestrator {
    area: String,
    /// Child-first fallback chain.
    chain: Vec<ThemeRef>,
    /// Enabled modules in `config.php` load order.
    modules: Vec<ModuleRef>,
    /// `<root>/lib/web`.
    lib_web: PathBuf,
}

impl LessOrchestrator {
    /// Build from raw parts (no `Magento` handle needed — tests and callers
    /// with their own discovery use this).
    pub fn new(
        root: &Path,
        area: &str,
        theme_id: &str,
        themes: &[(String, PathBuf)],
        modules: Vec<ModuleRef>,
    ) -> Result<Self, LessDeployError> {
        let chain = theme_chain(area, theme_id, themes)?;
        Ok(LessOrchestrator {
            area: area.to_string(),
            chain,
            modules,
            lib_web: root.join("lib").join("web"),
        })
    }

    /// Build from an open `magequery-core` handle: themes from
    /// `Magento::themes()`, enabled modules in `config.php` load order from
    /// `Magento::modules()`.
    pub fn from_magento(
        magento: &magequery_core::Magento,
        area: &str,
        theme_id: &str,
    ) -> Result<Self, LessDeployError> {
        let themes = magento.themes();
        let modules = magento
            .modules()
            .iter()
            .filter(|m| m.enabled)
            .map(|m| ModuleRef {
                name: m.name.to_string(),
                dir: m.path.clone(),
            })
            .collect();
        LessOrchestrator::new(magento.root(), area, theme_id, &themes, modules)
    }

    /// The child-first fallback chain (for reporting).
    pub fn chain(&self) -> &[ThemeRef] {
        &self.chain
    }

    /// Resolve a LOGICAL path to `(physical file, owning module)` through the
    /// fallback overlay. Module scope when the first segment looks like a
    /// `Vendor_Module` context; theme scope otherwise.
    fn probe(&self, logical: &str) -> Option<(PathBuf, Option<String>)> {
        if let Some((first, rest)) = logical.split_once('/') {
            if is_module_segment(first) {
                for t in &self.chain {
                    let p = t.dir.join(first).join("web").join(rest);
                    if p.is_file() {
                        return Some((p, Some(first.to_string())));
                    }
                }
                if let Some(m) = self.modules.iter().find(|m| m.name == first) {
                    let p = m.dir.join("view").join(&self.area).join("web").join(rest);
                    if p.is_file() {
                        return Some((p, Some(first.to_string())));
                    }
                    let p = m.dir.join("view").join("base").join("web").join(rest);
                    if p.is_file() {
                        return Some((p, Some(first.to_string())));
                    }
                }
                return None;
            }
        }
        for t in &self.chain {
            let p = t.dir.join("web").join(logical);
            if p.is_file() {
                return Some((p, None));
            }
        }
        let p = self.lib_web.join(logical);
        if p.is_file() {
            return Some((p, None));
        }
        None
    }

    /// The candidate physical locations for a logical path (fault reporting).
    fn candidates(&self, logical: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some((first, rest)) = logical.split_once('/') {
            if is_module_segment(first) {
                for t in &self.chain {
                    out.push(t.dir.join(first).join("web").join(rest));
                }
                if let Some(m) = self.modules.iter().find(|m| m.name == first) {
                    out.push(m.dir.join("view").join(&self.area).join("web").join(rest));
                    out.push(m.dir.join("view").join("base").join("web").join(rest));
                }
                return out;
            }
        }
        for t in &self.chain {
            out.push(t.dir.join("web").join(logical));
        }
        out.push(self.lib_web.join(logical));
        out
    }

    /// Compile one entry point (`"styles-m"` / `"styles-m.less"` /
    /// `"css/styles-m.less"`) for this theme. Locale does not influence the
    /// compilation itself (no i18n overlay is modeled — a known limitation;
    /// `web/i18n/<locale>` theme overrides are not consulted), it only picks
    /// the `pub/static` placement, which is the caller's concern.
    pub fn compile_entry(
        &self,
        entry: &str,
        opts: &LessDeployOptions,
    ) -> Result<CompiledEntry, LessDeployError> {
        let name = entry.strip_suffix(".less").unwrap_or(entry);
        let logical = if name.contains('/') {
            format!("{name}.less")
        } else {
            format!("css/{name}.less")
        };
        let Some((entry_file, _)) = self.probe(&logical) else {
            // A plain-CSS entry (Luma's `css/critical.css`): SCD copies it
            // VERBATIM (pre-minified, no LESS pass — verified byte-identical
            // source→deploy on the reference install). No compile, no
            // variable-notation pass.
            let css_logical = logical.trim_end_matches(".less").to_string() + ".css";
            if let Some((css_file, _)) = self.probe(&css_logical) {
                let css = std::fs::read_to_string(&css_file).map_err(|e| LessDeployError {
                    entry: Some(css_logical.clone()),
                    module: None,
                    file: Some(css_file.clone()),
                    message: format!("read failed: {e}"),
                })?;
                return Ok(CompiledEntry {
                    entry: css_logical,
                    entry_file: css_file,
                    css,
                    warnings: Vec::new(),
                    skipped_modules: Vec::new(),
                });
            }
            return Err(LessDeployError {
                entry: Some(logical.clone()),
                module: None,
                file: None,
                message: format!(
                    "entry point not found; tried {}",
                    join_paths(&self.candidates(&logical))
                ),
            });
        };
        let source = std::fs::read_to_string(&entry_file).map_err(|e| LessDeployError {
            entry: Some(logical.clone()),
            module: None,
            file: Some(entry_file.clone()),
            message: format!("read failed: {e}"),
        })?;
        let source = if entry_strip_applies(&source) {
            strip_entry_comments(&source)
        } else {
            source
        };

        let mut less_opts = LessOptions::magento_production();
        less_opts.filename = Some(logical.clone());

        let mut skipped: Vec<(String, String)> = Vec::new();
        // Loud-fail by default; under `skip_broken_modules` each retry bans
        // the module whose partial broke the previous attempt (§7.5).
        loop {
            let resolver = OrchestratorResolver {
                orch: self,
                seen: Mutex::new(BTreeMap::new()),
                banned: skipped.iter().map(|(m, _)| m.clone()).collect(),
            };
            match magecommand_less::compile(&source, &less_opts, &resolver) {
                Ok(css) => {
                    return Ok(CompiledEntry {
                        entry: logical,
                        entry_file,
                        css: self.apply_variable_notation(css.code),
                        warnings: css.warnings.iter().map(|w| w.message.clone()).collect(),
                        skipped_modules: skipped,
                    });
                }
                Err(e) => {
                    let fault = self.attribute(&logical, &e, &resolver);
                    let can_skip = opts.skip_broken_modules
                        && fault.module.is_some()
                        && skipped.len() < self.modules.len();
                    if can_skip {
                        let module = fault.module.clone().expect("checked");
                        skipped.push((module, e.to_string().trim_end().to_string()));
                        continue;
                    }
                    return Err(fault);
                }
            }
        }
    }

    /// Magento's `View\Asset\PreProcessor\VariableNotation` post-compile
    /// pass (verified against the framework source + real SCD output): every
    /// `{{base_url_path}}` placeholder in the compiled CSS is expanded to
    /// `{{base_url_path}}<area>/<theme-path>/{{locale}}` for the DEPLOYED
    /// theme context — the email `@baseUrl` idiom
    /// (`url("@{baseUrl}css/email-fonts.css")` in `_email-extend.less`)
    /// becomes `url("{{base_url_path}}frontend/Magento/luma/{{locale}}/css/…")`
    /// in the real email-inline.css. (Magento routes this through
    /// `CssResolver::replaceRelativeUrls`, i.e. url()/@import references;
    /// the placeholder only ever occurs there, so a direct replace matches.)
    fn apply_variable_notation(&self, css: String) -> String {
        const VAR: &str = "{{base_url_path}}";
        if !css.contains(VAR) {
            return css;
        }
        let theme_path = self
            .chain
            .first()
            .map(|t| {
                t.id.strip_prefix(&format!("{}/", self.area))
                    .unwrap_or(&t.id)
                    .to_string()
            })
            .unwrap_or_default();
        let replacement = if theme_path.is_empty() {
            format!("{VAR}{}/{{{{locale}}}}", self.area)
        } else {
            format!("{VAR}{}/{}/{{{{locale}}}}", self.area, theme_path)
        };
        css.replace(VAR, &replacement)
    }

    /// Turn a compiler error into the §7.5 fault report: name the owning
    /// module (from the failing file's logical module scope) and the physical
    /// file (from the resolver's logical→physical record).
    fn attribute(
        &self,
        entry: &str,
        e: &magecommand_less::LessError,
        resolver: &OrchestratorResolver<'_>,
    ) -> LessDeployError {
        let logical = e.filename.clone();
        let module = logical
            .as_deref()
            .and_then(|l| l.split_once('/'))
            .filter(|(first, _)| is_module_segment(first))
            .map(|(first, _)| first.to_string());
        let file = logical
            .as_deref()
            .and_then(|l| resolver.seen.lock().ok().and_then(|s| s.get(l).cloned()))
            .or_else(|| logical.as_deref().and_then(|l| self.probe(l).map(|(p, _)| p)));
        LessDeployError {
            entry: Some(entry.to_string()),
            module,
            file,
            message: e.to_string().trim_end().to_string(),
        }
    }

    /// The `//@magento_import` expansion (§7.1/§7.3), mirroring Magento's
    /// `Css\PreProcessor\File\Collector\Aggregated` **exactly as probed live**
    /// (ObjectManager probe on a real 2.4.8-line install, 2026-07):
    ///
    /// 1. **library**: if `lib/web/<resolved>` or any chain theme's
    ///    `web/<resolved>` exists, one plain relative entry (no module) —
    ///    resolves through the theme-scope fallback;
    /// 2. **module base files**: enabled modules in `config.php` load order
    ///    (Magento's `ModuleDependency` sort decorator), each contributing if
    ///    `view/base/web/<resolved>` or `view/<area>/web/<resolved>` exists;
    /// 3. **theme module contexts**, ancestors first (root → child), modules
    ///    alphabetical within a theme (PHP glob order): each found
    ///    `<theme>/<Module>/web/<resolved>` REMOVES any earlier entry for the
    ///    module and appends at the end (the `FileList\Collator`
    ///    remove-and-append semantics — how a Luma override lands at Luma's
    ///    position while blank-only modules stay in the blank group).
    ///
    /// The entry's `Module::<resolved>` path then resolves through the normal
    /// modular fallback, so the most-specific file wins regardless of which
    /// group produced the entry.
    fn magento_import_entries(
        &self,
        path: &str,
        reference: bool,
        from_dir: &str,
        banned: &std::collections::BTreeSet<String>,
    ) -> Vec<MagentoImportEntry> {
        let resolved = normalize_logical(&join_logical(from_dir, path));
        // (module scope key, import path) — None = the plain library entry.
        let mut entries: Vec<(Option<String>, String)> = Vec::new();

        // 1. library / theme-scope file.
        let theme_scope_exists = self.lib_web.join(&resolved).is_file()
            || self
                .chain
                .iter()
                .any(|t| t.dir.join("web").join(&resolved).is_file());
        if theme_scope_exists {
            entries.push((None, path.to_string()));
        }

        // 2. module-own files, load order.
        for m in &self.modules {
            if banned.contains(&m.name) {
                continue;
            }
            let base = m.dir.join("view").join("base").join("web").join(&resolved);
            let area = m
                .dir
                .join("view")
                .join(&self.area)
                .join("web")
                .join(&resolved);
            if base.is_file() || area.is_file() {
                entries.push((Some(m.name.clone()), format!("{}::{resolved}", m.name)));
            }
        }

        // 3. theme module contexts, root ancestor first, alphabetical within.
        for t in self.chain.iter().rev() {
            let mut found: Vec<String> = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&t.dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if is_module_segment(&name)
                        && e.path().join("web").join(&resolved).is_file()
                    {
                        found.push(name);
                    }
                }
            }
            found.sort();
            for name in found {
                if banned.contains(&name) {
                    continue;
                }
                entries.retain(|(m, _)| m.as_deref() != Some(name.as_str()));
                entries.push((Some(name.clone()), format!("{name}::{resolved}")));
            }
        }

        entries
            .into_iter()
            .map(|(_, import_path)| MagentoImportEntry {
                import_path,
                reference,
            })
            .collect()
    }
}

/// Magento's `Css\PreProcessor\Instruction\Import::removeComments` runs on
/// every asset that goes through the preprocessor CHAIN — i.e. the ENTRY
/// file only (related/imported files are materialized verbatim by
/// `RelatedGenerator`; verified in the oracle's view_preprocessed tree:
/// entry banners blanked, partial banners kept). Its regex
/// (`#(^\s*//.*$)|((^\s*/\*(?s).*?(\*/)(?!\*/))$)#m`) removes whole-line
/// `//` comments and whole-line `/* … */` blocks, AFTER `MagentoImport`
/// has already replaced the `//@magento_import` directives.
///
/// Only the BLOCK-comment half is modeled here: `//` line comments never
/// survive LESS compilation (removing them is output-invisible), and our
/// `//@magento_import` expansion happens at parse time — i.e. after this
/// strip — so eating those lines would break the splice. This is what
/// removes the entry file's own license banner from the output, matching
/// the real SCD bytes (Phase-5 review TG-1/RT-2/DS-6).
///
/// The strip only takes EFFECT when [`entry_strip_applies`] — see there.
fn strip_entry_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut line_start = true;
    while i < bytes.len() {
        if line_start {
            // `^\s*/\*` — leading blanks then a block opener on this line.
            let mut j = i;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j + 1 < bytes.len() && bytes[j] == b'/' && bytes[j + 1] == b'*' {
                // Lazily find the first `*/` that is NOT followed by another
                // `*/` AND sits at end-of-line (the regex's `(\*/)(?!\*/)$`).
                let mut k = j + 2;
                let mut end = None;
                while k + 1 < bytes.len() {
                    if bytes[k] == b'*' && bytes[k + 1] == b'/' {
                        if bytes.get(k + 2) == Some(&b'*') && bytes.get(k + 3) == Some(&b'/') {
                            k += 2; // `*/` immediately followed by `*/` — skip
                            continue;
                        }
                        let after = k + 2;
                        let at_eol = after == bytes.len()
                            || bytes[after] == b'\n'
                            || (bytes[after] == b'\r' && bytes.get(after + 1) == Some(&b'\n'));
                        if at_eol {
                            end = Some(after);
                            break;
                        }
                    }
                    k += 1;
                }
                if let Some(end) = end {
                    // Drop `[i..end]`; the trailing newline (outside the
                    // match) stays, leaving one blank line like Magento.
                    i = end;
                    line_start = false;
                    continue;
                }
            }
        }
        let ch = src[i..].chars().next().unwrap();
        out.push(ch);
        line_start = ch == '\n';
        i += ch.len_utf8();
    }
    out
}

/// Whether Magento's entry comment strip takes effect for this entry.
///
/// `Import::process` ends with `if ($processedContent !== $content) {
/// $chain->setContent($processedContent); }` — `$content` is the
/// comment-STRIPPED text and `$processedContent` is that text after the
/// `@import` rewrite. The strip itself is not a "change": it only sticks
/// when the rewrite altered at least one import statement. Real triggers:
/// - a `//@magento_import` directive (its expansion produces module-notation
///   `Mod_X::…` imports, which `convertModuleNotationToPath` rewrites);
/// - an `@import` path carrying `::` module notation directly;
/// - an extension-less `@import` path (`fixFileExtension` appends `.less`).
///
/// Verified against the oracle SCD output: styles-m/styles-l/email/
/// email-inline (directives present) lose their banners; print/email-fonts
/// (plain fully-extensioned imports only — identity rewrite) KEEP theirs.
fn entry_strip_applies(src: &str) -> bool {
    if src.contains("//@magento_import") {
        return true;
    }
    let mut rest = src;
    while let Some(pos) = rest.find("@import") {
        rest = &rest[pos + "@import".len()..];
        let Some(q) = rest.find(['\'', '"']) else { continue };
        let quote = rest.as_bytes()[q];
        let after = &rest[q + 1..];
        let Some(end) = after.find(quote as char) else { continue };
        let path = &after[..end];
        if path.contains("::") {
            return true;
        }
        let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
        if !base.contains('.') {
            return true; // fixFileExtension appends `.less` — a rewrite
        }
        rest = &after[end + 1..];
    }
    false
}

/// Does a path segment look like a `Vendor_Module` context dir?
///
/// Mirrors Magento's collector glob `*_*` (`Override\Base` searches
/// `<theme>/*_*/web/<path>`; PHP `glob` never matches a leading `.`), so a
/// lowercase or dotted dir like `vendor_module` IS a module context — the
/// earlier uppercase-first/no-dot predicate skipped dirs Magento collects
/// (review ORD-4; no such dir exists in blank/luma).
fn is_module_segment(s: &str) -> bool {
    s.contains('_') && !s.starts_with('.')
}

/// Join a logical directory (may carry a trailing `/`) and a relative path.
fn join_logical(dir: &str, path: &str) -> String {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        path.to_string()
    } else {
        format!("{dir}/{path}")
    }
}

/// Collapse `.` and `x/..` hops in a logical path.
fn normalize_logical(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if out.is_empty() || out.last() == Some(&"..") {
                    out.push(seg);
                } else {
                    out.pop();
                }
            }
            _ => out.push(seg),
        }
    }
    out.join("/")
}

fn logical_dir(logical: &str) -> String {
    match logical.rfind('/') {
        Some(i) => logical[..=i].to_string(),
        None => String::new(),
    }
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The compiler-facing resolver: logical-path resolution over the orchestrator
/// plus the `//@magento_import` callback. Records every logical → physical
/// mapping for fault attribution (§7.5).
struct OrchestratorResolver<'a> {
    orch: &'a LessOrchestrator,
    /// logical path → physical file, filled as imports resolve.
    seen: Mutex<BTreeMap<String, PathBuf>>,
    /// Modules dropped under `skip_broken_modules`.
    banned: std::collections::BTreeSet<String>,
}

impl OrchestratorResolver<'_> {
    /// Map an import request path (relative or `Vendor_Module::rest`) to its
    /// logical path.
    fn logical_of(&self, req_path: &str, from_dir: &str) -> String {
        if let Some((module, rest)) = req_path.split_once("::") {
            normalize_logical(&format!("{module}/{rest}"))
        } else {
            normalize_logical(&join_logical(from_dir, req_path))
        }
    }
}

impl ImportResolver for OrchestratorResolver<'_> {
    fn resolve(&self, req: &ImportRequest) -> Result<ResolvedImport, ImportError> {
        let logical = self.logical_of(&req.path, &req.from.current_directory);
        // less.js extension defaulting: a path without an extension gets
        // `.less` appended if the bare form misses.
        let (logical, hit) = match self.orch.probe(&logical) {
            Some(hit) => (logical, hit),
            None => {
                let has_ext = logical
                    .rsplit('/')
                    .next()
                    .is_some_and(|f| f.contains('.'));
                let with_ext = format!("{logical}.less");
                match (!has_ext).then(|| self.orch.probe(&with_ext)).flatten() {
                    Some(hit) => (with_ext, hit),
                    None => {
                        let tried = if has_ext {
                            self.orch.candidates(&logical)
                        } else {
                            self.orch.candidates(&format!("{logical}.less"))
                        };
                        return Err(ImportError::NotFound(join_paths(&tried)));
                    }
                }
            }
        };
        let (physical, _module) = hit;
        let source = std::fs::read_to_string(&physical).map_err(|e| ImportError::Io {
            path: physical.display().to_string(),
            message: e.to_string(),
        })?;
        if let Ok(mut seen) = self.seen.lock() {
            seen.insert(logical.clone(), physical.clone());
        }
        let payload = if req.options.inline {
            ImportPayload::Inline(source.into())
        } else if logical.ends_with(".css") {
            ImportPayload::Css(source.into())
        } else {
            ImportPayload::Less(source.into())
        };
        Ok(ResolvedImport {
            file: FileInfo {
                filename: logical.clone(),
                current_directory: logical_dir(&logical),
                ..Default::default()
            },
            payload,
        })
    }

    fn magento_import(
        &self,
        path: &str,
        reference: bool,
        from: &FileInfo,
    ) -> Result<Vec<MagentoImportEntry>, ImportError> {
        let entries = self.orch.magento_import_entries(
            path,
            reference,
            &from.current_directory,
            &self.banned,
        );
        // Diagnostic: `MAGECOMMAND_LESS_TRACE_IMPORTS=1` dumps every
        // `//@magento_import` expansion (the §7.3 splice list) to stderr for
        // textual comparison against a real install's var/view_preprocessed.
        if std::env::var_os("MAGECOMMAND_LESS_TRACE_IMPORTS").is_some() {
            eprintln!("//@magento_import '{path}' @ {}:", from.filename);
            for e in &entries {
                eprintln!("  {}", e.import_path);
            }
        }
        Ok(entries)
    }

    fn load_binary(&self, path: &str, current_directory: &str) -> Option<Vec<u8>> {
        let logical = self.logical_of(path, current_directory);
        let (physical, _) = self.orch.probe(&logical)?;
        std::fs::read(physical).ok()
    }
}

/// The `pub/static` placement for one compiled entry:
/// `pub/static/<area>/<Vendor>/<theme>/<locale>/css/<entry>.css`.
pub fn output_path(root: &Path, area: &str, theme_id: &str, locale: &str, entry: &str) -> PathBuf {
    let theme_path = theme_id
        .strip_prefix(&format!("{area}/"))
        .unwrap_or(theme_id);
    let name = entry.strip_suffix(".less").unwrap_or(entry);
    let name = name.strip_prefix("css/").unwrap_or(name);
    root.join("pub")
        .join("static")
        .join(area)
        .join(theme_path)
        .join(locale)
        .join("css")
        .join(format!("{name}.css"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic Magento-shaped source tree; returns its root.
    fn synth_tree() -> tempfile::TempDir {
        let td = tempfile::tempdir().expect("tempdir");
        let r = td.path();
        let w = |rel: &str, content: &str| {
            let p = r.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        };

        // lib/web: the library, imported relatively from the base theme.
        w(
            "lib/web/css/source/lib/_lib.less",
            ".lib-badge() { border: 1px solid; }\n@lib-color: navy;\n",
        );

        // Base theme (no parent): entry + sources + a module context override.
        w(
            "vendor/acme/theme-base/theme.xml",
            "<!-- header -->\n<theme><title>Base</title></theme>\n",
        );
        w(
            "vendor/acme/theme-base/web/css/styles-m.less",
            "@import 'source/lib/_lib.less';\n\
             @import (reference) 'source/_extends.less';\n\
             //@magento_import 'source/_module.less';\n\
             @import 'source/_theme.less';\n\
             //@magento_import 'source/_extend.less';\n\
             .base { color: @accent; }\n",
        );
        w(
            "vendor/acme/theme-base/web/css/source/_extends.less",
            ".abs-badge { .lib-badge(); }\n",
        );
        w(
            "vendor/acme/theme-base/web/css/source/_theme.less",
            "@accent: red;\n",
        );
        // Theme-context override for Acme_Widgets (beats the module's own).
        w(
            "vendor/acme/theme-base/Acme_Widgets/web/css/source/_module.less",
            ".widgets { from: theme-base; }\n",
        );

        // Child theme: parent declared in theme.xml; overrides _theme.less.
        w(
            "vendor/acme/theme-child/theme.xml",
            "<theme><title>Child</title><parent>Acme/base</parent></theme>\n",
        );
        w(
            "vendor/acme/theme-child/web/css/source/_theme.less",
            "@accent: green;\n",
        );

        // Modules with their own view files.
        w(
            "vendor/acme/module-base/view/frontend/web/css/source/_module.less",
            ".mod-base { b: @lib-color; }\n",
        );
        w(
            "vendor/acme/module-widgets/view/frontend/web/css/source/_module.less",
            ".widgets { from: module; }\n",
        );
        // The singular `_extend.less` hook (distinct from `_extends.less`).
        w(
            "vendor/acme/module-base/view/frontend/web/css/source/_extend.less",
            ".hooked:extend(.abs-badge) { h: 1; }\n",
        );

        td
    }

    fn themes(root: &Path) -> Vec<(String, PathBuf)> {
        vec![
            (
                "frontend/Acme/base".to_string(),
                root.join("vendor/acme/theme-base"),
            ),
            (
                "frontend/Acme/child".to_string(),
                root.join("vendor/acme/theme-child"),
            ),
        ]
    }

    fn modules(root: &Path) -> Vec<ModuleRef> {
        vec![
            ModuleRef {
                name: "Acme_Base".to_string(),
                dir: root.join("vendor/acme/module-base"),
            },
            ModuleRef {
                name: "Acme_Widgets".to_string(),
                dir: root.join("vendor/acme/module-widgets"),
            },
        ]
    }

    fn orchestrator(root: &Path, theme: &str) -> LessOrchestrator {
        LessOrchestrator::new(root, "frontend", theme, &themes(root), modules(root)).unwrap()
    }

    #[test]
    fn theme_chain_is_child_first_via_theme_xml_parent() {
        let td = synth_tree();
        let chain = theme_chain("frontend", "Acme/child", &themes(td.path())).unwrap();
        let ids: Vec<&str> = chain.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["frontend/Acme/child", "frontend/Acme/base"]);
    }

    #[test]
    fn theme_chain_reports_missing_parent() {
        let td = synth_tree();
        std::fs::write(
            td.path().join("vendor/acme/theme-child/theme.xml"),
            "<theme><parent>Acme/gone</parent></theme>",
        )
        .unwrap();
        let e = theme_chain("frontend", "Acme/child", &themes(td.path())).unwrap_err();
        assert!(e.message.contains("parent theme 'frontend/Acme/gone'"), "{e}");
    }

    /// The load-bearing fallback property: compiling the CHILD theme, blank's
    /// entry (inherited — the child has no styles-m.less) imports
    /// `source/_theme.less`, which must resolve to the CHILD's override.
    /// Also: `//@magento_import` splices module partials in load order, the
    /// theme-context override beats the module's own file, `(reference)`
    /// `_extends.less` emits nothing bare, and the `_extend.less` hook grafts
    /// onto the `.abs-*` selector.
    #[test]
    fn compile_entry_child_theme_full_fallback() {
        let td = synth_tree();
        let orch = orchestrator(td.path(), "Acme/child");
        let out = orch
            .compile_entry("styles-m", &LessDeployOptions::default())
            .unwrap();
        // Child override wins for @accent (green, not red).
        assert!(out.css.contains(".base {\n  color: green;\n}"), "css:\n{}", out.css);
        // Module partial from Acme_Base's own view dir, seeing the lib var.
        assert!(out.css.contains(".mod-base {\n  b: navy;\n}"), "css:\n{}", out.css);
        // Theme-context override beats the module's own file.
        assert!(out.css.contains(".widgets {\n  from: theme-base;\n}"), "css:\n{}", out.css);
        assert!(!out.css.contains("from: module"), "css:\n{}", out.css);
        // (reference) _extends: `.abs-badge` appears only via the extend graft.
        assert!(out.css.contains(".hooked"), "css:\n{}", out.css);
        let bare_abs = out
            .css
            .lines()
            .filter(|l| l.contains(".abs-badge") && !l.contains(".hooked"))
            .count();
        assert_eq!(bare_abs, 0, "bare .abs-badge leaked:\n{}", out.css);
        assert!(out.skipped_modules.is_empty());
    }

    /// Module partials splice in `config.php` load order: Acme_Base's output
    /// precedes Acme_Widgets' (theme-context group comes after the module
    /// group, matching the probed Magento collator order).
    #[test]
    fn magento_import_order_module_group_then_theme_group() {
        let td = synth_tree();
        let orch = orchestrator(td.path(), "Acme/base");
        let entries = orch.magento_import_entries(
            "source/_module.less",
            false,
            "css/",
            &Default::default(),
        );
        let paths: Vec<&str> = entries.iter().map(|e| e.import_path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Acme_Base::css/source/_module.less",
                // Acme_Widgets moved to the theme-collation position.
                "Acme_Widgets::css/source/_module.less",
            ]
        );
    }

    /// §7.5 loud-fail: a broken module partial fails the entry point naming
    /// the module and physical file; `skip_broken_modules` drops it instead.
    #[test]
    fn broken_module_partial_faults_with_module_name() {
        let td = synth_tree();
        let bad = td
            .path()
            .join("vendor/acme/module-widgets/view/frontend/web/css/source/_extend.less");
        std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
        std::fs::write(&bad, ".broken { color: @does-not-exist; }\n").unwrap();

        let orch = orchestrator(td.path(), "Acme/base");
        let e = orch
            .compile_entry("styles-m", &LessDeployOptions::default())
            .unwrap_err();
        assert_eq!(e.module.as_deref(), Some("Acme_Widgets"), "{e}");
        assert_eq!(e.file.as_deref(), Some(bad.as_path()), "{e}");
        assert!(e.message.contains("@does-not-exist"), "{e}");

        let out = orch
            .compile_entry(
                "styles-m",
                &LessDeployOptions {
                    skip_broken_modules: true,
                },
            )
            .unwrap();
        assert_eq!(out.skipped_modules.len(), 1);
        assert_eq!(out.skipped_modules[0].0, "Acme_Widgets");
        // The rest still compiled.
        assert!(out.css.contains(".mod-base"));
    }

    #[test]
    fn missing_entry_point_lists_candidates() {
        let td = synth_tree();
        let orch = orchestrator(td.path(), "Acme/child");
        let e = orch
            .compile_entry("styles-xl", &LessDeployOptions::default())
            .unwrap_err();
        // Candidate paths render with the platform separator on Windows —
        // normalize before matching so the assertion is separator-agnostic.
        let msg = e.message.replace('\\', "/");
        assert!(msg.contains("entry point not found"), "{e}");
        assert!(msg.contains("theme-child"), "{e}");
        assert!(msg.contains("lib/web"), "{e}");
    }

    #[test]
    fn strip_entry_comments_matches_magento() {
        // Whole-line block banner removed (its trailing newline stays —
        // one blank line, like the oracle's view_preprocessed entries).
        let src = "/**\n * Copyright\n */\n\n.a { color: red; }\n";
        assert_eq!(strip_entry_comments(src), "\n\n.a { color: red; }\n");
        // Indented whole-line block removed too (`^\s*`).
        assert_eq!(strip_entry_comments("  /* x */\n.b {}\n"), "\n.b {}\n");
        // A block NOT ending at end-of-line is kept (regex `$` fails)…
        let keep = "/* x */ .c {}\n";
        assert_eq!(strip_entry_comments(keep), keep);
        // …and one not starting at line begin is kept.
        let keep = ".d {} /* x */\n";
        assert_eq!(strip_entry_comments(keep), keep);
        // Line comments (incl. //@magento_import, expanded later at parse
        // time) are untouched — output-invisible either way.
        let keep = "//@magento_import 'source/_module.less';\n// note\n";
        assert_eq!(strip_entry_comments(keep), keep);
    }

    #[test]
    fn entry_strip_gate_mirrors_import_rewrite() {
        // Strip sticks only when the @import rewrite would CHANGE something
        // (Magento setContent gate): magento_import directives, module
        // notation, or an extension-less path.
        assert!(entry_strip_applies("//@magento_import 'source/_module.less';\n"));
        assert!(entry_strip_applies("@import 'Magento_Theme::css/source/_x.less';\n"));
        assert!(entry_strip_applies("@import 'source/lib/_lib';\n"));
        // Fully-extensioned plain imports = identity rewrite → banner kept
        // (the real SCD print.css / email-fonts.css keep theirs).
        assert!(!entry_strip_applies(
            "/**\n * banner\n */\n@import 'source/lib/_lib.less';\n@import (reference) 'source/_email-base.less';\n"
        ));
    }

    #[test]
    fn output_path_shape() {
        let p = output_path(
            Path::new("/root"),
            "frontend",
            "frontend/Acme/child",
            "en_US",
            "styles-m",
        );
        assert_eq!(
            p,
            Path::new("/root/pub/static/frontend/Acme/child/en_US/css/styles-m.css")
        );
    }
}
