//! `js/bundle/bundle<N>.js` generation — Magento's SCD JS bundling
//! (`Magento\Deploy\Service\Bundle` + `Package\Bundle\RequireJs`), reproduced
//! in pure Rust from the SOURCE tree (no deployed `pub/static` needed).
//!
//! ## What the real deploy does
//!
//! After every package is materialized, `DeployStaticContent` runs
//! `Service\Bundle::deploy(area, theme, locale)` per non-virtual package —
//! **unconditionally** (the `dev/js/enable_js_bundling` config flag gates only
//! runtime page inclusion, never generation; only `--no-js-bundle` /
//! `--no-javascript` skip it). The service:
//!
//! 1. deletes `<package>/js/bundle`, then recursively globs the DEPLOYED
//!    package dir (`Files::getFiles([$dir], '*.*')` — per directory: files
//!    matching `*.*` first, then every subdirectory, both in **readdir order**
//!    via `GLOB_NOSORT`);
//! 2. keeps only `js`/`html` extensions;
//! 3. drops files via `hasMinVersion` (the `.min` sibling cache — see below)
//!    and `isExcluded` (theme `etc/view.xml` `<exclude>` items);
//! 4. pools survivors — `js` → `jsbuild`, `html` → `text` — and flushes each
//!    pool into `bundle<N>.js` files capped by the view var
//!    `Js_Bundle`/`bundle_size` (1MB on blank/luma), numbering continuous
//!    across pools, the very last file getting the RequireJS init snippet.
//!
//! ## Reproducing the deployed tree from source
//!
//! Bundling reads the DEPLOYED package, so this module first builds that view
//! of the world from the pristine source tree ([`deployed_tree`]): the
//! js/html subset of the static-deploy file resolution —
//!
//! - `lib/web/**` at the package root (minus `css/docs/**`,
//!   `Files::getStaticLibraryFiles`'s one exclusion);
//! - per enabled module (load order): `view/base/web/**` then
//!   `view/<area>/web/**` under `<Vendor_Module>/`, then the
//!   `view/{base,<area>}/web/i18n/<locale>/**` overlays at the stripped path;
//! - per theme, ancestor-first: `<theme>/web/**` at the package root,
//!   `<theme>/web/i18n/<locale>/**` overlaid, then each
//!   `<theme>/<Vendor_Module>/web/**` (+ i18n overlay) under the module dir —
//!   later layers win on path collision;
//! - the two generated artifacts the deploy adds before bundling runs:
//!   `requirejs-config.js` and `requirejs-min-resolver.js` (assembled by
//!   [`super::requirejs`] — both are bundled on real deploys).
//!
//! **Covered subset**: only `.js`/`.html` files are resolved (all the bundler
//! reads). CSS/LESS compilation, images, fonts, `js-translation.json` and
//! friends are out of scope here; `.min.js` *sources* are ordinary files and
//! are included. Validated against a real bundled deploy: the js/html file
//! set and every file's bytes match the deployed tree exactly.
//!
//! ## Ordering — the byte-faithfulness crux
//!
//! In-bundle key order and split points follow PHP's `GLOB_NOSORT` readdir
//! order of the deployed tree: **filesystem-dependent, not sorted**. On
//! hash-ordered filesystems (ext4 htree — the common server case) the order
//! is a pure function of each directory's name set, so [`OrderMode::Probe`]
//! reproduces it exactly: for every virtual directory it creates the names in
//! a scratch directory on the target filesystem and reads them back
//! (verified: probe order == deployed readdir order at every directory size,
//! independent of creation order and file-vs-dir type). On creation-ordered
//! filesystems (tmpfs) the probe degrades to its own creation order (sorted —
//! deterministic, but not what a historical PHP deploy produced there).
//! [`OrderMode::Sorted`] is the portable, filesystem-independent alternative.
//!
//! ## The `.min` sibling cache — shared across themes
//!
//! `hasMinVersion` keeps files containing `.min.` and records their non-min
//! name in a cache; a plain file is dropped iff its `.min` sibling was
//! iterated EARLIER. (The `isExist` sibling probe resolves against the
//! `pub/static` ROOT, not the package dir, so it never fires for package
//! files — upstream bug, reproduced by omission.) Crucially the cache lives
//! on the **service**, which the deploy creates ONCE for all packages: a
//! `.min` pair iterated in the first theme's package poisons the plain name
//! for every later theme in the same run (real effect: `vimeo/player.js` and
//! `Magento_PageBuilder/.../slick.js` are bundled for blank but absent from
//! luma when both deploy together). [`build_themes`] therefore processes the
//! requested themes in order with one shared cache, exactly like one deploy
//! run; a single-theme invocation equals a single-theme deploy.
//!
//! ## Bundle file format (locked byte-for-byte)
//!
//! `require.config({"config": {\n        "<pool>":<one-line JSON>\n}});\n`
//! where the JSON is PHP `json_encode($map, JSON_UNESCAPED_SLASHES)`: keys in
//! insertion order, `/` literal, non-ASCII as lowercase `\uXXXX` (surrogate
//! pairs for astral), `{}` for an empty map ([`php_json_encode_map`]). Size
//! accounting is fractional KB of `mb_strlen` — UTF-8 **character count**,
//! not bytes, not JSON-escaped length — with a strict `>` comparison, so real
//! files routinely exceed the nominal cap once escaping and keys are added.
//! The init snippet ([`INIT_JS`]) is appended to the last file created.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::less::{is_module_segment, theme_chain, ModuleRef, ThemeRef};
use super::requirejs;

/// Faults reuse the static-deploy error shape (theme/module/file provenance +
/// message), like [`super::requirejs`].
pub type BundleError = super::less::LessDeployError;

/// The bundle directory within a package — `BundleInterface::BUNDLE_JS_DIR`.
pub const BUNDLE_JS_DIR: &str = "js/bundle";

/// The init snippet appended to the LAST bundle file created
/// (`Package\Bundle\RequireJs::getInitJs`), verbatim.
pub const INIT_JS: &str = "require.config({\n\
    \x20   bundles: {\n\
    \x20       'mage/requirejs/static': [\n\
    \x20           'jsbuild',\n\
    \x20           'buildTools',\n\
    \x20           'text',\n\
    \x20           'statistician'\n\
    \x20       ]\n\
    \x20   },\n\
    \x20   deps: [\n\
    \x20       'jsbuild'\n\
    \x20   ]\n\
    });\n";

fn err(message: String) -> BundleError {
    BundleError {
        entry: None,
        module: None,
        file: None,
        message,
    }
}

