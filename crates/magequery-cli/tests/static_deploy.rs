//! The whole-repo gate for `magecommand static deploy` — the finale that
//! wires every static building block into one command reproducing
//! `setup:static-content:deploy` over a theme × locale × area MATRIX.
//!
//! Four gates, all against the captured goldens (SKIPPED, not failed, when the
//! environment is absent — CI has no Magento install):
//!
//! 1. **Full-deploy byte gate** — `static deploy en_US --theme Magento/blank
//!    --theme Magento/luma --theme Magento/backend` reproduces the UNION of the
//!    frontend (`fulltree`) and adminhtml (`fulltree-adminhtml`) en_US goldens,
//!    set + bytes, only the documented float-artifact deltas + one shared
//!    `deployed_version.txt`.
//! 2. **Second-locale gate** — `static deploy fr_FR --theme Magento/blank
//!    --theme Magento/luma --area frontend` byte-matches the `fulltree-fr_FR`
//!    goldens (the sri-hashes path substitution + the real 30-phrase
//!    `js-translation.json`, everything else identical).
//! 3. **Matrix gate** — `--theme A:en_US --theme B:fr_FR` deploys A only in
//!    en_US and B only in fr_FR (locale-scoped trees, distinct dictionaries).
//! 4. **Fan-out determinism** — the parallel deploy is byte-identical to a
//!    forced-serial (`--jobs 1`) one.
//!
//! Env: `MAGECOMMAND_CORPUS` (default `/home/jelle/mg-install-310`),
//! `MAGECOMMAND_FULLTREE_GOLDENS` / `MAGECOMMAND_ADMINHTML_GOLDENS` /
//! `MAGECOMMAND_FRFR_GOLDENS`, and `MAGECOMMAND_DEPLOY_SCRATCH` for the
//! `--order probe` scratch (must be on the goldens' ext4 filesystem — $HOME,
//! not tmpfs).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn magecommand() -> Command {
    Command::new(env!("CARGO_BIN_EXE_magecommand"))
}

fn reference_root() -> PathBuf {
    PathBuf::from(
        std::env::var("MAGECOMMAND_CORPUS").unwrap_or_else(|_| "/home/jelle/mg-install-310".to_owned()),
    )
}

fn fulltree_goldens() -> PathBuf {
    PathBuf::from(
        std::env::var("MAGECOMMAND_FULLTREE_GOLDENS")
            .unwrap_or_else(|_| "/home/jelle/mg-scd-goldens/fulltree".to_owned()),
    )
}

fn adminhtml_goldens() -> PathBuf {
    PathBuf::from(
        std::env::var("MAGECOMMAND_ADMINHTML_GOLDENS")
            .unwrap_or_else(|_| "/home/jelle/mg-scd-goldens/fulltree-adminhtml".to_owned()),
    )
}

fn frfr_goldens() -> PathBuf {
    PathBuf::from(
        std::env::var("MAGECOMMAND_FRFR_GOLDENS")
            .unwrap_or_else(|_| "/home/jelle/mg-scd-goldens/fulltree-fr_FR".to_owned()),
    )
}

