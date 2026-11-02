//! `di compile` and a constructor it cannot resolve statically: a default it
//! cannot fold, or a parent constructor it cannot see.
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
//! Nor can the entry be `NULL`. `Compiled::getArguments()` turns a present NULL
//! into `[]`, and the factory answers `[]` with a bare `new $type()` — every
//! argument dropped, an ArgumentCountError for any required one. Only an ABSENT
//! key makes `getArguments()` return null and sends the factory down its runtime
//! branch:
//!
//! ```php
//! $parameters = $this->getDefinitions()->getParameters($type) ?: [];
//! $args = $this->resolveArgumentsInRuntime($type, $parameters, $arguments);
//! ```
//!
//! where PHP reflects the constructor and reads the real default itself. So the
//! class gets no key at all: it loses its cached arguments, and never receives
//! an invented value. The same holds for a class whose constructor comes from a
//! parent magecommand cannot read — a PHP built-in it has no signature for
//! (`DateInterval`), or an eval-obfuscated vendor class.
//!
//! The runtime branch does not read di.xml `<argument>` config, which lives only
//! in the compiled row, so a class that has some is built without it
//! (`AbstractFactory::getResolvedArgument` only sees call-time arguments). A
//! required non-object argument set only in di.xml then throws `Missing
//! required argument`; an object one gets its type-hinted class, an optional
//! one its PHP default. That is still the best output available, so the
//! compile keeps it and names each such class on stderr, with why its key went:
//! the unfoldable default, or the unreadable constructor.

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
        // A required argument inherited from a built-in magecommand has no
        // constructor signature for. A present key — NULL or empty — makes the
        // factory call `new BuiltinChild()`, an ArgumentCountError.
        write(
            "app/code/Acme/Unres/Model/BuiltinChild.php",
            "<?php\nnamespace Acme\\Unres\\Model;\n\nclass BuiltinChild extends \\DateInterval\n{\n}\n",
        );
        // The built-in parent case again, with di.xml config the runtime
        // branch will ignore: the compile still succeeds, and names it.
        write(
            "app/code/Acme/Unres/Model/ConfiguredBuiltin.php",
            "<?php\nnamespace Acme\\Unres\\Model;\n\nclass ConfiguredBuiltin extends \\DateInterval\n{\n}\n",
        );
        let mut types = String::from(
            "<type name=\"Acme\\Unres\\Model\\ConfiguredBuiltin\">\n        <arguments>\n            \
             <argument name=\"duration\" xsi:type=\"string\">P1D</argument>\n        \
             </arguments>\n    </type>\n    ",
        );
        // An unfoldable default next to a configured argument. Without a row
        // the runtime branch throws `Missing required argument $label`, so the
        // compile must say so.
        write(
            "app/code/Acme/Unres/Model/Configured.php",
            "<?php\nnamespace Acme\\Unres\\Model;\n\nclass Configured\n{\n    \
             public function __construct(\n        \
             private readonly string $label,\n        \
             private readonly string $mystery = ACME_MYSTERY_CONSTANT\n    ) {\n    }\n}\n",
        );
        types.push_str(
            "<type name=\"Acme\\Unres\\Model\\Configured\">\n        <arguments>\n            \
             <argument name=\"label\" xsi:type=\"string\">configured</argument>\n        \
             </arguments>\n    </type>\n    ",
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
            &format!(
                "<?xml version=\"1.0\"?>\n\
                 <config xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n    \
                 <type name=\"Acme\\Unres\\Model\\Overridden\">\n        <arguments>\n            \
                 <argument name=\"mystery\" xsi:type=\"string\">configured</argument>\n        \
                 </arguments>\n    </type>\n    {types}</config>\n"
            ),
        );
        Fixture(root)
    }

    /// Run the compile; returns its stderr.
    fn compile(&self) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["--color", "never", "di", "compile", "--root"])
            .arg(&self.0)
            .output()
            .expect("run magecommand");
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "di compile failed: {stderr}");
        stderr
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

/// Whether the `arguments` section has any entry for `class` (nonLazyTypes
/// lists class names at the same indent, so the section is cut out first).
fn has_key(metadata: &str, class: &str) -> bool {
    let start = metadata.find("  'arguments' => ").expect("an arguments section");
    let section = &metadata[start..];
    let section = &section[..section[1..].find("\n  '").map_or(section.len(), |i| i + 1)];
    section.contains(&format!("    '{}' => ", class.replace('\\', "\\\\")))
}

/// No key at all: not `_vn_` (a literal null), not a partial row (shifts the
/// positional arguments), not NULL (a bare `new X()`).
#[test]
fn an_unfoldable_default_leaves_the_class_without_a_key() {
    let fx = Fixture::new("unfoldable");
    fx.compile();
    let metadata = fx.global_metadata();

    assert!(
        !has_key(&metadata, "Acme\\Unres\\Model\\Unfoldable"),
        "the class must have NO arguments key, so the factory reflects at runtime — \
         a NULL or partial row builds it wrongly; got:\n{metadata}"
    );
}

/// The built-in parent case: a present key would build `new BuiltinChild()`
/// and throw ArgumentCountError for DateInterval's required argument.
#[test]
fn a_class_extending_an_unmodelled_builtin_has_no_key() {
    let fx = Fixture::new("builtin");
    fx.compile();
    let metadata = fx.global_metadata();

    assert!(
        !has_key(&metadata, "Acme\\Unres\\Model\\BuiltinChild"),
        "a constructor inherited from an unmodelled built-in must leave no key; got:\n{metadata}"
    );
}

/// An unreadable parent constructor leaves no choice but runtime reflection,
/// which drops the class's di.xml arguments, so the compile names it rather
/// than losing the configuration silently.
#[test]
fn a_configured_class_with_an_unreadable_constructor_is_reported() {
    let fx = Fixture::new("configured-builtin");
    let stderr = fx.compile();

    assert!(!has_key(&fx.global_metadata(), "Acme\\Unres\\Model\\ConfiguredBuiltin"));
    assert!(
        stderr.contains("lose their di.xml arguments")
            && stderr.contains("Acme\\Unres\\Model\\ConfiguredBuiltin  (its constructor could not be read)"),
        "the lost di.xml configuration must be reported by name and reason; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("Acme\\Unres\\Model\\Unfoldable  ("),
        "a class with no di.xml arguments loses nothing and must not be listed; stderr:\n{stderr}"
    );
}

/// An unfoldable default with di.xml arguments takes the same fallback: no
/// key, so Magento reflects the class and ignores di.xml. The warning names
/// the default, which is what to fix to get the compiled row back.
#[test]
fn a_configured_class_with_an_unfoldable_default_is_reported() {
    let fx = Fixture::new("configured");
    let stderr = fx.compile();

    assert!(!has_key(&fx.global_metadata(), "Acme\\Unres\\Model\\Configured"));
    assert!(
        stderr.contains(
            "Acme\\Unres\\Model\\Configured  (cannot evaluate the default `$mystery = ACME_MYSTERY_CONSTANT`)"
        ),
        "the warning must name the class and the default; stderr:\n{stderr}"
    );
}

/// The omission is scoped to the class that needs it — one unreadable default
/// must not drop every row in the file.
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
/// so there is nothing to omit — omitting anyway would needlessly uncache
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
