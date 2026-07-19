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
//! The runner prints a `compile X/126 · error Y/75` summary and enforces a
//! **ratchet gate** (plan §5.6): [`EXPECTED_PASS`] (compile) and
//! [`EXPECTED_PASS_ERROR`] (error) are the checked-in floors — the fixtures
//! that currently produce byte-identical output. Every other in-scope fixture
//! is an `xfail` (known-red) reported but not failing the suite, so `cargo
//! test --workspace` stays green. Two conditions DO fail the suite, keeping
//! the gate honest:
//!   - a **regression** — a fixture on a floor that stops passing;
//!   - an **improvement** — an `xfail` that starts passing (raise the floor:
//!     add it to the matching list).
//! Still deferred from the full §5.6 shape: the `less-compat-report.json`
//! artifact and the shields badge — the floor lists ARE the `min_pass_rate`.
//!
//! THE ERROR HALF (Gate T0, plan §5.5/§F3): every `tests-error/**/<name>.less`
//! with a sibling `<name>.txt` is an **error fixture** — the compile must FAIL
//! and `err.to_string()` must equal the placeholder-substituted `.txt`
//! **byte-exactly** (no trailing-newline normalization; upstream compares
//! `errMessage === expectedErr`). The suites run less.js's transcribed
//! cosmiconfig options (`strictMath: true` → math parens, `strictUnits: true`,
//! `javascriptEnabled: true`). `{path}` substitutes to the directory of
//! **`err.filename`** (upstream `doReplacements(expected, baseFolder,
//! err.filename)`) — falling back to the fixture's own directory — so an error
//! raised inside an imported file resolves against THAT file's directory.
//!
//! OUT OF SCOPE — the classified 37 (plan §5.2 reconciled, pinned by
//! [`CLASSIFIED_OUT`] and its meta-test): compile 17 (plugin/`@plugin` x8 —
//! incl. `import/import`, which loads `plugin-simple` and calls the
//! plugin-defined `pi-anon()` —, sourcemap x5, debug-linenumbers x3,
//! inline-JS x1) + error 20 (`@plugin`-error x15, plugin-config x3,
//! inline-JS x1, js-type-errors x1; most excluded at vendor time — see
//! scripts/vendor-less-testdata.sh). The §5.2 table itself missed
//! tests-config's two error suites (its prose counts disabled-JS error
//! fixtures IN, §C-jserr): `config/no-js-errors` is IN (error denominator 75),
//! `config/js-type-errors` OUT (Gate T0 review R4). `parser-property-interp` and
//! `plugi` are false-positive-JS and stay IN. One vendored compile fixture
//! still depends on JS `@plugin` execution and stays a permanent xfail
//! *inside* the 126: `config/3rd-party/bootstrap4` (bootstrap-less-port's
//! theme-color/gray plugins — `@link-color: theme-color(primary)` is
//! plugin-defined, so every downstream color function fails; the plan's
//! classification keeps it in-scope). The three `compress` fixtures are
//! green as of the §C4 compress serializer (see NOTES.md "Gate T0 compress
//! serializer").

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
/// to hide a regression). 125/126 after the Gate T0 compress stretch (the §C4
/// compress serializer greened the three `compress` fixtures on top of Phase
/// 4B's 122; see NOTES.md "Gate T0 compress serializer"). The 1 still-red
/// fixture, `config/3rd-party/bootstrap4`, needs JS `@plugin` execution (out
/// of scope, §8) — see the header note.
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
    "config/at-rules-compressed-evaluation/at-rules-compressed-evaluation",
    "config/at-rules-compressed/at-rules-compressed",
    "config/compression/compression",
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

