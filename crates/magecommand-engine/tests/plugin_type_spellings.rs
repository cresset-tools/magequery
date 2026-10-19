//! A plugin declared under both spellings of its type — `<type name="\X">`
//! and `<type name="X">` — within ONE scope read.
//!
//! The two spellings are distinct DOM nodes, so the XML merge never folds
//! them together: the ObjectManager mapper (`Config/Mapper/Dom.php`) maps each
//! node on its own, keyed by the raw name, and `PluginListGenerator::merge`
//! ltrims the name and folds the entries with `array_replace_recursive`, in
//! node order. Three consequences, each of which an attribute-level merge
//! across spellings gets wrong:
//!
//!   - field ORDER: the fold keeps existing keys and appends new ones, so a
//!     `type=` under one spelling and a `disabled=` under the other compile to
//!     `[sortOrder, instance, disabled]`, not the mapper's own
//!     `[sortOrder, disabled, instance]`;
//!   - `sortOrder` RESET: the mapper writes `sortOrder` into every entry
//!     (default 0), so the later spelling's omission overwrites an earlier 30;
//!   - NODE order, not file order: a later file adding to the spelling whose
//!     node came FIRST is folded first, and loses to the other spelling.
//!
//! The first is what a real store hit (a vendor module declaring its plugin on
//! `\X`, a project module disabling it on `X`). Ground truth for all three,
//! from a real `setup:di:compile` of exactly this pair of modules spliced into
//! a 2.4.9 install:
//!
//! ```text
//! 'p_order' => ['sortOrder' => 0, 'instance' => '…\Early', 'disabled' => true],
//! 'p_reset' => ['sortOrder' => 0, 'instance' => '…\Early', 'disabled' => false],
//! 'p_value' => ['sortOrder' => 5, 'instance' => '…\Early'],
//! ```

use std::path::Path;

use magecommand_engine::build::compute_outputs;
use magecommand_engine::definitions::Definitions;
use magequery_core::Magento;

fn put(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).expect("mkdir");
    std::fs::write(&p, body).expect("write");
}

fn module(root: &Path, name: &str, sequence: Option<&str>, di: &str) {
    let dir = format!("app/code/Acme/{name}");
    let seq = sequence
        .map(|s| format!("<sequence><module name=\"Acme_{s}\"/></sequence>"))
        .unwrap_or_default();
    put(
        root,
        &format!("{dir}/etc/module.xml"),
        &format!("<?xml version=\"1.0\"?>\n<config xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:noNamespaceSchemaLocation=\"urn:magento:framework:Module/etc/module.xsd\">\n    <module name=\"Acme_{name}\">{seq}</module>\n</config>\n"),
    );
    put(
        root,
        &format!("{dir}/registration.php"),
        &format!("<?php\n\\Magento\\Framework\\Component\\ComponentRegistrar::register(\\Magento\\Framework\\Component\\ComponentRegistrar::MODULE, 'Acme_{name}', __DIR__);\n"),
    );
    put(
        root,
        &format!("{dir}/etc/di.xml"),
        &format!("<?xml version=\"1.0\"?>\n<config xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:noNamespaceSchemaLocation=\"urn:magento:framework:ObjectManager/etc/config.xsd\">\n{di}</config>\n"),
    );
}

#[test]
fn the_two_spellings_of_a_type_fold_in_node_order() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    put(
        root,
        "app/etc/config.php",
        "<?php\nreturn ['modules' => ['Acme_Probe' => 1, 'Acme_Probe2' => 1]];\n",
    );
    module(
        root,
        "Probe",
        None,
        r#"    <type name="\Acme\Probe\Model\Pair">
        <plugin name="p_order" type="Acme\Probe\Plugin\Early"/>
        <plugin name="p_reset" type="Acme\Probe\Plugin\Early" sortOrder="30"/>
    </type>
    <type name="Acme\Probe\Model\Pair">
        <plugin name="p_order" disabled="true"/>
        <plugin name="p_reset" disabled="false"/>
        <plugin name="p_value" type="Acme\Probe\Plugin\Early" sortOrder="5"/>
    </type>
"#,
    );
    module(
        root,
        "Probe2",
        Some("Probe"),
        r#"    <type name="\Acme\Probe\Model\Pair">
        <plugin name="p_value" type="Acme\Probe\Plugin\Middle"/>
    </type>
"#,
    );
    put(
        root,
        "app/code/Acme/Probe/Model/Pair.php",
        "<?php\nnamespace Acme\\Probe\\Model;\n\nclass Pair\n{\n    public function act(int $n): int\n    {\n        return $n;\n    }\n}\n",
    );
    for cls in ["Early", "Middle"] {
        put(
            root,
            &format!("app/code/Acme/Probe/Plugin/{cls}.php"),
            &format!("<?php\nnamespace Acme\\Probe\\Plugin;\n\nclass {cls}\n{{\n    public function afterAct($subject, int $result): int\n    {{\n        return $result;\n    }}\n}}\n"),
        );
    }

    let magento = Magento::open(root).expect("open synthetic root");
    let mut defs = Definitions::scan(&magento, root, &root.join("generated/code"));
    let files = compute_outputs(&magento, &mut defs, root).files;
    let list = files
        .iter()
        .find(|(n, _)| n == "metadata/global|primary|plugin-list.php")
        .map(|(_, b)| b.as_str())
        .expect("global plugin list");
    let data = &list[..list.find("\n  1 => ").expect("_inherited section")];

    let p_order = "      'p_order' => \n      array (\n        'sortOrder' => 0,\n        'instance' => 'Acme\\\\Probe\\\\Plugin\\\\Early',\n        'disabled' => true,\n      ),";
    let p_reset = "      'p_reset' => \n      array (\n        'sortOrder' => 0,\n        'instance' => 'Acme\\\\Probe\\\\Plugin\\\\Early',\n        'disabled' => false,\n      ),";
    let p_value = "      'p_value' => \n      array (\n        'sortOrder' => 5,\n        'instance' => 'Acme\\\\Probe\\\\Plugin\\\\Early',\n      ),";
    for (what, want) in [("field order", p_order), ("sortOrder reset", p_reset), ("node order", p_value)] {
        assert!(data.contains(want), "{what}: expected\n{want}\nin _data:\n{data}");
    }
}
