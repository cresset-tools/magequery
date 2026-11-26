//! What `dev upgrade` writes into an emptied `generated/code`: the classes a
//! request's autoloader would generate, rendered like PHP's generators. Two
//! things an autoloader-driven load must do that a full scan gets for free,
//! both found comparing against Mage-OS 4.0.0's own on-demand output:
//!
//! - a parameter default naming another class's constant renders as the
//!   constant's VALUE (`EntityAbstract` reads `getDefaultValue()`), so the
//!   defining class has to be loaded even though nothing else needs it;
//! - a generated class whose source is generated too (`FooFactory\Proxy`)
//!   renders once its source has been generated and folded in.

use std::path::Path;

use magecommand_engine::interception::runtime_definitions_generating;
use magequery_core::Magento;

fn put(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).expect("mkdir");
    std::fs::write(&p, body).expect("write");
}

#[test]
fn generates_what_the_autoloader_would() {
    let dir = std::env::temp_dir().join(format!("mc-ondemand-codegen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.as_path();

    put(root, "app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Base' => 1]];\n");
    put(root, "app/code/Acme/Base/etc/module.xml", r#"<?xml version="1.0"?><config><module name="Acme_Base"/></config>"#);
    put(
        root,
        "app/code/Acme/Base/registration.php",
        "<?php\n\\Magento\\Framework\\Component\\ComponentRegistrar::register(\\Magento\\Framework\\Component\\ComponentRegistrar::MODULE, 'Acme_Base', __DIR__);\n",
    );
    put(
        root,
        "app/code/Acme/Base/etc/di.xml",
        r#"<?xml version="1.0"?><config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
            <type name="Acme\Base\Model\Consumer"><arguments>
                <argument name="thing" xsi:type="object">Acme\Base\Model\Thing\Proxy</argument>
                <argument name="factory" xsi:type="object">Acme\Base\Model\ThingFactory\Proxy</argument>
            </arguments></type>
        </config>"#,
    );
    put(root, "app/code/Acme/Base/Model/Modes.php", "<?php\nnamespace Acme\\Base\\Model;\n\nclass Modes\n{\n    const ALL = 'all';\n}\n");
    put(
        root,
        "app/code/Acme/Base/Model/Thing.php",
        "<?php\nnamespace Acme\\Base\\Model;\n\nclass Thing\n{\n    public function clean($mode = Modes::ALL)\n    {\n    }\n}\n",
    );
    put(root, "app/code/Acme/Base/Model/Consumer.php", "<?php\nnamespace Acme\\Base\\Model;\n\nclass Consumer\n{\n}\n");

    let magento = Magento::open(root).expect("open");
    // What generated/code held before it was emptied (argument objects are not
    // config types: they materialize when instantiated).
    let previous = vec!["Acme\\Base\\Model\\Thing\\Proxy".to_owned(), "Acme\\Base\\Model\\ThingFactory\\Proxy".to_owned()];
    let (_, _, files) = runtime_definitions_generating(&magento, root, previous).expect("generate");
    let file = |rel: &str| files.iter().find(|(p, _)| p == rel).map(|(_, b)| b.as_str());

    let proxy = file("Acme/Base/Model/Thing/Proxy.php").expect("Thing\\Proxy generated");
    assert!(proxy.contains("public function clean($mode = 'all')"), "default rendered as its value:\n{proxy}");
    assert!(file("Acme/Base/Model/ThingFactory.php").is_some(), "the factory a proxy needs is generated");
    let factory_proxy = file("Acme/Base/Model/ThingFactory/Proxy.php").expect("ThingFactory\\Proxy generated");
    assert!(factory_proxy.contains("class Proxy extends \\Acme\\Base\\Model\\ThingFactory"), "{factory_proxy}");

    let _ = std::fs::remove_dir_all(&dir);
}
