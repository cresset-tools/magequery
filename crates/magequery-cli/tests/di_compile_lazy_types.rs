//! `di compile` and Mage-OS's PHP 8.4 lazy-ghost pass.
//!
//! Not built on Windows, for the same reason as the sibling `di compile`
//! fixtures: the metadata filenames join their cache scope with `|`, a reserved
//! character in a Windows filename.
//!
//! Mage-OS 3.3+ runs `Chain\LazyTypes`, which writes a `lazyTypes` ALLOW-list
//! (only listed classes may be lazy ghosts); its runtime ignores the older
//! `nonLazyTypes` deny-list. Writing the deny-list made no class lazy on a
//! Mage-OS 3.6 / 4.0 store. The pass returns early below PHP 8.4, and on 4.0 a
//! class also needs a constructor `ConstructorSideEffectScanner` proves free of
//! side effects — not ported yet, so nothing is listed there.
//!
//! https://github.com/cresset-tools/magequery/issues/219

#![cfg(not(windows))]

use std::path::PathBuf;
use std::process::Command;

const COMPILER: &str = "setup/src/Magento/Setup/Module/Di/Compiler";

struct Fixture(PathBuf);

impl Fixture {
    /// A store whose compiler ships the given files under [`COMPILER`].
    fn new(name: &str, compiler_files: &[&str]) -> Self {
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
            "app/code/Acme/Repro/Model/Dependency.php",
            "<?php\nnamespace Acme\\Repro\\Model;\n\nclass Dependency\n{\n}\n",
        );
        // `Consumer` is lazy-eligible; `Sealed` is final, so never.
        write(
            "app/code/Acme/Repro/Model/Consumer.php",
            "<?php\nnamespace Acme\\Repro\\Model;\n\nclass Consumer\n{\n    \
             public function __construct(Dependency $dep, $label = 'x')\n    {\n    }\n}\n",
        );
        write(
            "app/code/Acme/Repro/Model/Sealed.php",
            "<?php\nnamespace Acme\\Repro\\Model;\n\nfinal class Sealed\n{\n    \
             public function __construct(Dependency $dep, $label = 'x')\n    {\n    }\n}\n",
        );
        write(
            "app/code/Acme/Repro/etc/di.xml",
            "<?xml version=\"1.0\"?>\n\
             <config xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n    \
             <type name=\"Acme\\Repro\\Model\\Consumer\">\n        <arguments>\n            \
             <argument name=\"label\" xsi:type=\"string\">y</argument>\n        \
             </arguments>\n    </type>\n    \
             <type name=\"Acme\\Repro\\Model\\Sealed\">\n        <arguments>\n            \
             <argument name=\"label\" xsi:type=\"string\">y</argument>\n        \
             </arguments>\n    </type>\n</config>\n",
        );
        for f in compiler_files {
            write(&format!("{COMPILER}/{f}"), "<?php\n");
        }
        Fixture(root)
    }

    fn compile(&self, extra: &[&str]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["di", "compile", "--root"])
            .arg(&self.0)
            .args(extra)
            .output()
            .expect("run magecommand");
        assert!(
            out.status.success(),
            "di compile failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let path = self.0.join("generated/metadata/global.php");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The `class => true` keys of the top-level `name` section, or `None` when the
/// file has no such section.
fn section(metadata: &str, name: &str) -> Option<Vec<String>> {
    let header = format!("\n  '{name}' => \n  array (\n");
    let start = metadata.find(&header)? + header.len();
    let rest = &metadata[start..];
    let end = rest.find("  ),\n").unwrap_or(rest.len());
    Some(
        rest[..end]
            .lines()
            .filter_map(|l| l.trim().strip_prefix('\'')?.strip_suffix("' => true,"))
            .map(|k| k.replace("\\\\", "\\"))
            .collect(),
    )
}

const CONSUMER: &str = "Acme\\Repro\\Model\\Consumer";
const SEALED: &str = "Acme\\Repro\\Model\\Sealed";

#[test]
fn mageos_3_6_gets_a_lazy_types_allow_list() {
    let fx = Fixture::new("mq-lazy-3-6", &["Config/Chain/LazyTypes.php"]);
    let metadata = fx.compile(&[]);

    let lazy = section(&metadata, "lazyTypes")
        .unwrap_or_else(|| panic!("a Mage-OS 3.3+ checkout must get `lazyTypes`:\n{metadata}"));
    assert!(lazy.iter().any(|c| c == CONSUMER), "an eligible class is allowed: {lazy:?}");
    assert!(!lazy.iter().any(|c| c == SEALED), "a final class never is: {lazy:?}");
    assert_eq!(
        section(&metadata, "nonLazyTypes"),
        None,
        "Mage-OS 3.3+ ignores the deny-list; it must not be written"
    );
}

#[test]
fn below_php_8_4_the_pass_writes_nothing() {
    let fx = Fixture::new("mq-lazy-php83", &["Config/Chain/LazyTypes.php"]);
    let metadata = fx.compile(&["--target-php", "8.3"]);
    assert_eq!(section(&metadata, "lazyTypes"), None, "{metadata}");
    assert_eq!(section(&metadata, "nonLazyTypes"), None, "{metadata}");
}

#[test]
fn mageos_4_0_lists_nothing_until_the_side_effect_scanner_is_ported() {
    let fx = Fixture::new(
        "mq-lazy-4-0",
        &["Config/Chain/LazyTypes.php", "ConstructorSideEffectScanner.php"],
    );
    let metadata = fx.compile(&[]);
    assert_eq!(
        section(&metadata, "lazyTypes"),
        Some(Vec::new()),
        "without the scanner no constructor is proven side-effect free:\n{metadata}"
    );
}

#[test]
fn mageos_3_2_keeps_the_non_lazy_types_deny_list() {
    let fx = Fixture::new("mq-lazy-3-2", &["Config/Chain/NonLazyTypes.php"]);
    let metadata = fx.compile(&[]);
    let non_lazy = section(&metadata, "nonLazyTypes")
        .unwrap_or_else(|| panic!("a Mage-OS 3.0–3.2 checkout gets `nonLazyTypes`:\n{metadata}"));
    assert!(non_lazy.iter().any(|c| c == SEALED), "{non_lazy:?}");
    assert!(!non_lazy.iter().any(|c| c == CONSUMER), "{non_lazy:?}");
    assert_eq!(section(&metadata, "lazyTypes"), None);
}

#[test]
fn adobe_commerce_has_no_lazy_pass() {
    let fx = Fixture::new("mq-lazy-adobe", &["Config/Chain/BackslashTrim.php"]);
    let metadata = fx.compile(&[]);
    assert_eq!(section(&metadata, "lazyTypes"), None, "{metadata}");
    assert_eq!(section(&metadata, "nonLazyTypes"), None, "{metadata}");
}

#[test]
fn a_target_platform_overrides_the_checkout() {
    // No compiler classes to read: told it is Mage-OS 3.3+.
    let fx = Fixture::new("mq-lazy-target-mageos", &[]);
    let metadata = fx.compile(&["--target-platform", "mageos-3.3"]);
    let lazy = section(&metadata, "lazyTypes").unwrap_or_else(|| panic!("{metadata}"));
    assert!(lazy.iter().any(|c| c == CONSUMER), "{lazy:?}");

    // A Mage-OS checkout compiled for Magento: no pass.
    let fx = Fixture::new("mq-lazy-target-magento", &["Config/Chain/LazyTypes.php"]);
    let metadata = fx.compile(&["--target-platform", "magento-2.4.9"]);
    assert_eq!(section(&metadata, "lazyTypes"), None, "{metadata}");
    assert_eq!(section(&metadata, "nonLazyTypes"), None, "{metadata}");
}
