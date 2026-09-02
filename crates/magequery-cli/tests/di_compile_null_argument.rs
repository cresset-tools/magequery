//! `di compile` and `<argument xsi:type="null"/>`.
//!
//! Not built on Windows, for the same reason as the sibling `di compile`
//! fixtures: the metadata filenames join their cache scope with `|`, a reserved
//! character in a Windows filename.
//!
//! `ArgumentsResolver::getResolvedConstructorArguments` gates the configured
//! value behind:
//!
//! ```php
//! if (isset($configuredArguments[$constructorArgument->getName()])) {
//! ```
//!
//! and **`isset()` is false for null**. So `<argument name="x" xsi:type="null"/>`
//! does not pass null — it UNSETS the argument and lets the default resolution
//! stand, which for a typed parameter means injecting that type. It is the
//! idiom a module uses to revert an argument it inherited, and
//! Amasty_CompanyAccount applies it to four constructor parameters.
//!
//! Honouring the null instead emitted `_vn_` (non-object, no value) where the
//! real compiler emits `_i_` — a wrong `arguments` row for every class using the
//! idiom, and eight unexplained metadata files on the store that surfaced it.

#![cfg(not(windows))]

use std::path::PathBuf;
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mq-null-argument");
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
            "app/code/Acme/Repro/Model/Dependency.php",
            "<?php\nnamespace Acme\\Repro\\Model;\n\nclass Dependency\n{\n}\n",
        );
        // `$dep` is typed and has NO default, so the default resolution injects
        // the type; `$plain` is an untyped optional, whose default stands.
        write(
            "app/code/Acme/Repro/Model/Consumer.php",
            "<?php\nnamespace Acme\\Repro\\Model;\n\nclass Consumer\n{\n    \
             public function __construct(Dependency $dep, $plain = 'kept')\n    {\n    }\n}\n",
        );
        write(
            "app/code/Acme/Repro/etc/di.xml",
            "<?xml version=\"1.0\"?>\n\
             <config xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n    \
             <type name=\"Acme\\Repro\\Model\\Consumer\">\n        <arguments>\n            \
             <argument name=\"dep\" xsi:type=\"null\"/>\n            \
             <argument name=\"plain\" xsi:type=\"null\"/>\n        \
             </arguments>\n    </type>\n</config>\n",
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
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The `arguments` row for `Consumer`, from `'Acme\\Repro\\Model\\Consumer' =>`
/// to the close of its block.
fn consumer_block(metadata: &str) -> String {
    let start = metadata
        .find("    'Acme\\\\Repro\\\\Model\\\\Consumer' =>")
        .unwrap_or_else(|| panic!("no Consumer arguments row in:\n{metadata}"));
    let rest = &metadata[start..];
    let end = rest[1..].find("\n    ),").map(|i| i + 8).unwrap_or(rest.len());
    rest[..end].to_owned()
}

#[test]
fn a_null_argument_unsets_rather_than_passing_null() {
    let fx = Fixture::new();
    fx.compile();
    let block = consumer_block(&fx.global_metadata());

    assert!(
        block.contains("'_i_' => 'Acme\\\\Repro\\\\Model\\\\Dependency'"),
        "a null-configured TYPED argument must fall back to injecting its type, \
         not become `_vn_`; got:\n{block}"
    );
    assert!(
        !block.contains("'dep' => \n      array (\n        '_vn_' => true,"),
        "the typed argument must not be emitted as a valueless non-object; got:\n{block}"
    );
    assert!(
        block.contains("'_v_' => 'kept'"),
        "a null-configured OPTIONAL argument must keep its constructor default; \
         got:\n{block}"
    );
}