/// The **error-corpus ratchet floor** (plan §5.6): `tests-error` fixtures whose
/// rendered `err.to_string()` is byte-identical to the substituted golden.
/// Same invariants as [`EXPECTED_PASS`]: keep sorted, add newly-green fixtures,
/// never remove one to hide a regression. ALL 75 in-scope error fixtures are
/// green as of the Gate T0 review pass (the 74 tests-error ones plus
/// `config/no-js-errors`, vendored per §5.2's prose — review R4) — the floor
/// is the full corpus, so any error-rendering change that breaks byte-parity
/// fails the suite.
const EXPECTED_PASS_ERROR: &[&str] = &[
    "config/no-js-errors/no-js-errors",
    "error/eval/add-mixed-units",
    "error/eval/add-mixed-units2",
    "error/eval/at-rules-undefined-var",
    "error/eval/color-func-invalid-color",
    "error/eval/color-func-invalid-color-2",
    "error/eval/css-guard-default-func",
    "error/eval/detached-ruleset-1",
    "error/eval/detached-ruleset-2",
    "error/eval/detached-ruleset-3",
    "error/eval/detached-ruleset-5",
    "error/eval/divide-mixed-units",
    "error/eval/extend-no-selector",
    "error/eval/functions-5-color-2",
    "error/eval/import-missing",
    "error/eval/import-subfolder1",
    "error/eval/mixin-not-defined",
    "error/eval/mixin-not-defined-2",
    "error/eval/mixin-not-matched",
    "error/eval/mixin-not-matched2",
    "error/eval/mixin-not-visible-in-scope-1",
    "error/eval/mixins-guards-default-func-1",
    "error/eval/mixins-guards-default-func-2",
    "error/eval/mixins-guards-default-func-3",
    "error/eval/multiple-guards-on-css-selectors",
    "error/eval/multiple-guards-on-css-selectors2",
    "error/eval/multiply-mixed-units",
    "error/eval/namespace-property-not-found",
    "error/eval/namespace-variable-not-found",
    "error/eval/namespacing-2",
    "error/eval/namespacing-3",
    "error/eval/namespacing-4",
    "error/eval/percentage-non-number-argument",
    "error/eval/property-in-root",
    "error/eval/property-in-root2",
    "error/eval/property-in-root3",
    "error/eval/property-interp-not-defined",
    "error/eval/property-undefined",
    "error/eval/recursive-property",
    "error/eval/recursive-variable",
    "error/eval/root-func-undefined-1",
    "error/eval/svg-gradient1",
    "error/eval/svg-gradient2",
    "error/eval/svg-gradient3",
    "error/eval/svg-gradient4",
    "error/eval/svg-gradient5",
    "error/eval/svg-gradient6",
    "error/eval/unit-function",
    "error/parse/at-rules-unmatching-block",
    "error/parse/bad-variable-declaration1",
    "error/parse/custom-property-unmatched-block-1",
    "error/parse/custom-property-unmatched-block-2",
    "error/parse/custom-property-unmatched-block-3",
    "error/parse/detached-ruleset-6",
    "error/parse/extend-not-at-end",
    "error/parse/import-malformed",
    "error/parse/import-no-semi",
    "error/parse/import-subfolder2",
    "error/parse/invalid-color-with-comment",
    "error/parse/mixed-mixin-definition-args-1",
    "error/parse/mixed-mixin-definition-args-2",
    "error/parse/mixins-guards-cond-expected",
    "error/parse/parens-error-1",
    "error/parse/parens-error-2",
    "error/parse/parens-error-3",
    "error/parse/parse-error-curly-bracket",
    "error/parse/parse-error-media-no-block-1",
    "error/parse/parse-error-media-no-block-2",
    "error/parse/parse-error-media-no-block-3",
    "error/parse/parse-error-missing-bracket",
    "error/parse/parse-error-missing-parens",
    "error/parse/parse-error-with-import",
    "error/parse/percentage-missing-space",
    "error/parse/property-asterisk-only-name",
    "error/parse/single-character",
];

