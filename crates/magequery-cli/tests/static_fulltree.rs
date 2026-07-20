//! The whole-tree gate for `magecommand static files`: deploy the FULL
//! static-file package for blank AND luma from the SOURCE tree of a real
//! Magento checkout and compare against the `fulltree` goldens captured from
//! a fresh stock `setup:static-content:deploy` (quick strategy, default
//! mode, minification off) — file-SET equality plus per-file byte equality.
//!
//! **Environment-dependent, and SKIPPED (not failed) when the environment is
//! absent** — CI has no Magento install. The reference root defaults to
//! `/home/jelle/mg-install-310` (`MAGECOMMAND_CORPUS`); the goldens to
//! `/home/jelle/mg-scd-goldens/fulltree` (`MAGECOMMAND_FULLTREE_GOLDENS`) —
//! per-theme trees plus `MANIFEST-{blank,luma}.tsv`
//! (`path<TAB>size<TAB>sha256`, sorted).
//!
//! ## The expected-delta list (everything else = failure)
//!
//! - `deployed_version.txt` — run-scoped (unix timestamp of the golden run);
//!   parameterized via `--deployed-version`, so the test PASSES the golden
//!   run's value and compares it exactly like any other file.
//! - The **6 float-artifact css lines**: the golden-side less.php compiler
//!   leaks a PHP float-print artifact (`71.42857143000001%`) once each in
//!   `css/{styles-m,email,email-inline}.css` × both themes; ours prints the
//!   exact decimal (`71.42857143%`). Byte-exact PHP float formatting is a
//!   non-goal (magecommand-less NOTES §8); these three files per theme are
//!   compared after normalizing that one token on the golden side — and any
//!   OTHER difference in them still fails.
//!
//! Ordering notes: package deployment order (which only `sri-hashes.json`
//! keys depend on) follows the SOURCE tree's readdir order — reproduced
//! directly, no probe. Bundle-internal order is the DEPLOYED tree's readdir
//! order, so `--order probe` needs scratch space on the goldens' filesystem
//! (`$HOME`, not tmpfs) — `MAGECOMMAND_BUNDLE_SCRATCH` overrides the default
//! `$HOME/.cache/magecommand-fulltree-gate`.

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
        std::env::var("MAGECOMMAND_FULLTREE_GOLDENS")
            .unwrap_or_else(|_| "/home/jelle/mg-scd-goldens/fulltree".to_owned()),
    )
}

