//! The flags that change when a run fails, under their descriptive names:
//! `magequery doctor --fail-on-warnings` and `magecommand static verify
//! --byte-exact-css`. Both were `--strict`, which said neither what became
//! stricter nor how; the old spelling stays accepted so scripts keep working.

use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A store whose only finding is a warning: a module on disk that config.php
/// does not list (E1002).
fn store_with_one_warning() -> PathBuf {
    let root = scratch("mq-doctor-warning");
    write(&root, "app/etc/config.php", "<?php\nreturn ['modules' => []];\n");
    write(
        &root,
        "app/code/Acme/Loose/etc/module.xml",
        "<?xml version=\"1.0\"?>\n<config><module name=\"Acme_Loose\"/></config>\n",
    );
    write(
        &root,
        "app/code/Acme/Loose/registration.php",
        "<?php\n\\Magento\\Framework\\Component\\ComponentRegistrar::register(\
         \\Magento\\Framework\\Component\\ComponentRegistrar::MODULE, 'Acme_Loose', __DIR__);\n",
    );
    root
}

/// The exit code: 0 clean, 1 failed on a finding (2 is a usage error).
fn doctor(root: &Path, extra: &[&str]) -> Option<i32> {
    Command::new(env!("CARGO_BIN_EXE_magequery"))
        .arg("--root")
        .arg(root)
        .args(["doctor", "--no-baseline"])
        .args(extra)
        .output()
        .expect("run magequery")
        .status
        .code()
}

#[test]
fn doctor_fails_on_warnings_only_when_asked() {
    let root = store_with_one_warning();
    assert_eq!(doctor(&root, &[]), Some(0), "a warning alone does not fail the run");
    assert_eq!(doctor(&root, &["--fail-on-warnings"]), Some(1), "--fail-on-warnings fails it");
    assert_eq!(doctor(&root, &["--strict"]), Some(1), "the old name still works");
}

/// A reference and an output whose one stylesheet differs only in formatting.
fn trees_differing_in_css_formatting() -> (PathBuf, PathBuf) {
    let base = scratch("mq-static-verify-css");
    let (reference, output) = (base.join("reference"), base.join("output"));
    let css = "frontend/Acme/theme/en_US/css/styles.css";
    write(&reference, css, "a{color:#FFFFFF}\n");
    write(&output, css, "a {\n  color: #fff;\n}\n");
    (reference, output)
}

/// The exit code, as for [`doctor`].
fn verify(reference: &Path, output: &Path, extra: &[&str]) -> Option<i32> {
    Command::new(env!("CARGO_BIN_EXE_magecommand"))
        .args(["static", "verify", "--fail-on-diff", "--reference"])
        .arg(reference)
        .arg("--output")
        .arg(output)
        .args(extra)
        .output()
        .expect("run magecommand")
        .status
        .code()
}

#[test]
fn static_verify_requires_identical_css_bytes_only_when_asked() {
    let (reference, output) = trees_differing_in_css_formatting();
    assert_eq!(verify(&reference, &output, &[]), Some(0), "formatting-only css is equivalent");
    assert_eq!(verify(&reference, &output, &["--byte-exact-css"]), Some(1), "--byte-exact-css counts it");
    assert_eq!(verify(&reference, &output, &["--strict"]), Some(1), "the old name still works");
}
