//! less.js v4.6.7 compile-fixture harness (plan §5.5) — the FULL compile
//! corpus: `tests-unit/` (default options) plus the option-driven
//! `tests-config/` suites (per-directory options in [`config_options`],
//! transcribed from upstream's cosmiconfig `styles.config.cjs` files). Each
//! `<name>.less` with a golden `<name>.css` becomes one [`libtest_mimic`]
//! Trial that:
//!   1. compiles the input with the directory's [`LessOptions`] + a filesystem
//!      [`ImportResolver`] rooted at the fixture's directory (plus any
//!      include-paths and the vendored `node_modules/` packages), with the
//!      less.js test runner's registered custom functions
//!      (`add`/`increment`/`_color`),
//!   2. applies less.js's `doReplacements` placeholder substitution (§5.5) and
//!      trailing-newline normalization to the expected `.css`,
//!   3. byte-diffs the two.
//!
//! The runner prints a pass-rate `X/Y` and enforces a **ratchet gate**
//! (plan §5.6): [`EXPECTED_PASS`] is the checked-in floor — the set of fixtures
//! that currently produce byte-identical output. Every other in-scope fixture is
//! an `xfail` (known-red, the milestone-1 engine doesn't cover it yet) and is
//! reported but does NOT fail the suite, so `cargo test --workspace` stays green.
//! Two conditions DO fail the suite, keeping the gate honest:
//!   - a **regression** — a fixture on the floor that stops passing;
//!   - an **improvement** — an `xfail` that starts passing (raise the floor:
//!     add it to [`EXPECTED_PASS`]).
//! The full manifest-driven ratchet (`min_pass_rate` + per-fixture tracking issue
//! + `tests-config`/`tests-error` denominator, plan §5.6) is DEFERRED; this is its
//! milestone-1 form over the default-option compile corpus.
//!
//! OUT OF SCOPE (plan §5.2/§8, documented not silent): `tests-error/` (a later
//! increment), the JS-plugin config dirs (`filemanagerPlugin`,
//! `postProcessorPlugin`, `preProcessorPlugin`, `visitorPlugin` — not
//! vendored), `debug/` (dumpLineNumbers), `sourcemaps*`, the
//! javascript-enabled error suites, and — within `tests-unit/` — the
//! `javascript`/`plugin*` sub-suites. Two vendored fixtures depend on JS
//! `@plugin` EXECUTION and stay permanent xfails: `import/import` (needs the
//! plugin-defined `pi-anon()`) and `config/3rd-party/bootstrap4`
//! (bootstrap-less-port's theme-color/gray plugins). The three `compress`
//! fixtures wait on the §C4 compress serializer (deliberately not forced).

use libtest_mimic::{Arguments, Failed, Trial};
use magecommand_less::{
    compile, FileInfo, ImportError, ImportPayload, ImportRequest, ImportResolver, LessOptions,
    MathMode, ResolvedImport, RewriteUrls,
};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The vendored less.js tag (plan §0/§5.3). Kept in sync with VENDOR.txt.
const TAG: &str = "v4.6.7";

/// `tests-unit` sub-suites deferred to later phases (JS/plugin — plan §5.2).
const SKIP_SUITES: &[&str] = &["javascript", "plugin", "plugin-module", "plugin-preeval"];

