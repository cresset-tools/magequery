//! Full static-file PLACEMENT of a theme package — everything a real
//! `setup:static-content:deploy` (quick strategy, the default) writes:
//! the plain-copy engine over the deploy's source collectors, LESS-derived
//! CSS (via [`super::less`]), the css notation processors
//! ([`super::cssnotation`]), the requirejs artifacts ([`super::requirejs`]),
//! `js-translation.json`, the JS bundles ([`super::bundle`]),
//! `sri-hashes.json` (Magento_Csp) and `deployed_version.txt`.
//!
//! ## The package model (verified against module-deploy source + goldens)
//!
//! `Magento\Deploy` collects every deployable file into per-`(area, theme,
//! locale)` packages (`Collector::collect` over the `SourcePool`: `lib`,
//! `modules`, `themes` — module files of DISABLED modules are skipped there,
//! Collector.php:92), then the real package aggregates its ancestor packages
//! (`Package::aggregate` / `getParentFiles`): PHP `array_merge` over the
//! ancestor maps keyed by **fileId** (`Module::sourceName` or plain source
//! name) — a later declaration overrides the VALUE but keeps the EARLIEST
//! position, and the real package's own files (theme `web/i18n/<locale>/`
//! overlays) come first (collected before `aggregate` appends).
//!
//! Ancestor order for `(frontend, T, L)` (collectParentPaths, reversed):
//! `base/./.` (lib + module `view/base/web`), `base/./L` (module base i18n),
//! `frontend/./.` (module `view/frontend/web`), `frontend/./L`, then each
//! theme ancestor root-first: `frontend/<A>/.` and — via the ancestor's real
//! package — its `web/i18n/L` overlays, ending with the deployed theme's own
//! files. Within every source, file order is PHP's `Files::getFiles` glob:
//! per directory files first, then subdirectories depth-first, both in
//! **readdir order of the SOURCE tree** (`GLOB_NOSORT`) — reproduced here by
//! plain `read_dir` (hidden names skipped, like glob). Module iteration
//! order is **registration order** (composer `autoload_files.php`, then
//! `app/code` sorted) — NOT `config.php` order; verified against the golden
//! `sri-hashes.json` entry order, which is the deployment order.
//!
//! This order matters for exactly one artifact: `sri-hashes.json` keys.
//! File CONTENT is order-independent (fileIds are namespaced per module).
//!
//! ## Per-file pipeline (`DeployPackage::processFile` — publish path)
//!
//! - non-partial `.less` → LESS-compile at its logical path (theme entries,
//!   `mage/gallery/gallery.less`, module `hljs.less`), deployed as `.css`;
//!   `_*.less` partials are never deployed (`checkFileSkip`);
//! - `.css` → byte copy + the css pre-processor notation passes
//!   (VariableNotation then ModuleNotation — [`super::cssnotation`]; the
//!   LESS pipeline already applies VariableNotation to compiled output);
//! - everything else → byte copy (the pre-processor pool defaults to
//!   `Passthrough`; `js_min`/`css_min` are inert with minification off, and
//!   `js_translation` is inert under the default `dictionary` strategy).
//!
//! **Always the publish path, never the parent-package copy**: quick
//! deploy's `checkIfCanCopy` requires `origPackage === parentPackage`,
//! which virtual-package-originated files never satisfy; and even where a
//! pub/static→pub/static copy occurs it is byte-identical to publishing
//! from the same source. We regenerate every file from source.
//!
//! ## Generated files
//!
//! After the package files: `requirejs-config.js` +
//! `requirejs-min-resolver.js` (DeployRequireJsConfig), `js-translation.json`
//! (DeployTranslationsDictionary — the merged js dictionary; empty on
//! locales where no phrase translates differently, serialized as PHP
//! `json_encode([])` = the literal `[]`; **limitation**: phrase extraction
//! from js/html sources is not implemented, so only locales with an empty
//! dictionary — like en_US — are byte-faithful), `js/bundle/bundle<N>.js`
//! ([`super::bundle`], shared `.min`-sibling cache across the themes of one
//! run — the bundler deliberately keeps its own merged js/html view of the
//! package instead of consuming this resolver: that view is locked by the
//! 14-bundle byte gate and needs a merged MAP in deployed-tree glob order,
//! not this resolver's provenance-ordered entry list; both are gated
//! against the same golden deploy), and `sri-hashes.json` — sha256-base64
//! of every deployed `.js` (package js in deployment order, then the two
//! requirejs artifacts, then the bundles; PHP `json_encode` with escaped
//! slashes).
//! `deployed_version.txt` (pub/static root, run-scoped) is written only when
//! the caller supplies a version — never an invented timestamp.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::bundle::{self, MinSiblingCache, OrderMode};
use super::cssnotation;
use super::less::{
    is_module_segment, theme_chain, LessDeployOptions, LessOrchestrator, ModuleRef, ThemeRef,
};
use super::requirejs;

/// Faults reuse the static-deploy error shape (entry/module/file provenance).
pub type FilesError = super::less::LessDeployError;

/// `Magento\Translation\Model\Js\Config::DICTIONARY_FILE_NAME`.
pub const DICTIONARY_FILE_NAME: &str = "js-translation.json";

/// `Csp\Model\SubresourceIntegrity\Storage\File::FILENAME`.
pub const SRI_HASHES_FILE_NAME: &str = "sri-hashes.json";

/// `pub/static/deployed_version.txt` (Version\Storage\File, app/etc/di.xml).
pub const DEPLOYED_VERSION_FILE_NAME: &str = "deployed_version.txt";

// ---------------------------------------------------------------------------
// Module registration order
// ---------------------------------------------------------------------------