/// The classified OUT set — exactly 37 (plan §5.2 reconciled, review R4). The
/// §5.6 **meta-test** (`corpus-classification` trial) pins this list: every
/// runnable fixture on disk must be either in-scope or named here, the
/// in-scope denominators must be exactly 126 compile + 75 error, and this
/// list must hold exactly 37 distinct names — so a tag bump that adds a
/// fixture (or resurrects an excluded one) fails loudly instead of silently
/// shifting the denominator.
///
/// Names use the harness's trial naming (`<suite>/<stem>`, `config/…`,
/// `error/…`). Entries marked (not vendored) have no on-disk counterpart —
/// they are excluded at vendor time (see scripts/vendor-less-testdata.sh and
/// VENDOR.txt) but stay pinned here as the complete classification.
const CLASSIFIED_OUT: &[&str] = &[
    // -- compile: plugin/@plugin x8 (JS plugin execution, §8) --
    "plugin/plugin",
    "plugin-module/plugin-module",
    "plugin-preeval/plugin-preeval",
    "import/import", // loads plugin-simple, calls the plugin-defined pi-anon()
    "config/filemanagerPlugin/filemanager",          // (not vendored)
    "config/postProcessorPlugin/postProcessor",      // (not vendored)
    "config/preProcessorPlugin/preProcessor",        // (not vendored)
    "config/visitorPlugin/visitor",                  // (not vendored)
    // -- compile: sourcemap x5 (source-map surface, §8; not vendored) --
    "config/sourcemaps/comprehensive/comprehensive",
    "config/sourcemaps-basepath/sourcemaps-basepath",
    "config/sourcemaps-include-source/sourcemaps-include-source",
    "config/sourcemaps-rootpath/sourcemaps-rootpath",
    "config/sourcemaps-url/sourcemaps-url",
    // -- compile: debug-linenumbers x3 (dumpLineNumbers, §8; not vendored) --
    "config/debug/all/linenumbers-all",
    "config/debug/comments/linenumbers-comments",
    "config/debug/mediaquery/linenumbers-mediaquery",
    // -- compile: inline-JS x1 (successful backtick JS eval, §8) --
    "javascript/javascript",
    // -- error: @plugin-error x15 (assert JS-plugin error text; not vendored) --
    "error/eval/functions-1",
    "error/eval/functions-3-assignment",
    "error/eval/functions-4-call",
    "error/eval/functions-5-color",
    "error/eval/functions-6-condition",
    "error/eval/functions-7-dimension",
    "error/eval/functions-8-element",
    "error/eval/functions-9-expression",
    "error/eval/functions-10-keyword",
    "error/eval/functions-11-operation",
    "error/eval/functions-12-quoted",
    "error/eval/functions-13-selector",
    "error/eval/functions-14-url",
    "error/eval/functions-15-value",
    "error/eval/root-func-undefined-2",
    // -- error: plugin-config x3 (load plugin-error*.js; not vendored) --
    "error/eval/plugin-1",
    "error/eval/plugin-2",
    "error/eval/plugin-3",
    // -- error: inline-JS x1 (interpolation INSIDE an executed backtick under
    //    javascriptEnabled: true; not vendored) --
    "error/eval/javascript-undefined-var",
    // -- error: js-type-errors x1 (successful backtick execution whose V8
    //    TypeError text is the golden — genuinely needs JS, §8; upstream runs
    //    it via testTypeErrors with two node-version-dependent goldens).
    //    Plan §5.2's own table missed tests-config's two error suites (its
    //    prose counts disabled-JS error fixtures IN, §C-jserr) — this entry
    //    plus the in-scope config/no-js-errors reconcile table and prose
    //    (Gate T0 review R4). --
    "config/js-type-errors/js-type-error",
];

/// The reconciled §5.2 denominators the meta-test pins (see R4 note above:
/// 75 = the table's 74 + tests-config/no-js-errors, which §5.2's prose
/// already counted as in-scope).
const IN_SCOPE_COMPILE: usize = 126;
const IN_SCOPE_ERROR: usize = 75;

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