// ---------------------------------------------------------------------------
// view.xml — excludes + bundle_size
// ---------------------------------------------------------------------------

/// The bundling-relevant slice of the merged view config for (area, theme):
/// `<exclude><item type="file|directory">` entries (already run through
/// `prepareExcludePath`: `Lib::p` → `p`, `Module::p` → `Module/p`) and the
/// `Js_Bundle`/`bundle_size` view var.
#[derive(Debug, Clone, Default)]
pub struct BundleViewConfig {
    /// Prepared file excludes, in declaration order.
    pub excluded_files: Vec<String>,
    /// Prepared directory excludes (raw string-prefix matched, like PHP's
    /// `strpos(...) === 0` — a dir exclude `foo` also kills `foobar/x.js`).
    pub excluded_dirs: Vec<String>,
    /// The raw `bundle_size` var value (`"1MB"`), when declared.
    pub bundle_size: Option<String>,
}

impl BundleViewConfig {
    /// `BundleConfig::getBundleFileMaxSize`, faithfully: strip non-letters
    /// for the unit, uppercase; `KB` → the leading int; `MB` → int × 1024;
    /// anything else (incl. no var at all) → int/1024 truncated — so a bare
    /// number is bytes, and a missing var yields **0** (PHP's
    /// `(int)(null/1024)`).
    pub fn max_size_kb(&self) -> f64 {
        let raw = self.bundle_size.as_deref().unwrap_or("");
        let unit: String = raw
            .chars()
            .filter(|c| c.is_ascii_alphabetic())
            .collect::<String>()
            .to_uppercase();
        let int = php_leading_int(raw);
        match unit.as_str() {
            "KB" => int as f64,
            "MB" => (int * 1024) as f64,
            _ => ((int as f64) / 1024.0).trunc(),
        }
    }
}

/// PHP `(int)` cast of a string: optional sign + leading digits, 0 otherwise.
fn php_leading_int(s: &str) -> i64 {
    let t = s.trim_start();
    let (sign, rest) = match t.strip_prefix('-') {
        Some(r) => (-1, r),
        None => (1, t.strip_prefix('+').unwrap_or(t)),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    sign * digits.parse::<i64>().unwrap_or(0)
}

/// Merge the view config the bundler consults: each module's `etc/view.xml`
/// (load order), then the theme chain's `etc/view.xml` ancestor-first —
/// Magento merges `<exclude><item>` by (type, item) id (a re-declaration is
/// identity) and later `bundle_size` declarations win. `chain` is CHILD-first
/// (as [`theme_chain`] returns it); it is walked in reverse here.
pub fn view_bundle_config(chain: &[ThemeRef], modules: &[ModuleRef]) -> BundleViewConfig {
    let mut cfg = BundleViewConfig::default();
    let mut seen: BTreeSet<(bool, String)> = BTreeSet::new();
    let mut merge_file = |cfg: &mut BundleViewConfig, path: PathBuf| {
        let Ok(xml) = std::fs::read_to_string(path) else {
            return;
        };
        let xml = strip_xml_comments(&xml);
        for (is_dir, item) in view_exclude_items(&xml) {
            let prepared = prepare_exclude_path(&item);
            if seen.insert((is_dir, prepared.clone())) {
                if is_dir {
                    cfg.excluded_dirs.push(prepared);
                } else {
                    cfg.excluded_files.push(prepared);
                }
            }
        }
        if let Some(size) = view_bundle_size(&xml) {
            cfg.bundle_size = Some(size);
        }
    };
    for m in modules {
        merge_file(&mut cfg, m.dir.join("etc").join("view.xml"));
    }
    for t in chain.iter().rev() {
        merge_file(&mut cfg, t.dir.join("etc").join("view.xml"));
    }
    cfg
}

/// `<exclude><item type="file|directory">X</item>` entries of one view.xml,
/// in document order, as (is_directory, raw item) pairs. Tolerant text scan
/// (the file is schema-fixed; comments already stripped).
fn view_exclude_items(xml: &str) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    let Some(start) = xml.find("<exclude>") else {
        return out;
    };
    let body = match xml[start..].find("</exclude>") {
        Some(end) => &xml[start..start + end],
        None => &xml[start..],
    };
    let mut rest = body;
    while let Some(at) = rest.find("<item") {
        rest = &rest[at + "<item".len()..];
        let Some(gt) = rest.find('>') else { break };
        let attrs = &rest[..gt];
        rest = &rest[gt + 1..];
        let Some(close) = rest.find("</item>") else { break };
        let text = rest[..close].trim().to_string();
        rest = &rest[close + "</item>".len()..];
        let is_dir = attr_value(attrs, "type").as_deref() == Some("directory");
        if !text.is_empty() {
            out.push((is_dir, text));
        }
    }
    out
}

/// `<vars module="Js_Bundle"><var name="bundle_size">1MB</var></vars>`.
fn view_bundle_size(xml: &str) -> Option<String> {
    let mut rest = xml;
    while let Some(at) = rest.find("<vars") {
        rest = &rest[at + "<vars".len()..];
        let gt = rest.find('>')?;
        let attrs = &rest[..gt];
        rest = &rest[gt + 1..];
        let end = rest.find("</vars>")?;
        let body = &rest[..end];
        rest = &rest[end..];
        if attr_value(attrs, "module").as_deref() != Some("Js_Bundle") {
            continue;
        }
        let mut vb = body;
        while let Some(v) = vb.find("<var") {
            vb = &vb[v + "<var".len()..];
            let Some(gt) = vb.find('>') else { break };
            let vattrs = &vb[..gt];
            vb = &vb[gt + 1..];
            let Some(close) = vb.find("</var>") else { break };
            let text = vb[..close].trim().to_string();
            vb = &vb[close + "</var>".len()..];
            if attr_value(vattrs, "name").as_deref() == Some("bundle_size") {
                return Some(text);
            }
        }
    }
    None
}

