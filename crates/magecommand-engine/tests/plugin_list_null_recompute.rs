//! `PluginListGenerator::inheritPlugins` caches with `!isset($inherited[$type])`
//! and stores a type without plugins as `null` — which `isset` reports as
//! unset. A plugin-less type is therefore computed again each time it is
//! reached, and a later pass picks up an ancestor's DISABLED plugin once that
//! ancestor is cached (the cache-hit path returns the stored list, disabled
//! entries included).
//!
//! The shape that exposed it on a real store: an area disables a plugin
//! declared on a base class (`currentPageDetection` on `Data\Collection`,
//! disabled for webapi), and an abstract class in between with no plugins of
//! its own (`Order\Collection\AbstractCollection`) is first reached while the
//! base is computed for the first time. Magento's plugin list carries the
//! disabled plugin on the middle class; magecommand wrote NULL.

use std::path::Path;

use magecommand_engine::build::compute_outputs;
use magecommand_engine::definitions::Definitions;
use magequery_core::Magento;

fn put(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).expect("mkdir");
    std::fs::write(&p, body).expect("write");
}

fn class(root: &Path, name: &str, extends: Option<&str>, body: &str) {
    let ext = extends.map(|e| format!(" extends {e}")).unwrap_or_default();
    put(
        root,
        &format!("app/code/Acme/Chain/Model/{name}.php"),
        &format!("<?php\nnamespace Acme\\Chain\\Model;\n\nclass {name}{ext}\n{{\n{body}}}\n"),
    );
}

fn plugin(root: &Path, name: &str) {
    put(
        root,
        &format!("app/code/Acme/Chain/Plugin/{name}.php"),
        &format!("<?php\nnamespace Acme\\Chain\\Plugin;\n\nclass {name}\n{{\n    public function afterRun($subject, $result)\n    {{\n        return $result;\n    }}\n}}\n"),
    );
}

/// The entry text of one type in the `_inherited` section of a plugin list.
fn inherited_entry<'a>(file: &'a str, ty: &str) -> &'a str {
    let section = &file[file.find("  1 => ").expect("_inherited section")..];
    let section = &section[..section.find("\n  2 => ").unwrap_or(section.len())];
    let key = format!("    '{}' => ", ty.replace('\\', "\\\\"));
    let start = section.find(&key).unwrap_or_else(|| panic!("no entry for {ty}"));
    let rest = &section[start + key.len()..];
    let end = rest.find("\n    '").unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn plugin_less_middle_class_picks_up_the_area_disabled_base_plugin() {
    let dir = std::env::temp_dir().join(format!("mc-null-recompute-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.as_path();

    put(root, "app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Chain' => 1]];\n");
    put(
        root,
        "app/code/Acme/Chain/etc/module.xml",
        r#"<?xml version="1.0"?><config><module name="Acme_Chain"/></config>"#,
    );
    put(
        root,
        "app/code/Acme/Chain/registration.php",
        "<?php\n\\Magento\\Framework\\Component\\ComponentRegistrar::register(\\Magento\\Framework\\Component\\ComponentRegistrar::MODULE, 'Acme_Chain', __DIR__);\n",
    );
    class(root, "Base", None, "    public function run()\n    {\n        return 1;\n    }\n");
    class(root, "Mid", Some("Base"), "");
    class(root, "ALeaf", Some("Mid"), "");
    class(root, "ZLeaf", Some("Mid"), "");
    for p in ["LeafA", "BaseP", "LeafZ"] {
        plugin(root, p);
    }
    // ALeaf is seeded before Base (declaration and name order agree), so Mid is
    // first reached during Base's first computation; ZLeaf reaches it again.
    put(
        root,
        "app/code/Acme/Chain/etc/di.xml",
        r#"<?xml version="1.0"?><config>
    <type name="Acme\Chain\Model\ALeaf">
        <plugin name="a" type="Acme\Chain\Plugin\LeafA"/>
    </type>
    <type name="Acme\Chain\Model\Base">
        <plugin name="p" type="Acme\Chain\Plugin\BaseP"/>
    </type>
    <type name="Acme\Chain\Model\ZLeaf">
        <plugin name="z" type="Acme\Chain\Plugin\LeafZ"/>
    </type>
</config>"#,
    );
    put(
        root,
        "app/code/Acme/Chain/etc/webapi_rest/di.xml",
        r#"<?xml version="1.0"?><config>
    <type name="Acme\Chain\Model\Base">
        <plugin name="p" disabled="true"/>
    </type>
</config>"#,
    );

    let magento = Magento::open(root).expect("open synthetic root");
    let mut defs = Definitions::scan(&magento, root, &root.join("generated/code"));
    let files = compute_outputs(&magento, &mut defs, root).files;
    let webapi = files
        .iter()
        .find(|(n, _)| n.ends_with("/global|primary|webapi_rest|plugin-list.php"))
        .map(|(_, b)| b.as_str())
        .expect("webapi_rest plugin list");

    let mid = inherited_entry(webapi, "Acme\\Chain\\Model\\Mid");
    assert!(!mid.trim_start().starts_with("NULL"), "Mid recomputed, not left NULL:\n{mid}");
    assert!(mid.contains("'p'") && mid.contains("'disabled' => true"), "Mid carries the disabled base plugin:\n{mid}");

    let _ = std::fs::remove_dir_all(&dir);
}
