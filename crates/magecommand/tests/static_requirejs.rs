//! The byte-exact gate for `magecommand static requirejs`: assemble
//! `requirejs-config.js` for blank and luma against a real Magento checkout
//! and compare against goldens captured from a real
//! `setup:static-content:deploy`.
//!
//! **Environment-dependent, and SKIPPED (not failed) when the environment is
//! absent** — CI has no Magento install. The reference root defaults to
//! `/home/jelle/mg-install-310` (the same default the `magecommand-php`
//! corpus test uses) and is overridable with `MAGECOMMAND_CORPUS`; the
//! goldens default to `/home/jelle/mg-scd-goldens/requirejs`, overridable
//! with `MAGECOMMAND_SCD_GOLDENS`. A missing root or golden prints a skip
//! notice and returns.
//!
//! The ordering/wrapping semantics themselves are covered by the
//! `static_deploy::requirejs` unit tests over a synthetic tree; this test is
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
            .unwrap_or_else(|_| "/home/jelle/mg-scd-goldens/requirejs".to_owned()),
    )
}

/// Assemble `theme` against the reference install and compare with its
/// golden. Returns false (with a printed notice) when the environment is
/// absent — the caller then skips.
fn gate(theme: &str, golden_name: &str) -> bool {
    let root = reference_root();
    let golden = goldens_dir().join(golden_name);
    if !root.is_dir() || !golden.is_file() {
        println!(
            "SKIP: reference install ({}) or golden ({}) not present",
            root.display(),
            golden.display()
        );
        return false;
    }

    let out = magecommand()
        .args([
            "static",
            "requirejs",
            "--root",
            &root.display().to_string(),
            "--theme",
            theme,
            "--stdout",
        ])
        .output()
        .expect("run magecommand");
    assert!(
        out.status.success(),
        "exit {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let expected = std::fs::read(&golden).expect("read golden");
    assert_eq!(
        out.stdout.len(),
        expected.len(),
        "{theme}: {} bytes, golden {} bytes",
        out.stdout.len(),
        expected.len()
    );
    if out.stdout != expected {
        let at = out
            .stdout
            .iter()
            .zip(&expected)
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        let lo = at.saturating_sub(80);
        panic!(
            "{theme}: first byte difference at offset {at}\n  ours:   {:?}\n  golden: {:?}",
            String::from_utf8_lossy(&out.stdout[lo..(at + 80).min(out.stdout.len())]),
            String::from_utf8_lossy(&expected[lo..(at + 80).min(expected.len())]),
        );
    }
    true
}

#[test]
fn blank_matches_the_deployed_golden_byte_for_byte() {
    gate("Magento/blank", "blank-requirejs-config.js");
}

/// Luma inherits blank, so the collector walks two themes (ancestor-first) —
/// the ordering rule's load-bearing case.
#[test]
fn luma_matches_the_deployed_golden_byte_for_byte() {
    gate("Magento/luma", "luma-requirejs-config.js");
}

/// `--json` renders the ordered source list: the library layer (absent on a
/// stock install), module contexts in `config.php` load order, then the theme
/// layers ancestor-first.
#[test]
fn json_lists_the_sources_in_collector_order() {
    let root = reference_root();
    if !root.is_dir() {
        println!("SKIP: reference install ({}) not present", root.display());
        return;
    }
    let out = magecommand()
        .args([
            "--json",
            "static",
            "requirejs",
            "--root",
            &root.display().to_string(),
            "--theme",
            "Magento/luma",
            "--stdout",
        ])
        .output()
        .expect("run magecommand");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let doc: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("parse --json output");

    let chain: Vec<&str> = doc["theme_chain"]
        .as_array()
        .expect("theme_chain")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(chain, ["frontend/Magento/luma", "frontend/Magento/blank"]);

    let sources = doc["sources"].as_array().expect("sources");
    assert!(!sources.is_empty());
    // Every theme-layer source comes after every module-layer one.
    let first_theme = sources
        .iter()
        .position(|s| s["origin"].as_str().unwrap().starts_with("theme"));
    if let Some(i) = first_theme {
        assert!(
            sources[i..]
                .iter()
                .all(|s| s["origin"].as_str().unwrap().starts_with("theme")),
            "a module source follows a theme source"
        );
    }
    // `--stdout` with `--json` writes nothing.
    assert!(doc["output"].is_null());
}