/// Order `modules` like `ComponentRegistrar` sees them: composer packages in
/// `vendor/composer/autoload_files.php` order (the order the
/// `registration.php` files execute), then the non-composer `app/code`
/// modules in sorted-glob order (`NonComposerComponentRegistration`), then
/// anything unmatched in the given order. The DI/config `config.php` order
/// is NOT the registration order — the golden `sri-hashes.json` proves the
/// deployment iterates modules in registration order.
pub fn registration_order(root: &Path, modules: &[ModuleRef]) -> Vec<ModuleRef> {
    use rayon::prelude::*;
    let autoload = root.join("vendor").join("composer").join("autoload_files.php");
    let vendor = root.join("vendor");
    // Collect the raw registration dirs first, then canonicalize them in
    // parallel: `canonicalize` is a per-path syscall walk and there are ~700 of
    // them plus one per module — a serial storm on the deploy's serial prepare
    // step. `par_iter().collect()` preserves order, so `reg_dirs` keeps its
    // autoload (deployment) order — the order that drives `out` below.
    let mut reg_raw: Vec<PathBuf> = Vec::new();
    if let Ok(src) = std::fs::read_to_string(&autoload) {
        // Lines look like: `'<hash>' => $vendorDir . '/pkg/path/registration.php',`
        let mut rest = src.as_str();
        while let Some(at) = rest.find("$vendorDir . '") {
            rest = &rest[at + "$vendorDir . '".len()..];
            let Some(end) = rest.find('\'') else { break };
            let rel = &rest[..end];
            rest = &rest[end..];
            if let Some(dir) = rel.strip_suffix("/registration.php") {
                reg_raw.push(vendor.join(dir.trim_start_matches('/')));
            }
        }
    }
    let reg_dirs: Vec<PathBuf> = reg_raw
        .par_iter()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();
    let canon: Vec<(PathBuf, usize)> = modules
        .par_iter()
        .enumerate()
        .map(|(i, m)| (std::fs::canonicalize(&m.dir).unwrap_or_else(|_| m.dir.clone()), i))
        .collect();
    let by_dir: HashMap<&Path, usize> = canon.iter().map(|(p, i)| (p.as_path(), *i)).collect();

    let mut out: Vec<ModuleRef> = Vec::with_capacity(modules.len());
    let mut taken = vec![false; modules.len()];
    for dir in &reg_dirs {
        if let Some(&i) = by_dir.get(dir.as_path()) {
            if !taken[i] {
                taken[i] = true;
                out.push(modules[i].clone());
            }
        }
    }
    // app/code (and any unmatched) modules: sorted by path, the sorted-glob
    // order of `app/code/*/*/registration.php`.
    let mut rest: Vec<&ModuleRef> = modules
        .iter()
        .enumerate()
        .filter(|(i, _)| !taken[*i])
        .map(|(_, m)| m)
        .collect();
    rest.sort_by(|a, b| a.dir.cmp(&b.dir));
    out.extend(rest.into_iter().cloned());
    out
}

// ---------------------------------------------------------------------------
// The source glob (Files::getFiles order)
// ---------------------------------------------------------------------------

/// `Files::getFiles([dir], '*')`: per directory, files first, then each
/// subdirectory depth-first — both in **readdir order** (`GLOB_NOSORT`);
/// hidden (leading-dot) names never match. Returns `(relative_path, file)`
/// pairs with `/` separators. Missing dirs yield nothing.
fn glob_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    fn rec(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut files: Vec<(String, PathBuf)> = Vec::new();
        let mut dirs: Vec<(String, PathBuf)> = Vec::new();
        for e in rd.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let path = e.path();
            if path.is_dir() {
                dirs.push((name, path));
            } else {
                files.push((name, path));
            }
        }
        for (name, path) in files {
            let r = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            out.push((r, path));
        }
        for (name, path) in dirs {
            let r = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            rec(&path, &r, out);
        }
    }
    rec(dir, "", &mut out);
    out
}

// ---------------------------------------------------------------------------
// Package resolution (ordered fileId map)
// ---------------------------------------------------------------------------

/// One resolved package file, in deployment order.
#[derive(Debug, Clone)]
pub struct PackageEntry {
    /// Package-relative deployed path (`css/styles-m.css`,
    /// `Magento_Catalog/js/price-box.js`) — `.less` renamed to `.css`.
    pub deployed: String,
    /// The winning physical source file.
    pub source: PathBuf,
    /// Compile via the LESS pipeline (non-partial `.less` source)?
    pub less: bool,
    /// The logical path for the LESS resolver / provenance
    /// (`css/styles-m.less`, `Magento_PageBuilder/css/source/hljs/hljs.less`).
    pub logical: String,
}

/// The ordered fileId → source map with the deploy's two override rules:
/// among AGGREGATED ancestor files (PHP `array_merge` over the ancestor
/// maps) a later add replaces the value but keeps the earliest position
/// ([`OrderedFiles::add`]); the package's OWN files (its `i18n/<locale>`
/// overlays, collected before `Package::aggregate` runs) come first and are
/// never overridden — `aggregate` skips fileIds the package already has
/// ([`OrderedFiles::add_own`]).
#[derive(Default)]
struct OrderedFiles {
    order: Vec<(String, PathBuf)>, // (fileId, source)
    index: HashMap<String, usize>,
    owned: std::collections::HashSet<String>,
}

impl OrderedFiles {
    /// An aggregated (inherited) file: later wins, position kept — unless
    /// the fileId is one of the package's own.
    fn add(&mut self, file_id: String, source: PathBuf) {
        if self.owned.contains(&file_id) {
            return;
        }
        match self.index.get(&file_id) {
            Some(&i) => self.order[i].1 = source,
            None => {
                self.index.insert(file_id.clone(), self.order.len());
                self.order.push((file_id, source));
            }
        }
    }

    /// One of the package's own files (pre-aggregate): keeps its value
    /// against every later aggregated add.
    fn add_own(&mut self, file_id: String, source: PathBuf) {
        self.add(file_id.clone(), source);
        self.owned.insert(file_id);
    }
}

/// The `i18n/<locale>/rest` prefix parse of a theme/module web path (the
/// collectors only treat a TOP-LEVEL `i18n/` segment as a locale overlay).
fn split_i18n(rel: &str) -> Option<(&str, &str)> {
    let rest = rel.strip_prefix("i18n/")?;
    let (locale, path) = rest.split_once('/')?;
    Some((locale, path))
}

