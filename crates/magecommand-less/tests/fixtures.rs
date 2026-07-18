//! less.js v4.6.7 default-option compile-fixture harness (plan §5.5, Step 2).
//!
//! Vendored DEFAULT-OPTION compile fixtures only (`tests/fixtures/less-testdata/
//! tests-unit/`). Each `<name>.less` with a sibling `<name>.css` becomes one
//! [`libtest_mimic`] Trial that:
//!   1. compiles the input with default [`LessOptions`] + a filesystem
//!      [`ImportResolver`] rooted at the fixture's directory,
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
//! DEFERRED (plan §5.2, see NOTES.md): the option-driven `tests-config/` and
//! `tests-error/` suites, and — within `tests-unit/` — the `javascript`/`plugin*`
//! JS/plugin sub-suites.

use libtest_mimic::{Arguments, Failed, Trial};
use magecommand_less::{
    compile, FileInfo, ImportError, ImportPayload, ImportRequest, ImportResolver, LessOptions,
    ResolvedImport,
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
/// to hide a regression). 48/87 after Phase 3 (function library + strings: the
/// full §2.7 registry with min/max dual behavior, calc `_SELF` folding + paren
/// semantics, iterated interpolation, IE filters/`progid`, `anonymousValue`
/// raw declarations, value comments, merge `+:`/`+_:`, and `each()`).
const EXPECTED_PASS: &[&str] = &[
    "at-rules-declarations/at-rules-declarations",
    "at-rules-empty-block/at-rules-empty-block",
    "at-rules-empty/at-rules-empty",
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
    "comments/comments2",
    "css-3/css-3",
    "css-escapes/css-escapes",
    "css-grid/css-grid",
    "css-guards/css-guards",
    "empty/empty",
    "extract-and-length/extract-and-length",
    "ie-filters/ie-filters",
    "impor/impor",
    "lazy-eval/lazy-eval",
    "merge/merge",
    "mixin-noparens/mixin-noparens",
    "mixins-closure/mixins-closure",
    "mixins-guards-default-func/mixins-guards-default-func",
    "mixins-guards/mixins-guards",
    "mixins-important/mixins-important",
    "mixins-named-args/mixins-named-args",
    "mixins-nested/mixins-nested",
    "mixins-pattern/mixins-pattern",
    "mixins/mixins",
    "mixins/mixins-advanced",
    "no-output/no-output",
    "operations/operations",
    "operations/operations-advanced",
    "parser-slashed-combinator/parser-slashed-combinator",
    "plugi/plugi",
    "property-name-interp/property-name-interp",
    "rulesets/rulesets",
    "scope/scope",
    "strings/strings",
    "tailwind/tailwind",
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
/// rule (`.css` stays CSS; no extension ⇒ append `.less`, plan §2.9).
struct FsResolver {
    /// Directory the importing file lives in (imports are relative to it).
    root: PathBuf,
}

impl ImportResolver for FsResolver {
    fn resolve(&self, req: &ImportRequest) -> Result<ResolvedImport, ImportError> {
        let raw = req.path.as_str();
        let force_css = req.options.css == Some(true);
        let force_less = req.options.css == Some(false);
        let is_css = force_css || (!force_less && raw.ends_with(".css"));

        let mut candidate = self.root.join(raw);
        if candidate.extension().is_none() && !is_css {
            candidate.set_extension("less");
        }

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

        let payload = if is_css {
            ImportPayload::Css(Arc::from(bytes.as_str()))
        } else if req.options.inline {
            ImportPayload::Inline(Arc::from(bytes.as_str()))
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
        fs::read(base.join(path)).ok()
    }
}

// ---------------------------------------------------------------------------
// Fixture discovery
// ---------------------------------------------------------------------------

/// Every in-scope `.less` input (a `.less` with a sibling `.css`, outside the
/// deferred JS/plugin sub-suites), sorted for a stable test order.
fn discover() -> Vec<PathBuf> {
    let unit = testdata_root().join("tests-unit");
    let mut out = Vec::new();
    walk(&unit, &mut out);
    out.retain(|less| less.with_extension("css").is_file() && !is_skipped(less));
    out.sort();
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

fn run_one(less: &Path, root: &Path, passed: &AtomicUsize) -> Result<(), Failed> {
    let src = fs::read_to_string(less)
        .map_err(|e| Failed::from(format!("read input {}: {e}", less.display())))?;
    let expected_raw = fs::read_to_string(less.with_extension("css"))
        .map_err(|e| Failed::from(format!("read expected {}: {e}", less.display())))?;

    let dir = less.parent().unwrap_or(root);
    let opts = LessOptions {
        filename: Some(less.display().to_string()),
        ..LessOptions::default()
    };
    let resolver = FsResolver {
        root: dir.to_path_buf(),
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
    let name = trial_name(less);
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
        .map(|less| {
            let name = trial_name(&less);
            // `xfail` fixtures are labelled so the milestone-1 red set is visible
            // in the libtest output rather than masquerading as a plain pass.
            let kind = if EXPECTED_PASS.contains(&name.as_str()) {
                "compile-diff"
            } else {
                "xfail"
            };
            let passed = Arc::clone(&passed);
            let root = root.clone();
            Trial::test(name, move || run_one(&less, &root, &passed)).with_kind(kind)
        })
        .collect();

    let conclusion = libtest_mimic::run(&args, trials);

    let p = passed.load(Ordering::Relaxed);
    println!(
        "\nless.js {TAG} default-option compile corpus (Phase 3 — functions + strings): \
         {p}/{total} passing (ratchet floor {floor}; {} xfail).",
        total - floor
    );

    conclusion.exit();
}
