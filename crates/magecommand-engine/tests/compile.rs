//! The committed compile gate: `cargo test -p magecommand-engine` runs the
//! whole DI compile over synthetic Magento trees and pins every emitted byte.
//!
//! The compile's real oracle is `magecommand di verify` against an archived
//! `setup:di:compile` — 4106 code + 16 metadata files reproduced exactly. That
//! oracle needs a 685 MB install *and* the archive, so it runs on one machine
//! and never in CI. This is the other half: small enough to commit, complete
//! enough to fail when an emitter changes shape.
//!
//! Each case under `tests/compile-fixtures/<case>/` is
//!   - `in/` — a synthetic Magento root (`app/etc/config.php`, module.xml,
//!     di.xml, PHP sources). Read-only: the compute never touches disk.
//!   - `expected.txt` — every file [`compute_outputs`] would write under
//!     `generated/`, concatenated with a header line per file, then the
//!     run's `findings` and `unresolved` diagnostics.
//!
//! One golden document per case rather than a mirrored output tree, because
//! Magento's plugin-list cache names embed the scope separator `|`
//! (`metadata/global|primary|plugin-list.php`) — a character Windows will not
//! put in a filename, so a mirrored tree could not be checked out at all.
//!
//! Intentional changes re-bless:
//!
//! ```sh
//! MAGECOMMAND_COMPILE_BLESS=1 cargo test -p magecommand-engine --test compile
//! ```

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use magecommand_engine::build::compute_outputs;
use magecommand_engine::definitions::Definitions;
use magequery_core::Magento;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/compile-fixtures")
}

const HEADER: &str = "\
# magecommand-engine compile fixture — the expected output of compute_outputs()
# over `in/`, byte for byte. Everything below is generated; edit `in/` instead.
# Re-bless: MAGECOMMAND_COMPILE_BLESS=1 cargo test -p magecommand-engine --test compile
";

/// Render one compile's complete output as the golden document. Files are
/// sorted by name: their order in `CompileOutputs` carries no meaning (the
/// writer keys by path) and the scan is parallel, so pinning it would pin
/// rayon's scheduling.
fn render(files: &[(String, String)], findings: &[String], unresolved: &[String]) -> String {
    let mut sorted: Vec<&(String, String)> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::from(HEADER);
    let _ = writeln!(
        out,
        "# files: {}  findings: {}  unresolved: {}",
        sorted.len(),
        findings.len(),
        unresolved.len()
    );
    for (name, body) in sorted {
        let _ = write!(out, "\n===== file: {name} =====\n{body}");
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    // Diagnostics are output too: a compile that starts failing to resolve a
    // class, or stops reporting one, has changed behaviour.
    let mut f: Vec<&String> = findings.iter().collect();
    f.sort();
    let mut u: Vec<&String> = unresolved.iter().collect();
    u.sort();
    out.push_str("\n===== findings =====\n");
    for line in f {
        let _ = writeln!(out, "{line}");
    }
    out.push_str("\n===== unresolved =====\n");
    for line in u {
        let _ = writeln!(out, "{line}");
    }
    out
}

/// Run the compile for one fixture root.
fn compile_fixture(case_in: &Path) -> String {
    let magento = Magento::open(case_in)
        .unwrap_or_else(|e| panic!("open synthetic root {}: {e}", case_in.display()));
    // No `generated/code` in a fixture: nothing has been compiled before, so
    // the scan sees only the module sources. (A previously-generated tree is
    // an input to the real compile; a fixture that needs one commits it.)
    let generated_code = case_in.join("generated/code");
    let mut defs = Definitions::scan(&magento, case_in, &generated_code);
    let out = compute_outputs(&magento, &mut defs, case_in);
    render(&out.files, &out.findings, &out.unresolved)
}

#[test]
fn compile_fixtures() {
    // Bless on the VALUE, never on presence: a variable left exported in a
    // shell must not silently rewrite the goldens and report green.
    let blessing = match std::env::var("MAGECOMMAND_COMPILE_BLESS") {
        Ok(v) => {
            let v = v.trim().to_owned();
            assert!(
                matches!(v.as_str(), "0" | "1"),
                "MAGECOMMAND_COMPILE_BLESS must be 0 or 1, got {v:?}"
            );
            v == "1"
        }
        Err(_) => false,
    };

    let dir = fixtures_dir();
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        // A case is a directory with an `in/`. Anything else in here is not a
        // fixture — notably `vendor/bougie/`, which any `php` invocation run
        // from this directory materialises.
        .filter(|p| p.join("in").is_dir())
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "no fixtures under {}", dir.display());

    let mut problems: Vec<String> = Vec::new();
    let mut total_files = 0usize;

    for case in &cases {
        let name = case.file_name().unwrap().to_string_lossy().to_string();
        let case_in = case.join("in");
        assert!(
            case_in.join("app/etc/config.php").is_file(),
            "{name}: fixture has no in/app/etc/config.php — Magento::open needs one"
        );

        let fresh = compile_fixture(&case_in);
        total_files += fresh.matches("\n===== file: ").count();
        let golden = case.join("expected.txt");

        if blessing {
            std::fs::write(&golden, &fresh).expect("write expected.txt");
            println!("bless: {name} — {} bytes", fresh.len());
            continue;
        }

        let Ok(old) = std::fs::read_to_string(&golden) else {
            problems.push(format!(
                "{name}: no expected.txt — bless it (MAGECOMMAND_COMPILE_BLESS=1)"
            ));
            continue;
        };
        if old != fresh {
            problems.push(format!("{name}:\n{}", first_difference(&old, &fresh)));
        }
    }

    if blessing {
        return;
    }

    println!(
        "compile fixtures: {} cases · {total_files} emitted files",
        cases.len()
    );
    assert!(
        problems.is_empty(),
        "compile gate failed — a compile changed shape. Either a regression \
         (fix it) or an intentional change (re-bless and review the diff):\n\n{}",
        problems.join("\n\n")
    );
}

