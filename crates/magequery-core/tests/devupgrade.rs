//! The fast development `setup:upgrade` (`devupgrade`, `indexerhash`): config.php as
//! `Installer::createModulesConfig` writes it, and indexer config hashes pinned against
//! Magento's own `Config\Dom` + `Indexer\Config\Converter` (see
//! `fixtures/indexer-hash/regenerate.php`), and the global DI config the same way
//! (`fixtures/di-config/regenerate.php`).
#![cfg(feature = "db")]

use std::path::Path;

use magequery_core::Magento;
use magequery_core::devupgrade::ConfigPhpUpdate;
use magequery_core::phparray::PhpValue;

fn put(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).expect("mkdir");
    std::fs::write(&p, body).expect("write");
}

fn module(root: &Path, name: &str, sequence: &[&str]) {
    let (vendor, short) = name.split_once('_').unwrap();
    let dir = format!("app/code/{vendor}/{short}");
    let seq: String = sequence.iter().map(|s| format!("<module name=\"{s}\"/>")).collect();
    put(
        root,
        &format!("{dir}/etc/module.xml"),
        &format!("<?xml version=\"1.0\"?>\n<config><module name=\"{name}\"><sequence>{seq}</sequence></module></config>\n"),
    );
    put(
        root,
        &format!("{dir}/registration.php"),
        &format!("<?php\n\\Magento\\Framework\\Component\\ComponentRegistrar::register(\\Magento\\Framework\\Component\\ComponentRegistrar::MODULE, '{name}', __DIR__);\n"),
    );
}

/// config.php as Magento writes it, with the modules given and a `system` section.
fn config_php(modules: &[(&str, u8)]) -> String {
    let list: Vec<String> = modules.iter().map(|(n, v)| format!("        '{n}' => {v}")).collect();
    format!(
        "<?php\nreturn [\n    'modules' => [\n{}\n    ],\n    'system' => [\n        'default' => [\n            'web' => [\n                'x' => '1'\n            ]\n        ]\n    ]\n];\n",
        list.join(",\n")
    )
}

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    module(root, "Acme_Base", &[]);
    module(root, "Acme_Child", &["Acme_Base"]);
    module(root, "Acme_Off", &[]);
    dir
}

fn update(root: &Path) -> ConfigPhpUpdate {
    Magento::open(root).expect("open").upgraded_config_php().expect("config.php")
}

#[test]
fn an_up_to_date_config_php_is_left_alone() {
    let dir = tree();
    put(dir.path(), "app/etc/config.php", &config_php(&[("Acme_Base", 1), ("Acme_Child", 1), ("Acme_Off", 0)]));
    assert_eq!(update(dir.path()), ConfigPhpUpdate::Unchanged);
}

#[test]
fn a_new_module_is_added_in_sequence_order_and_a_disabled_one_stays_off() {
    let dir = tree();
    let root = dir.path();
    module(root, "Acme_New", &["Acme_Child"]);
    module(root, "Aaa_First", &[]);
    put(root, "app/etc/config.php", &config_php(&[("Acme_Base", 1), ("Acme_Child", 1), ("Acme_Off", 0)]));
    let ConfigPhpUpdate::Rewrite { contents, added, removed, reordered } = update(root) else {
        panic!("expected a rewrite")
    };
    assert_eq!(added, ["Aaa_First", "Acme_New"]);
    assert!(removed.is_empty() && !reordered);
    // Loader::sortBySequence: bytewise, then dependencies pulled ahead.
    assert_eq!(
        contents,
        config_php(&[("Aaa_First", 1), ("Acme_Base", 1), ("Acme_Child", 1), ("Acme_New", 1), ("Acme_Off", 0)])
    );
}

#[test]
fn a_module_gone_from_disk_is_dropped() {
    let dir = tree();
    put(
        dir.path(),
        "app/etc/config.php",
        &config_php(&[("Acme_Base", 1), ("Acme_Child", 1), ("Acme_Gone", 1), ("Acme_Off", 0)]),
    );
    let ConfigPhpUpdate::Rewrite { contents, removed, .. } = update(dir.path()) else { panic!("expected a rewrite") };
    assert_eq!(removed, ["Acme_Gone"]);
    assert_eq!(contents, config_php(&[("Acme_Base", 1), ("Acme_Child", 1), ("Acme_Off", 0)]));
}

#[test]
fn a_hand_edited_config_php_is_left_to_php() {
    let dir = tree();
    put(dir.path(), "app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Base' => 1, 'Acme_Child' => 1, 'Acme_Off' => 0]];\n");
    assert!(matches!(update(dir.path()), ConfigPhpUpdate::CannotReproduce(_)));
}

