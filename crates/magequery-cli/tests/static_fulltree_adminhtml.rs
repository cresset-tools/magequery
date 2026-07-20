//! The whole-tree gate for `magecommand static files --area adminhtml`: deploy
//! the FULL static-file package for `Magento/backend` from the SOURCE tree of a
//! real Magento checkout and compare against the `fulltree-adminhtml` goldens
//! captured from a fresh stock `setup:static-content:deploy --area adminhtml`
//! (quick strategy, default mode, minification off) — file-SET equality plus
//! per-file byte equality.
//!
//! **Environment-dependent, and SKIPPED (not failed) when the environment is
//! absent** — CI has no Magento install. The reference root defaults to
//! `/home/jelle/mg-install-310` (`MAGECOMMAND_CORPUS`); the goldens to
//! `/home/jelle/mg-scd-goldens/fulltree-adminhtml`
//! (`MAGECOMMAND_ADMINHTML_GOLDENS`) — the `backend/` tree plus
//! `MANIFEST-backend.tsv` (`path<TAB>size<TAB>sha256`, sorted).
//!
//! `Magento/backend` has NO parent theme, so the pipeline degenerates to one
//! package — no copy-from-parent path; every file write/publish. Adminhtml gets
//! every post-package generated file the frontend gets (requirejs-config.js +
//! requirejs-min-resolver.js, js-translation.json, sri-hashes.json, the js
//! bundles), plus the backend-only entry points (`styles`, `styles-old`, the
//! `_module-old.less` `@magento_import` token) — there is exactly ONE
//! area-conditional in the deploy code (auto-detected locales), irrelevant with
//! `-f en_US`.
//!
//! ## The expected-delta list (everything else = failure)
//!
//! - `deployed_version.txt` — run-scoped; parameterized via
//!   `--deployed-version`, so the test PASSES the golden run's value and
//!   compares it exactly like any other file.
//! - **`css/styles.css` and `css/styles-old.css` — PHP float-print artifacts**:
//!   the golden-side less.php leaks float-print noise (`8.800000000000001rem`,
//!   `0.8100000000000001s`, `77.90000000000001%`, …) where ours prints the
//!   exact decimal (`8.8rem`). Byte-exact PHP float formatting is a non-goal
//!   (magecommand-less NOTES §8) — the same class the frontend gate allows on
//!   its 6 float-artifact files. These two files are compared after collapsing
//!   that artifact on BOTH sides; any OTHER difference still fails.
//!
//! Ordering: the bundle min/plain-sibling drop (`slick.js`/`slick.min.js`,
//! `vimeo/player.js`/`player.min.js`) is order-sensitive — whichever of a
//! min/plain pair the deployed-tree readdir yields first survives (Magento's
//! `Service\Bundle` `GLOB_NOSORT`). `--order probe` reproduces that readdir
//! order by touching names in a scratch dir on the OUTPUT filesystem, so `--out`
//! must sit on the same filesystem the goldens were captured on ($HOME, not
//! tmpfs) — `MAGECOMMAND_ADMINHTML_SCRATCH` overrides the default
//! `$HOME/.cache/magecommand-adminhtml-gate`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn magecommand() -> Command {
    Command::new(env!("CARGO_BIN_EXE_magecommand"))
}

fn reference_root() -> PathBuf {
    PathBuf::from(
        std::env::var("MAGECOMMAND_CORPUS")
            .unwrap_or_else(|_| "/home/jelle/mg-install-310".to_owned()),
    )
}

fn goldens_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("MAGECOMMAND_ADMINHTML_GOLDENS")
            .unwrap_or_else(|_| "/home/jelle/mg-scd-goldens/fulltree-adminhtml".to_owned()),
    )
}