/// A writable scratch dir on the goldens' filesystem, or None (→ skip).
/// UNIQUE per call (an atomic counter appended to the pid), because these four
/// gates run concurrently in one test binary and must not share an output or
/// probe directory.
fn scratch_dir() -> Option<PathBuf> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let base = match std::env::var("MAGECOMMAND_DEPLOY_SCRATCH") {
        Ok(v) => PathBuf::from(v),
        Err(_) => PathBuf::from(std::env::var("HOME").ok()?)
            .join(".cache")
            .join("magecommand-deploy-gate"),
    };
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = base.join(format!("run-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// The en_US frontend golden run's deploy timestamp.
const GOLDEN_DEPLOYED_VERSION: &str = "1784559170";

/// The float-artifact files (both areas): golden-side PHP float-print noise
/// (`71.42857143000001%` on the frontend email css; `8.8...001rem`-style runs
/// on the backend styles), collapsed before comparing. Anything ELSE in these
/// files still fails.
const FLOAT_ARTIFACT_FILES: [&str; 5] = [
    "css/styles-m.css",
    "css/email.css",
    "css/email-inline.css",
    "css/styles.css",
    "css/styles-old.css",
];

/// Normalize BOTH known PHP float-print artifacts: the exact frontend token,
/// then any fractional run of 7+ zeros truncated at that run (the backend
/// form). Applied to both sides — a clean value is unchanged.
fn normalize_floats(s: &str) -> String {
    let s = s.replace("71.42857143000001%", "71.42857143%");
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            let leading_digit = i > 0 && b[i - 1].is_ascii_digit();
            let frac_start = i + 1;
            let mut j = frac_start;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
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
    let src =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read manifest {}: {e}", path.display()));
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
            let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
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
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Hold one deployed tree against its golden manifest + golden dir: set
/// equality + per-file byte equality, float-artifact files allowed only after
/// the normalization.
fn assert_tree(ours_dir: &Path, manifest_path: &Path, golden_dir: &Path, label: &str) {
    let manifest = read_manifest(manifest_path);
    let ours = walk_files(ours_dir);

    let missing: Vec<&String> = manifest.keys().filter(|p| ours.binary_search(p).is_err()).collect();
    let extra: Vec<&String> = ours.iter().filter(|p| !manifest.contains_key(*p)).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "{label}: file set differs\n  missing ({}): {:?}\n  extra ({}): {:?}",
        missing.len(),
        &missing[..missing.len().min(20)],
        extra.len(),
        &extra[..extra.len().min(20)]
    );

    let mut deltas: Vec<String> = Vec::new();
    for (path, golden_sha) in &manifest {
        let ours_bytes = std::fs::read(ours_dir.join(path)).unwrap_or_else(|e| panic!("{label}: read {path}: {e}"));
        if &sha256_hex(&ours_bytes) == golden_sha {
            continue;
        }
        if FLOAT_ARTIFACT_FILES.contains(&path.as_str()) {
            let golden = std::fs::read_to_string(golden_dir.join(path)).expect("read golden css");
            let ours_str = String::from_utf8_lossy(&ours_bytes);
            if normalize_floats(&ours_str) == normalize_floats(&golden) {
                continue;
            }
        }
        deltas.push(path.clone());
    }
    assert!(
        deltas.is_empty(),
        "{label}: {} unexpected byte delta(s): {:?}",
        deltas.len(),
        &deltas[..deltas.len().min(20)]
    );
}

/// Gate 1: the full en_US deploy (blank + luma + backend) reproduces the
/// union of the frontend and adminhtml goldens in one command.
#[test]
fn full_deploy_reproduces_frontend_and_adminhtml_goldens() {
    let root = reference_root();
    let fe = fulltree_goldens();
    let adm = adminhtml_goldens();
    if !root.is_dir() || !fe.join("blank").is_dir() || !adm.join("backend").is_dir() {
        println!("SKIP: reference install or goldens not present");
        return;
    }
    let Some(out) = scratch_dir() else {
        println!("SKIP: no writable scratch dir");
        return;
    };
    let deploy = out.join("deploy");

    let run = magecommand()
        .args([
            "static", "deploy", "en_US",
            "--root", &root.display().to_string(),
            "--theme", "Magento/blank",
            "--theme", "Magento/luma",
            "--theme", "Magento/backend",
            "--out", &deploy.display().to_string(),
            "--order", "probe",
            "--probe-dir", &out.display().to_string(),
            "--deployed-version", GOLDEN_DEPLOYED_VERSION,
        ])
        .output()
        .expect("run magecommand");
    assert!(run.status.success(), "exit {:?}; stderr: {}", run.status.code(), String::from_utf8_lossy(&run.stderr));

    // ONE shared deployed_version.txt at the static root.
    let version = std::fs::read(deploy.join("deployed_version.txt")).expect("deployed_version.txt");
    assert_eq!(version, GOLDEN_DEPLOYED_VERSION.as_bytes());

    assert_tree(
        &deploy.join("frontend/Magento/blank/en_US"),
        &fe.join("MANIFEST-blank.tsv"),
        &fe.join("blank"),
        "blank en_US",
    );
    assert_tree(
        &deploy.join("frontend/Magento/luma/en_US"),
        &fe.join("MANIFEST-luma.tsv"),
        &fe.join("luma"),
        "luma en_US",
    );
    assert_tree(
        &deploy.join("adminhtml/Magento/backend/en_US"),
        &adm.join("MANIFEST-backend.tsv"),
        &adm.join("backend"),
        "backend en_US",
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// Gate 2: the second-locale deploy byte-matches the fr_FR goldens — the real
/// 30-phrase `js-translation.json` and the sri-hashes path substitution.
#[test]
fn second_locale_deploy_matches_frfr_goldens() {
    let root = reference_root();
    let fr = frfr_goldens();
    if !root.is_dir() || !fr.join("blank").is_dir() {
        println!("SKIP: reference install or fr_FR goldens not present");
        return;
    }
    let Some(out) = scratch_dir() else {
        println!("SKIP: no writable scratch dir");
        return;
    };
    let deploy = out.join("deploy");

    let run = magecommand()
        .args([
            "static", "deploy", "fr_FR",
            "--root", &root.display().to_string(),
            "--theme", "Magento/blank",
            "--theme", "Magento/luma",
            "--area", "frontend",
            "--out", &deploy.display().to_string(),
            "--order", "probe",
            "--probe-dir", &out.display().to_string(),
        ])
        .output()
        .expect("run magecommand");
    assert!(run.status.success(), "exit {:?}; stderr: {}", run.status.code(), String::from_utf8_lossy(&run.stderr));

    // The genuine non-empty dictionary (theme-independent).
    for theme in ["blank", "luma"] {
        let ours = std::fs::read(deploy.join(format!("frontend/Magento/{theme}/fr_FR/js-translation.json")))
            .expect("js-translation.json");
        let golden = std::fs::read(fr.join(theme).join("js-translation.json")).expect("golden js-translation");
        assert_eq!(ours, golden, "{theme}: js-translation.json must be byte-identical to the fr_FR golden");
        assert_ne!(ours, b"[]", "{theme}: the fr_FR dictionary must NOT be empty");
    }

    assert_tree(
        &deploy.join("frontend/Magento/blank/fr_FR"),
        &fr.join("MANIFEST-blank.tsv"),
        &fr.join("blank"),
        "blank fr_FR",
    );
    assert_tree(
        &deploy.join("frontend/Magento/luma/fr_FR"),
        &fr.join("MANIFEST-luma.tsv"),
        &fr.join("luma"),
        "luma fr_FR",
    );

    let _ = std::fs::remove_dir_all(&out);
}

/// Gate 3: the per-theme locale matrix places each theme only in its own
/// locale, with the right (empty vs non-empty) dictionary. `--no-parent`
/// isolates the pure locale-scoping (parent auto-deploy is gate 5).
#[test]
fn matrix_places_each_theme_in_its_own_locale() {
    let root = reference_root();
    if !root.is_dir() {
        println!("SKIP: reference install not present");
        return;
    }
    let Some(out) = scratch_dir() else {
        println!("SKIP: no writable scratch dir");
        return;
    };
    let deploy = out.join("deploy");

    let run = magecommand()
        .args([
            "static", "deploy", "en_US",
            "--root", &root.display().to_string(),
            "--theme", "Magento/blank:en_US",
            "--theme", "Magento/luma:fr_FR",
            "--no-parent",
            "--out", &deploy.display().to_string(),
            "--order", "sorted",
        ])
        .output()
        .expect("run magecommand");
    assert!(run.status.success(), "stderr: {}", String::from_utf8_lossy(&run.stderr));

    // blank only in en_US, luma only in fr_FR.
    assert!(deploy.join("frontend/Magento/blank/en_US/js-translation.json").exists());
    assert!(!deploy.join("frontend/Magento/blank/fr_FR").exists(), "blank must not deploy in fr_FR");
    assert!(deploy.join("frontend/Magento/luma/fr_FR/js-translation.json").exists());
    assert!(!deploy.join("frontend/Magento/luma/en_US").exists(), "luma must not deploy in en_US");

    // The locale's dictionary decides the js-translation content.
    let blank_en = std::fs::read(deploy.join("frontend/Magento/blank/en_US/js-translation.json")).unwrap();
    assert_eq!(blank_en, b"[]", "en_US dictionary is empty");
    let luma_fr = std::fs::read(deploy.join("frontend/Magento/luma/fr_FR/js-translation.json")).unwrap();
    assert_ne!(luma_fr, b"[]", "fr_FR dictionary is non-empty");

    let _ = std::fs::remove_dir_all(&out);
}

/// Gate 5: a child `--theme` auto-deploys its parent (Magento's quick-strategy
/// `parentCompilationRequested`), in the child's locales; `--no-parent` opts
/// out. `Magento/luma`'s parent is `Magento/blank`. The pulled-in parent must
/// be byte-identical to deploying blank directly (same group, blank-first).
#[test]
fn child_theme_auto_deploys_its_parent() {
    let root = reference_root();
    if !root.is_dir() {
        println!("SKIP: reference install not present");
        return;
    }
    let Some(out) = scratch_dir() else {
        println!("SKIP: no writable scratch dir");
        return;
    };
    let root_s = root.display().to_string();

    // luma alone → blank (parent) AND luma both deploy.
    let child = out.join("child");
    let run = magecommand()
        .args([
            "static", "deploy", "en_US",
            "--root", &root_s,
            "--theme", "Magento/luma",
            "--out", &child.display().to_string(),
            "--order", "sorted",
        ])
        .output()
        .expect("run magecommand");
    assert!(run.status.success(), "stderr: {}", String::from_utf8_lossy(&run.stderr));
    assert!(child.join("frontend/Magento/blank/en_US/js-translation.json").exists(),
        "parent blank must be auto-deployed alongside child luma");
    assert!(child.join("frontend/Magento/luma/en_US/js-translation.json").exists());

    // --no-parent suppresses it.
    let np = out.join("noparent");
    let run = magecommand()
        .args([
            "static", "deploy", "en_US",
            "--root", &root_s,
            "--theme", "Magento/luma", "--no-parent",
            "--out", &np.display().to_string(),
            "--order", "sorted",
        ])
        .output()
        .expect("run magecommand");
    assert!(run.status.success(), "stderr: {}", String::from_utf8_lossy(&run.stderr));
    assert!(!np.join("frontend/Magento/blank").exists(), "--no-parent must not deploy blank");
    assert!(np.join("frontend/Magento/luma/en_US").exists());

    // The auto-deployed parent equals a direct blank deploy, byte-for-byte
    // (same (area, locale) group order: blank-first, then luma).
    let direct = out.join("direct");
    let run = magecommand()
        .args([
            "static", "deploy", "en_US",
            "--root", &root_s,
            "--theme", "Magento/blank", "--theme", "Magento/luma",
            "--out", &direct.display().to_string(),
            "--order", "sorted",
        ])
        .output()
        .expect("run magecommand");
    assert!(run.status.success(), "stderr: {}", String::from_utf8_lossy(&run.stderr));
    let manifest = |base: &Path| -> BTreeMap<String, String> {
        walk_files(base)
            .into_iter()
            .map(|rel| {
                let bytes = std::fs::read(base.join(&rel)).unwrap();
                (rel, sha256_hex(&bytes))
            })
            .collect()
    };
    let a = manifest(&child.join("frontend/Magento/blank"));
    let b = manifest(&direct.join("frontend/Magento/blank"));
    assert_eq!(a, b, "auto-deployed parent must match a direct blank deploy");

    let _ = std::fs::remove_dir_all(&out);
}

/// Gate 4: the parallel fan-out is byte-identical to a forced-serial run.
#[test]
fn fan_out_is_deterministic_vs_serial() {
    let root = reference_root();
    if !root.is_dir() {
        println!("SKIP: reference install not present");
        return;
    }
    let Some(out) = scratch_dir() else {
        println!("SKIP: no writable scratch dir");
        return;
    };
    let par = out.join("par");
    let ser = out.join("ser");

    let common = |dst: &Path, jobs: &str| {
        let mut c = magecommand();
        c.args([
            "static", "deploy", "en_US",
            "--root", &root.display().to_string(),
            "--theme", "Magento/blank",
            "--theme", "Magento/luma",
            "--theme", "Magento/backend",
            "--out", &dst.display().to_string(),
            // `sorted` keeps the comparison independent of any filesystem
            // readdir nondeterminism — the point is the fan-out scheduling.
            "--order", "sorted",
            "--deployed-version", "v-det",
            "--jobs", jobs,
        ]);
        c.output().expect("run magecommand")
    };

    let a = common(&par, "0");
    assert!(a.status.success(), "parallel: {}", String::from_utf8_lossy(&a.stderr));
    let b = common(&ser, "1");
    assert!(b.status.success(), "serial: {}", String::from_utf8_lossy(&b.stderr));

    // Compare the two full trees file-by-file.
    let files_par = walk_files(&par);
    let files_ser = walk_files(&ser);
    assert_eq!(files_par, files_ser, "parallel/serial file sets differ");
    for f in &files_par {
        let pa = std::fs::read(par.join(f)).unwrap();
        let se = std::fs::read(ser.join(f)).unwrap();
        assert_eq!(pa, se, "parallel vs serial byte mismatch at {f}");
    }

    let _ = std::fs::remove_dir_all(&out);
}