#[test]
fn indexer_config_matches_magento() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    module(root, "Acme_Base", &[]);
    module(root, "Acme_Child", &["Acme_Base"]);
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/indexer-hash");
    let read = |f: &str| std::fs::read_to_string(fixtures.join(f)).expect("fixture");
    put(root, "app/code/Acme/Base/etc/indexer.xml", &read("base.xml"));
    put(root, "app/code/Acme/Child/etc/indexer.xml", &read("child.xml"));
    put(root, "app/etc/config.php", &config_php(&[("Acme_Base", 1), ("Acme_Child", 1)]));
    // base64 of 32 × "k", the key regenerate.php hashes with.
    put(
        root,
        "app/etc/env.php",
        "<?php\nreturn ['crypt' => ['key' => 'base64a2tra2tra2tra2tra2tra2tra2tra2tra2tra2tra2s=']];\n",
    );

    let expected: Vec<(String, String, String)> = read("expected.tsv")
        .lines()
        .map(|l| {
            let mut f = l.splitn(3, '\t');
            (f.next().unwrap().into(), f.next().unwrap().into(), f.next().unwrap().into())
        })
        .collect();
    let mage = Magento::open(root).expect("open");
    let json = mage.indexer_config_json().expect("json");
    let hashes = mage.indexer_config_hashes().expect("hashes");
    assert_eq!(
        json.iter().map(|(id, j)| (id.as_str(), j.as_str())).collect::<Vec<_>>(),
        expected.iter().map(|(id, _, j)| (id.as_str(), j.as_str())).collect::<Vec<_>>()
    );
    assert_eq!(
        hashes.iter().map(|(id, h)| (id.as_str(), h.as_str())).collect::<Vec<_>>(),
        expected.iter().map(|(id, h, _)| (id.as_str(), h.as_str())).collect::<Vec<_>>()
    );
}

#[test]
fn global_di_config_matches_magento() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    module(root, "Acme_Base", &[]);
    module(root, "Acme_Child", &["Acme_Base"]);
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/di-config");
    let read = |f: &str| std::fs::read_to_string(fixtures.join(f)).expect("fixture");
    put(root, "app/code/Acme/Base/etc/di.xml", &read("base.xml"));
    put(root, "app/code/Acme/Child/etc/di.xml", &read("child.xml"));
    put(root, "app/etc/config.php", &config_php(&[("Acme_Base", 1), ("Acme_Child", 1)]));

    // Fixture\C as regenerate.php declares it.
    let constant = |name: &str| match name {
        "Fixture\\C::A" => Some(PhpValue::Str("constant a".into())),
        "Fixture\\C::N" => Some(PhpValue::Int(5)),
        _ => None,
    };
    let mage = Magento::open(root).expect("open");
    assert_eq!(mage.global_di_config(&constant).expect("di config"), read("expected.ser"));
    // A constant that cannot be evaluated leaves the entry to PHP.
    assert!(mage.global_di_config(&|_| None).is_err());
}

/// Loki's `Setup\Recurring` (Loki_Components, LokiCheckout_Core: the fixtures are
/// their source verbatim) only enables missing `<sequence>` modules, so it is done
/// natively when there are none, and left to PHP when there are, or when the
/// script is anything else.
#[test]
fn loki_dependency_recurring_is_native_when_nothing_is_missing() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/loki-recurring");
    let read = |f: &str| std::fs::read_to_string(fixtures.join(f)).expect("fixture");
    let tree = |checkout_recurring: &str, dep_enabled: u8| {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        module(root, "Acme_Dep", &[]);
        module(root, "Loki_Components", &["Acme_Dep"]);
        module(root, "LokiCheckout_Core", &["Loki_Components"]);
        put(root, "app/code/Loki/Components/Setup/ModuleDependencies.php", &read("ModuleDependencies.php"));
        put(root, "app/code/Loki/Components/Setup/Recurring.php", &read("Recurring.php"));
        put(root, "app/code/LokiCheckout/Core/Setup/Recurring.php", checkout_recurring);
        put(root, "app/etc/env.php", "<?php\nreturn [\n    'MAGE_MODE' => 'developer'\n];\n");
        put(
            root,
            "app/etc/config.php",
            &config_php(&[("Acme_Dep", dep_enabled), ("Loki_Components", 1), ("LokiCheckout_Core", 1)]),
        );
        let check = Magento::open(root).expect("open").dev_upgrade_check().expect("check");
        let php: Vec<String> = check.recurring_php.iter().map(|n| format!("{}: {}", n.module, n.reason)).collect();
        (check.recurring_skipped, php)
    };

    let (skipped, php) = tree(&read("CheckoutRecurring.php"), 1);
    assert!(skipped.contains(&"Loki_Components (sequence modules enabled)".to_owned()), "{skipped:?}");
    assert!(skipped.contains(&"LokiCheckout_Core (sequence modules enabled)".to_owned()), "{skipped:?}");
    assert!(php.is_empty(), "{php:?}");

    // A sequence module that is off: Loki enables it, in PHP.
    let (_, php) = tree(&read("CheckoutRecurring.php"), 0);
    assert_eq!(php, ["Loki_Components: Loki enables Loki_Components's sequence modules Acme_Dep"]);

    // A recurring script that does anything more is not recognised.
    let more = read("CheckoutRecurring.php")
        .replace("enableMissingDependencies('LokiCheckout_Core');", "enableMissingDependencies('LokiCheckout_Core');\n        $setup->startSetup();");
    let (_, php) = tree(&more, 1);
    assert_eq!(php, ["LokiCheckout_Core: LokiCheckout\\Core\\Setup\\Recurring is not ported"]);
}
