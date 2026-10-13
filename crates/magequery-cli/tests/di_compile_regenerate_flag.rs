//! `di compile` and Magento's `var/.regenerate` flag.
//!
//! Not built on Windows, for the same reason as the sibling `di compile`
//! fixtures: the metadata filenames join their cache scope with `|`, a reserved
//! character in a Windows filename.
//!
//! `magento/magento-composer-installer` touches `var/.regenerate` on every
//! `composer install`. The next `bin/magento` call to boot finds it in
//! `GeneratedFiles::cleanGeneratedFiles()` and deletes `generated/code` and
//! `generated/metadata`. With Magento's own compiler, that call is
//! `setup:di:compile`, which consumes the flag before compiling. A build that
//! compiled with magecommand instead kept the flag, so its first `bin/magento`
//! command (the static deploy) wiped the fresh tree and failed on a missing
//! generated proxy. Verify mode hid it: it runs `setup:di:compile` too.

#![cfg(not(windows))]

use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str, content: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write("app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Repro' => 1]];\n");
        write(
            "app/code/Acme/Repro/etc/module.xml",
            "<?xml version=\"1.0\"?>\n<config><module name=\"Acme_Repro\"/></config>\n",
        );
        write(
            "app/code/Acme/Repro/Model/Thing.php",
            "<?php\nnamespace Acme\\Repro\\Model;\n\nclass Thing\n{\n}\n",
        );
        // A factory in a constructor gives generated/code a file, which the
        // `--incremental` short-circuit requires.
        write(
            "app/code/Acme/Repro/Model/Maker.php",
            "<?php\nnamespace Acme\\Repro\\Model;\n\nclass Maker\n{\n    \
             public function __construct(ThingFactory $factory)\n    {\n    }\n}\n",
        );
        Fixture(root)
    }

    fn touch_flag(&self) {
        std::fs::create_dir_all(self.0.join("var")).unwrap();
        std::fs::write(self.flag(), "").unwrap();
    }

    fn flag(&self) -> PathBuf {
        self.0.join("var/.regenerate")
    }

    fn compile(&self, extra: &[&str]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["di", "compile"])
            .args(extra)
            .arg("--root")
            .arg(&self.0)
            .output()
            .expect("run magecommand");
        assert!(
            out.status.success(),
            "di compile failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn assert_tree(root: &Path) {
    assert!(
        root.join("generated/metadata/global.php").is_file(),
        "the compile must still write its tree"
    );
    assert!(
        root.join("generated/code/Acme/Repro/Model/ThingFactory.php").is_file(),
        "the generated factory must be there"
    );
}

#[test]
fn a_compile_consumes_the_flag_composer_install_left() {
    let fx = Fixture::new("mq-regenerate-flag-full");
    fx.touch_flag();
    fx.compile(&[]);
    assert_tree(&fx.0);
    assert!(
        !fx.flag().exists(),
        "var/.regenerate must be gone, or the next bin/magento boot wipes generated/"
    );
}

#[test]
fn an_up_to_date_incremental_compile_consumes_it_too() {
    let fx = Fixture::new("mq-regenerate-flag-incremental");
    fx.compile(&[]);
    // A later `composer install` that changed no compile input.
    fx.touch_flag();
    let stdout = fx.compile(&["--incremental"]);
    assert!(
        stdout.contains("up to date"),
        "the fixture must take the short-circuit path; got:\n{stdout}"
    );
    assert_tree(&fx.0);
    assert!(
        !fx.flag().exists(),
        "an up-to-date tree must survive the next bin/magento boot as well"
    );
}
