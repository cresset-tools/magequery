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
//! The runner prints a pass-rate `X/Y`. **Passthrough state:** nearly every
//! fixture fails now — that is EXPECTED before the Phase-1 evaluator lands. The
//! ratchet gate + libtest-mimic per-fixture xfail allowlist (plan §5.6) arrive
//! with the real engine; today the harness only has to build, run, and report.
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

    let got = compile(&src, &opts, &resolver)
        .map_err(|e| Failed::from(format!("compile error: {e}")))?;

    let expected = do_replacements(&expected_raw, dir, root);
    if strip_trailing_newlines(&got.code) == strip_trailing_newlines(&expected) {
        passed.fetch_add(1, Ordering::Relaxed);
        Ok(())
    } else {
        Err(Failed::from(first_diff(&expected, &got.code)))
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

    let trials: Vec<Trial> = fixtures
        .into_iter()
        .map(|less| {
            let name = trial_name(&less);
            let passed = Arc::clone(&passed);
            let root = root.clone();
            Trial::test(name, move || run_one(&less, &root, &passed)).with_kind("compile-diff")
        })
        .collect();

    let conclusion = libtest_mimic::run(&args, trials);

    let p = passed.load(Ordering::Relaxed);
    println!(
        "\nless.js {TAG} default-option compile fixtures (Step 3, parser + plain-CSS genCSS): {p}/{total} passing"
    );

    conclusion.exit();
}