/// `name="value"` (or `'value'`) lookup in an attribute string.
fn attr_value(attrs: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let at = attrs.find(&needle)?;
    let rest = &attrs[at + needle.len()..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &rest[1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// Remove `<!-- ... -->` spans (shared shape with `less::strip_xml_comments`,
/// kept private there).
fn strip_xml_comments(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(at) = rest.find("<!--") {
        out.push_str(&rest[..at]);
        match rest[at..].find("-->") {
            Some(end) => rest = &rest[at + end + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// `Service\Bundle::prepareExcludePath`: `Lib::p` → `p` (lib files deploy at
/// the package root); `Module::p` → `Module/p`; no `::` → as-is.
pub fn prepare_exclude_path(item: &str) -> String {
    match item.split_once("::") {
        Some(("Lib", path)) => path.to_string(),
        Some((module, path)) => format!("{module}/{path}"),
        None => item.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The virtual deployed tree (js/html subset)
// ---------------------------------------------------------------------------

/// Build the js/html subset of the deployed package as a map of
/// package-relative path → source file (see the module docs for the layer
/// order; later layers overwrite). Deterministic; pure file inspection.
pub fn deployed_tree(
    root: &Path,
    area: &str,
    chain: &[ThemeRef],
    modules: &[ModuleRef],
    locale: &str,
) -> std::collections::BTreeMap<String, PathBuf> {
    let mut map = std::collections::BTreeMap::new();

    // lib/web at the package root, minus css/docs (getStaticLibraryFiles).
    walk_into(&mut map, &root.join("lib").join("web"), "", &["css/docs"]);
    map.retain(|p, _| !p.starts_with("css/docs"));

    // Modules: base then <area>, plain then the i18n/<locale> overlay.
    for m in modules {
        for sub in ["base", area] {
            let web = m.dir.join("view").join(sub).join("web");
            walk_into_ns(&mut map, &web, &m.name, locale);
        }
    }

    // Themes, ancestor-first (the chain is child-first).
    for t in chain.iter().rev() {
        walk_into_ns(&mut map, &t.dir.join("web"), "", locale);
        let Ok(rd) = std::fs::read_dir(&t.dir) else {
            continue;
        };
        let mut contexts: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| is_module_segment(n))
            .collect();
        contexts.sort(); // deterministic; namespaced paths cannot collide
        for ctx in contexts {
            walk_into_ns(&mut map, &t.dir.join(&ctx).join("web"), &ctx, locale);
        }
    }

    map
}

/// Walk `web/` (skipping the top-level `i18n/` subtree), then overlay
/// `web/i18n/<locale>/` at the stripped path — the deploy's locale handling
/// (`Files::getStaticPreProcessingFiles` parses only a top-level
/// `web/i18n/<locale>/` segment).
fn walk_into_ns(
    map: &mut std::collections::BTreeMap<String, PathBuf>,
    web: &Path,
    ns: &str,
    locale: &str,
) {
    walk_into(map, web, ns, &["i18n"]);
    walk_into(map, &web.join("i18n").join(locale), ns, &[]);
}

/// Recursively add every js/html file under `base` to `map` at
/// `<ns>/<relative>` (or `<relative>` for an empty ns). `skip` prunes
/// base-relative prefixes. Hidden entries (leading `.`) are skipped — PHP's
/// glob never matches them (`FNM_PERIOD`), so they can't reach a bundle.
fn walk_into(
    map: &mut std::collections::BTreeMap<String, PathBuf>,
    base: &Path,
    ns: &str,
    skip: &[&str],
) {
    fn rec(map: &mut std::collections::BTreeMap<String, PathBuf>, dir: &Path, rel: &str, ns: &str, skip: &[&str]) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if skip.iter().any(|s| child_rel == *s) {
                continue;
            }
            let path = e.path();
            if path.is_dir() {
                rec(map, &path, &child_rel, ns, skip);
            } else if is_js_html(&name) {
                let key = if ns.is_empty() {
                    child_rel
                } else {
                    format!("{ns}/{child_rel}")
                };
                map.insert(key, path);
            }
        }
    }
    rec(map, base, "", ns, skip);
}

/// A bundleable name: `.js`/`.html` extension (case-sensitive, like PHP's
/// `in_array`) with a real dot (a leading-dot name is hidden, not an
/// extension).
fn is_js_html(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((stem, ext)) => !stem.is_empty() && (ext == "js" || ext == "html"),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

/// How directory listings are ordered when simulating the deployed tree's
/// recursive glob.
#[derive(Debug, Clone)]
pub enum OrderMode {
    /// Lexicographic (byte) order — portable and filesystem-independent, but
    /// NOT what a PHP deploy produces on hash-ordered filesystems.
    Sorted,
    /// Reproduce the target filesystem's readdir order by creating each
    /// directory's names in a scratch dir under the given root and reading
    /// them back. Byte-faithful to a real deploy on name-hash-ordered
    /// filesystems (ext4 htree); degrades to sorted on creation-ordered ones.
    Probe(PathBuf),
}

/// The per-name-set order oracle. Probe results are cached (name sets repeat
/// across themes), and the scratch tree is removed on drop.
struct Orderer {
    mode: OrderMode,
    cache: HashMap<Vec<String>, Vec<String>>,
    counter: usize,
    probe_base: Option<PathBuf>,
}

impl Orderer {
    fn new(mode: OrderMode) -> Self {
        Orderer {
            mode,
            cache: HashMap::new(),
            counter: 0,
            probe_base: None,
        }
    }

    /// Order `names` (a sorted set) as a directory listing.
    fn order(&mut self, names: &BTreeSet<String>) -> Result<Vec<String>, BundleError> {
        let key: Vec<String> = names.iter().cloned().collect();
        match &self.mode {
            OrderMode::Sorted => Ok(key),
            OrderMode::Probe(root) => {
                if let Some(hit) = self.cache.get(&key) {
                    return Ok(hit.clone());
                }
                let base = match &self.probe_base {
                    Some(b) => b.clone(),
                    None => {
                        // Unique per Orderer, not just per process — tests
                        // (and future parallel callers) may hold several.
                        static SEQ: std::sync::atomic::AtomicUsize =
                            std::sync::atomic::AtomicUsize::new(0);
                        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let b = root.join(format!(
                            ".mq-order-probe-{}-{seq}",
                            std::process::id()
                        ));
                        std::fs::create_dir_all(&b).map_err(|e| {
                            err(format!("create order-probe dir {}: {e}", b.display()))
                        })?;
                        self.probe_base = Some(b.clone());
                        b
                    }
                };
                let dir = base.join(format!("p{}", self.counter));
                self.counter += 1;
                std::fs::create_dir(&dir)
                    .map_err(|e| err(format!("create probe dir {}: {e}", dir.display())))?;
                for n in &key {
                    std::fs::File::create(dir.join(n))
                        .map_err(|e| err(format!("probe touch {n}: {e}")))?;
                }
                let mut got = Vec::with_capacity(key.len());
                let rd = std::fs::read_dir(&dir)
                    .map_err(|e| err(format!("probe readdir {}: {e}", dir.display())))?;
                for e in rd {
                    let e = e.map_err(|e| err(format!("probe readdir entry: {e}")))?;
                    if let Some(n) = e.file_name().to_str() {
                        got.push(n.to_string());
                    }
                }
                self.cache.insert(key, got.clone());
                Ok(got)
            }
        }
    }
}

impl Drop for Orderer {
    fn drop(&mut self) {
        if let Some(base) = &self.probe_base {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

/// Simulate `Files::getFiles([$packageDir], '*.*')` over the virtual tree:
/// per directory, files first (only names with a dot — the `*.*` pattern),
/// then each subdirectory recursively; both lists in `order`'s readdir order.
/// `paths` uses forward slashes; the result is the bundler's iteration order.
fn glob_order(
    paths: &BTreeSet<String>,
    orderer: &mut Orderer,
) -> Result<Vec<String>, BundleError> {
    // dir path ("" = root) → (file names, subdir names)
    #[derive(Default)]
    struct DirNode {
        files: BTreeSet<String>,
        dirs: BTreeSet<String>,
    }
    let mut tree: HashMap<String, DirNode> = HashMap::new();
    for p in paths.iter() {
        let parts: Vec<&str> = p.split('/').collect();
        for i in 0..parts.len() - 1 {
            let parent = parts[..i].join("/");
            tree.entry(parent)
                .or_default()
                .dirs
                .insert(parts[i].to_string());
        }
        let parent = parts[..parts.len() - 1].join("/");
        tree.entry(parent)
            .or_default()
            .files
            .insert(parts[parts.len() - 1].to_string());
    }

    fn rec(
        tree: &HashMap<String, DirNode>,
        orderer: &mut Orderer,
        dir: &str,
        out: &mut Vec<String>,
    ) -> Result<(), BundleError> {
        let Some(node) = tree.get(dir) else {
            return Ok(());
        };
        let mut all: BTreeSet<String> = node.files.clone();
        all.extend(node.dirs.iter().cloned());
        let ordered = orderer.order(&all)?;
        let join = |n: &str| {
            if dir.is_empty() {
                n.to_string()
            } else {
                format!("{dir}/{n}")
            }
        };
        for n in &ordered {
            if node.files.contains(n) && n.contains('.') && !n.starts_with('.') {
                out.push(join(n));
            }
        }
        for n in &ordered {
            if node.dirs.contains(n) {
                rec(tree, orderer, &join(n), out)?;
            }
        }
        Ok(())
    }
    let mut out = Vec::with_capacity(paths.len());
    rec(&tree, orderer, "", &mut out)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Filters (hasMinVersion + isExcluded)
// ---------------------------------------------------------------------------

/// The `.min` sibling exclusion cache — `Service\Bundle::$excludedCache`.
/// The real service is instantiated ONCE per deploy run, so the cache is
/// shared across every package (theme) of that run; keep one instance across
/// [`build_theme`] calls to reproduce a multi-theme deploy.
#[derive(Debug, Default)]
pub struct MinSiblingCache(BTreeSet<String>);

impl MinSiblingCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// `hasMinVersion($filePath)`: cached names are dropped; a path
    /// containing `.min.` is kept but poisons its plain sibling's name.
    /// (The upstream `isExist` fallback probes relative to the `pub/static`
    /// root — never the package — so it is faithfully not implemented.)
    fn has_min_version(&mut self, path: &str) -> bool {
        if self.0.contains(path) {
            return true;
        }
        if path.contains(".min.") {
            let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
            self.0
                .insert(path.replace(&format!(".min.{ext}"), &format!(".{ext}")));
        }
        false
    }
}

/// `Service\Bundle::isExcluded`: exact file match (also matching a deployed
/// `.min.js` against an exclude naming the plain `.js`), then raw
/// string-prefix directory match.
fn is_excluded(path: &str, cfg: &BundleViewConfig) -> bool {
    let normalized = path.replace(".min.js", ".js");
    if cfg
        .excluded_files
        .iter()
        .any(|e| e == path || *e == normalized)
    {
        return true;
    }
    cfg.excluded_dirs.iter().any(|d| path.starts_with(d.as_str()))
}

// ---------------------------------------------------------------------------
// PHP json_encode
// ---------------------------------------------------------------------------

/// PHP `json_encode($map, JSON_UNESCAPED_SLASHES)` over an ordered string
/// map: one line, keys in the given order, `/` literal, `"`/`\` escaped,
/// control chars as `\b`/`\t`/`\n`/`\f`/`\r` (others `\u00xx`), every
/// non-ASCII code point as lowercase `\uXXXX` (surrogate pairs beyond the
/// BMP). An empty map is the caller's business (`{}` is written verbatim).
pub fn php_json_encode_map(entries: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(entries.iter().map(|(k, v)| k.len() + v.len() + 8).sum());
    out.push('{');
    for (i, (k, v)) in entries.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        php_json_string(k, &mut out);
        out.push(':');
        php_json_string(v, &mut out);
    }
    out.push('}');
    out
}

fn php_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{09}' => out.push_str("\\t"),
            '\u{0a}' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\u{0d}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if (c as u32) < 0x80 => out.push(c),
            c => {
                let cp = c as u32;
                if cp <= 0xFFFF {
                    out.push_str(&format!("\\u{cp:04x}"));
                } else {
                    let v = cp - 0x1_0000;
                    let hi = 0xD800 + (v >> 10);
                    let lo = 0xDC00 + (v & 0x3FF);
                    out.push_str(&format!("\\u{hi:04x}\\u{lo:04x}"));
                }
            }
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// Flush (pooling + splitting + rendering)
// ---------------------------------------------------------------------------

/// One rendered `bundle<N>.js`.
#[derive(Debug)]
pub struct BundleFile {
    /// `bundle0.js`, `bundle1.js`, … (continuous across pools).
    pub name: String,
    /// The pool this file's map belongs to (`jsbuild` or `text`).
    pub pool: &'static str,
    /// The package-relative keys inside, in order.
    pub keys: Vec<String>,
    /// The full file bytes (UTF-8).
    pub content: String,
}

/// `Package\Bundle\RequireJs::flush`, faithfully: pools in fixed order
/// (`jsbuild`, then `text`), per pool a fresh `freeSpace = max_kb`, sizes in
/// fractional KB of **character count**, strict `>` (a pool's first candidate
/// overflowing closes the just-opened file with an empty `{}` map), file
/// numbering continuous across pools, [`INIT_JS`] appended to the last file
/// created. Empty pools create no file.
pub fn flush(
    jsbuild: &[(String, String)],
    text: &[(String, String)],
    max_kb: f64,
) -> Vec<BundleFile> {
    let mut out: Vec<BundleFile> = Vec::new();
    let mut index = 0usize;

    for (pool, files) in [("jsbuild", jsbuild), ("text", text)] {
        if files.is_empty() {
            continue;
        }
        let mut current: Vec<(&str, &str)> = Vec::new();
        let mut current_name = format!("bundle{index}.js");
        index += 1;
        let mut free = max_kb;
        for (path, content) in files {
            let size = content.chars().count() as f64 / 1024.0;
            if free > size {
                free -= size;
                current.push((path, content));
            } else {
                out.push(render_bundle(&current_name, pool, &current));
                current_name = format!("bundle{index}.js");
                index += 1;
                free = max_kb - size;
                current = vec![(path, content)];
            }
        }
        out.push(render_bundle(&current_name, pool, &current));
    }

    if let Some(last) = out.last_mut() {
        last.content.push_str(INIT_JS);
    }
    out
}

fn render_bundle(name: &str, pool: &'static str, entries: &[(&str, &str)]) -> BundleFile {
    let json = if entries.is_empty() {
        "{}".to_string()
    } else {
        php_json_encode_map(entries)
    };
    BundleFile {
        name: name.to_string(),
        pool,
        keys: entries.iter().map(|(k, _)| k.to_string()).collect(),
        content: format!("require.config({{\"config\": {{\n        \"{pool}\":{json}\n}}}});\n"),
    }
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// One theme's bundle output.
#[derive(Debug)]
pub struct ThemeBundles {
    /// The theme id as given (`Magento/blank`).
    pub theme: String,
    /// The child-first fallback chain used.
    pub chain: Vec<ThemeRef>,
    /// The effective max bundle size in KB.
    pub max_size_kb: f64,
    /// The rendered bundle files, in emission order.
    pub files: Vec<BundleFile>,
    /// How many js/html files the virtual deployed tree resolved.
    pub tree_files: usize,
    /// How many survived the filters into the pools (jsbuild, text).
    pub pooled: (usize, usize),
}

/// Bundle one theme from raw parts. `generated` supplies the deploy-produced
/// package files that exist before bundling runs but have no source file —
/// (package-relative path, content) pairs; pass the assembled
/// `requirejs-config.js` and `requirejs-min-resolver.js` (see
/// [`build_from_magento`]). `min_cache` is the run-scoped `.min` sibling
/// cache — share one across themes to reproduce a multi-theme deploy.
#[allow(clippy::too_many_arguments)]
pub fn build_theme(
    root: &Path,
    area: &str,
    theme_id: &str,
    locale: &str,
    themes: &[(String, PathBuf)],
    modules: &[ModuleRef],
    generated: &[(String, String)],
    order: &OrderMode,
    min_cache: &mut MinSiblingCache,
) -> Result<ThemeBundles, BundleError> {
    let chain = theme_chain(area, theme_id, themes)?;
    let disk = deployed_tree(root, area, &chain, modules, locale);
    let mut paths: std::collections::BTreeMap<String, Option<&PathBuf>> =
        disk.iter().map(|(k, v)| (k.clone(), Some(v))).collect();
    for (p, _) in generated {
        paths.insert(p.clone(), None);
    }

    // Ordering runs over path names only.
    let names: BTreeSet<String> = paths.keys().cloned().collect();
    let mut orderer = Orderer::new(order.clone());
    let ordered = glob_order(&names, &mut orderer)?;
    drop(orderer);

    let view_cfg = view_bundle_config(&chain, modules);
    let max_kb = view_cfg.max_size_kb();

    let mut jsbuild: Vec<(String, String)> = Vec::new();
    let mut text: Vec<(String, String)> = Vec::new();
    for path in &ordered {
        let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        if ext != "js" && ext != "html" {
            continue;
        }
        if min_cache.has_min_version(path) || is_excluded(path, &view_cfg) {
            continue;
        }
        let content = match paths.get(path).and_then(|v| *v) {
            Some(src) => read_utf8(src)?,
            None => generated
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, c)| c.clone())
                .expect("generated path present"),
        };
        if ext == "js" {
            jsbuild.push((path.clone(), content));
        } else {
            text.push((path.clone(), content));
        }
    }
    let pooled = (jsbuild.len(), text.len());
    let files = flush(&jsbuild, &text, max_kb);

    Ok(ThemeBundles {
        theme: theme_id.to_string(),
        chain,
        max_size_kb: max_kb,
        files,
        tree_files: paths.len(),
        pooled,
    })
}

/// Read a source file as UTF-8. (The PHP side detours through
/// `mb_convert_encoding` for non-UTF-8 content; no such file exists in the
/// validated corpus, so invalid UTF-8 is replaced lossily and flagged only by
/// the bytes differing — never a hard error.)
fn read_utf8(path: &Path) -> Result<String, BundleError> {
    let bytes = std::fs::read(path).map_err(|e| BundleError {
        entry: None,
        module: None,
        file: Some(path.to_path_buf()),
        message: format!("read failed: {e}"),
    })?;
    Ok(match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    })
}

/// Bundle a sequence of themes exactly like one deploy run: the given order,
/// one shared `.min` sibling cache. The per-theme `requirejs-config.js` and
/// `requirejs-min-resolver.js` are assembled from source via
/// [`super::requirejs`] (both are package files by the time bundling runs).
pub fn build_from_magento(
    magento: &magequery_core::Magento,
    area: &str,
    theme_ids: &[String],
    locale: &str,
    order: &OrderMode,
) -> Result<Vec<ThemeBundles>, BundleError> {
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
    let excludes = requirejs::min_resolver_excludes_from_magento(magento)?;
    let resolver = requirejs::min_resolver_code(&excludes);

    let mut min_cache = MinSiblingCache::new();
    let mut out = Vec::with_capacity(theme_ids.len());
    for theme_id in theme_ids {
        let rjs = requirejs::build(magento.root(), area, theme_id, &themes, &modules)?;
        let generated = vec![
            (requirejs::CONFIG_FILE_NAME.to_string(), rjs.js),
            (requirejs::MIN_RESOLVER_FILE_NAME.to_string(), resolver.clone()),
        ];
        out.push(build_theme(
            magento.root(),
            area,
            theme_id,
            locale,
            &themes,
            &modules,
            &generated,
            order,
            &mut min_cache,
        )?);
    }
    Ok(out)
}

/// The deployed placement of a theme's bundle dir:
/// `pub/static/<area>/<Vendor>/<theme>/<locale>/js/bundle`.
pub fn output_dir(root: &Path, area: &str, theme_id: &str, locale: &str) -> PathBuf {
    let theme_path = theme_id
        .strip_prefix(&format!("{area}/"))
        .unwrap_or(theme_id);
    root.join("pub")
        .join("static")
        .join(area)
        .join(theme_path)
        .join(locale)
        .join(BUNDLE_JS_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    // ---- format / wrapper -------------------------------------------------

    /// A single tiny file: the exact bytes — header (8-space indent, no space
    /// after the colon), one-line JSON, footer, and the init snippet since
    /// it is the last (only) file.
    #[test]
    fn bundle_file_format_is_locked() {
        let js = vec![(s("a.js"), s("var a = 1;\n"))];
        let files = flush(&js, &[], 1024.0);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "bundle0.js");
        assert_eq!(
            files[0].content,
            "require.config({\"config\": {\n        \"jsbuild\":{\"a.js\":\"var a = 1;\\n\"}\n}});\n".to_string()
                + INIT_JS
        );
    }

    /// Pools flush jsbuild first, then text; numbering is continuous across
    /// pools; only the very last file carries the init snippet.
    #[test]
    fn pools_are_ordered_and_numbering_continuous() {
        let js = vec![(s("a.js"), s("A"))];
        let html = vec![(s("t.html"), s("<b>"))];
        let files = flush(&js, &html, 1024.0);
        assert_eq!(files.len(), 2);
        assert_eq!((files[0].name.as_str(), files[0].pool), ("bundle0.js", "jsbuild"));
        assert_eq!((files[1].name.as_str(), files[1].pool), ("bundle1.js", "text"));
        assert!(!files[0].content.contains("mage/requirejs/static"));
        assert!(files[1].content.ends_with(INIT_JS));
    }

    /// An empty jsbuild pool creates no file at all — text starts at bundle0.
    #[test]
    fn empty_pool_creates_no_file() {
        let html = vec![(s("t.html"), s("x"))];
        let files = flush(&[], &html, 1024.0);
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].name.as_str(), files[0].pool), ("bundle0.js", "text"));
    }

    // ---- splitting --------------------------------------------------------

    /// Size accounting is chars/1024 with a strict `>`: a file that no longer
    /// fits closes the current bundle and opens the next one with itself as
    /// the first (unconditional) entry.
    #[test]
    fn split_boundary_starts_a_new_bundle() {
        // max 1 KB; two 600-char files: the second (600/1024 each) overflows
        // freeSpace = 1 - 0.586 = 0.414 < 0.586.
        let big = "x".repeat(600);
        let js = vec![(s("a.js"), big.clone()), (s("b.js"), big.clone())];
        let files = flush(&js, &[], 1.0);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].keys, vec![s("a.js")]);
        assert_eq!(files[1].keys, vec![s("b.js")]);
    }

    /// The strict `>` uses CHARACTER count: multi-byte content is measured in
    /// code points (600 two-byte chars fit exactly like 600 ASCII ones).
    #[test]
    fn size_is_character_count_not_bytes() {
        let big = "é".repeat(600); // 1200 bytes, 600 chars
        let js = vec![(s("a.js"), big.clone()), (s("b.js"), big)];
        let files = flush(&js, &[], 1.0);
        assert_eq!(files.len(), 2, "two 600-char files must split like ASCII");
        // and by bytes (1200/1024 > 1.0) the FIRST file alone would overflow,
        // producing a leading {} bundle — which must NOT happen.
        assert_eq!(files[0].keys, vec![s("a.js")]);
    }

    /// Overflow on a pool's FIRST candidate: the just-opened file is closed
    /// with an empty `{}` map (the PHP quirk), the file itself lands alone in
    /// the next bundle.
    #[test]
    fn overflow_on_first_file_leaves_an_empty_bundle() {
        let big = "x".repeat(2048); // 2 KB > max 1 KB
        let js = vec![(s("a.js"), big)];
        let files = flush(&js, &[], 1.0);
        assert_eq!(files.len(), 2);
        assert!(files[0].keys.is_empty());
        assert!(files[0].content.starts_with(
            "require.config({\"config\": {\n        \"jsbuild\":{}\n}});\n"
        ));
        assert_eq!(files[1].keys, vec![s("a.js")]);
    }

    /// A missing `bundle_size` var is PHP's `(int)(null/1024)` = 0 KB: every
    /// file overflows, so each lands alone after a leading `{}` bundle.
    #[test]
    fn zero_max_size_reproduces_the_php_quirk() {
        let js = vec![(s("a.js"), s("A")), (s("b.js"), s("B"))];
        let files = flush(&js, &[], BundleViewConfig::default().max_size_kb());
        assert_eq!(files.len(), 3);
        assert!(files[0].keys.is_empty());
        assert_eq!(files[1].keys, vec![s("a.js")]);
        assert_eq!(files[2].keys, vec![s("b.js")]);
    }

    #[test]
    fn bundle_size_units_parse_like_php() {
        let cfg = |v: &str| BundleViewConfig {
            bundle_size: Some(v.to_string()),
            ..Default::default()
        };
        assert_eq!(cfg("1MB").max_size_kb(), 1024.0);
        assert_eq!(cfg("400KB").max_size_kb(), 400.0);
        assert_eq!(cfg("2048").max_size_kb(), 2.0); // bytes / 1024
        assert_eq!(cfg("512kb").max_size_kb(), 512.0); // unit uppercased
        assert_eq!(BundleViewConfig::default().max_size_kb(), 0.0);
    }

    // ---- json -------------------------------------------------------------

    /// PHP `json_encode(..., JSON_UNESCAPED_SLASHES)`: `/` literal, `"`/`\`
    /// escaped, control shorthands, lowercase `\uXXXX` for non-ASCII with
    /// surrogate pairs beyond the BMP.
    #[test]
    fn php_json_escaping() {
        assert_eq!(
            php_json_encode_map(&[("a/b.js", "x = \"1\" \\ \n\t\r\u{8}\u{c}\u{1}")]),
            r#"{"a/b.js":"x = \"1\" \\ \n\t\r\b\f\u0001"}"#
        );
        // non-ASCII → lowercase \uXXXX (goldens verified: ö, ’)
        assert_eq!(
            php_json_encode_map(&[("k", "ö’")]),
            "{\"k\":\"\\u00f6\\u2019\"}"
        );
        // astral → surrogate pair
        assert_eq!(
            php_json_encode_map(&[("k", "😀")]),
            "{\"k\":\"\\ud83d\\ude00\"}"
        );
        assert_eq!(php_json_encode_map(&[]), "{}");
        // insertion order, comma-separated, no spaces
        assert_eq!(
            php_json_encode_map(&[("z", "1"), ("a", "2")]),
            r#"{"z":"1","a":"2"}"#
        );
    }

    // ---- filters ----------------------------------------------------------

    #[test]
    fn prepare_exclude_paths() {
        assert_eq!(prepare_exclude_path("Lib::jquery/jquery.min.js"), "jquery/jquery.min.js");
        assert_eq!(
            prepare_exclude_path("Magento_Ui::js/grid"),
            "Magento_Ui/js/grid"
        );
        assert_eq!(prepare_exclude_path("no-separator.js"), "no-separator.js");
    }

    /// File excludes match exactly, and an exclude naming `X.js` also kills a
    /// deployed `X.min.js` (the candidate is min-normalized, not the
    /// exclude). Directory excludes are RAW string prefixes — `foo` also
    /// matches `foobar/x.js` (the upstream quirk, locked).
    #[test]
    fn exclude_matching_quirks() {
        let cfg = BundleViewConfig {
            excluded_files: vec![s("mage/captcha.js")],
            excluded_dirs: vec![s("Magento_Ui/js/grid"), s("foo")],
            bundle_size: None,
        };
        assert!(is_excluded("mage/captcha.js", &cfg));
        assert!(is_excluded("mage/captcha.min.js", &cfg)); // min-normalized candidate
        assert!(!is_excluded("mage/captcha2.js", &cfg));
        assert!(is_excluded("Magento_Ui/js/grid/x.js", &cfg));
        assert!(is_excluded("Magento_Ui/js/gridiron.js", &cfg)); // no `/` anchor
        assert!(is_excluded("foobar/x.js", &cfg)); // raw prefix quirk
        assert!(!is_excluded("bar/foo/x.js", &cfg));
    }

    /// hasMinVersion is iteration-order-dependent: plain-before-min keeps
    /// BOTH; min-before-plain drops the plain one; and the cache persists
    /// across themes (the shared-service leak).
    #[test]
    fn min_sibling_cache_is_order_dependent_and_shared() {
        let mut cache = MinSiblingCache::new();
        // plain first: kept; its min sibling then poisons the name.
        assert!(!cache.has_min_version("vimeo/player.js"));
        assert!(!cache.has_min_version("vimeo/player.min.js"));
        // second theme, same shared cache: the plain name is now poisoned.
        assert!(cache.has_min_version("vimeo/player.js"));
        // min-first in a fresh cache: plain dropped immediately.
        let mut fresh = MinSiblingCache::new();
        assert!(!fresh.has_min_version("slick/slick.min.js"));
        assert!(fresh.has_min_version("slick/slick.js"));
    }

    // ---- view.xml parsing -------------------------------------------------

    #[test]
    fn view_xml_excludes_and_size_parse() {
        let xml = r#"<view>
            <vars module="Magento_Catalog"><var name="gallery">1</var></vars>
            <vars module="Js_Bundle"><var name="bundle_size">1MB</var></vars>
            <exclude>
                <item type="file">Lib::jquery/jquery.min.js</item>
                <item type="file">Magento_Ui::js/form/element/ui-select.js</item>
                <item type="directory">Lib::varien</item>
                <!-- <item type="file">Lib::commented-out.js</item> -->
            </exclude>
        </view>"#;
        let stripped = strip_xml_comments(xml);
        let items = view_exclude_items(&stripped);
        assert_eq!(
            items,
            vec![
                (false, s("Lib::jquery/jquery.min.js")),
                (false, s("Magento_Ui::js/form/element/ui-select.js")),
                (true, s("Lib::varien")),
            ]
        );
        assert_eq!(view_bundle_size(&stripped), Some(s("1MB")));
    }

    // ---- ordering + orchestration over a synthetic tree -------------------

    /// A synthetic Magento-shaped root exercising every tree layer and the
    /// glob-order rule.
    fn synth_tree() -> tempfile::TempDir {
        let td = tempfile::tempdir().expect("tempdir");
        let r = td.path();
        let w = |rel: &str, content: &str| {
            let p = r.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        };

        // lib: root files + a subdir + an excluded css/docs file + non-js.
        w("lib/web/alpha.js", "LIB-alpha");
        w("lib/web/zeta.js", "LIB-zeta");
        w("lib/web/mage/util.js", "LIB-mage-util");
        w("lib/web/css/docs/doc.js", "EXCLUDED-css-docs");
        w("lib/web/styles.css", "NOT-BUNDLED");
        w("lib/web/README", "dotless: never globbed");

        // module: base + area, area overriding base; a template; i18n overlay.
        w("app/code/Acme/Widgets/etc/module.xml", "<config/>");
        w("app/code/Acme/Widgets/view/base/web/js/both.js", "BASE-both");
        w("app/code/Acme/Widgets/view/frontend/web/js/both.js", "AREA-both");
        w("app/code/Acme/Widgets/view/frontend/web/template/t.html", "TPL");
        w("app/code/Acme/Widgets/view/frontend/web/js/local.js", "PLAIN-local");
        w(
            "app/code/Acme/Widgets/view/frontend/web/i18n/nl_NL/js/local.js",
            "I18N-local",
        );

        // themes: base + child; theme web root file overriding lib; module
        // context override; theme view.xml with excludes + bundle_size.
        w("vendor/acme/theme-base/theme.xml", "<theme><title>B</title></theme>");
        w("vendor/acme/theme-base/web/theme-only.js", "THEME-BASE-own");
        w("vendor/acme/theme-base/web/alpha.js", "THEME-BASE-alpha");
        w(
            "vendor/acme/theme-base/etc/view.xml",
            r#"<view>
                 <vars module="Js_Bundle"><var name="bundle_size">1MB</var></vars>
                 <exclude>
                     <item type="file">Lib::zeta.js</item>
                     <item type="directory">Lib::mage</item>
                 </exclude>
               </view>"#,
        );
        w(
            "vendor/acme/theme-child/theme.xml",
            "<theme><title>C</title><parent>Acme/base</parent></theme>",
        );
        w(
            "vendor/acme/theme-child/Acme_Widgets/web/js/both.js",
            "THEME-CHILD-both",
        );

        td
    }

    fn themes(root: &Path) -> Vec<(String, PathBuf)> {
        vec![
            ("frontend/Acme/base".to_string(), root.join("vendor/acme/theme-base")),
            ("frontend/Acme/child".to_string(), root.join("vendor/acme/theme-child")),
        ]
    }

    fn modules(root: &Path) -> Vec<ModuleRef> {
        vec![ModuleRef {
            name: "Acme_Widgets".to_string(),
            dir: root.join("app/code/Acme/Widgets"),
        }]
    }

    fn build(root: &Path, theme: &str, locale: &str) -> ThemeBundles {
        let mut cache = MinSiblingCache::new();
        build_theme(
            root,
            "frontend",
            theme,
            locale,
            &themes(root),
            &modules(root),
            &[
                (s("requirejs-config.js"), s("RJS-CONFIG")),
                (s("requirejs-min-resolver.js"), s("RJS-RESOLVER")),
            ],
            &OrderMode::Sorted,
            &mut cache,
        )
        .expect("build")
    }

    /// The whole pipeline over the synthetic tree: fallback layering, the
    /// glob order (root files first, then subtrees, sorted mode), view.xml
    /// excludes, pool split, and the generated entries.
    #[test]
    fn synthetic_tree_end_to_end() {
        let td = synth_tree();
        let out = build(td.path(), "Acme/child", "en_US");
        assert_eq!(out.max_size_kb, 1024.0);
        assert_eq!(out.files.len(), 2, "one jsbuild + one text bundle");

        // jsbuild: root files (sorted) then subtrees; zeta.js and mage/*
        // excluded by the theme view.xml; both.js resolved through the CHILD
        // theme's module context; alpha.js through the base theme's web root.
        assert_eq!(
            out.files[0].keys,
            vec![
                s("alpha.js"),
                s("requirejs-config.js"),
                s("requirejs-min-resolver.js"),
                s("theme-only.js"),
                s("Acme_Widgets/js/both.js"),
                s("Acme_Widgets/js/local.js"),
            ]
        );
        assert!(out.files[0].content.contains("\"alpha.js\":\"THEME-BASE-alpha\""));
        assert!(out.files[0].content.contains("\"Acme_Widgets/js/both.js\":\"THEME-CHILD-both\""));
        assert!(out.files[0].content.contains("\"Acme_Widgets/js/local.js\":\"PLAIN-local\""));
        assert!(out.files[0].content.contains("\"requirejs-config.js\":\"RJS-CONFIG\""));

        // text pool: the template, in bundle1, with the init snippet.
        assert_eq!(out.files[1].keys, vec![s("Acme_Widgets/template/t.html")]);
        assert!(out.files[1].content.ends_with(INIT_JS));
    }

    /// Locale overlays: the nl_NL build resolves the i18n variant, en_US the
    /// plain file. Theme-less parent resolution: the BASE theme sees the
    /// module's own (area) file, not the child theme's override.
    #[test]
    fn locale_overlay_and_parent_theme_resolution() {
        let td = synth_tree();
        let nl = build(td.path(), "Acme/child", "nl_NL");
        assert!(nl.files[0].content.contains("\"Acme_Widgets/js/local.js\":\"I18N-local\""));
        let base = build(td.path(), "Acme/base", "en_US");
        assert!(base.files[0].content.contains("\"Acme_Widgets/js/both.js\":\"AREA-both\""));
    }

    /// The virtual tree honors the collection rules: css/docs pruned, non-js/
    /// html dropped, dotless names never collected, hidden files skipped.
    #[test]
    fn deployed_tree_filters() {
        let td = synth_tree();
        let r = td.path();
        std::fs::write(r.join("lib/web/.hidden.js"), "x").unwrap();
        let chain = theme_chain("frontend", "Acme/child", &themes(r)).unwrap();
        let tree = deployed_tree(r, "frontend", &chain, &modules(r), "en_US");
        assert!(!tree.keys().any(|k| k.starts_with("css/docs")));
        assert!(!tree.contains_key("styles.css"));
        assert!(!tree.contains_key("README"));
        assert!(!tree.contains_key(".hidden.js"));
        assert!(tree.contains_key("mage/util.js")); // view.xml excludes apply later
    }

    /// Probe mode on this machine's filesystem must agree with the real
    /// readdir order — locked by comparing a probe against a directory
    /// created here in REVERSE name order (on hash-ordered filesystems this
    /// is a real assertion; on creation-ordered ones both sides degrade to
    /// the same creation order and the test still holds).
    #[test]
    fn probe_order_reproduces_readdir() {
        let td = tempfile::tempdir().unwrap();
        let names = ["jquery.js", "underscore.js", "moment.js", "matchMedia.js"];
        let real = td.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let mut rev = names;
        rev.reverse();
        for n in rev {
            std::fs::File::create(real.join(n)).unwrap();
        }
        let real_order: Vec<String> = std::fs::read_dir(&real)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();

        let mut orderer = Orderer::new(OrderMode::Probe(td.path().to_path_buf()));
        let set: BTreeSet<String> = names.iter().map(|n| n.to_string()).collect();
        let probed = orderer.order(&set).unwrap();
        // On a hash-ordered fs both are the same hash order; on a
        // creation-ordered fs real=reversed, probed=sorted — only assert
        // equality when the fs is order-deterministic (probe of the reverse
        // creation matches).
        let deterministic = {
            let again = td.path().join("again");
            std::fs::create_dir(&again).unwrap();
            for n in names {
                std::fs::File::create(again.join(n)).unwrap();
            }
            let sorted_creation: Vec<String> = std::fs::read_dir(&again)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            sorted_creation == real_order
        };
        if deterministic {
            assert_eq!(probed, real_order);
        } else {
            // Creation-ordered fs (e.g. tmpfs): the probe can't reproduce a
            // historical readdir, but it must still be self-consistent — the
            // same name set probes to the same order every time.
            let mut orderer2 = Orderer::new(OrderMode::Probe(td.path().to_path_buf()));
            assert_eq!(probed, orderer2.order(&set).unwrap());
            let mut back: Vec<String> = probed.clone();
            back.sort();
            let want: Vec<String> = set.iter().cloned().collect();
            assert_eq!(back, want, "probe returns exactly the given names");
        }
    }

    /// glob-order structure: all files of a directory precede its subtrees,
    /// recursively (sorted mode makes the expectation explicit).
    #[test]
    fn glob_order_files_before_subdirs() {
        let set: BTreeSet<String> = ["z.js", "a/inner.js", "a/deep/x.js", "b.js", "a/b.js"]
            .iter()
            .map(|p| p.to_string())
            .collect();
        let mut orderer = Orderer::new(OrderMode::Sorted);
        let got = glob_order(&set, &mut orderer).unwrap();
        assert_eq!(
            got,
            vec![
                s("b.js"),
                s("z.js"),
                s("a/b.js"),
                s("a/inner.js"),
                s("a/deep/x.js"),
            ]
        );
    }

    #[test]
    fn output_dir_is_the_bundle_dir() {
        assert_eq!(
            output_dir(Path::new("/srv/m2"), "frontend", "Magento/luma", "en_US"),
            PathBuf::from("/srv/m2/pub/static/frontend/Magento/luma/en_US/js/bundle")
        );
    }
}
