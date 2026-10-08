//! The two ways the compiled plugin lists read config scopes DIFFERENTLY from
//! the ObjectManager config. Both were divergences from `setup:di:compile`
//! found by differential testing, both are fixed, and both are easy to undo by
//! "simplifying" the merge — which is why they are pinned here.
//!
//! `PluginListGenerator` does not share a reader with the ObjectManager
//! config, and the two disagree about the `primary`/`global` pair in two
//! independent ways:
//!
//!   ORDER  the generator reads module `global` first and `primary` second, so
//!          `app/etc/di.xml` wins the pair. The ObjectManager config merges
//!          `primary` first, so a module overrides it. (`write()` moves the
//!          scope being compiled to the END of `scopePriorityScheme` and is
//!          called with `primary`, not `global`.)
//!
//!   FILES  the generator's primary glob is `{di.xml,*/di.xml}` — the exact
//!          filename — against the ObjectManager config's
//!          `{*di.xml,*/*di.xml}`. So `app/etc/zz_di.xml` contributes
//!          preferences and virtualTypes but no plugins.
//!
//! Neither is reachable on a stock install — nothing declares one plugin in
//! both `app/etc/di.xml` and a module's `etc/di.xml`, and there is no second
//! `app/etc/*di.xml` — which is why the whole-tree oracle comparison was clean
//! at 4108 generated classes and 16 metadata files while both were wrong. The
//! ground truth below came from spliced probes against a real install, and
//! these tests are what keeps that evidence from evaporating into prose.

use std::path::Path;

use magecommand_engine::build::compute_outputs;
use magecommand_engine::definitions::Definitions;
use magequery_core::Magento;

/// Write one file, creating parents.
fn put(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).expect("mkdir");
    std::fs::write(&p, body).expect("write");
}

/// A minimal module: one plugged class and four plugin classes.
fn scaffold(root: &Path) {
    put(root, "app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Scope' => 1]];\n");
    put(
        root,
        "app/code/Acme/Scope/etc/module.xml",
        r#"<?xml version="1.0"?>
<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:noNamespaceSchemaLocation="urn:magento:framework:Module/etc/module.xsd">
    <module name="Acme_Scope"/>
</config>
"#,
    );
    put(
        root,
        "app/code/Acme/Scope/registration.php",
        "<?php\n\n\\Magento\\Framework\\Component\\ComponentRegistrar::register(\n    \\Magento\\Framework\\Component\\ComponentRegistrar::MODULE,\n    'Acme_Scope',\n    __DIR__\n);\n",
    );
    put(
        root,
        "app/code/Acme/Scope/Model/Pair.php",
        "<?php\n\nnamespace Acme\\Scope\\Model;\n\nclass Pair\n{\n    public function act(int $n): int\n    {\n        return $n;\n    }\n}\n",
    );
    for cls in ["Early", "Late", "Replaced", "Middle", "ZzOnly"] {
        put(
            root,
            &format!("app/code/Acme/Scope/Plugin/{cls}.php"),
            &format!("<?php\n\nnamespace Acme\\Scope\\Plugin;\n\nclass {cls}\n{{\n    public function afterAct($subject, int $result): int\n    {{\n        return $result;\n    }}\n}}\n"),
        );
    }
}

/// Compile a tree and return its emitted files by name.
fn compile(root: &Path) -> Vec<(String, String)> {
    let magento = Magento::open(root).expect("open synthetic root");
    let generated_code = root.join("generated/code");
    let mut defs = Definitions::scan(&magento, root, &generated_code);
    compute_outputs(&magento, &mut defs, root).files
}

fn file_of<'a>(files: &'a [(String, String)], name: &str) -> &'a str {
    files
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, b)| b.as_str())
        .unwrap_or_else(|| {
            panic!(
                "no emitted file {name}; emitted: {:?}",
                files.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()
            )
        })
}

