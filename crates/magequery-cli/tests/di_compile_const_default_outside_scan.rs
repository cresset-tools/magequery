//! `di compile` and a constructor default naming a class constant from a class
//! OUTSIDE the compile's scan universe.
//!
//! Not built on Windows, for the same reason as the sibling `di compile`
//! fixtures: the metadata filenames join their cache scope with `|`, a reserved
//! character in a Windows filename.
//!
//! The real compiler reflects the default, and reflection goes through the
//! AUTOLOADER — which does not care whether the constant's owning class belongs
//! to an enabled module. A plain composer package resolves either way, so the
//! oracle folds the constant normally.
//!
//! magecommand only knows the classes it scanned, so `DefsLookup::class_const`
//! missed such a class and the default failed to fold. `build.rs` already seeds
//! the hierarchy walk with classes named by di.xml `xsi:type="const"` values for
//! exactly this reason; constructor defaults are the same problem one level
//! deeper, and were not seeded.
//!
//! The blast radius is the whole class: a single unfoldable default takes the
//! entire `arguments` row down with it, losing the correctly-resolved entries
//! for every other parameter too — which is why `$kept` is asserted here.

#![cfg(not(windows))]

use std::path::PathBuf;
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mq-const-outside-scan");
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str, content: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write("app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Seed' => 1]];\n");
        write(
            "app/code/Acme/Seed/etc/module.xml",
            "<?xml version=\"1.0\"?>\n<config><module name=\"Acme_Seed\"/></config>\n",
        );
        // `Other\Lib\Codes` is a plain composer package: PSR-4 reachable, but
        // not a Magento module, so the compile never scans it.
        write(
            "vendor/composer/installed.json",
            "{\n  \"packages\": [\n    {\n      \"name\": \"other/lib\",\n      \
             \"version\": \"1.0.0\",\n      \"install-path\": \"../other/lib\",\n      \
             \"autoload\": { \"psr-4\": { \"Other\\\\Lib\\\\\": \"src/\" } }\n    }\n  ]\n}\n",
        );
        write(
            "vendor/other/lib/src/Codes.php",
            "<?php\nnamespace Other\\Lib;\n\nclass Codes\n{\n    \
             public const DEFAULT_CODE = 'seeded';\n}\n",
        );
        write(
            "app/code/Acme/Seed/Model/Consumer.php",
            "<?php\nnamespace Acme\\Seed\\Model;\n\nuse Other\\Lib\\Codes;\n\n\
             class Consumer\n{\n    public function __construct(\n        \
             private readonly string $code = Codes::DEFAULT_CODE,\n        \
             private readonly string $kept = 'kept'\n    ) {\n    }\n}\n",
        );
        Fixture(root)
    }

    fn compile(&self) {
        let out = Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["di", "compile", "--root"])
            .arg(&self.0)
            .output()
            .expect("run magecommand");
        assert!(
            out.status.success(),
            "di compile failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn global_metadata(&self) -> String {
        let path = self.0.join("generated/metadata/global.php");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_const_default_from_outside_the_scan_universe_still_folds() {
    let fx = Fixture::new();
    fx.compile();
    let metadata = fx.global_metadata();

    assert!(
        metadata.contains("'_v_' => 'seeded'"),
        "the constant's class is PSR-4 reachable, so reflection folds it and so \
         must we; got:\n{metadata}"
    );
    assert!(
        metadata.contains("'_v_' => 'kept'"),
        "and the rest of the row must survive — one unfoldable default takes the \
         whole class's arguments down with it; got:\n{metadata}"
    );
}
