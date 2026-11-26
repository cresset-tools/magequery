//! The developer-mode `interception` cache entry: what
//! `Interception\Config\Config::initializeUncompiled` saves when the cache is
//! cold. Unlike the compiled map it resolves only the types the merged config
//! names (runtime definitions list no classes), and it does so through the
//! autoloader: classes of disabled modules are found, and generatable names
//! materialize — a `\Proxy` of a plugged class inherits its plugins even when
//! `generated/` is empty.
//!
//! Verified against Mage-OS 4.0.0's own entry (3301 keys, 711 true) with
//! `generated/` filled and emptied; this fixture pins each behaviour that
//! comparison needed.

use std::path::Path;

use magecommand_engine::interception::{runtime_definitions, runtime_interception_map};
use magequery_core::Magento;

fn put(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).expect("mkdir");
    std::fs::write(&p, body).expect("write");
}

fn module(root: &Path, name: &str, di: Option<&str>) {
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
    if let Some(di) = di {
        put(
            root,
            &format!("app/code/{dir}/etc/di.xml"),
            &format!(
                r#"<?xml version="1.0"?><config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">{di}</config>"#
            ),
        );
    }
}

#[test]
fn runtime_map_resolves_config_types_through_the_autoloader() {
    let dir = std::env::temp_dir().join(format!("mc-runtime-interception-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.as_path();

    put(
        root,
        "app/etc/config.php",
        "<?php\nreturn ['modules' => ['Acme_Base' => 1, 'Acme_Gone' => 0, 'Acme_Use' => 1]];\n",
    );
    module(
        root,
        "Acme_Base",
        Some(
            r#"<type name="Acme\Base\Api\ServiceInterface"><plugins><plugin name="p" type="Acme\Base\Plugin\P"/></plugins></type>
               <type name="Acme\Base\Model\Plugged"><plugins><plugin name="q" type="Acme\Base\Plugin\P"/></plugins></type>"#,
        ),
    );
    put(root, "app/code/Acme/Base/Api/ServiceInterface.php", "<?php\nnamespace Acme\\Base\\Api;\n\ninterface ServiceInterface\n{\n    public function run();\n}\n");
    put(root, "app/code/Acme/Base/Plugin/P.php", "<?php\nnamespace Acme\\Base\\Plugin;\n\nclass P\n{\n    public function afterRun($s, $r)\n    {\n        return $r;\n    }\n}\n");
    put(root, "app/code/Acme/Base/Model/Plugged.php", "<?php\nnamespace Acme\\Base\\Model;\n\nclass Plugged\n{\n    public function run()\n    {\n    }\n}\n");
    put(root, "app/code/Acme/Base/Model/Service.php", "<?php\nnamespace Acme\\Base\\Model;\n\nclass Service implements \\Acme\\Base\\Api\\ServiceInterface\n{\n    public function run()\n    {\n    }\n}\n");
    put(root, "app/code/Acme/Base/Model/NotInConfig.php", "<?php\nnamespace Acme\\Base\\Model;\n\nclass NotInConfig implements \\Acme\\Base\\Api\\ServiceInterface\n{\n    public function run()\n    {\n    }\n}\n");

    // A disabled module's class still autoloads (vendor PSR-4 / app/code).
    module(root, "Acme_Gone", None);
    put(root, "app/code/Acme/Gone/Model/Provider.php", "<?php\nnamespace Acme\\Gone\\Model;\n\nclass Provider implements \\Acme\\Base\\Api\\ServiceInterface\n{\n    public function run()\n    {\n    }\n}\n");

    module(
        root,
        "Acme_Use",
        Some(
            r#"<virtualType name="Acme\Use\Model\GoneProvider" type="Acme\Gone\Model\Provider"/>
               <type name="Acme\Base\Model\Service" shared="false"/>
               <virtualType name="Acme\Use\Model\LazyPlugged" type="Acme\Base\Model\Plugged\Proxy"/>
               <type name="Acme\Use\Model\Consumer"><arguments><argument name="plugged" xsi:type="object">Acme\Base\Model\Plugged\Proxy</argument></arguments></type>"#,
        ),
    );
    put(root, "app/code/Acme/Use/Model/Consumer.php", "<?php\nnamespace Acme\\Use\\Model;\n\nclass Consumer\n{\n    public function __construct(\\Acme\\Base\\Model\\Plugged $plugged)\n    {\n    }\n}\n");

    let magento = Magento::open(root).expect("open");
    let defs = runtime_definitions(&magento, root);
    let map = runtime_interception_map(&magento, &defs);
    let get = |k: &str| map.get(k).copied();

    assert_eq!(get("Acme\\Base\\Api\\ServiceInterface"), Some(true), "declared plugins");
    assert_eq!(get("Acme\\Base\\Model\\Service"), Some(true), "inherits from its interface");
    // Virtual type -> disabled module's class -> plugged interface.
    assert_eq!(get("Acme\\Use\\Model\\GoneProvider"), Some(true), "virtual type over a disabled module's class");
    assert_eq!(get("Acme\\Gone\\Model\\Provider"), Some(true), "disabled module's class is walked");
    assert_eq!(get("Acme\\Use\\Model\\Consumer"), Some(false), "config type without plugins");
    // Runtime definitions list no classes: a scanned class the config never
    // names gets no entry (it is resolved lazily on first use).
    assert_eq!(get("Acme\\Base\\Model\\NotInConfig"), None, "no class sweep at runtime");
    // No generated/ at all: the proxy a virtual type is based on is generated
    // on demand, and extends the plugged class.
    assert!(!root.join("generated/code/Acme").exists());
    assert_eq!(get("Acme\\Base\\Model\\Plugged\\Proxy"), Some(true), "generated proxy inherits");
    assert_eq!(get("Acme\\Use\\Model\\LazyPlugged"), Some(true), "virtual type over a generated proxy");

    let _ = std::fs::remove_dir_all(&dir);
}
