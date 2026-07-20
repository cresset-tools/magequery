//! Flag plumbing for `magecommand static less --file` / `--compress` — the
//! per-file compile mode the Magento bridge adapter shells out to. Runs the
//! real binary over a tiny synthetic file tree; the compilation semantics
//! themselves are covered by the `static_deploy::less` unit tests and the
//! magecommand-less fixture suite.

use std::process::Command;

fn magecommand() -> Command {
    Command::new(env!("CARGO_BIN_EXE_magecommand"))
}

/// A materialized-style tree: the entry imports relatively (extension-less
/// and extensioned) from its own directory.
fn file_tree() -> tempfile::TempDir {
    let td = tempfile::tempdir().expect("tempdir");
    let w = |rel: &str, content: &str| {
        let p = td.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    };
    w(
        "css/entry.less",
        "@import 'source/vars';\n.a { color: @c; }\n",
    );
    w("css/source/vars.less", "@c: red;\n");
    td
}

fn entry(td: &tempfile::TempDir) -> String {
    td.path().join("css/entry.less").display().to_string()
}

#[test]
fn file_mode_defaults_to_stdout() {
    let td = file_tree();
    let out = magecommand()
        .args(["static", "less", "--file", &entry(&td)])
        .output()
        .expect("run magecommand");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let css = String::from_utf8_lossy(&out.stdout);
    assert!(css.contains(".a {\n  color: red;\n}"), "stdout:\n{css}");
}

#[test]
fn file_mode_explicit_stdout_flag_is_accepted() {
    let td = file_tree();
    let out = magecommand()
        .args(["static", "less", "--file", &entry(&td), "--stdout"])
        .output()
        .expect("run magecommand");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains(".a {"),);
}

#[test]
fn file_mode_compress() {
    let td = file_tree();
    let out = magecommand()
        .args(["static", "less", "--file", &entry(&td), "--compress"])
        .output()
        .expect("run magecommand");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let css = String::from_utf8_lossy(&out.stdout);
    assert!(css.contains(".a{color:red}"), "stdout:\n{css}");
}

#[test]
fn file_mode_out_dir_writes_stem_css() {
    let td = file_tree();
    let out_dir = td.path().join("dist");
    let out = magecommand()
        .args(["static", "less", "--file", &entry(&td)])
        .arg("--out")
        .arg(&out_dir)
        .output()
        .expect("run magecommand");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let css = std::fs::read_to_string(out_dir.join("entry.css")).expect("entry.css written");
    assert!(css.contains(".a {\n  color: red;\n}"), "file:\n{css}");
    // The report line names input and target; the CSS itself stays off stdout.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("entry.css"), "stdout:\n{stdout}");
    assert!(!stdout.contains("color: red"), "stdout:\n{stdout}");
}

/// A compile error exits non-zero and surfaces the compiler's rendering
/// verbatim on stderr (file, line — the message the PHP adapter shows).
#[test]
fn file_mode_error_exits_nonzero_naming_file_and_line() {
    let td = file_tree();
    let bad = td.path().join("css/broken.less");
    std::fs::write(&bad, ".b { color: @missing; }\n").unwrap();
    let out = magecommand()
        .args(["static", "less", "--file"])
        .arg(&bad)
        .output()
        .expect("run magecommand");
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "stdout not clean: {}", String::from_utf8_lossy(&out.stdout));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("broken.less"), "stderr:\n{err}");
    assert!(err.contains("line 1"), "stderr:\n{err}");
}

#[test]
fn file_conflicts_with_theme_and_entry() {
    let td = file_tree();
    for extra in [["--theme", "Magento/luma"], ["--entry", "styles-m"]] {
        let out = magecommand()
            .args(["static", "less", "--file", &entry(&td)])
            .args(extra)
            .output()
            .expect("run magecommand");
        assert!(!out.status.success(), "{extra:?} should conflict with --file");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("cannot be used with"), "stderr:\n{err}");
    }
}

#[test]
fn theme_or_file_is_required() {
    let out = magecommand()
        .args(["static", "less"])
        .output()
        .expect("run magecommand");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--theme") && err.contains("required"), "stderr:\n{err}");
}

#[test]
fn missing_input_file_fails_cleanly() {
    let td = file_tree();
    let gone = td.path().join("css/gone.less");
    let out = magecommand()
        .args(["static", "less", "--file"])
        .arg(&gone)
        .output()
        .expect("run magecommand");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("gone.less") && err.contains("read failed"), "stderr:\n{err}");
}