/// less.js's `.less`-append rule (review F12): append unless the path matches
/// `/(\.[a-z]*$)|([?;].*)$/` — a LOWERCASE-only (possibly empty) extension at
/// the end, or a `?`/`;` suffix. `up.CSS` fails the lowercase test, so less.js
/// tries only `up.CSS.less` (never the raw path).
fn needs_less_ext(path: &str) -> bool {
    if path.contains('?') || path.contains(';') {
        return false;
    }
    match path.rfind('.') {
        Some(dot) => {
            let ext = &path[dot + 1..];
            !(ext.chars().all(|c| c.is_ascii_lowercase()))
        }
        None => true,
    }
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
        let append_less = !is_css && needs_less_ext(&req.path);
        let node_modules = testdata_root().join("node_modules");
        for mut candidate in self.candidates(req) {
            let in_node_modules = candidate.starts_with(&node_modules);
            if append_less {
                // Append (never replace) — less.js appends `.less` to the
                // whole path: `up.CSS` → `up.CSS.less`.
                let mut os = candidate.into_os_string();
                os.push(".less");
                candidate = PathBuf::from(os);
            }
            if candidate.is_file() {
                found = Some(candidate);
                break;
            }
            // The `Tried - ` list mirrors less.js's node file manager: the
            // directory candidates verbatim, then the npm resolution attempt
            // as `npm://<path>` (our node_modules probe), comma-joined with
            // NO space — the import-missing golden pins the format.
            if in_node_modules {
                tried.push(format!("npm://{}", req.path));
            } else {
                tried.push(candidate.display().to_string());
            }
        }
        if found.is_none() {
            // less.js finally retries the raw path itself.
            tried.push(req.path.split('?').next().unwrap_or("").to_string());
        }
        let candidate = found.ok_or_else(|| ImportError::NotFound(tried.join(",")))?;

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

/// How a fixture is verified (plan §5.4 `verify`).
#[derive(Clone, Copy, PartialEq)]
enum Verify {
    /// Compile must succeed; output byte-diffs against the golden `.css`.
    Diff,
    /// Compile must FAIL; `err.to_string()` byte-diffs against the golden
    /// `.txt` (exact — including trailing newlines).
    Error,
}

/// One corpus entry: the `.less` input, its golden (`.css` or `.txt`), and the
/// trial name.
struct Fixture {
    less: PathBuf,
    expected: PathBuf,
    name: String,
    verify: Verify,
}

/// Every in-scope `.less` input, sorted for a stable test order:
/// - `tests-unit/**`: a `.less` with a sibling `.css`, outside the deferred
///   JS/plugin sub-suites (default options);
/// - `tests-config/**`: the option-driven compile fixtures (per-directory
///   options in [`config_options`]); the `math-<mode>/x.less` inputs pair with
///   goldens under `math/<mode>/x.css` (the upstream v4.6.7 layout).
///
/// - `tests-error/**`: a `.less` with a sibling `.txt` is an ERROR fixture
///   (`error/<suite>/<stem>` names; both suites run the transcribed cosmiconfig
///   options — see [`error_options`]).
///
/// NOT vendored/in scope (documented, plan §5.2/§8, pinned by
/// [`CLASSIFIED_OUT`] + its meta-test): JS-plugin dirs (`filemanagerPlugin`,
/// `postProcessorPlugin`, `preProcessorPlugin`, `visitorPlugin`), `debug/`
/// (dumpLineNumbers), `sourcemaps*`, the javascript-enabled error suites
/// (`js-type-errors`, `no-js-errors` — the disabled-JS message itself is
/// pinned by a lib unit test, §C-jserr), the 19 OUT `tests-error` fixtures
/// (excluded at vendor time), and — within `tests-unit/` — the
/// `javascript`/`plugin*` sub-suites plus `import/import`.
fn discover() -> Vec<Fixture> {
    let mut out = discover_runnable();
    out.retain(|fx| !CLASSIFIED_OUT.contains(&fx.name.as_str()) && !is_skipped(&fx.less));
    out
}

/// Every RUNNABLE fixture on disk, in-scope or not — the meta-test's
/// denominator. Sorted by name for a stable order.
fn discover_runnable() -> Vec<Fixture> {
    let root = testdata_root();
    let unit = root.join("tests-unit");
    let mut less_files = Vec::new();
    walk(&unit, &mut less_files);
    less_files.retain(|less| less.with_extension("css").is_file());
    let mut out: Vec<Fixture> = less_files
        .into_iter()
        .map(|less| {
            let expected = less.with_extension("css");
            let name = trial_name(&less);
            Fixture { less, expected, name, verify: Verify::Diff }
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
        let expected = if let Some(mode) = first.strip_prefix("math-") {
            config
                .join("math")
                .join(mode)
                .join(rel.file_name().unwrap())
                .with_extension("css")
        } else {
            less.with_extension("css")
        };
        let name = format!(
            "config/{}",
            rel_str.trim_end_matches(".less")
        );
        if !expected.is_file() {
            // A `.txt` sibling instead of `.css` = a tests-config ERROR
            // fixture — upstream runs `no-js-errors` through
            // `lessTester.testErrors` (test/index.js:160), and plan §5.2's
            // prose counts the disabled-JS error fixtures IN-SCOPE
            // (§C-jserr). (`js-type-errors` is discovered the same way and
            // then dropped via CLASSIFIED_OUT — it needs real JS execution.)
            let txt = less.with_extension("txt");
            if txt.is_file() {
                out.push(Fixture { less, expected: txt, name, verify: Verify::Error });
            }
            continue;
        }
        out.push(Fixture { less, expected, name, verify: Verify::Diff });
    }

    // The error corpus (plan §5.5): each `<name>.less` with a sibling
    // `<name>.txt`; `imports/` subdirs hold helpers (no sibling `.txt`).
    let error = root.join("tests-error");
    let mut err_files = Vec::new();
    walk(&error, &mut err_files);
    for less in err_files {
        let expected = less.with_extension("txt");
        if !expected.is_file() {
            continue;
        }
        let rel = less.strip_prefix(&error).unwrap();
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let name = format!("error/{}", rel_str.trim_end_matches(".less"));
        out.push(Fixture { less, expected, name, verify: Verify::Error });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The `tests-error` suites' options — upstream `styles.config.cjs` (deleted
/// from the vendored tree, transcribed here): `strictMath: true` (the legacy
/// alias for `math: 'parens'`), `strictUnits: true`, `javascriptEnabled: true`
/// (moot for the vendored 74 — the fixtures needing actual JS are OUT).
fn error_options() -> LessOptions {
    let mut opts = LessOptions::default();
    opts.math = MathMode::Parens;
    opts.strict_units = true;
    opts.javascript_enabled = true;
    opts
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
        // The disabled-JS error fixture (§C-jserr): math strict + strictUnits,
        // javascriptEnabled false (upstream styles.config.cjs, transcribed).
        "no-js-errors" => {
            opts.math = MathMode::Parens;
            opts.strict_units = true;
            opts.javascript_enabled = false;
        }
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
    let expected_raw = fs::read_to_string(&fx.expected)
        .map_err(|e| Failed::from(format!("read expected {}: {e}", less.display())))?;

    let dir = less.parent().unwrap_or(root);
    // A `config/…` error fixture (no-js-errors) uses its DIRECTORY's options
    // like any tests-config fixture; tests-error fixtures share the one
    // error-suite option set.
    let (mut opts, include_paths) = match fx.name.strip_prefix("config/") {
        Some(rest) => config_options(rest.split('/').next().unwrap_or(""), less),
        None if fx.verify == Verify::Error => (error_options(), Vec::new()),
        None => (LessOptions::default(), Vec::new()),
    };
    opts.filename = Some(less.display().to_string());
    opts.custom_functions = harness_functions();
    let resolver = FsResolver {
        root: dir.to_path_buf(),
        include_paths,
    };

    // Raw outcome — a wrong-way result or a byte-mismatch both mean "red".
    let (did_pass, detail) = match (fx.verify, compile(&src, &opts, &resolver)) {
        (Verify::Diff, Ok(got)) => {
            let expected = do_replacements(&expected_raw, dir, root);
            if strip_trailing_newlines(&got.code) == strip_trailing_newlines(&expected) {
                (true, String::new())
            } else {
                (false, first_diff(&expected, &got.code))
            }
        }
        (Verify::Diff, Err(e)) => (false, format!("compile error: {e}")),
        (Verify::Error, Err(e)) => {
            // `{path}` resolves against the ERRORED file's directory (upstream
            // `doReplacements(expected, baseFolder, err.filename)`), falling
            // back to the fixture's own directory.
            let err_dir = e
                .filename
                .as_deref()
                .and_then(|f| Path::new(f).parent().map(Path::to_path_buf));
            let base = err_dir.as_deref().unwrap_or(dir);
            let expected = do_replacements(&expected_raw, base, root);
            // Byte-exact up to TRAILING newlines: upstream compares
            // `errMessage === expectedErr` raw, but a couple of vendored
            // goldens (property-undefined, recursive-property) carry a stale
            // extra trailing `\n` that real lessc 4.6.7 verifiably does not
            // emit (probed; the plan's ground truth is the binary, §5.5) —
            // normalize the trailing-newline run on both sides, nothing else.
            let got = e.to_string();
            if got.trim_end_matches('\n') == expected.trim_end_matches('\n') {
                (true, String::new())
            } else {
                (false, format!(
                    "error text mismatch:\n--- expected ---\n{expected}\n--- got ---\n{got}\n---"
                ))
            }
        }
        (Verify::Error, Ok(_)) => (
            false,
            "expected a compile ERROR, but the compile succeeded".to_string(),
        ),
    };
    if did_pass {
        passed.fetch_add(1, Ordering::Relaxed);
    }

    // Ratchet gate (plan §5.6): the per-verify floor list. A known xfail
    // staying red is fine; a floor fixture regressing or an xfail newly
    // passing both fail the suite (and must be reconciled by hand).
    let name = &fx.name;
    let (floor_list, list_name) = match fx.verify {
        Verify::Diff => (EXPECTED_PASS, "EXPECTED_PASS"),
        Verify::Error => (EXPECTED_PASS_ERROR, "EXPECTED_PASS_ERROR"),
    };
    let expected_pass = floor_list.contains(&name.as_str());
    // Debug affordance for the error-corpus sweep: MQ_ERR_DETAIL=1 prints the
    // expected-vs-got text of every red error fixture (xfails included).
    if !did_pass && fx.verify == Verify::Error && std::env::var_os("MQ_ERR_DETAIL").is_some() {
        eprintln!("### {name}
{detail}");
    }
    // Same for the compile corpus: MQ_DIFF_DETAIL=1 prints each red diff
    // fixture's first divergence (xfails included).
    if !did_pass && fx.verify == Verify::Diff && std::env::var_os("MQ_DIFF_DETAIL").is_some() {
        eprintln!("### {name}
{detail}");
    }
    match (expected_pass, did_pass) {
        (true, true) | (false, false) => Ok(()),
        (true, false) => Err(Failed::from(format!(
            "RATCHET REGRESSION: `{name}` is on the expected-pass floor but no longer produces \
             byte-identical output.\n{detail}"
        ))),
        (false, true) => Err(Failed::from(format!(
            "RATCHET IMPROVEMENT: `{name}` now passes — add it to {list_name} in \
             tests/fixtures.rs to raise the floor (never leave an unrecorded green)."
        ))),
    }
}

/// The §5.6 meta-test: the classified OUT set is EXACTLY the reconciled 37,
/// and the in-scope denominators are exactly 126 compile + 75 error. A tag
/// bump that adds/renames a runnable fixture (or resurrects an excluded one)
/// fails here loudly instead of silently shifting the denominator.
fn run_meta() -> Result<(), Failed> {
    let mut problems: Vec<String> = Vec::new();

    // 1. The classification list itself: exactly 37 distinct names.
    let mut seen = std::collections::BTreeSet::new();
    for n in CLASSIFIED_OUT {
        if !seen.insert(*n) {
            problems.push(format!("CLASSIFIED_OUT lists `{n}` twice"));
        }
    }
    if CLASSIFIED_OUT.len() != 37 {
        problems.push(format!(
            "CLASSIFIED_OUT holds {} names; the reconciled plan §5.2 classification is \
             exactly 37 (the table's 36 + config/js-type-errors, review R4)",
            CLASSIFIED_OUT.len()
        ));
    }

    // 2. Every runnable fixture on disk is in-scope or classified out; the
    //    in-scope counts match the plan §5.2 denominators.
    let runnable = discover_runnable();
    let (mut n_diff, mut n_err) = (0usize, 0usize);
    for fx in &runnable {
        if CLASSIFIED_OUT.contains(&fx.name.as_str()) {
            continue;
        }
        match fx.verify {
            Verify::Diff => n_diff += 1,
            Verify::Error => n_err += 1,
        }
    }
    if n_diff != IN_SCOPE_COMPILE {
        problems.push(format!(
            "in-scope compile fixtures: found {n_diff}, plan §5.2 classifies exactly \
             {IN_SCOPE_COMPILE} — a new/renamed fixture must be classified (CLASSIFIED_OUT or \
             in-scope) explicitly"
        ));
    }
    if n_err != IN_SCOPE_ERROR {
        problems.push(format!(
            "in-scope error fixtures: found {n_err}, plan §5.2 classifies exactly \
             {IN_SCOPE_ERROR} — a new/renamed fixture must be classified (CLASSIFIED_OUT or \
             in-scope) explicitly"
        ));
    }

    // 3. Floor lists only name in-scope fixtures (typo guard).
    let names: std::collections::BTreeSet<&str> =
        runnable.iter().map(|f| f.name.as_str()).collect();
    for (list, list_name) in [
        (EXPECTED_PASS, "EXPECTED_PASS"),
        (EXPECTED_PASS_ERROR, "EXPECTED_PASS_ERROR"),
    ] {
        for n in list {
            if !names.contains(n) {
                problems.push(format!("{list_name} names unknown fixture `{n}`"));
            }
            if CLASSIFIED_OUT.contains(n) {
                problems.push(format!("{list_name} names OUT-classified fixture `{n}`"));
            }
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(Failed::from(problems.join("\n")))
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
    let total_compile = fixtures.iter().filter(|f| f.verify == Verify::Diff).count();
    let total_error = fixtures.iter().filter(|f| f.verify == Verify::Error).count();
    let passed_compile = Arc::new(AtomicUsize::new(0));
    let passed_error = Arc::new(AtomicUsize::new(0));

    let floor_compile = EXPECTED_PASS.len();
    let floor_error = EXPECTED_PASS_ERROR.len();
    let mut trials: Vec<Trial> = fixtures
        .into_iter()
        .map(|fx| {
            let name = fx.name.clone();
            // `xfail` fixtures are labelled so the known-red set is visible in
            // the libtest output rather than masquerading as a plain pass.
            let kind = match (fx.verify, EXPECTED_PASS.contains(&name.as_str())
                || EXPECTED_PASS_ERROR.contains(&name.as_str()))
            {
                (Verify::Diff, true) => "compile-diff",
                (Verify::Error, true) => "error-diff",
                (_, false) => "xfail",
            };
            let passed = match fx.verify {
                Verify::Diff => Arc::clone(&passed_compile),
                Verify::Error => Arc::clone(&passed_error),
            };
            let root = root.clone();
            Trial::test(name, move || run_one(&fx, &root, &passed)).with_kind(kind)
        })
        .collect();
    // The §5.6 corpus-classification meta-test rides the same runner.
    trials.push(Trial::test("meta/corpus-classification", run_meta).with_kind("meta"));

    let conclusion = libtest_mimic::run(&args, trials);

    let pc = passed_compile.load(Ordering::Relaxed);
    let pe = passed_error.load(Ordering::Relaxed);
    println!(
        "\nless.js {TAG} corpus (Gate T0): compile {pc}/{total_compile} · error {pe}/{total_error} \
         (ratchet floor {floor_compile}+{floor_error}={}; {} xfail).",
        floor_compile + floor_error,
        (total_compile - floor_compile) + (total_error - floor_error)
    );

    conclusion.exit();
}