/// The **ratchet floor** (plan §5.6): fixtures whose output is byte-identical to
/// the vendored less.js golden under the milestone-1 engine. Every in-scope
/// fixture NOT listed here is a known `xfail` — reported red, but not a hard
/// failure, so the workspace test suite stays green while the engine grows.
///
/// Invariant enforced by the harness: a fixture on this list that regresses, or
/// an off-list fixture that starts passing, fails the suite. Keep it sorted; when
/// a phase lands new coverage, ADD the newly-green fixtures here (never remove one
/// to hide a regression). 122/127 after Phase 4B (`:extend` full §2.8, full
/// two-stage `@import` §2.9, URL rewriting §2.18, the tests-config option
/// corpus, and the Phase-3 parser-debt catalog; see NOTES.md "Phase 4B"). The
/// 5 still-red fixtures: 3 wait on the §C4 compress serializer, 2 on JS
/// `@plugin` execution (out of scope, §8) — see the header note.
const EXPECTED_PASS: &[&str] = &[
    "at-rules-declarations/at-rules-declarations",
    "at-rules-empty-block/at-rules-empty-block",
    "at-rules-empty/at-rules-empty",
    "at-rules-keyword-comments/at-rules-keyword-comments",
    "at-rules-targeted/at-rules-targeted",
    "at-rules/at-rules",
    "calc/calc",
    "charsets/charsets",
    "color-functions/alpha",
    "color-functions/basic",
    "color-functions/comprehensive",
    "color-functions/formats",
    "color-functions/modern",
    "color-functions/modern-syntax",
    "color-functions/operations",
    "color-functions/rgba",
    "comments/comments",
    "comments/comments2",
    "config/globalVars/extended",
    "config/globalVars/simple",
    "config/include-path-string/include-path-string",
    "config/include-path/include-path",
    "config/math-always/mixins-guards",
    "config/math-always/no-sm-operations",
    "config/math-parens-division/media-math",
    "config/math-parens-division/mixins-args",
    "config/math-parens-division/new-division",
    "config/math-parens-division/parens",
    "config/math-strict/css",
    "config/math-strict/media-math",
    "config/math-strict/mixins-args",
    "config/math-strict/parens",
    "config/modifyVars/extended",
    "config/namespacing/namespacing-1",
    "config/namespacing/namespacing-2",
    "config/namespacing/namespacing-3",
    "config/namespacing/namespacing-4",
    "config/namespacing/namespacing-5",
    "config/namespacing/namespacing-6",
    "config/namespacing/namespacing-7",
    "config/namespacing/namespacing-8",
    "config/namespacing/namespacing-functions",
    "config/namespacing/namespacing-media",
    "config/namespacing/namespacing-operations",
    "config/process-imports/google",
    "config/rewrite-urls-all/rewrite-urls-all",
    "config/rewrite-urls-local/rewrite-urls-local",
    "config/rootpath-rewrite-urls-all/rootpath-rewrite-urls-all",
    "config/rootpath-rewrite-urls-local/rootpath-rewrite-urls-local",
    "config/static-urls/urls",
    "config/strict-imports/strict-imports",
    "config/units/no-strict/no-strict",
    "config/units/strict/strict-units",
    "config/url-args/urls",
    "container/container",
    "css-3/css-3",
    "css-escapes/css-escapes",
    "css-grid/css-grid",
    "css-guards/css-guards",
    "detached-rulesets/detached-rulesets",
    "directives-bubbling/directives-bubbling",
    "empty/empty",
    "extend-chaining/extend-chaining",
    "extend-clearfix/extend-clearfix",
    "extend-exact/extend-exact",
    "extend-media/extend-media",
    "extend-nest/extend-nest",
    "extend-selector/extend-selector",
    "extend/extend",
    "extend/extend-clearfix",
    "extract-and-length/extract-and-length",
    "functions-each/functions-each",
    "functions/functions",
    "ie-filters/ie-filters",
    "impor/impor",
    "import/import-inline",
    "import/import-interpolation",
    "import/import-module",
    "import/import-once",
    "import/import-reference",
    "import/import-reference-issues",
    "import/import-remote",
    "layer/layer",
    "lazy-eval/lazy-eval",
    "media/media",
    "merge/merge",
    "mixin-noparens/mixin-noparens",
    "mixins-closure/mixins-closure",
    "mixins-guards-default-func/mixins-guards-default-func",
    "mixins-guards/mixins-guards",
    "mixins-important/mixins-important",
    "mixins-interpolated/mixins-interpolated",
    "mixins-named-args/mixins-named-args",
    "mixins-nested/mixins-nested",
    "mixins-pattern/mixins-pattern",
    "mixins/maps",
    "mixins/mixins",
    "mixins/mixins-advanced",
    "namespace-targeted/namespace-targeted",
    "nesting/nesting",
    "no-output/no-output",
    "operations/operations",
    "operations/operations-advanced",
    "parse-interpolation/parse-interpolation",
    "parser-property-interp/parser-property-interp",
    "parser-slashed-combinator/parser-slashed-combinator",
    "permissive-parse/permissive-parse",
    "plugi/plugi",
    "property-accessors/property-accessors",
    "property-name-interp/property-name-interp",
    "property-targeted/property-targeted",
    "rulesets/rulesets",
    "scope/scope",
    "selectors/selectors",
    "starting-style/starting-style",
    "strings/strings",
    "tailwind/tailwind",
    "urls/urls",
    "variables-in-at-rules/variables-in-at-rules",
    "variables/variable-advanced",
    "variables/variables",
    "whitespace/whitespace",
];

/// Absolute path of the vendored fixture root (`…/tests/fixtures/less-testdata`).
fn testdata_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/less-testdata")
}

