//! `di compile` and nullable class types spelled as a union (`Foo|null`).
//!
//! Not built on Windows, for the same reason as the sibling `di compile`
//! fixtures: the metadata filenames join their cache scope with `|`, a reserved
//! character in a Windows filename.
//!
//! PHP reflection normalises `Foo|null` (and `null|Foo`) to the same
//! `ReflectionNamedType` as `?Foo`, so Magento's `GetParameterClassTrait` sees a
//! class type and an `xsi:type="object"` argument for it compiles to `_i_`.
//! Treating it as a genuine union instead dropped the class type and emitted the
//! object argument as a plain `_v_` array (`['instance' => …]`), which throws a
//! `TypeError` when the object manager passes it to the constructor.
//!
//! Reported in https://github.com/cresset-tools/magequery/issues/212 against
//! `RedChamps\CleanMenu\Model\Config\Backend\Cache`.

#![cfg(not(windows))]

use std::path::PathBuf;
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mq-union-nullable");
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
        // `$short` is the `?T` control; `$union` and `$flipped` are the two
        // union spellings reflection normalises to the same thing.
        write(
            "app/code/Acme/Repro/Model/Consumer.php",
            "<?php\nnamespace Acme\\Repro\\Model;\n\nclass Consumer\n{\n    \
             public function __construct(\n        \
             ?Dependency $short = null,\n        \
             Dependency|null $union = null,\n        \
             null|Dependency $flipped = null\n    \
             ) {\n    }\n}\n",
        );
        write(
            "app/code/Acme/Repro/etc/di.xml",
            "<?xml version=\"1.0\"?>\n\
             <config xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n    \
             <type name=\"Acme\\Repro\\Model\\Consumer\">\n        <arguments>\n            \
             <argument name=\"short\" xsi:type=\"object\">Acme\\Repro\\Model\\Dependency</argument>\n            \
             <argument name=\"union\" xsi:type=\"object\">Acme\\Repro\\Model\\Dependency</argument>\n            \
             <argument name=\"flipped\" xsi:type=\"object\">Acme\\Repro\\Model\\Dependency</argument>\n        \
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

/// The sub-block for one parameter: from `'<name>' =>` up to the next
/// parameter's key or the end of the class block.
fn param_block<'a>(block: &'a str, name: &str) -> &'a str {
    let key = format!("'{name}' =>");
    let start = block
        .find(&key)
        .unwrap_or_else(|| panic!("no `{name}` argument in:\n{block}"));
    let rest = &block[start..];
    let end = ["'short' =>", "'union' =>", "'flipped' =>"]
        .iter()
        .filter_map(|k| rest[key.len()..].find(k).map(|i| i + key.len()))
        .min()
        .unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn a_union_nullable_class_type_takes_an_object_argument() {
    let fx = Fixture::new();
    fx.compile();
    let block = consumer_block(&fx.global_metadata());

    for name in ["short", "union", "flipped"] {
        let param = param_block(&block, name);
        assert!(
            param.contains("'_i_' => 'Acme\\\\Repro\\\\Model\\\\Dependency'"),
            "`{name}` is a nullable class type, so its object argument must \
             compile to `_i_`; got:\n{param}"
        );
        assert!(
            !param.contains("'instance' =>"),
            "`{name}` must not get the object argument as a plain `_v_` array; \
             got:\n{param}"
        );
    }
}
