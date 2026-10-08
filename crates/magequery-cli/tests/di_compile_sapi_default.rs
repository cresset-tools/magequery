//! `di compile` and a `PHP_SAPI` constructor default.
//!
//! Not built on Windows, for the same reason as the sibling `di compile`
//! fixtures: the metadata filenames join their cache scope with `|`, a reserved
//! character in a Windows filename.
//!
//! The real compiler reflects a constructor's default and bakes the *value* it
//! sees. For `PHP_SAPI` that value is decided by the process doing the
//! compiling, and `setup:di:compile` is always CLI — so the oracle holds
//! `'_v_' => 'cli'`.
//!
//! Failing to fold the constant was not a harmless gap. The argument degraded
//! to `'_vn_' => true`, and `Magento\Framework\ObjectManager\Factory\Compiled`
//! reads `_vn_` as a literal null:
//!
//! ```php
//! } elseif (isset($argument['_vn_'])) {
//!     $argument = null;
//! }
//! ...
//! $args = array_values($args);
//! ```
//!
//! so a non-nullable `string $sapiName` got null passed positionally and
//! fataled with a TypeError the first time the class was built. A real store
//! hit exactly this: all eight per-area DI config files diverged from the
//! oracle on this single argument, and the class sat behind a quote-save
//! observer.

#![cfg(not(windows))]

use std::path::PathBuf;
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mq-sapi-default");
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str, content: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write("app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Sapi' => 1]];\n");
        write(
            "app/code/Acme/Sapi/etc/module.xml",
            "<?xml version=\"1.0\"?>\n<config><module name=\"Acme_Sapi\"/></config>\n",
        );
        // The shape from the store that surfaced this: a non-nullable, typed,
        // promoted parameter defaulting to PHP_SAPI. `$eol` is the control —
        // an already-folding constant, so a regression here is visibly about
        // PHP_SAPI and not about constant folding in general.
        write(
            "app/code/Acme/Sapi/Model/SourceResolver.php",
            "<?php\nnamespace Acme\\Sapi\\Model;\n\nclass SourceResolver\n{\n    \
             public function __construct(\n        \
             private readonly string $sapiName = PHP_SAPI,\n        \
             private readonly string $eol = PHP_EOL\n    ) {\n    }\n}\n",
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
fn a_php_sapi_default_folds_to_cli_rather_than_null() {
    let fx = Fixture::new();
    fx.compile();
    let metadata = fx.global_metadata();

    assert!(
        metadata.contains("'_v_' => 'cli'"),
        "a PHP_SAPI default must fold to the compile-time SAPI; got:\n{metadata}"
    );
    assert!(
        !metadata.contains("'_vn_' => true"),
        "it must NOT degrade to `_vn_` — the compiled factory reads that as a \
         literal null and a non-nullable `string` parameter then fatals; got:\n{metadata}"
    );
    // The control: ordinary constant folding is unaffected.
    assert!(
        metadata.contains("'_v_' => '\n'"),
        "PHP_EOL must still fold; got:\n{metadata}"
    );
}