/// A writable scratch dir on the goldens' filesystem, or None (→ skip).
fn scratch_dir() -> Option<PathBuf> {
    let base = match std::env::var("MAGECOMMAND_BUNDLE_SCRATCH") {
        Ok(v) => PathBuf::from(v),
        Err(_) => PathBuf::from(std::env::var("HOME").ok()?)
            .join(".cache")
            .join("magecommand-fulltree-gate"),
    };
    let dir = base.join(format!("run-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// The golden run's `--content-version` equivalent (its deploy timestamp).
const GOLDEN_DEPLOYED_VERSION: &str = "1784559170";

/// The 6 float-artifact files (per theme): golden-side `71.42857143000001%`
/// normalized to the exact decimal before comparing.
const FLOAT_ARTIFACT_FILES: [&str; 3] =
    ["css/styles-m.css", "css/email.css", "css/email-inline.css"];
const FLOAT_ARTIFACT: &str = "71.42857143000001%";
const FLOAT_EXACT: &str = "71.42857143%";

/// Read a `MANIFEST-<theme>.tsv` into path → sha256.
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

/// Every file under `dir`, as sorted relative paths.
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

/// Deploy blank+luma in one run and hold the whole tree against the goldens:
/// set equality (extras and missing reported by name), per-file byte
/// equality, the documented expected-delta list ONLY.
#[test]
fn full_tree_matches_the_deployed_goldens() {
    let root = reference_root();
    let goldens = goldens_dir();
    if !root.is_dir() || !goldens.join("blank").is_dir() {
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
            "--theme",
            "Magento/blank",
            "--theme",
            "Magento/luma",
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

    // deployed_version.txt — parameterized; exact bytes, no newline.
    let version = std::fs::read(out.join("deployed_version.txt")).expect("deployed_version.txt");
    assert_eq!(version, GOLDEN_DEPLOYED_VERSION.as_bytes());

    for (theme, expected_count) in [("blank", 1590usize), ("luma", 1602usize)] {
        let manifest = read_manifest(&goldens.join(format!("MANIFEST-{theme}.tsv")));
        assert_eq!(manifest.len(), expected_count, "{theme}: golden manifest size");

        let ours_dir = out
            .join("frontend")
            .join("Magento")
            .join(theme)
            .join("en_US");
        let ours: Vec<String> = walk_files(&ours_dir);

        // File-SET equality, both directions, by name.
        let missing: Vec<&String> = manifest
            .keys()
            .filter(|p| ours.binary_search(p).is_err())
            .collect();
        let extra: Vec<&String> = ours
            .iter()
            .filter(|p| !manifest.contains_key(*p))
            .collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "{theme}: file set differs\n  missing ({}): {:?}\n  extra ({}): {:?}",
            missing.len(),
            &missing[..missing.len().min(20)],
            extra.len(),
            &extra[..extra.len().min(20)]
        );

        // Per-file byte equality with the documented expected deltas.
        let mut deltas: Vec<String> = Vec::new();
        for (path, golden_sha) in &manifest {
            let ours_bytes = std::fs::read(ours_dir.join(path))
                .unwrap_or_else(|e| panic!("{theme}: read {path}: {e}"));
            if &sha256_hex(&ours_bytes) == golden_sha {
                continue;
            }
            if FLOAT_ARTIFACT_FILES.contains(&path.as_str()) {
                // Allowed ONLY as the float-artifact token; anything else in
                // these files still fails.
                let golden = std::fs::read_to_string(goldens.join(theme).join(path))
                    .expect("read golden css");
                assert!(
                    golden.contains(FLOAT_ARTIFACT),
                    "{theme}/{path}: differs but golden has no float artifact to normalize"
                );
                let normalized = golden.replace(FLOAT_ARTIFACT, FLOAT_EXACT);
                if ours_bytes != normalized.as_bytes() {
                    let at = ours_bytes
                        .iter()
                        .zip(normalized.as_bytes())
                        .position(|(a, b)| a != b)
                        .unwrap_or_else(|| ours_bytes.len().min(normalized.len()));
                    let lo = at.saturating_sub(60);
                    panic!(
                        "{theme}/{path}: differs beyond the known float artifact; \
                         first difference at byte {at}\n  ours:   {:?}\n  golden: {:?}",
                        String::from_utf8_lossy(
                            &ours_bytes[lo..(at + 60).min(ours_bytes.len())]
                        ),
                        &normalized[lo..(at + 60).min(normalized.len())],
                    );
                }
                continue;
            }
            deltas.push(path.clone());
        }
        assert!(
            deltas.is_empty(),
            "{theme}: {} unexpected byte delta(s): {:?}",
            deltas.len(),
            &deltas[..deltas.len().min(20)]
        );
    }

    let _ = std::fs::remove_dir_all(&out);
}

/// Without `--deployed-version` no version file is invented.
#[test]
fn no_deployed_version_flag_no_file() {
    let root = reference_root();
    if !root.is_dir() {
        println!("SKIP: reference install not present");
        return;
    }
    let Some(out) = scratch_dir() else {
        println!("SKIP: no writable scratch dir");
        return;
    };
    let out = out.join("noversion");
    std::fs::create_dir_all(&out).expect("mkdir");

    let run = magecommand()
        .args([
            "static",
            "files",
            "--root",
            &root.display().to_string(),
            "--theme",
            "Magento/blank",
            "--out",
            &out.display().to_string(),
        ])
        .output()
        .expect("run magecommand");
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert!(
        !out.join("deployed_version.txt").exists(),
        "deployed_version.txt must not be invented without --deployed-version"
    );

    let _ = std::fs::remove_dir_all(&out);
}
