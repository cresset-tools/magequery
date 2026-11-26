//! `di compile --fail-on <kind>`: fail a compile that Magento would complete,
//! on a result that breaks the store at runtime, for a CI gate.
//!
//! Not built on Windows, for the same reason as the sibling `di compile`
//! fixtures: the metadata filenames join their cache scope with `|`.
//!
//! `lost-arguments`: di.xml configures arguments for a type whose constructor
//! the compile cannot resolve, so the compiled config has no row for it and
//! Magento builds it by runtime reflection, without that configuration.
//! `setup:di:compile` does the same and succeeds; the store then throws
//! `Missing required argument`, or quietly runs on PHP defaults.

#![cfg(not(windows))]

use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture(PathBuf);

impl Fixture {
    /// A module with one type that loses its di.xml arguments: it extends a
    /// class that does not exist, so its constructor cannot be read.
    fn new(name: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = std::fs::remove_dir_all(&root);
        let fx = Fixture(root);
        fx.write("app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Probe' => 1]];\n");
        fx.write(
            "app/code/Acme/Probe/etc/module.xml",
            "<?xml version=\"1.0\"?>\n<config><module name=\"Acme_Probe\"/></config>\n",
        );
        fx.write(
            "app/code/Acme/Probe/Model/Child.php",
            "<?php\nnamespace Acme\\Probe\\Model;\n\nclass Child extends \\Missing\\Base\n{\n}\n",
        );
        fx.write(
            "app/code/Acme/Probe/etc/di.xml",
            "<?xml version=\"1.0\"?>\n<config>\n<type name=\"Acme\\Probe\\Model\\Child\"><arguments>\
             <argument name=\"x\" xsi:type=\"string\">y</argument></arguments></type>\n</config>\n",
        );
        fx
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn compile(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["di", "compile", "--root"])
            .arg(&self.0)
            .args(args)
            .output()
            .expect("run magecommand")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn lost_arguments_warn_by_default() {
    let fx = Fixture::new("mq-fail-on-default");
    let out = fx.compile(&["--force"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("Acme\\Probe\\Model\\Child"), "still named:\n{}", stderr(&out));
}

#[test]
fn fail_on_lost_arguments_fails_the_compile() {
    let fx = Fixture::new("mq-fail-on-lost");
    let out = fx.compile(&["--force", "--fail-on", "lost-arguments"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("Acme\\Probe\\Model\\Child"), "{}", stderr(&out));
    assert!(stderr(&out).contains("--fail-on lost-arguments"), "names the rule that failed it:\n{}", stderr(&out));
    // The tree is what Magento writes too: the failure is the gate's, not the compile's.
    assert!(fx.0.join("generated/metadata/global.php").is_file());
}

#[test]
fn an_up_to_date_incremental_run_still_fails() {
    let fx = Fixture::new("mq-fail-on-incremental");
    // A generated factory: --incremental only trusts a tree with generated/code.
    fx.write(
        "app/code/Acme/Probe/Model/Uses.php",
        "<?php\nnamespace Acme\\Probe\\Model;\n\nclass Uses\n{\n    \
         public function __construct(UsesFactory $factory)\n    {\n    }\n}\n",
    );
    assert!(fx.compile(&["--incremental"]).status.success());
    let out = fx.compile(&["--incremental", "--fail-on", "lost-arguments"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("up to date"), "{}", stderr(&out));
    assert_eq!(out.status.code(), Some(1), "skipping the compile must not skip the gate:\n{}", stderr(&out));
    assert!(stderr(&out).contains("Acme\\Probe\\Model\\Child"), "{}", stderr(&out));
}

#[test]
fn an_unknown_kind_is_a_usage_error() {
    let fx = Fixture::new("mq-fail-on-unknown");
    let out = fx.compile(&["--force", "--fail-on", "missing-class"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}