/// Scope ORDER: the plugin lists read `primary` AFTER module `global`, so
/// `app/etc/di.xml` wins the pair.
///
/// Ground truth, from a real compile with the primary `<type>` spliced into
/// `app/etc/di.xml` (a second `app/etc/*di.xml` would not have been read at
/// all — see the next test):
///
/// ```text
/// primary   scope_reset type=Early    sortOrder=70
///           scope_kept  type=Late     sortOrder=80
/// global    scope_reset type=Replaced (no sortOrder)
///           scope_middle type=Middle  sortOrder=10
///
/// MAGENTO  _data: scope_reset(70,Early) scope_middle(10,Middle) scope_kept(80,Late)
/// ```
///
/// Three things have to come out right, and they fail independently:
///
///   - the `instance` is `app/etc`'s `Early`, not the module's `Replaced`;
///   - the `sortOrder` is 70. Note this one passed even BEFORE the order was
///     fixed, for the wrong reason: merging primary first left 70 in place as
///     an inherited attribute rather than as the winning scope's value. So it
///     is not evidence on its own;
///   - `_data` key order is `array_replace_recursive(global, primary)` —
///     global's keys in global's order, then primary-only keys appended. That
///     falls out of recording each plugin's first appearance in the READ
///     order, which is why the merge and the emitter's `band()` have to agree
///     on what that order is.
///
/// This is specific to plugin lists. The ObjectManager config merges the same
/// two scopes the OTHER way, module-over-primary, which is already pinned by
/// the `OperationPool` behaviour noted in `engine/di.rs` — so a fix must not
/// simply reverse the merge order for everything.
#[test]
fn primary_scope_wins_over_module_global_in_plugin_lists() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    scaffold(root);
    put(
        root,
        "app/etc/di.xml",
        r#"<?xml version="1.0"?>
<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:noNamespaceSchemaLocation="urn:magento:framework:ObjectManager/etc/config.xsd">
    <type name="Acme\Scope\Model\Pair">
        <plugin name="scope_reset" type="Acme\Scope\Plugin\Early" sortOrder="70"/>
        <plugin name="scope_kept" type="Acme\Scope\Plugin\Late" sortOrder="80"/>
    </type>
</config>
"#,
    );
    put(
        root,
        "app/code/Acme/Scope/etc/di.xml",
        r#"<?xml version="1.0"?>
<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:noNamespaceSchemaLocation="urn:magento:framework:ObjectManager/etc/config.xsd">
    <type name="Acme\Scope\Model\Pair">
        <plugin name="scope_reset" type="Acme\Scope\Plugin\Replaced"/>
        <plugin name="scope_middle" type="Acme\Scope\Plugin\Middle" sortOrder="10"/>
    </type>
</config>
"#,
    );

    let files = compile(root);
    let list = file_of(&files, "metadata/global|primary|plugin-list.php");

    let entry = entry_of(list, "scope_reset");
    assert!(
        entry.contains("'sortOrder' => 70"),
        "primary's sortOrder 70 must survive the module's re-declaration, got:\n{entry}"
    );
    assert!(
        entry.contains("Acme\\\\Scope\\\\Plugin\\\\Early"),
        "primary's type must survive too, got:\n{entry}"
    );
    // Execution order follows from the values: 10, 70, 80.
    assert_eq!(
        processed_chain(list, "Acme\\\\Scope\\\\Model\\\\Pair_act___self"),
        ["scope_middle", "scope_reset", "scope_kept"]
    );
    // `_data` keeps the fold's key order: the global scope's order, with
    // primary-only keys appended.
    assert_eq!(
        data_key_order(list),
        ["scope_reset", "scope_middle", "scope_kept"],
        "_data must carry array_replace_recursive(global, primary) key order"
    );
}