/// The first differing region, with a little context. A whole-document diff of
/// a 20-file compile is unreadable in a test failure; the first difference is
/// almost always the whole story.
fn first_difference(old: &str, new: &str) -> String {
    // Line endings first: `lines()` strips \r\n and \n alike, so a golden
    // checked out with CRLF compares equal on every line while the documents
    // differ — which reads as "no visible difference" and sent one Windows CI
    // failure the long way round. `.gitattributes` marks these fixtures
    // `-text` to prevent it; this names it if that ever stops working.
    if old.contains("\r\n") != new.contains("\r\n") {
        let (crlf, lf) = if old.contains("\r\n") {
            ("expected.txt", "the compile")
        } else {
            ("the compile", "expected.txt")
        };
        return format!(
            "  line endings differ: {crlf} uses CRLF, {lf} uses LF. The fixtures are \
             `-text` in .gitattributes precisely so a Windows checkout does not convert \
             them — check that the attribute still covers this path."
        );
    }
    let (o, n): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let at = (0..o.len().max(n.len())).find(|&i| o.get(i) != n.get(i));
    let Some(at) = at else {
        return "  documents differ but every line compares equal — a trailing-newline \
                or lone-\\r difference"
            .to_owned();
    };
    // Name the file the difference falls in — the header above the hit.
    let file = o[..=at.min(o.len().saturating_sub(1))]
        .iter()
        .rev()
        .find_map(|l| {
            l.strip_prefix("===== file: ")
                .and_then(|n| n.strip_suffix(" ====="))
        })
        .unwrap_or("(header)");
    let lo = at.saturating_sub(3);
    let mut s = format!("  first difference at line {} (in {file}):\n", at + 1);
    for i in lo..at {
        let _ = writeln!(s, "     {}", o.get(i).unwrap_or(&""));
    }
    let _ = writeln!(s, "   - {}", o.get(at).unwrap_or(&"(end of file)"));
    let _ = writeln!(s, "   + {}", n.get(at).unwrap_or(&"(end of file)"));
    for i in (at + 1)..(at + 4).min(o.len().max(n.len())) {
        let _ = writeln!(s, "     {}", n.get(i).unwrap_or(&""));
    }
    s
}

/// The compute must not depend on the parallel scan's scheduling: two runs of
/// the same fixture produce the same bytes. (A `last-wins` collision resolved
/// by whichever rayon thread finished first would be invisible in a single
/// run and produce a corpus that fails intermittently in CI.)
#[test]
fn compile_is_deterministic() {
    let dir = fixtures_dir();
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read fixtures")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("in").is_dir())
        .collect();
    cases.sort();
    for case in &cases {
        let case_in = case.join("in");
        let a = compile_fixture(&case_in);
        let b = compile_fixture(&case_in);
        assert!(
            a == b,
            "{}: two compiles of the same tree differ — the scan or an emitter \
             depends on thread scheduling",
            case.file_name().unwrap().to_string_lossy()
        );
    }
}