/// Resolve the full package file list for `(area, chain, locale)` in
/// deployment order. `modules` must already be in REGISTRATION order (see
/// [`registration_order`]). The chain is child-first (as [`theme_chain`]
/// returns it).
pub fn resolve_package(
    root: &Path,
    area: &str,
    chain: &[ThemeRef],
    modules: &[ModuleRef],
    locale: &str,
) -> Vec<PackageEntry> {
    let mut files = OrderedFiles::default();
    let enabled: std::collections::HashSet<&str> =
        modules.iter().map(|m| m.name.as_str()).collect();

    // A theme layer: `web/**` then each `*_*` module context's `web/**`
    // (theme-dir readdir order, like the collector's `*_*/web` glob).
    // A context for a module that is NOT enabled is dropped whole —
    // `Collector::collect` skips every module-attributed file whose module
    // fails `Manager::isEnabled` (Collector.php:93), theme-context files
    // included (backend-real: the theme ships a `Magento_Analytics/` context
    // but no such module is installed; its icon never deploys).
    // `with_i18n` selects the locale overlay (stripped path) or the plain
    // files; both passes walk the same glob stream, preserving its order.
    // `own` marks the deployed package's own files (its locale overlays).
    let theme_layer = |files: &mut OrderedFiles, t: &ThemeRef, with_i18n: bool, own: bool| {
        let mut contexts: Vec<(String, PathBuf)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&t.dir) {
            for e in rd.flatten() {
                let Some(name) = e.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if name.starts_with('.') || !is_module_segment(&name) {
                    continue;
                }
                if !enabled.contains(name.as_str()) {
                    continue;
                }
                let path = e.path();
                if path.is_dir() {
                    contexts.push((name, path));
                }
            }
        }
        let mut layer = |ctx: Option<&str>, web: &Path| {
            for (rel, src) in glob_files(web) {
                let (is_ours, path) = match split_i18n(&rel) {
                    Some((loc, path)) => (with_i18n && loc == locale, path),
                    None => (!with_i18n, rel.as_str()),
                };
                if !is_ours {
                    continue;
                }
                let fid = match ctx {
                    Some(c) => format!("{c}::{path}"),
                    None => path.to_string(),
                };
                if own {
                    files.add_own(fid, src);
                } else {
                    files.add(fid, src);
                }
            }
        };
        layer(None, &t.dir.join("web"));
        for (name, path) in &contexts {
            layer(Some(name), &path.join("web"));
        }
    };

    // Own package first: the deployed theme's `i18n/<locale>` overlays are
    // its own collected files, positioned before everything aggregated and
    // never overridden by it (`aggregate` skips already-present fileIds).
    if let Some(own) = chain.first() {
        theme_layer(&mut files, own, true, true);
    }

    // base/./.: lib/web (minus css/docs — Source\Lib's one exclusion) …
    for (rel, src) in glob_files(&root.join("lib").join("web")) {
        if rel.starts_with("css/docs") {
            continue;
        }
        files.add(rel, src);
    }
    // … then module `view/base/web` (registration order).
    for m in modules {
        for (rel, src) in glob_files(&m.dir.join("view").join("base").join("web")) {
            files.add(format!("{}::{rel}", m.name), src);
        }
    }
    // base/./<locale>: module base i18n overlays (stripped path).
    for m in modules {
        let base = m.dir.join("view").join("base").join("web").join("i18n").join(locale);
        for (rel, src) in glob_files(&base) {
            files.add(format!("{}::{rel}", m.name), src);
        }
    }
    // <area>/./.: module `view/<area>/web`.
    for m in modules {
        for (rel, src) in glob_files(&m.dir.join("view").join(area).join("web")) {
            files.add(format!("{}::{rel}", m.name), src);
        }
    }
    // <area>/./<locale>: module area i18n overlays.
    for m in modules {
        let base = m.dir.join("view").join(area).join("web").join("i18n").join(locale);
        for (rel, src) in glob_files(&base) {
            files.add(format!("{}::{rel}", m.name), src);
        }
    }
    // Theme ancestors, root-first; each ancestor's plain files, then its
    // i18n overlays (the ancestor's own real package in the ancestor walk).
    for (i, t) in chain.iter().enumerate().rev() {
        theme_layer(&mut files, t, false, false);
        if i > 0 {
            theme_layer(&mut files, t, true, false);
        }
        // i == 0 (the deployed theme): its i18n overlays were added first.
    }

    // Project fileIds to deployed entries: `Module::p` → `Module/p`, skip
    // `_*.less` partials (checkFileSkip), rename non-partial `.less` → `.css`.
    let mut out = Vec::with_capacity(files.order.len());
    for (fid, source) in files.order {
        let logical = fid.replacen("::", "/", 1);
        let basename = logical.rsplit('/').next().unwrap_or(&logical);
        let is_less = basename.ends_with(".less");
        if is_less && basename.starts_with('_') {
            continue;
        }
        let deployed = if is_less {
            format!("{}.css", &logical[..logical.len() - ".less".len()])
        } else {
            logical.clone()
        };
        out.push(PackageEntry {
            deployed,
            source,
            less: is_less,
            logical,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// PHP json_encode (default flags — slashes escaped)
// ---------------------------------------------------------------------------

/// PHP `json_encode($map)` with DEFAULT flags over an ordered string map:
/// like [`bundle::php_json_encode_map`] but with `/` escaped as `\/`
/// (`sri-hashes.json` is written without `JSON_UNESCAPED_SLASHES`). An empty
/// map is PHP's empty ARRAY and serializes as `[]`.
pub fn php_json_encode_map_default(entries: &[(&str, &str)]) -> String {
    if entries.is_empty() {
        return "[]".to_string();
    }
    bundle::php_json_encode_map_opts(entries, true)
}

/// The `js-translation.json` payload for a dictionary (phrase →
/// translation, `ksort`ed by the caller): PHP `json_encode` of the array —
/// the literal `[]` when empty (the en_US case: no phrase translates
/// differently), an escaped-slash object otherwise.
pub fn dictionary_json(entries: &[(&str, &str)]) -> String {
    php_json_encode_map_default(entries)
}

/// `sri-hashes.json`: `path → "sha256-<base64>"` in deployment order.
pub fn sri_hashes_json(entries: &[(String, String)]) -> String {
    let refs: Vec<(&str, &str)> = entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    php_json_encode_map_default(&refs)
}

/// `sha256-<base64(raw sha256)>` (Csp HashGenerator).
pub fn sri_hash(content: &[u8]) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(content);
    format!(
        "sha256-{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// How a placed file was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacedKind {
    /// Byte copy of a source file.
    Copy,
    /// Byte copy whose css notation pass changed the content.
    CssProcessed,
    /// LESS-compiled css.
    LessCompiled,
    /// A generated requirejs artifact.
    RequireJs,
    /// The js dictionary.
    Translation,
    /// A js bundle.
    Bundle,
    /// The Csp integrity map.
    SriHashes,
}

/// One file of the deployed package, in write order.
#[derive(Debug)]
pub struct PlacedFile {
    /// Package-relative path (`css/styles-m.css`, `js/bundle/bundle0.js`).
    pub path: String,
    pub content: Vec<u8>,
    pub kind: PlacedKind,
}

/// One theme's fully placed package.
#[derive(Debug)]
pub struct ThemePackage {
    /// The theme id as given (`Magento/blank`).
    pub theme: String,
    /// Bare theme path (`Magento/blank`) — the `pub/static` segment.
    pub theme_path: String,
    /// Child-first fallback chain.
    pub chain: Vec<ThemeRef>,
    /// Every file of the package, in deployment/write order.
    pub files: Vec<PlacedFile>,
    /// Compiler warnings from the LESS entries (logical path, message).
    pub warnings: Vec<(String, String)>,
}

impl ThemePackage {
    /// Count files of a kind.
    pub fn count(&self, kind: PlacedKind) -> usize {
        self.files.iter().filter(|f| f.kind == kind).count()
    }

    /// Total content bytes.
    pub fn bytes(&self) -> usize {
        self.files.iter().map(|f| f.content.len()).sum()
    }
}

/// Deploy options.
#[derive(Debug, Clone)]
pub struct PlacementOptions {
    /// Compress LESS-compiled css (`Less_Parser` `compress=true` — what a
    /// default/production-mode SCD produces). Default ON.
    pub compress: bool,
    /// Bundle-internal iteration order (the deployed-tree glob simulation).
    pub order: OrderMode,
}

impl Default for PlacementOptions {
    fn default() -> Self {
        PlacementOptions {
            compress: true,
            order: OrderMode::Sorted,
        }
    }
}

/// A rendered entry: its content and how it was produced (`None` = the entry
/// deploys no file), plus LESS warnings (`(logical path, message)`).
type RenderedEntry = (Option<(Vec<u8>, PlacedKind)>, Vec<(String, String)>);

/// Render ONE package entry to bytes — a pure per-file function (fan-out
/// friendly): LESS entries compile through `orchestrator` (which applies the
/// VariableNotation pass to compiled css) + the ModuleNotation pass; plain
/// css gets both notation passes; everything else is a byte copy.
fn render_entry(
    entry: &PackageEntry,
    orchestrator: &LessOrchestrator,
    less_opts: &LessDeployOptions,
    area: &str,
    theme_path: &str,
) -> Result<RenderedEntry, FilesError> {
    if entry.less {
        // A LESS entry that will not compile is a per-file diagnostic, not a
        // fatal run: `DeployPackage` catches the `ContentProcessorException`,
        // logs it and deploys the rest of the package, leaving no file behind.
        // Third-party stylesheets that compile on no install are common
        // (Hyva_Email's `email-inline.less` imports a lib path that resolves
        // in no fallback layer, and is absent from every real deploy on this
        // machine's stores), so a hard error here would let one of them sink
        // an otherwise complete deploy.
        let compiled = match orchestrator.compile_entry(&entry.logical, less_opts) {
            Ok(c) => c,
            Err(e) => return Ok((None, vec![(entry.logical.clone(), format!("{e}"))])),
        };
        let css = cssnotation::module_notation(&compiled.css, &entry.deployed);
        let warnings = compiled
            .warnings
            .into_iter()
            .map(|w| (entry.logical.clone(), w))
            .collect();
        return Ok((Some((css.into_bytes(), PlacedKind::LessCompiled)), warnings));
    }
    let bytes = std::fs::read(&entry.source).map_err(|e| FilesError {
        entry: Some(entry.logical.clone()),
        module: None,
        file: Some(entry.source.clone()),
        message: format!("read failed: {e}"),
    })?;
    if entry.deployed.ends_with(".css") {
        // The publish pipeline's css pre-processor chain (variable_notation
        // then module_notation; css_min inert). Non-UTF-8 css would round-
        // trip lossily — process only when valid, else copy verbatim.
        if let Ok(css) = std::str::from_utf8(&bytes) {
            let processed = cssnotation::variable_notation(css, area, theme_path);
            let processed = cssnotation::module_notation(&processed, &entry.deployed);
            if processed.as_bytes() != bytes.as_slice() {
                return Ok((Some((processed.into_bytes(), PlacedKind::CssProcessed)), Vec::new()));
            }
        }
    }
    Ok((Some((bytes, PlacedKind::Copy)), Vec::new()))
}

/// Place one theme's full package. `modules` is the enabled set in
/// **config.php load order** (what the LESS `//@magento_import` expansion,
/// requirejs assembly and view.xml merges consume); `reg_modules` the SAME
/// set in **registration order** (what the source collectors iterate — the
/// deployment order, see [`registration_order`]). `min_cache` is the
/// run-scoped `.min`-sibling bundle cache (share across themes);
/// `min_resolver` the assembled `requirejs-min-resolver.js` body
/// (theme-independent).
/// A package rendered up to — but not including — the bundle + sri step: the
/// theme-INDEPENDENT work (source resolution, per-file render/LESS, requirejs,
/// js-translation). This is the expensive part (LESS compilation dominates)
/// and it shares no state between a group's themes, so it fans out across
/// rayon. Only the bundle step reads/mutates the ordered `.min`-sibling cache
/// and must run serially, in theme order — that's [`finalize_theme`].
struct PrebuiltPackage {
    theme_id: String,
    theme_path: String,
    chain: Vec<ThemeRef>,
    /// Package files + the two requirejs artifacts + js-translation.json, in
    /// deployment order (no bundles, no sri yet).
    files: Vec<PlacedFile>,
    warnings: Vec<(String, String)>,
    /// The generated requirejs artifacts the bundler consumes as package files.
    generated: Vec<(String, String)>,
}

/// The theme-independent render (source resolution + per-file pipeline +
/// requirejs + js-translation). Pure w.r.t. other themes, so a group's themes
/// run this concurrently; the bundle step ([`finalize_theme`]) is the only
/// part that must stay ordered.
#[allow(clippy::too_many_arguments)]
fn build_theme_prebundle(
    root: &Path,
    area: &str,
    theme_id: &str,
    locale: &str,
    themes: &[(String, PathBuf)],
    modules: &[ModuleRef],
    reg_modules: &[ModuleRef],
    min_resolver: &str,
    js_translation: &str,
    opts: &PlacementOptions,
) -> Result<PrebuiltPackage, FilesError> {
    let chain = theme_chain(area, theme_id, themes)?;
    let theme_path = chain[0]
        .id
        .strip_prefix(&format!("{area}/"))
        .unwrap_or(&chain[0].id)
        .to_string();

    let entries = resolve_package(root, area, &chain, reg_modules, locale);

    let orchestrator = LessOrchestrator::new(root, area, theme_id, themes, modules.to_vec())?;
    let less_opts = LessDeployOptions {
        skip_broken_modules: false,
        compress: opts.compress,
    };

    // Render every package entry to bytes. Each entry is an independent pure
    // function of its source (LESS compiles carry a fresh resolver; the shared
    // `orchestrator` is immutable), so fan out across rayon — LESS compilation
    // dominates a package's wall time and its entries are the parallelism.
    // `collect` into an indexed Vec preserves entry order, so the deployed file
    // order (hence `sri-hashes.json` deployment order) is identical to serial,
    // and a `--jobs 1` run (one-thread pool) is byte-identical.
    use rayon::prelude::*;
    let rendered: Vec<(Option<PlacedFile>, Vec<(String, String)>)> = entries
        .par_iter()
        .map(|entry| {
            let (rendered, warns) =
                render_entry(entry, &orchestrator, &less_opts, area, &theme_path)?;
            let placed = rendered
                .map(|(content, kind)| PlacedFile { path: entry.deployed.clone(), content, kind });
            Ok((placed, warns))
        })
        .collect::<Result<Vec<_>, FilesError>>()?;
    let mut files: Vec<PlacedFile> = Vec::with_capacity(entries.len() + 16);
    let mut warnings: Vec<(String, String)> = Vec::new();
    for (pf, mut warns) in rendered {
        // `None` = the entry deploys no file (a LESS entry that would not
        // compile); its diagnostic rides along in `warns`.
        if let Some(pf) = pf {
            files.push(pf);
        }
        warnings.append(&mut warns);
    }

    // The requirejs artifacts (DeployRequireJsConfig, post-package).
    let rjs = requirejs::build(root, area, theme_id, themes, modules)?;
    files.push(PlacedFile {
        path: requirejs::CONFIG_FILE_NAME.to_string(),
        content: rjs.js.into_bytes(),
        kind: PlacedKind::RequireJs,
    });
    files.push(PlacedFile {
        path: requirejs::MIN_RESOLVER_FILE_NAME.to_string(),
        content: min_resolver.as_bytes().to_vec(),
        kind: PlacedKind::RequireJs,
    });

    // js-translation.json (DeployTranslationsDictionary): the merged js
    // dictionary — precomputed per (area, locale) by [`super::jstranslation`]
    // (theme-independent) and passed in; the constant `[]` on en_US.
    files.push(PlacedFile {
        path: DICTIONARY_FILE_NAME.to_string(),
        content: js_translation.as_bytes().to_vec(),
        kind: PlacedKind::Translation,
    });

    // The generated requirejs artifacts the bundler needs as package files.
    let rjs_config = files
        .iter()
        .find(|f| f.path == requirejs::CONFIG_FILE_NAME)
        .map(|f| String::from_utf8_lossy(&f.content).into_owned())
        .unwrap_or_default();
    let generated = vec![
        (requirejs::CONFIG_FILE_NAME.to_string(), rjs_config),
        (requirejs::MIN_RESOLVER_FILE_NAME.to_string(), min_resolver.to_string()),
    ];

    Ok(PrebuiltPackage {
        theme_id: theme_id.to_string(),
        theme_path,
        chain,
        files,
        warnings,
        generated,
    })
}

/// Finish a [`PrebuiltPackage`]: the JS bundles (Service\Bundle) followed by
/// `sri-hashes.json`. The bundler shares the ordered `.min`-sibling cache, so
/// this runs serially in a group's theme order (blank-before-luma poisoning),
/// while [`build_theme_prebundle`] already ran in parallel.
fn finalize_theme(
    pre: PrebuiltPackage,
    root: &Path,
    area: &str,
    locale: &str,
    themes: &[(String, PathBuf)],
    modules: &[ModuleRef],
    opts: &PlacementOptions,
    min_cache: &mut MinSiblingCache,
) -> Result<ThemePackage, FilesError> {
    let PrebuiltPackage {
        theme_id,
        theme_path,
        chain,
        mut files,
        warnings,
        generated,
    } = pre;

    // js/bundle/bundle<N>.js (Service\Bundle) — the bundler resolves its own
    // js/html view of the package (proven byte-exact by its own gate) and
    // needs the generated requirejs artifacts as package files.
    let bundles = bundle::build_theme(
        root,
        area,
        &theme_id,
        locale,
        themes,
        modules,
        &generated,
        &opts.order,
        min_cache,
    )?;
    for b in bundles.files {
        files.push(PlacedFile {
            path: format!("{}/{}", bundle::BUNDLE_JS_DIR, b.name),
            content: b.content.into_bytes(),
            kind: PlacedKind::Bundle,
        });
    }

    // sri-hashes.json (Magento_Csp Integrity + the requirejs/bundle
    // collector plugins): every deployed `.js`, full static-relative paths,
    // in deployment order — EXCEPT the bundles: GenerateBundleAssetIntegrity
    // runs after `Bundle::deploy` and lists them via
    // `$pubStaticDir->search("<pkg>/js/bundle/*.js")`, a SORTING glob, so
    // their entries are lexicographic (`bundle10` between `bundle1` and
    // `bundle2`). Invisible on the frontend's 7 bundles; real on the
    // backend's 11.
    let prefix = format!("{area}/{theme_path}/{locale}");
    let is_js = |f: &&PlacedFile| {
        f.path
            .rsplit('/')
            .next()
            .and_then(|b| b.rsplit_once('.'))
            .is_some_and(|(stem, ext)| !stem.is_empty() && ext.eq_ignore_ascii_case("js"))
    };
    let mut bundle_js: Vec<&PlacedFile> = files
        .iter()
        .filter(|f| f.kind == PlacedKind::Bundle)
        .filter(is_js)
        .collect();
    bundle_js.sort_by(|a, b| a.path.cmp(&b.path));
    let sri: Vec<(String, String)> = files
        .iter()
        .filter(|f| f.kind != PlacedKind::Bundle)
        .filter(is_js)
        .chain(bundle_js)
        .map(|f| (format!("{prefix}/{}", f.path), sri_hash(&f.content)))
        .collect();
    files.push(PlacedFile {
        path: SRI_HASHES_FILE_NAME.to_string(),
        content: sri_hashes_json(&sri).into_bytes(),
        kind: PlacedKind::SriHashes,
    });

    Ok(ThemePackage {
        theme: theme_id,
        theme_path,
        chain,
        files,
        warnings,
    })
}

/// Place one theme's full package: [`build_theme_prebundle`] then
/// [`finalize_theme`]. The two-phase form exists so a group's themes can render
/// in parallel and bundle in order (see [`build_group`]); this wrapper keeps
/// the single-theme path (`build_from_magento`, tests) one call.
#[allow(clippy::too_many_arguments)]
fn build_theme(
    root: &Path,
    area: &str,
    theme_id: &str,
    locale: &str,
    themes: &[(String, PathBuf)],
    modules: &[ModuleRef],
    reg_modules: &[ModuleRef],
    min_resolver: &str,
    js_translation: &str,
    opts: &PlacementOptions,
    min_cache: &mut MinSiblingCache,
) -> Result<ThemePackage, FilesError> {
    let pre = build_theme_prebundle(
        root, area, theme_id, locale, themes, modules, reg_modules, min_resolver, js_translation, opts,
    )?;
    finalize_theme(pre, root, area, locale, themes, modules, opts, min_cache)
}

/// The run-scoped, area/locale-INDEPENDENT inputs a deploy needs, computed
/// once from an open handle and shared across every `(area, theme, locale)`
/// group of a `static deploy` fan-out (so the themes/modules/min-resolver
/// aren't re-derived per group). Cheap to clone the refs; the owned data is
/// held once by the caller.
pub struct DeployInputs {
    /// `root` of the checkout.
    pub root: PathBuf,
    /// All discovered themes as `(id, dir)` (`frontend/Vendor/name`).
    pub themes: Vec<(String, PathBuf)>,
    /// Enabled modules in config.php load order (di/view/less consumers).
    pub modules: Vec<ModuleRef>,
    /// The same set in registration (deployment) order.
    pub reg_modules: Vec<ModuleRef>,
    /// Every module (enabled or not) for the js/html translation scan — the
    /// real deploy's `ComponentRegistrar` sees all installed modules.
    pub scan_modules: Vec<super::jstranslation::ScanModule>,
    /// The assembled `requirejs-min-resolver.js` body (theme-independent).
    pub min_resolver: String,
}

impl DeployInputs {
    /// Derive the shared inputs from an open handle.
    pub fn prepare(magento: &magequery_core::Magento) -> Result<Self, FilesError> {
        let themes = magento.themes();
        let modules: Vec<ModuleRef> = magento
            .modules()
            .iter()
            .filter(|m| m.enabled)
            .map(|m| ModuleRef {
                name: m.name.to_string(),
                dir: m.path.clone(),
            })
            .collect();
        let reg_modules = registration_order(magento.root(), &modules);
        // All installed modules (enabled or not) for the phrase scan.
        let scan_modules: Vec<super::jstranslation::ScanModule> = magento
            .modules()
            .iter()
            .map(|m| super::jstranslation::ScanModule { dir: m.path.clone() })
            .collect();
        let excludes = requirejs::min_resolver_excludes_from_magento(magento)?;
        let min_resolver = requirejs::min_resolver_code(&excludes);
        Ok(DeployInputs {
            root: magento.root().to_path_buf(),
            themes,
            modules,
            reg_modules,
            scan_modules,
            min_resolver,
        })
    }

    /// The theme dirs (roots) belonging to `area`, in discovery order.
    pub fn area_theme_dirs(&self, area: &str) -> Vec<PathBuf> {
        let prefix = format!("{area}/");
        self.themes
            .iter()
            .filter(|(id, _)| id.starts_with(&prefix))
            .map(|(_, dir)| dir.clone())
            .collect()
    }
}

/// The `js-translation.json` body for `(area, locale)` — theme-independent, so
/// compute once per group.
///
/// The js/html phrase scan (`extract_area_phrases`) is the expensive half, but
/// it only ever FILTERS the dictionary: with an empty dictionary the result is
/// `[]` no matter what the phrases are (the en_US case — no `i18n/en_US.csv`
/// ships). So build the dictionary FIRST (a cheap `i18n/<locale>.csv` stat
/// storm) and scan phrases only when it is non-empty. This skips the scan
/// outright for source-locale deploys, and — because this runs inside the
/// parallel group task — overlaps it across areas/groups when it is needed.
pub fn js_translation_for(
    inputs: &DeployInputs,
    area: &str,
    locale: &str,
    theme_chain_dirs: &[PathBuf],
) -> String {
    // The dictionary uses the ENABLED modules in config.php load order (the
    // real `Translate::_moduleList->getNames()`), NOT the wider registration
    // scan set — the identity-delete merge is order-sensitive.
    let dict_modules: Vec<super::jstranslation::ScanModule> = inputs
        .modules
        .iter()
        .map(|m| super::jstranslation::ScanModule { dir: m.dir.clone() })
        .collect();
    let dict = super::jstranslation::merged_dictionary(&dict_modules, theme_chain_dirs, locale);
    if dict.is_empty() {
        // No translations for this locale ⇒ `[]`, no phrase scan needed.
        return dictionary_json(&[]);
    }
    let area_phrases =
        super::jstranslation::extract_area_phrases(&inputs.root, area, &inputs.scan_modules, &inputs.area_theme_dirs(area));
    super::jstranslation::js_translation_json(&dict, &area_phrases)
}

/// Place a sequence of themes exactly like one deploy sub-run for a single
/// `(area, locale)`: given theme order, ONE shared `.min`-sibling bundle
/// cache (blank-before-luma poisoning honored), the shared inputs, and the
/// precomputed `js-translation.json` for this area+locale.
#[allow(clippy::too_many_arguments)]
pub fn build_group(
    inputs: &DeployInputs,
    area: &str,
    theme_ids: &[String],
    locale: &str,
    js_translation: &str,
    opts: &PlacementOptions,
) -> Result<Vec<ThemePackage>, FilesError> {
    use rayon::prelude::*;
    // Phase A — the theme-independent render (source resolution + per-file
    // pipeline + requirejs + js-translation), where LESS compilation dominates.
    // A group's themes share no state here, so fan them out; `collect` keeps
    // theme order for the ordered phase B below.
    let prebuilt: Vec<PrebuiltPackage> = theme_ids
        .par_iter()
        .map(|theme_id| {
            build_theme_prebundle(
                &inputs.root,
                area,
                theme_id,
                locale,
                &inputs.themes,
                &inputs.modules,
                &inputs.reg_modules,
                &inputs.min_resolver,
                js_translation,
                opts,
            )
        })
        .collect::<Result<Vec<_>, FilesError>>()?;

    // Phase B — the bundle step reads and mutates the ordered `.min`-sibling
    // cache, so finalize the themes serially in their given order on ONE fresh
    // cache (blank-before-luma poisoning preserved). Cheap relative to phase A,
    // and identical whether phase A ran parallel or serial.
    let mut min_cache = MinSiblingCache::new();
    let mut out = Vec::with_capacity(prebuilt.len());
    for pre in prebuilt {
        out.push(finalize_theme(
            pre,
            &inputs.root,
            area,
            locale,
            &inputs.themes,
            &inputs.modules,
            opts,
            &mut min_cache,
        )?);
    }
    Ok(out)
}

/// Place a sequence of themes exactly like one deploy run: given order, one
/// shared `.min`-sibling bundle cache, one min-resolver assembly. The
/// `js-translation.json` is generated faithfully for the locale (empty `[]`
/// for locales without a dictionary, like en_US).
pub fn build_from_magento(
    magento: &magequery_core::Magento,
    area: &str,
    theme_ids: &[String],
    locale: &str,
    opts: &PlacementOptions,
) -> Result<Vec<ThemePackage>, FilesError> {
    let inputs = DeployInputs::prepare(magento)?;
    // Theme chain dirs for the dictionary: use the first theme's chain when
    // present, else no theme overlay. Theme i18n rarely differs; the extracted
    // set is theme-independent regardless.
    let chain_dirs: Vec<PathBuf> = theme_ids
        .first()
        .and_then(|id| theme_chain(area, id, &inputs.themes).ok())
        .map(|chain| {
            chain
                .iter()
                .filter_map(|t| inputs.themes.iter().find(|(tid, _)| tid == &t.id).map(|(_, d)| d.clone()))
                .collect()
        })
        .unwrap_or_default();
    let js_translation = js_translation_for(&inputs, area, locale, &chain_dirs);
    build_group(&inputs, area, theme_ids, locale, &js_translation, opts)
}

/// The deployed package root:
/// `<static root>/<area>/<Vendor>/<theme>/<locale>`.
pub fn package_dir(static_root: &Path, area: &str, theme_id: &str, locale: &str) -> PathBuf {
    let theme_path = theme_id
        .strip_prefix(&format!("{area}/"))
        .unwrap_or(theme_id);
    static_root.join(area).join(theme_path).join(locale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    // ---- json --------------------------------------------------------------

    /// PHP json_encode DEFAULT flags: `/` escaped, empty map = `[]` (the
    /// js-translation.json en_US bytes), object otherwise.
    #[test]
    fn php_default_json_escapes_slashes() {
        assert_eq!(php_json_encode_map_default(&[]), "[]");
        assert_eq!(dictionary_json(&[]), "[]");
        assert_eq!(
            php_json_encode_map_default(&[("a/b.js", "sha256-x/y=")]),
            r#"{"a\/b.js":"sha256-x\/y="}"#
        );
        assert_eq!(
            dictionary_json(&[("Add to Cart", "In den Warenkorb")]),
            r#"{"Add to Cart":"In den Warenkorb"}"#
        );
    }

    /// The Csp hash format over known bytes.
    #[test]
    fn sri_hash_format() {
        // sha256("") = e3b0c442... → base64 47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=
        assert_eq!(
            sri_hash(b""),
            "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="
        );
    }

    // ---- registration order ------------------------------------------------

    #[test]
    fn registration_order_prefers_autoload_files() {
        let td = tempfile::tempdir().unwrap();
        let r = td.path();
        let w = |rel: &str, content: &str| {
            let p = r.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        };
        w("vendor/acme/module-b/registration.php", "<?php");
        w("vendor/acme/module-a/registration.php", "<?php");
        w("app/code/Zeta/Widgets/registration.php", "<?php");
        w("app/code/Alpha/Widgets/registration.php", "<?php");
        w(
            "vendor/composer/autoload_files.php",
            "<?php\nreturn array(\n\
             'h1' => $vendorDir . '/acme/module-b/registration.php',\n\
             'h2' => $vendorDir . '/acme/module-a/registration.php',\n\
             'h3' => $vendorDir . '/other/files.php',\n\
             );\n",
        );
        // Input in config.php-ish order (NOT registration order).
        let modules = vec![
            ModuleRef { name: s("Acme_A"), dir: r.join("vendor/acme/module-a") },
            ModuleRef { name: s("Zeta_Widgets"), dir: r.join("app/code/Zeta/Widgets") },
            ModuleRef { name: s("Acme_B"), dir: r.join("vendor/acme/module-b") },
            ModuleRef { name: s("Alpha_Widgets"), dir: r.join("app/code/Alpha/Widgets") },
        ];
        let ordered = registration_order(r, &modules);
        let names: Vec<&str> = ordered.iter().map(|m| m.name.as_str()).collect();
        // vendor in autoload order (b before a), then app/code sorted.
        assert_eq!(names, vec!["Acme_B", "Acme_A", "Alpha_Widgets", "Zeta_Widgets"]);
    }

    // ---- resolution over a synthetic tree ----------------------------------

    /// A synthetic root exercising the collector layers for NON-js types.
    fn synth_root() -> tempfile::TempDir {
        let td = tempfile::tempdir().unwrap();
        let r = td.path();
        let w = |rel: &str, content: &str| {
            let p = r.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        };
        // lib: root file, css/docs exclusion, a `.min` name (copied
        // verbatim, no sibling synthesis).
        w("lib/web/spacer.gif", "GIF-LIB");
        w("lib/web/css/docs/docs.less", "EXCLUDED");
        w("lib/web/legacy-build.min.js", "MIN-LIB");
        // module: base + area css, a partial, an i18n overlay of a css.
        w("app/code/Acme/Widgets/etc/module.xml", "<config/>");
        w("app/code/Acme/Widgets/view/base/web/css/w.css", "BASE-CSS");
        w("app/code/Acme/Widgets/view/frontend/web/css/_part.less", "PARTIAL");
        w("app/code/Acme/Widgets/view/frontend/web/img/logo.svg", "SVG-PLAIN");
        w(
            "app/code/Acme/Widgets/view/frontend/web/i18n/nl_NL/img/logo.svg",
            "SVG-NL",
        );
        // theme: overrides lib file; own plain css with a placeholder url;
        // module context override; theme i18n overlay.
        w("vendor/acme/theme-base/theme.xml", "<theme><title>B</title></theme>");
        w("vendor/acme/theme-base/web/spacer.gif", "GIF-THEME");
        w(
            "vendor/acme/theme-base/web/css/mail.css",
            "@import url(\"{{base_url_path}}/css/f.css\");",
        );
        w("vendor/acme/theme-base/Acme_Widgets/web/css/w.css", "THEME-CSS");
        w(
            "vendor/acme/theme-base/web/i18n/nl_NL/css/local.css",
            "NL-ONLY",
        );
        w("vendor/acme/theme-base/web/css/local.css", "PLAIN-LOCAL");
        td
    }

    fn refs(r: &Path) -> (Vec<(String, PathBuf)>, Vec<ModuleRef>) {
        (
            vec![(s("frontend/Acme/base"), r.join("vendor/acme/theme-base"))],
            vec![ModuleRef {
                name: s("Acme_Widgets"),
                dir: r.join("app/code/Acme/Widgets"),
            }],
        )
    }

    fn entry<'a>(entries: &'a [PackageEntry], path: &str) -> &'a PackageEntry {
        entries
            .iter()
            .find(|e| e.deployed == path)
            .unwrap_or_else(|| panic!("no entry {path}"))
    }

    /// Layering + skip rules: lib override keeps the lib POSITION with the
    /// theme SOURCE; partials skipped; css/docs pruned; `.min` names copied
    /// as-is with no sibling synthesis; theme context overrides module css.
    #[test]
    fn resolve_package_layers_and_skips() {
        let td = synth_root();
        let r = td.path();
        let (themes, modules) = refs(r);
        let chain = theme_chain("frontend", "Acme/base", &themes).unwrap();
        let entries = resolve_package(r, "frontend", &chain, &modules, "en_US");
        let paths: Vec<&str> = entries.iter().map(|e| e.deployed.as_str()).collect();

        // Theme override of a lib file: value = theme source, position = lib's.
        let spacer = entry(&entries, "spacer.gif");
        assert!(spacer.source.ends_with("theme-base/web/spacer.gif"));
        assert!(
            paths.iter().position(|p| *p == "spacer.gif").unwrap()
                < paths.iter().position(|p| *p == "Acme_Widgets/css/w.css").unwrap(),
            "lib position kept on theme override"
        );
        // Context override of the module css.
        assert!(entry(&entries, "Acme_Widgets/css/w.css")
            .source
            .ends_with("theme-base/Acme_Widgets/web/css/w.css"));
        // Partials and css/docs never deploy.
        assert!(!paths.iter().any(|p| p.contains("_part") || p.contains("css/docs")));
        // `.min` source name deploys verbatim; no plain sibling appears.
        assert!(paths.contains(&"legacy-build.min.js"));
        assert!(!paths.contains(&"legacy-build.js"));
        // Other locales' i18n overlays don't deploy for en_US.
        assert_eq!(
            entry(&entries, "css/local.css").source,
            r.join("vendor/acme/theme-base/web/css/local.css")
        );
    }

    /// The i18n overlay for a NON-js type: the nl_NL build resolves the
    /// module's translated svg and the theme's locale-only css.
    #[test]
    fn i18n_overlay_for_non_js_types() {
        let td = synth_root();
        let r = td.path();
        let (themes, modules) = refs(r);
        let chain = theme_chain("frontend", "Acme/base", &themes).unwrap();
        let nl = resolve_package(r, "frontend", &chain, &modules, "nl_NL");
        assert!(entry(&nl, "Acme_Widgets/img/logo.svg")
            .source
            .ends_with("i18n/nl_NL/img/logo.svg"));
        // The theme's own i18n overlay wins over its plain file…
        assert!(entry(&nl, "css/local.css").source.ends_with("i18n/nl_NL/css/local.css"));
        // …and the en_US build sees the plain file (asserted above).
        let en = resolve_package(r, "frontend", &chain, &modules, "en_US");
        assert!(entry(&en, "Acme_Widgets/img/logo.svg")
            .source
            .ends_with("frontend/web/img/logo.svg"));
    }

    /// End-to-end placement over the synthetic root: the plain-copy engine,
    /// the notation pass on a plain css, the generated artifacts and the
    /// sri map (js only, deployment order, escaped slashes).
    #[test]
    fn placement_end_to_end() {
        let td = synth_root();
        let r = td.path();
        let (themes, modules) = refs(r);
        let mut cache = MinSiblingCache::new();
        let pkg = build_theme(
            r,
            "frontend",
            "Acme/base",
            "en_US",
            &themes,
            &modules,
            &modules,
            "RESOLVER",
            "[]",
            &PlacementOptions::default(),
            &mut cache,
        )
        .expect("build");

        let by_path = |p: &str| {
            pkg.files
                .iter()
                .find(|f| f.path == p)
                .unwrap_or_else(|| panic!("no file {p}"))
        };
        // Plain copy.
        assert_eq!(by_path("spacer.gif").content, b"GIF-THEME");
        assert_eq!(by_path("spacer.gif").kind, PlacedKind::Copy);
        // The VariableNotation pass on a plain css.
        assert_eq!(
            String::from_utf8_lossy(&by_path("css/mail.css").content),
            "@import url(\"{{base_url_path}}frontend/Acme/base/{{locale}}/css/f.css\");"
        );
        assert_eq!(by_path("css/mail.css").kind, PlacedKind::CssProcessed);
        // Generated artifacts present.
        assert_eq!(by_path(DICTIONARY_FILE_NAME).content, b"[]");
        assert_eq!(by_path("requirejs-min-resolver.js").content, b"RESOLVER");
        assert!(by_path("requirejs-config.js").kind == PlacedKind::RequireJs);
        // sri: every .js (the lib .min, the two rjs artifacts, bundles),
        // full static-relative paths with escaped slashes, no non-js.
        let sri = String::from_utf8_lossy(&by_path(SRI_HASHES_FILE_NAME).content).into_owned();
        assert!(sri.contains(r#""frontend\/Acme\/base\/en_US\/legacy-build.min.js":"sha256-"#));
        assert!(sri.contains(r#"requirejs-config.js"#));
        assert!(!sri.contains("spacer.gif"));
        // deployment order: package js before the rjs artifacts.
        let a = sri.find("legacy-build.min.js").unwrap();
        let b = sri.find("requirejs-config.js").unwrap();
        assert!(a < b);
    }

    /// A LESS entry that will not compile must not sink the package: a real
    /// `setup:static-content:deploy` logs the `ContentProcessorException` and
    /// deploys everything else, leaving no file for it. Third-party stylesheets
    /// that compile on no install exist in the wild (Hyva_Email's
    /// `email-inline.less`), so the failure has to be a per-entry warning.
    #[test]
    fn broken_less_entry_warns_and_deploys_the_rest() {
        let td = synth_root();
        let r = td.path();
        std::fs::write(
            r.join("app/code/Acme/Widgets/view/frontend/web/css/broken.less"),
            "@import 'no/such/_partial.less';\n",
        )
        .unwrap();
        let (themes, modules) = refs(r);
        let mut cache = MinSiblingCache::new();
        let pkg = build_theme(
            r,
            "frontend",
            "Acme/base",
            "en_US",
            &themes,
            &modules,
            &modules,
            "RESOLVER",
            "[]",
            &PlacementOptions::default(),
            &mut cache,
        )
        .expect("a broken LESS entry must not fail the build");

        // No file for the entry that would not compile...
        assert!(!pkg.files.iter().any(|f| f.path == "Acme_Widgets/css/broken.css"));
        // ...but a warning naming it, carrying the compiler's diagnostic.
        let warn = pkg
            .warnings
            .iter()
            .find(|(logical, _)| logical.contains("broken"))
            .expect("the failure is reported as a warning");
        assert!(warn.1.contains("no/such/_partial.less"), "diagnostic kept: {}", warn.1);
        // ...and the rest of the package is untouched.
        assert!(pkg.files.iter().any(|f| f.path == "spacer.gif"));
        assert!(pkg.files.iter().any(|f| f.path == SRI_HASHES_FILE_NAME));
    }

    #[test]
    fn package_dir_shape() {
        assert_eq!(
            package_dir(Path::new("/srv/pub/static"), "frontend", "Magento/luma", "en_US"),
            PathBuf::from("/srv/pub/static/frontend/Magento/luma/en_US")
        );
    }
}