/// Scope FILES: the plugin lists read only `app/etc/di.xml`, not `*di.xml`.
/// Two different resolvers, and only one of them applies to plugins:
///
/// ```text
/// App\Arguments\FileResolver\Primary::get   {*di.xml,*/*di.xml}
/// App\Config\FileResolver::get 'primary'    {di.xml,*/di.xml}
/// ```
///
/// `PluginListGenerator`'s reader is `ObjectManager\Config\Reader\Dom`, which
/// uses the latter — so a file like `app/etc/zz_extra_di.xml` contributes
/// nothing to any plugin list. Probed on a real install: a plugin in
/// `app/etc/di.xml` lands in the compiled list (3 occurrences), the same
/// plugin in `app/etc/zz_probe_di.xml` appears 0 times.
///
/// The wide glob is RIGHT for the ObjectManager config, which is the other
/// half of this test: a `<preference>` and a `<virtualType>` in the same
/// `zz_` file DO reach `global.php` (verified on the same install). So this is
/// a second, narrower file list for the plugin map — narrowing
/// `primary_di_files` itself would break the half that works, and this test
/// fails if someone does.
#[test]
fn a_second_primary_di_file_feeds_the_object_manager_config_but_not_plugin_lists() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    scaffold(root);
    put(
        root,
        "app/etc/di.xml",
        r#"<?xml version="1.0"?>
<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:noNamespaceSchemaLocation="urn:magento:framework:ObjectManager/etc/config.xsd">
    <type name="Acme\Scope\Model\Pair">
        <plugin name="in_main_di" type="Acme\Scope\Plugin\Early" sortOrder="91"/>
    </type>
</config>
"#,
    );
    // Matched by `{*di.xml,*/*di.xml}` but NOT by `{di.xml,*/di.xml}`.
    put(
        root,
        "app/etc/zz_extra_di.xml",
        r#"<?xml version="1.0"?>
<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:noNamespaceSchemaLocation="urn:magento:framework:ObjectManager/etc/config.xsd">
    <preference for="Acme\Scope\Api\ProbeInterface" type="Acme\Scope\Model\Pair"/>
    <type name="Acme\Scope\Model\Pair">
        <plugin name="in_zz_di" type="Acme\Scope\Plugin\ZzOnly" sortOrder="92"/>
    </type>
</config>
"#,
    );

    let files = compile(root);

    // The half that is already correct: the ObjectManager config DOES read it.
    let global = file_of(&files, "metadata/global.php");
    assert!(
        global.contains("ProbeInterface"),
        "a preference in app/etc/zz_extra_di.xml belongs in global.php — \
         `App\\Arguments\\FileResolver\\Primary` globs `*di.xml`"
    );

    // The half that diverges: no plugin list may see it.
    let list = file_of(&files, "metadata/global|primary|plugin-list.php");
    assert!(
        list.contains("in_main_di"),
        "app/etc/di.xml's plugin must be in the list"
    );
    assert!(
        !list.contains("in_zz_di"),
        "app/etc/zz_extra_di.xml's plugin must NOT be in any plugin list — \
         the generator's reader globs `{{di.xml,*/di.xml}}`. Full list:\n{list}"
    );
}

/// The `'name' => array ( ... )` block for one plugin, for assertion messages
/// that show the whole entry rather than a bare `false`.
fn entry_of<'a>(list: &'a str, name: &str) -> &'a str {
    let key = format!("'{name}' => ");
    let start = list
        .find(&key)
        .unwrap_or_else(|| panic!("no entry {name} in:\n{list}"));
    let rest = &list[start..];
    let end = rest.find("),").map(|i| i + 2).unwrap_or(rest.len());
    &rest[..end]
}

/// Plugin keys of the `_data` section (index 0), in emitted order. Plugin
/// names sit one indent level deeper than the type they hang off.
fn data_key_order(list: &str) -> Vec<String> {
    let chunk = list.split("\n  1 => ").next().expect("a _data section");
    chunk
        .lines()
        .filter_map(|l| {
            let body = l.strip_prefix("      '")?;
            body.strip_suffix("' => ").map(str::to_owned)
        })
        .collect()
}

/// The `_processed` chain for one `<class>_<method>___self` key, in order.
fn processed_chain(list: &str, key: &str) -> Vec<String> {
    let start = list
        .find(key)
        .unwrap_or_else(|| panic!("no processed key {key} in:\n{list}"));
    let rest = &list[start..];
    let end = rest.find("),\n    ),").map(|i| i + 2).unwrap_or(rest.len());
    rest[..end]
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            t.strip_prefix("0 => '")
                .or_else(|| t.strip_prefix("1 => '"))
                .or_else(|| t.strip_prefix("2 => '"))
                .and_then(|s| s.strip_suffix("',"))
                .map(str::to_owned)
        })
        .collect()
}