// ---------------------------------------------------------------------------
// Filesystem import resolver — the compiler's only FS boundary (plan §9.5).
// Wired now for later phases; the passthrough `compile()` does not call it yet.
// ---------------------------------------------------------------------------

/// Resolves `@import`s against a directory tree, applying less.js's extension
/// rule (`.css` stays CSS; no extension ⇒ append `.less`, plan §2.9) and its
/// file-manager search order: the importing file's directory first, then the
/// fixture root, then the vendored `node_modules` (the `@less/test-import-module`
/// package). Remote `https://cdn.jsdelivr.net/npm/@less/test-data/…` URLs map
/// onto the identical vendored tree (`import-remote` — same bytes, no network).
struct FsResolver {
    /// The fixture's directory (fallback for imports with no current file).
    root: PathBuf,
    /// `paths` — extra import search roots (the include-path configs).
    include_paths: Vec<PathBuf>,
}

impl FsResolver {
    fn candidates(&self, req: &ImportRequest) -> Vec<PathBuf> {
        let raw = req.path.as_str();
        // Strip a `?query` suffix (the remote fixture imports `empty.less?arg`).
        let raw = raw.split('?').next().unwrap_or(raw);
        if let Some(rest) = raw.strip_prefix("https://cdn.jsdelivr.net/npm/@less/test-data/") {
            return vec![testdata_root().join(rest)];
        }
        let mut out = Vec::new();
        let from_dir = req.from.current_directory.trim_end_matches('/');
        if !from_dir.is_empty() {
            out.push(PathBuf::from(from_dir).join(raw));
        }
        out.push(self.root.join(raw));
        for p in &self.include_paths {
            out.push(p.join(raw));
        }
        out.push(testdata_root().join("node_modules").join(raw));
        out
    }
}

impl ImportResolver for FsResolver {
    fn resolve(&self, req: &ImportRequest) -> Result<ResolvedImport, ImportError> {
        let force_css = req.options.css == Some(true);
        let force_less = req.options.css == Some(false);
        let is_css = force_css || (!force_less && req.path.split('?').next().unwrap_or("").ends_with(".css"));

        let mut tried = Vec::new();
        let mut found: Option<PathBuf> = None;
        for mut candidate in self.candidates(req) {
            if candidate.extension().is_none() && !is_css {
                candidate.set_extension("less");
            }
            if candidate.is_file() {
                found = Some(candidate);
                break;
            }
            tried.push(candidate.display().to_string());
        }
        let candidate = found.ok_or_else(|| ImportError::NotFound(tried.join(", ")))?;

        let bytes = fs::read_to_string(&candidate).map_err(|e| ImportError::Io {
            path: candidate.display().to_string(),
            message: e.to_string(),
        })?;

        let dir = candidate
            .parent()
            .map(|p| format!("{}/", p.display()))
            .unwrap_or_default();
        let file = FileInfo {
            filename: candidate.display().to_string(),
            current_directory: dir,
            ..FileInfo::default()
        };

        let payload = if req.options.inline {
            ImportPayload::Inline(Arc::from(bytes.as_str()))
        } else if is_css {
            ImportPayload::Css(Arc::from(bytes.as_str()))
        } else {
            ImportPayload::Less(Arc::from(bytes.as_str()))
        };

        Ok(ResolvedImport { file, payload })
    }

