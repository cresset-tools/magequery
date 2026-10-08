//! `di compile` and a constructor default that cannot be folded statically.
//!
//! Not built on Windows, for the same reason as the sibling `di compile`
//! fixtures: the metadata filenames join their cache scope with `|`, a reserved
//! character in a Windows filename.
//!
//! magecommand never executes PHP, so some constructor defaults — a constant
//! from an extension it has no table entry for, one a module `define()`s at
//! runtime — cannot be evaluated. The question is what to emit then.
//!
//! Emitting `'_vn_' => true` is the one answer that must never be given:
//! `Magento\Framework\ObjectManager\Factory\Compiled` reads `_vn_` as a literal
//! null, so a non-nullable parameter is handed null and fatals with a TypeError
//! the first time the object manager builds that class. Not knowing a value is
//! not the same as knowing it is null.
//!
//! Nor can a single argument be dropped or nulled in isolation: the factory
//! passes the row POSITIONALLY (`$args = array_values($args)`), so removing one
//! entry shifts every later argument into the wrong parameter — a worse failure
//! than the one being fixed. The row has to be all-or-nothing.
//!
//! So the whole class's `arguments` entry degrades to `NULL`, which sends the
//! factory down its runtime branch:
//!
//! ```php
//! $parameters = $this->getDefinitions()->getParameters($type) ?: [];
//! $args = $this->resolveArgumentsInRuntime($type, $parameters, $arguments);
//! ```
//!
//! where PHP reflects the constructor and reads the real default itself. The
//! class loses its cached arguments; it never receives an invented value. This
//! is the same degradation magecommand already applies to eval-obfuscated
//! vendor source.

#![cfg(not(windows))]

use std::path::PathBuf;
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    /// Per-test root: the tests run in parallel and `Drop` removes the tree,
    /// so a shared path makes them race.
    fn new(name: &str) -> Self {
        let root =
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("mq-unresolvable-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str, content: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write("app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Unres' => 1]];\n");
        write(
            "app/code/Acme/Unres/etc/module.xml",
            "<?xml version=\"1.0\"?>\n<config><module name=\"Acme_Unres\"/></config>\n",
        );
        // ACME_MYSTERY_CONSTANT is defined nowhere magecommand can see. The
        // second parameter is the positional-shift canary: if the row were
        // emitted with only `$kept`, the factory would pass 'kept' as $mystery.
        write(
            "app/code/Acme/Unres/Model/Unfoldable.php",
            "<?php\nnamespace Acme\\Unres\\Model;\n\nclass Unfoldable\n{\n    \
             public function __construct(\n        \
             private readonly string $mystery = ACME_MYSTERY_CONSTANT,\n        \
             private readonly string $kept = 'kept'\n    ) {\n    }\n}\n",
        );
        // Control: everything folds, so this class keeps its cached row.
        write(
            "app/code/Acme/Unres/Model/Foldable.php",
            "<?php\nnamespace Acme\\Unres\\Model;\n\nclass Foldable\n{\n    \
             public function __construct(private readonly string $kept = 'kept')\n    {\n    }\n}\n",
        );
        // Control: the di config replaces the unfoldable default, so nothing
        // unreadable reaches the row and it must NOT be degraded.
        write(
            "app/code/Acme/Unres/Model/Overridden.php",
            "<?php\nnamespace Acme\\Unres\\Model;\n\nclass Overridden\n{\n    \
             public function __construct(private readonly string $mystery = ACME_MYSTERY_CONSTANT)\n    {\n    }\n}\n",
        );
        write(
            "app/code/Acme/Unres/etc/di.xml",
            "<?xml version=\"1.0\"?>\n\
             <config xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n    \
             <type name=\"Acme\\Unres\\Model\\Overridden\">\n        <arguments>\n            \
             <argument name=\"mystery\" xsi:type=\"string\">configured</argument>\n        \
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
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The row for `class`, from its key to the end of that entry.
fn row(metadata: &str, class: &str) -> String {
    let key = format!("    '{}' => ", class.replace('\\', "\\\\"));
    let start = metadata
        .find(&key)
        .unwrap_or_else(|| panic!("no row for {class} in:\n{metadata}"));
    let rest = &metadata[start..];
    let end = rest[1..].find("\n    '").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].to_owned()
}

#[test]
fn an_unfoldable_default_degrades_the_whole_row_to_null() {
    let fx = Fixture::new("unfoldable");
    fx.compile();
    let metadata = fx.global_metadata();
    let r = row(&metadata, "Acme\\Unres\\Model\\Unfoldable");

    assert!(
        r.contains("NULL"),
        "the row must degrade to NULL so the factory reflects at runtime; got:\n{r}"
    );
    assert!(
        !r.contains("_vn_"),
        "it must NOT emit `_vn_` — the factory reads that as a literal null and \
         a non-nullable `string` parameter then fatals; got:\n{r}"
    );
    assert!(
        !r.contains("'kept'"),
        "and it must not emit a PARTIAL row: arguments are passed positionally, \
         so a surviving later argument would land in the wrong parameter; got:\n{r}"
    );
}

/// The degradation is scoped to the class that needs it — one unreadable
/// default must not null out every row in the file.
#[test]
fn a_class_whose_defaults_all_fold_keeps_its_row() {
    let fx = Fixture::new("foldable");
    fx.compile();
    let r = row(&fx.global_metadata(), "Acme\\Unres\\Model\\Foldable");

    assert!(
        r.contains("'_v_' => 'kept'"),
        "a fully foldable constructor must keep its cached arguments; got:\n{r}"
    );
}

/// An unfoldable default that the di config replaces never reaches the output,
/// so there is nothing to degrade — degrading anyway would needlessly uncache
/// every class that configures such an argument.
#[test]
fn a_configured_override_of_an_unfoldable_default_keeps_its_row() {
    let fx = Fixture::new("overridden");
    fx.compile();
    let r = row(&fx.global_metadata(), "Acme\\Unres\\Model\\Overridden");

    assert!(
        r.contains("'_v_' => 'configured'"),
        "the configured value replaces the unreadable default, so the row stands; got:\n{r}"
    );
}
