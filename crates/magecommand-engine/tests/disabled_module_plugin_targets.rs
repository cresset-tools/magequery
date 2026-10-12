//! A plugin target that lives in a DISABLED module still inherits the plugins
//! declared on its ancestors. `setup:di:compile` asks `Relations` for the
//! target's parents through the autoloader, which does not care whether the
//! owning module is enabled; magecommand's class universe holds only the
//! enabled modules' scan, so the target needs seeding to be reflected at all.
//!
//! Found on a store where an enabled module plugs a router of a disabled
//! Amasty module: the compiled plugin lists kept the enabled module's plugin
//! but lost the Csp plugin the router inherits from `RouterInterface`.

use std::path::Path;

use magecommand_engine::build::compute_outputs;
use magecommand_engine::definitions::Definitions;
use magequery_core::Magento;

fn put(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).expect("mkdir");
    std::fs::write(&p, body).expect("write");
}

fn module(root: &Path, name: &str) {
    let dir = name.replace('_', "/");
    put(
        root,
        &format!("app/code/{dir}/etc/module.xml"),
        &format!(r#"<?xml version="1.0"?><config><module name="{name}"/></config>"#),
    );
    put(
        root,
        &format!("app/code/{dir}/registration.php"),
        &format!(
            "<?php\n\\Magento\\Framework\\Component\\ComponentRegistrar::register(\\Magento\\Framework\\Component\\ComponentRegistrar::MODULE, '{name}', __DIR__);\n"
        ),
    );
}

fn after_match_plugin(root: &Path, rel: &str, ns: &str, class: &str) {
    put(
        root,
        rel,
        &format!("<?php\nnamespace {ns};\n\nclass {class}\n{{\n    public function afterMatch($subject, $result)\n    {{\n        return $result;\n    }}\n}}\n"),
    );
}

/// The entry text of one type in the `_inherited` section of a plugin list.
fn inherited_entry<'a>(file: &'a str, ty: &str) -> &'a str {
    let section = &file[file.find("  1 => ").expect("_inherited section")..];
    let key = format!("    '{}' => ", ty.replace('\\', "\\\\"));
    let start = section.find(&key).unwrap_or_else(|| panic!("no entry for {ty}"));
    let rest = &section[start + key.len()..];
    let end = rest.find("\n    '").unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn plugged_class_of_a_disabled_module_inherits_its_interface_plugins() {
    let dir = std::env::temp_dir().join(format!("mc-disabled-target-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.as_path();

    put(
        root,
        "app/etc/config.php",
        "<?php\nreturn ['modules' => ['Acme_Base' => 1, 'Acme_Gone' => 0, 'Acme_Rewrite' => 1]];\n",
    );
    module(root, "Acme_Base");
    module(root, "Acme_Gone");
    module(root, "Acme_Rewrite");

    put(
        root,
        "app/code/Acme/Base/Api/RouterInterface.php",
        "<?php\nnamespace Acme\\Base\\Api;\n\ninterface RouterInterface\n{\n    public function match($request);\n}\n",
    );
    after_match_plugin(root, "app/code/Acme/Base/Plugin/Csp.php", "Acme\\Base\\Plugin", "Csp");
    put(
        root,
        "app/code/Acme/Base/etc/di.xml",
        r#"<?xml version="1.0"?><config>
    <type name="Acme\Base\Api\RouterInterface">
        <plugin name="csp_aware" type="Acme\Base\Plugin\Csp"/>
    </type>
</config>"#,
    );

    // The disabled module: its class is autoloadable, its config is not read.
    put(
        root,
        "app/code/Acme/Gone/Controller/Router.php",
        "<?php\nnamespace Acme\\Gone\\Controller;\n\nclass Router implements \\Acme\\Base\\Api\\RouterInterface\n{\n    public function match($request)\n    {\n        return null;\n    }\n}\n",
    );

    after_match_plugin(root, "app/code/Acme/Rewrite/Plugin/Router.php", "Acme\\Rewrite\\Plugin", "Router");
    put(
        root,
        "app/code/Acme/Rewrite/etc/di.xml",
        r#"<?xml version="1.0"?><config>
    <type name="Acme\Gone\Controller\Router">
        <plugin name="rewrite" type="Acme\Rewrite\Plugin\Router" sortOrder="10"/>
    </type>
</config>"#,
    );

    let magento = Magento::open(root).expect("open synthetic root");
    let mut defs = Definitions::scan(&magento, root, &root.join("generated/code"));
    let files = compute_outputs(&magento, &mut defs, root).files;
    let global = files
        .iter()
        .find(|(n, _)| n.ends_with("global|primary|plugin-list.php"))
        .map(|(_, b)| b.as_str())
        .expect("global plugin list");

    let entry = inherited_entry(global, "Acme\\Gone\\Controller\\Router");
    assert!(entry.contains("'rewrite'"), "own plugin kept:\n{entry}");
    assert!(entry.contains("'csp_aware'"), "interface plugin inherited:\n{entry}");

    let _ = std::fs::remove_dir_all(&dir);
}