/// A writable scratch dir on the goldens' filesystem, or None (→ skip). The
/// probe defaults to the `--out` base, so `--out` under $HOME makes readdir
/// order match the golden capture (same ext4 hash seed).
fn scratch_dir() -> Option<PathBuf> {
    let base = match std::env::var("MAGECOMMAND_ADMINHTML_SCRATCH") {
        Ok(v) => PathBuf::from(v),
        Err(_) => PathBuf::from(std::env::var("HOME").ok()?)
            .join(".cache")
            .join("magecommand-adminhtml-gate"),
    };
    let dir = base.join(format!("run-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// The golden run's deploy timestamp (its `deployed_version.txt`).
const GOLDEN_DEPLOYED_VERSION: &str = "1784563087";

/// The two float-artifact files (compared after collapsing the artifact).
const FLOAT_ARTIFACT_FILES: [&str; 2] = ["css/styles.css", "css/styles-old.css"];

/// Collapse PHP float-print artifacts: a fractional part carrying a run of 7+
/// consecutive `0`s (then more digits) is truncated at that run — `8.8` +
/// `00000000000001` → `8.8`; a fraction that is ALL such noise drops with its
/// `.` (`5` + `.0000000001` → `5`). Applied to BOTH sides, so a clean value is
/// unchanged and only the golden's artifact collapses. Threshold 7 is well
/// clear of any legitimate CSS decimal.
fn collapse_float_artifacts(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // A `.` followed by a digit begins a fractional part (a bare `.81s` —
        // the compressed leading-zero-stripped form — as well as `8.8`).
        if b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            let leading_digit = i > 0 && b[i - 1].is_ascii_digit();
            let frac_start = i + 1;
            let mut j = frac_start;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            // Find a run of >= 7 zeros within [frac_start, j).
            let mut cut = None;
            let mut k = frac_start;
            while k < j {
                if b[k] == b'0' {
                    let run_start = k;
                    while k < j && b[k] == b'0' {
                        k += 1;
                    }
                    if k - run_start >= 7 {
                        cut = Some(run_start);
                        break;
                    }
                } else {
                    k += 1;
                }
            }
            match cut {
                Some(c) if c == frac_start => {
                    // Whole fraction is noise → drop it; keep a leading `.0`
                    // when there is no integer part to fall back on.
                    if !leading_digit {
                        out.push(b'.');
                        out.push(b'0');
                    }
                    i = j;
                    continue;
                }
                Some(c) => {
                    out.push(b'.');
                    out.extend_from_slice(&b[frac_start..c]);
                    i = j;
                    continue;
                }
                None => {
                    out.push(b'.');
                    out.extend_from_slice(&b[frac_start..j]);
                    i = j;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn read_manifest(path: &Path) -> BTreeMap<String, String> {
    let src = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read manifest {}: {e}", path.display()));
    src.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut it = l.split('\t');
            let p = it.next().expect("manifest path").to_string();
            let _size = it.next();
            let sha = it.next().expect("manifest sha").to_string();
            (p, sha)
        })
        .collect()
}

fn walk_files(dir: &Path) -> Vec<String> {
    fn rec(dir: &Path, rel: &str, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let r = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if e.path().is_dir() {
                rec(&e.path(), &r, out);
            } else {
                out.push(r);
            }
        }
    }
    let mut out = Vec::new();
    rec(dir, "", &mut out);
    out.sort();
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let d = sha2::Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Deploy `Magento/backend` (adminhtml) and hold the whole tree against the
/// goldens: set equality (extras and missing reported by name), per-file byte
/// equality, the documented float-artifact delta ONLY.
#[test]
fn adminhtml_full_tree_matches_the_deployed_goldens() {
    let root = reference_root();
    let goldens = goldens_dir();
    if !root.is_dir() || !goldens.join("backend").is_dir() {
        println!(
            "SKIP: reference install ({}) or goldens ({}) not present",
            root.display(),
            goldens.display()
        );
        return;
    }
    let Some(out) = scratch_dir() else {
        println!("SKIP: no writable scratch dir");
        return;
    };

    let run = magecommand()
        .args([
            "static",
            "files",
            "--root",
            &root.display().to_string(),
            "--area",
            "adminhtml",
            "--theme",
            "Magento/backend",
            "--out",
            &out.display().to_string(),
            "--order",
            "probe",
            "--deployed-version",
            GOLDEN_DEPLOYED_VERSION,
        ])
        .output()
        .expect("run magecommand");
    assert!(
        run.status.success(),
        "exit {:?}; stderr: {}",
        run.status.code(),
        String::from_utf8_lossy(&run.stderr)
    );

    let version = std::fs::read(out.join("deployed_version.txt")).expect("deployed_version.txt");
    assert_eq!(version, GOLDEN_DEPLOYED_VERSION.as_bytes());

    let manifest = read_manifest(&goldens.join("MANIFEST-backend.tsv"));
    assert_eq!(manifest.len(), 2154, "golden manifest size");

    let ours_dir = out
        .join("adminhtml")
        .join("Magento")
        .join("backend")
        .join("en_US");
    let ours: Vec<String> = walk_files(&ours_dir);

    // File-SET equality, both directions, by name.
    let missing: Vec<&String> = manifest
        .keys()
        .filter(|p| ours.binary_search(p).is_err())
        .collect();
    let extra: Vec<&String> = ours.iter().filter(|p| !manifest.contains_key(*p)).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "file set differs\n  missing ({}): {:?}\n  extra ({}): {:?}",
        missing.len(),
        &missing[..missing.len().min(20)],
        extra.len(),
        &extra[..extra.len().min(20)]
    );

    // Per-file byte equality with the documented float-artifact delta.
    let mut deltas: Vec<String> = Vec::new();
    for (path, golden_sha) in &manifest {
        let ours_bytes = std::fs::read(ours_dir.join(path))
            .unwrap_or_else(|e| panic!("read {path}: {e}"));
        if &sha256_hex(&ours_bytes) == golden_sha {
            continue;
        }
        if FLOAT_ARTIFACT_FILES.contains(&path.as_str()) {
            let golden = std::fs::read_to_string(goldens.join("backend").join(path))
                .expect("read golden css");
            let ours_str = String::from_utf8_lossy(&ours_bytes);
            if collapse_float_artifacts(&ours_str) == collapse_float_artifacts(&golden) {
                continue;
            }
            // Show the first residual (non-float) difference.
            let on = collapse_float_artifacts(&ours_str);
            let gn = collapse_float_artifacts(&golden);
            let at = on
                .bytes()
                .zip(gn.bytes())
                .position(|(a, b)| a != b)
                .unwrap_or(on.len().min(gn.len()));
            let lo = at.saturating_sub(60);
            panic!(
                "{path}: differs beyond the known float artifact; first difference at byte {at}\n  \
                 ours:   {:?}\n  golden: {:?}",
                &on[lo..(at + 60).min(on.len())],
                &gn[lo..(at + 60).min(gn.len())],
            );
        }
        deltas.push(path.clone());
    }
    assert!(
        deltas.is_empty(),
        "{} unexpected byte delta(s): {:?}",
        deltas.len(),
        &deltas[..deltas.len().min(20)]
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// The LESS compiler gate: compile the backend theme's own entry points
/// NON-compressed (`static less --area adminhtml --theme Magento/backend
/// --entry <name> --stdout`) and byte-compare against the noncompressed goldens
/// (`backend-css-noncompressed/`, the SCD run captured with the less.php adapter
/// patched to `compress=false`). Two allowances, both classified honestly:
/// - the PHP float-print artifact (same collapse as the whole-tree gate);
/// - the ONE module-notation url the CSS ModuleNotation post-processor rewrites
///   in the full pipeline (`Magento_Analytics::images/…` →
///   `../Magento_Analytics/…`) but `static less --stdout` intentionally leaves
///   raw (that post-processor belongs to `static files`, and the whole-tree gate
///   verifies the rewrite there). Nothing else may differ.
///
/// The lib/module entries in the noncompressed golden set
/// (`mage/gallery/gallery`, `Magento_PageBuilder/css/source/hljs/hljs`) are not
/// theme entry points — they compile inside the full deploy, where the
/// whole-tree gate proves them byte-identical (compressed) — so they are not
/// re-compiled here.
#[test]
fn adminhtml_less_compiler_matches_noncompressed_goldens() {
    let root = reference_root();
    let goldens = goldens_dir();
    let nc = goldens.join("backend-css-noncompressed");
    if !root.is_dir() || !nc.is_dir() {
        println!(
            "SKIP: reference install ({}) or noncompressed goldens ({}) not present",
            root.display(),
            nc.display()
        );
        return;
    }

    for (entry, rel) in [("styles", "css/styles.css"), ("styles-old", "css/styles-old.css")] {
        let run = magecommand()
            .args([
                "static",
                "less",
                "--root",
                &root.display().to_string(),
                "--area",
                "adminhtml",
                "--theme",
                "Magento/backend",
                "--entry",
                entry,
                "--stdout",
            ])
            .output()
            .expect("run magecommand");
        assert!(
            run.status.success(),
            "{entry}: exit {:?}; stderr: {}",
            run.status.code(),
            String::from_utf8_lossy(&run.stderr)
        );
        let ours = String::from_utf8_lossy(&run.stdout);
        let ours = ours.trim_end_matches('\n');
        // `static less --stdout` stops before the css ModuleNotation pass.
        let ours = ours.replace(
            "url(\"Magento_Analytics::images/analytics-icon.svg\")",
            "url(\"../Magento_Analytics/images/analytics-icon.svg\")",
        );
        let golden = std::fs::read_to_string(nc.join(rel)).expect("read noncompressed golden");
        let golden = golden.trim_end_matches('\n');
        let on = collapse_float_artifacts(&ours);
        let gn = collapse_float_artifacts(golden);
        if on == gn {
            continue;
        }
        let at = on
            .bytes()
            .zip(gn.bytes())
            .position(|(a, b)| a != b)
            .unwrap_or(on.len().min(gn.len()));
        let lo = at.saturating_sub(60);
        panic!(
            "{entry}: compiled css differs beyond float/module-notation; first difference at byte \
             {at}\n  ours:   {:?}\n  golden: {:?}",
            &on[lo..(at + 60).min(on.len())],
            &gn[lo..(at + 60).min(gn.len())],
        );
    }
}