    /// Asset reads for `data-uri`/`image-size` (plan §C-assets): resolve
    /// relative to the requesting file's directory (fixture-relative).
    fn load_binary(&self, path: &str, current_directory: &str) -> Option<Vec<u8>> {
        let base = if current_directory.is_empty() {
            self.root.clone()
        } else {
            PathBuf::from(current_directory)
        };
        if let Ok(bytes) = fs::read(base.join(path)) {
            return Some(bytes);
        }
        for p in &self.include_paths {
            if let Ok(bytes) = fs::read(p.join(path)) {
                return Some(bytes);
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Fixture discovery
// ---------------------------------------------------------------------------

/// One corpus entry: the `.less` input, its golden `.css`, and the trial name.
struct Fixture {
    less: PathBuf,
    css: PathBuf,
    name: String,
}

/// Every in-scope `.less` input, sorted for a stable test order:
/// - `tests-unit/**`: a `.less` with a sibling `.css`, outside the deferred
///   JS/plugin sub-suites (default options);
/// - `tests-config/**`: the option-driven compile fixtures (per-directory
///   options in [`config_options`]); the `math-<mode>/x.less` inputs pair with
///   goldens under `math/<mode>/x.css` (the upstream v4.6.7 layout).
///
/// NOT vendored/in scope (documented, plan §5.2/§8): JS-plugin dirs
/// (`filemanagerPlugin`, `postProcessorPlugin`, `preProcessorPlugin`,
/// `visitorPlugin`), `debug/` (dumpLineNumbers), `sourcemaps*`, and the
/// javascript-enabled error suites (`js-type-errors`, `no-js-errors`) —
/// `tests-error/` is a later increment.
fn discover() -> Vec<Fixture> {
    let root = testdata_root();
    let unit = root.join("tests-unit");
    let mut less_files = Vec::new();
    walk(&unit, &mut less_files);
    less_files.retain(|less| less.with_extension("css").is_file() && !is_skipped(less));
    let mut out: Vec<Fixture> = less_files
        .into_iter()
        .map(|less| {
            let css = less.with_extension("css");
            let name = trial_name(&less);
            Fixture { less, css, name }
        })
        .collect();

    let config = root.join("tests-config");
    let mut cfg_files = Vec::new();
    walk(&config, &mut cfg_files);
    for less in cfg_files {
        let rel = less.strip_prefix(&config).unwrap();
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let first = rel_str.split('/').next().unwrap_or("").to_string();
        // The math goldens live under `math/<mode>/`; inputs under `math-<mode>/`.
        let css = if let Some(mode) = first.strip_prefix("math-") {
            config
                .join("math")
                .join(mode)
                .join(rel.file_name().unwrap())
                .with_extension("css")
        } else {
            less.with_extension("css")
        };
        if !css.is_file() {
            continue;
        }
        let name = format!(
            "config/{}",
            rel_str.trim_end_matches(".less")
        );
        out.push(Fixture { less, css, name });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Per-directory options for `tests-config/` fixtures — the vendored
/// `styles.config.cjs` contents (deleted from the tree), transcribed here so
/// the harness needs no Node. Returns (options, include-paths).
fn config_options(first_dir: &str, less: &Path) -> (LessOptions, Vec<PathBuf>) {
    let mut opts = LessOptions::default();
    let mut paths = Vec::new();
    match first_dir {
        "3rd-party" => opts.math = MathMode::Always,
        "at-rules-compressed" | "at-rules-compressed-evaluation" => opts.compress = true,
        "compression" => {
            opts.math = MathMode::Parens;
            opts.compress = true;
        }
        "include-path" | "include-path-string" => {
            paths.push(testdata_root().join("data"));
        }
        "process-imports" => opts.process_imports = false,
        "rewrite-urls-all" => opts.rewrite_urls = RewriteUrls::All,
        "rewrite-urls-local" => opts.rewrite_urls = RewriteUrls::Local,
        "rootpath-rewrite-urls-all" => {
            opts.rootpath = Some("http://example.com/assets/css/".to_string());
            opts.rewrite_urls = RewriteUrls::All;
        }
        "rootpath-rewrite-urls-local" => {
            opts.rootpath = Some("http://example.com/assets/css/".to_string());
            opts.rewrite_urls = RewriteUrls::Local;
        }
        "static-urls" => {
            opts.math = MathMode::Parens;
            opts.rootpath = Some("folder (1)/".to_string());
        }
        "strict-imports" => opts.strict_imports = true,
        "url-args" => opts.url_args = Some("424242".to_string()),
        "globalVars" => {
            opts.banner = Some("/**\n  * Test\n  */\n".to_string());
            opts.global_vars = read_vars_json(less);
        }
        "modifyVars" => opts.modify_vars = read_vars_json(less),
        "units" => {
            opts.math = MathMode::Always;
            opts.strict_units = less
                .to_string_lossy()
                .contains("/strict/");
        }
        "math-always" => opts.math = MathMode::Always,
        "math-parens-division" => opts.math = MathMode::ParensDivision,
        "math-strict" => opts.math = MathMode::Parens,
        "namespacing" => {}
        _ => {}
    }
    (opts, paths)
}

/// Read the sibling `<name>.json` flat string map (the globalVars/modifyVars
/// per-file variables the less.js runner loads).
fn read_vars_json(less: &Path) -> Vec<(String, String)> {
    let json = less.with_extension("json");
    let Ok(text) = fs::read_to_string(&json) else { return Vec::new() };
    parse_flat_json(&text)
}

/// A tiny parser for the flat `{"key": "value", …}` json the fixtures use.
fn parse_flat_json(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let inner = text.trim().trim_start_matches('{').trim_end_matches('}');
    for part in inner.split(',') {
        let mut kv = part.splitn(2, ':');
        let (Some(k), Some(v)) = (kv.next(), kv.next()) else { continue };
        let k = k.trim().trim_matches('"');
        let v = v.trim().trim_matches('"');
        if !k.is_empty() {
            out.push((k.to_string(), v.to_string()));
        }
    }
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("less") {
            out.push(path);
        }
    }
}

/// True if any path component names a deferred JS/plugin sub-suite (plan §5.2).
fn is_skipped(less: &Path) -> bool {
    less.components().any(|c| match c {
        Component::Normal(name) => name
            .to_str()
            .map(|n| SKIP_SUITES.contains(&n))
            .unwrap_or(false),
        _ => false,
    })
}

/// A short, stable name for the Trial: the path relative to `tests-unit/`,
/// minus the `.less` extension (e.g. `variables/variables`).
fn trial_name(less: &Path) -> String {
    let unit = testdata_root().join("tests-unit");
    let rel = less.strip_prefix(&unit).unwrap_or(less);
    rel.with_extension("").to_string_lossy().replace('\\', "/")
}

// ---------------------------------------------------------------------------
// less.js `doReplacements` (globalReplacements) port (plan §5.5)
// ---------------------------------------------------------------------------

/// less.js's `pathesc` escaping: each of `. : /` is backslash-prefixed and `\`
/// becomes `\/` (`a == '\\' ? '\/' : a`).
fn escape_path(p: &str) -> String {
    let mut s = String::with_capacity(p.len());
    for ch in p.chars() {
        match ch {
            '.' | ':' | '/' => {
                s.push('\\');
                s.push(ch);
            }
            '\\' => s.push_str("\\/"),
            _ => s.push(ch),
        }
    }
    s
}

/// Apply less.js's placeholder substitutions to an expected file, then normalize
/// CRLF (plan §5.5). `dir` is the fixture's directory; `root` the fixture root
/// (`lessFolder`). In-scope tests-unit fixtures use no path placeholders, so this
/// is dominated by the `\r\n`→`\n` rule today, but the full set is implemented
/// for the deferred suites.
fn do_replacements(input: &str, dir: &Path, root: &Path) -> String {
    let p = format!("{}/", dir.display());
    let pathimport = format!("{p}import/");
    let pathesc = escape_path(&p);
    let pathimportesc = escape_path(&pathimport);
    let pathrel = match dir.strip_prefix(root) {
        Ok(rel) => format!("{}/", rel.display()),
        Err(_) => p.clone(),
    };
    let nodepath = std::env::current_dir()
        .map(|c| format!("{}/node_modules/", c.display()))
        .unwrap_or_else(|_| "node_modules/".to_string());

    input
        .replace("{path}", &p)
        .replace("{node}", "")
        .replace("{/node}", "")
        .replace("{pathhref}", "")
        .replace("{404status}", "")
        .replace("{nodepath}", &nodepath)
        .replace("{pathrel}", &pathrel)
        .replace("{pathesc}", &pathesc)
        .replace("{pathimport}", &pathimport)
        .replace("{pathimportesc}", &pathimportesc)
        .replace("\r\n", "\n")
}

/// Trailing-newline normalization (plan §5.5): the only whitespace normalization
/// applied before the byte-diff.
fn strip_trailing_newlines(s: &str) -> &str {
    s.trim_end_matches('\n')
}

// ---------------------------------------------------------------------------
// One fixture
// ---------------------------------------------------------------------------

/// The less.js test runner's globally registered custom functions
/// (`less-test.js` `functionRegistry.addMultiple`): `add`, `increment`,
/// `_color` — exercised by `functions/functions`.
fn harness_functions() -> Vec<(String, magecommand_less::options::CustomFunction)> {
    use magecommand_less::ast::Node;
    use magecommand_less::value::Dimension;
    fn num(n: &Node) -> Option<f64> {
        match n {
            Node::Dimension(d) => Some(d.value),
            _ => None,
        }
    }
    fn f_add(a: &[Node]) -> Option<Node> {
        Some(Node::Dimension(Dimension::number(num(a.first()?)? + num(a.get(1)?)?)))
    }
    fn f_increment(a: &[Node]) -> Option<Node> {
        Some(Node::Dimension(Dimension::number(num(a.first()?)? + 1.0)))
    }
    fn f_color(a: &[Node]) -> Option<Node> {
        match a.first()? {
            Node::Quoted { value, .. } if value == "evil red" => {
                Some(Node::Color(magecommand_less::color::Color::rgb(0x66, 0, 0)))
            }
            _ => None,
        }
    }
    vec![
        ("add".to_string(), f_add as magecommand_less::options::CustomFunction),
        ("increment".to_string(), f_increment as _),
        ("_color".to_string(), f_color as _),
    ]
}

fn run_one(fx: &Fixture, root: &Path, passed: &AtomicUsize) -> Result<(), Failed> {
    let less = &fx.less;
    let src = fs::read_to_string(less)
        .map_err(|e| Failed::from(format!("read input {}: {e}", less.display())))?;
    let expected_raw = fs::read_to_string(&fx.css)
        .map_err(|e| Failed::from(format!("read expected {}: {e}", less.display())))?;

    let dir = less.parent().unwrap_or(root);
    let (mut opts, include_paths) = match fx.name.strip_prefix("config/") {
        Some(rest) => config_options(rest.split('/').next().unwrap_or(""), less),
        None => (LessOptions::default(), Vec::new()),
    };
    opts.filename = Some(less.display().to_string());
    opts.custom_functions = harness_functions();
    let resolver = FsResolver {
        root: dir.to_path_buf(),
        include_paths,
    };

    // Raw compile+diff outcome — an error or a byte-mismatch both mean "red".
    let (did_pass, detail) = match compile(&src, &opts, &resolver) {
        Ok(got) => {
            let expected = do_replacements(&expected_raw, dir, root);
            if strip_trailing_newlines(&got.code) == strip_trailing_newlines(&expected) {
                (true, String::new())
            } else {
                (false, first_diff(&expected, &got.code))
            }
        }
        Err(e) => (false, format!("compile error: {e}")),
    };
    if did_pass {
        passed.fetch_add(1, Ordering::Relaxed);
    }

    // Ratchet gate (plan §5.6): EXPECTED_PASS is the checked-in floor. A known
    // xfail staying red is fine; a floor fixture regressing or an xfail newly
    // passing both fail the suite (and must be reconciled by hand).
    let name = &fx.name;
    let expected_pass = EXPECTED_PASS.contains(&name.as_str());
    match (expected_pass, did_pass) {
        (true, true) | (false, false) => Ok(()),
        (true, false) => Err(Failed::from(format!(
            "RATCHET REGRESSION: `{name}` is on the expected-pass floor but no longer produces \
             byte-identical output.\n{detail}"
        ))),
        (false, true) => Err(Failed::from(format!(
            "RATCHET IMPROVEMENT: `{name}` now passes — add it to EXPECTED_PASS in \
             tests/fixtures.rs to raise the floor (never leave an unrecorded green)."
        ))),
    }
}

/// A compact human diff pointing at the first differing line (0-based capped).
fn first_diff(expected: &str, got: &str) -> String {
    let e: Vec<&str> = expected.lines().collect();
    let g: Vec<&str> = got.lines().collect();
    for (i, (le, lg)) in e.iter().zip(g.iter()).enumerate() {
        if le != lg {
            return format!(
                "first diff at line {}:\n  expected: {:?}\n  got:      {:?}",
                i + 1,
                le,
                lg
            );
        }
    }
    format!(
        "output length differs (expected {} lines, got {} lines)",
        e.len(),
        g.len()
    )
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

fn main() {
    let args = Arguments::from_args();
    let root = testdata_root();
    let fixtures = discover();
    let total = fixtures.len();
    let passed = Arc::new(AtomicUsize::new(0));

    let floor = EXPECTED_PASS.len();
    let trials: Vec<Trial> = fixtures
        .into_iter()
        .map(|fx| {
            let name = fx.name.clone();
            // `xfail` fixtures are labelled so the milestone-1 red set is visible
            // in the libtest output rather than masquerading as a plain pass.
            let kind = if EXPECTED_PASS.contains(&name.as_str()) {
                "compile-diff"
            } else {
                "xfail"
            };
            let passed = Arc::clone(&passed);
            let root = root.clone();
            Trial::test(name, move || run_one(&fx, &root, &passed)).with_kind(kind)
        })
        .collect();

    let conclusion = libtest_mimic::run(&args, trials);

    let p = passed.load(Ordering::Relaxed);
    println!(
        "\nless.js {TAG} compile corpus (Phase 4B — @import + :extend + tests-config): \
         {p}/{total} passing (ratchet floor {floor}; {} xfail).",
        total - floor
    );

    conclusion.exit();
}
