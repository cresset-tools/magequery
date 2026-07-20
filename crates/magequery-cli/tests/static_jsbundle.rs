//! The byte-exact gate for `magecommand static bundle`: generate the
//! `js/bundle/bundle<N>.js` files for blank and luma from the SOURCE tree of
//! a real Magento checkout and compare against goldens captured from a real
//! bundled `setup:static-content:deploy`.
//!
//! **Environment-dependent, and SKIPPED (not failed) when the environment is
//! absent** — CI has no Magento install. The reference root defaults to
//! `/home/jelle/mg-install-310`, overridable with `MAGECOMMAND_CORPUS`; the
//! goldens default to `/home/jelle/mg-scd-goldens/jsbundle`, overridable with
//! `MAGECOMMAND_SCD_GOLDENS`.
//!
//! Two environment subtleties this gate must respect:
//!
//! - **Ordering**: in-bundle key order (and therefore split points) is the
//!   deployed tree's readdir order — on the goldens' filesystem (ext4) a pure
//!   function of each directory's name set, which `--order probe` reproduces
//!   by probing scratch directories. The scratch/output dir therefore MUST be
//!   on the same filesystem the goldens were deployed on (`$HOME`, not
//!   tmpfs); `MAGECOMMAND_BUNDLE_SCRATCH` overrides the default
//!   `$HOME/.cache/magecommand-bundle-gate`.
//! - **The shared `.min`-sibling cache**: the goldens come from ONE deploy of
//!   blank+luma in that order, so luma's bundles are missing the plain
//!   variants whose `.min` siblings blank already iterated
//!   (`vimeo/player.js`, PageBuilder's `slick.js`). Both themes must be
//!   bundled in ONE invocation, in that order — exactly what the repeatable
//!   `--theme` flag models.
//!
//! The format/splitting/filter semantics are covered by the
//! `static_deploy::bundle` unit tests over a synthetic tree; this test is
//! purely the "does it match the real deploy, byte for byte" gate.

use std::path::PathBuf;
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
        std::env::var("MAGECOMMAND_SCD_GOLDENS")
            .unwrap_or_else(|_| "/home/jelle/mg-scd-goldens/jsbundle".to_owned()),
    )
}

/// A writable scratch dir on the goldens' filesystem, or None (→ skip).
fn scratch_dir() -> Option<PathBuf> {
    let base = match std::env::var("MAGECOMMAND_BUNDLE_SCRATCH") {
        Ok(v) => PathBuf::from(v),
        Err(_) => PathBuf::from(std::env::var("HOME").ok()?)
            .join(".cache")
            .join("magecommand-bundle-gate"),
    };
    let dir = base.join(format!("run-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Bundle blank+luma in one run (shared `.min` cache, deploy order) and
/// byte-compare all 14 files against the goldens.
#[test]
fn blank_and_luma_match_the_deployed_goldens_byte_for_byte() {
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
        println!("SKIP: no writable scratch dir for the order probe");
        return;
    };

    let run = magecommand()
        .args([
            "static",
            "bundle",
            "--root",
            &root.display().to_string(),
            "--theme",
            "Magento/blank",
            "--theme",
            "Magento/luma",
            "--out",
            &out.display().to_string(),
        ])
        .output()
        .expect("run magecommand");
    assert!(
        run.status.success(),
        "exit {:?}; stderr: {}",
        run.status.code(),
        String::from_utf8_lossy(&run.stderr)
    );

    let mut compared = 0usize;
    for (theme, golden_sub) in [("blank", "blank"), ("luma", "luma")] {
        let ours_dir = out.join("Magento").join(theme).join("js").join("bundle");
        for n in 0.. {
            let golden = goldens.join(golden_sub).join(format!("bundle{n}.js"));
            if !golden.is_file() {
                // no further goldens — and we must not have produced extras.
                assert!(
                    !ours_dir.join(format!("bundle{n}.js")).exists(),
                    "{theme}: extra bundle{n}.js not present in goldens"
                );
                break;
            }
            let ours_path = ours_dir.join(format!("bundle{n}.js"));
            let ours = std::fs::read(&ours_path)
                .unwrap_or_else(|e| panic!("{theme}: read {}: {e}", ours_path.display()));
            let expected = std::fs::read(&golden).expect("read golden");
            assert_eq!(
                ours.len(),
                expected.len(),
                "{theme}/bundle{n}.js: {} bytes, golden {}",
                ours.len(),
                expected.len()
            );
            if ours != expected {
                let at = ours
                    .iter()
                    .zip(&expected)
                    .position(|(a, b)| a != b)
                    .unwrap_or(0);
                let lo = at.saturating_sub(80);
                panic!(
                    "{theme}/bundle{n}.js: first byte difference at offset {at}\n  ours:   {:?}\n  golden: {:?}",
                    String::from_utf8_lossy(&ours[lo..(at + 80).min(ours.len())]),
                    String::from_utf8_lossy(&expected[lo..(at + 80).min(expected.len())]),
                );
            }
            compared += 1;
        }
    }
    assert_eq!(compared, 14, "expected 7 bundles per theme");

    let _ = std::fs::remove_dir_all(&out);
}

/// A single-theme run of the FIRST deploy target (blank) sees a fresh cache,
/// exactly like the real deploy's first package — its output must equal the
/// same goldens.
#[test]
fn blank_alone_matches_its_goldens() {
    let root = reference_root();
    let goldens = goldens_dir();
    if !root.is_dir() || !goldens.join("blank").is_dir() {
        println!("SKIP: reference environment not present");
        return;
    }
    let Some(out) = scratch_dir() else {
        println!("SKIP: no writable scratch dir for the order probe");
        return;
    };
    // distinct subdir so the parallel test above can't collide
    let out = out.join("single");
    std::fs::create_dir_all(&out).expect("mkdir");

    let run = magecommand()
        .args([
            "static",
            "bundle",
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

    for n in 0..7 {
        let ours = std::fs::read(out.join("Magento/blank/js/bundle").join(format!("bundle{n}.js")))
            .expect("read ours");
        let expected =
            std::fs::read(goldens.join("blank").join(format!("bundle{n}.js"))).expect("golden");
        assert_eq!(ours, expected, "blank/bundle{n}.js differs");
    }

    let _ = std::fs::remove_dir_all(&out);
}
