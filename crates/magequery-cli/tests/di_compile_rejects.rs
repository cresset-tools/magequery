//! `di compile` refuses the di.xml that `setup:di:compile` refuses, and only
//! that.
//!
//! Not built on Windows, for the same reason as the sibling `di compile`
//! fixtures: the metadata filenames join their cache scope with `|`.
//!
//! Magento's compile checks two things about the classes di.xml names, both
//! with `class_exists`, so an interface or a trait fails them too:
//!
//! - `Compiler\Config\Reader`: every class the compile covers, and every
//!   preference key, is followed through the preferences to its end. Unless
//!   that end is a virtual type, it must exist, and be part of the compile.
//! - `Interception\PluginListGenerator`: every plugin that is not disabled,
//!   with a virtual type taken to its class, must exist.
//!
//! Everything else that names a missing class compiles, checked against
//! Mage-OS 3.1 (Magento 2.4.9): a preference FOR one, a `<type>` of one, a
//! virtual type of one, even when a preference points at that virtual type,
//! and an object argument of one. Those fail at runtime, if at all.

#![cfg(not(windows))]

use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture(PathBuf);

const DI_HEAD: &str = "<?xml version=\"1.0\"?>\n<config xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n";

impl Fixture {
    /// A module with an interface, a class implementing it, and the di.xml
    /// given (inside `<config>`).
    fn new(name: &str, di: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = std::fs::remove_dir_all(&root);
        let fx = Fixture(root);
        fx.write("app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Probe' => 1]];\n");
        fx.write(
            "app/code/Acme/Probe/etc/module.xml",
            "<?xml version=\"1.0\"?>\n<config><module name=\"Acme_Probe\"/></config>\n",
        );
        fx.write(
            "app/code/Acme/Probe/Model/ApiInterface.php",
            "<?php\nnamespace Acme\\Probe\\Model;\n\ninterface ApiInterface\n{\n}\n",
        );
        fx.write(
            "app/code/Acme/Probe/Model/Real.php",
            "<?php\nnamespace Acme\\Probe\\Model;\n\nclass Real implements ApiInterface\n{\n    \
             public function __construct(?ApiInterface $dep = null)\n    {\n    }\n\n    \
             public function run()\n    {\n    }\n}\n",
        );
        fx.di("etc/di.xml", di);
        fx
    }

    fn di(&self, rel: &str, body: &str) {
        self.write(&format!("app/code/Acme/Probe/{rel}"), &format!("{DI_HEAD}{body}\n</config>\n"));
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn compile(&self, extra: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["di", "compile", "--force", "--root"])
            .arg(&self.0)
            .args(extra)
            .output()
            .expect("run magecommand")
    }

    /// The compile fails, with every `expected` message on stderr.
    fn refused(&self, expected: &[&str]) -> String {
        let out = self.compile(&[]);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(out.status.code(), Some(1), "di compile must refuse this store:\n{stderr}");
        for message in expected {
            assert!(stderr.contains(message), "expected {message:?} in:\n{stderr}");
        }
        assert!(
            !self.0.join("generated/metadata/global.php").exists(),
            "a refused compile writes no output"
        );
        stderr
    }

    fn accepted(&self) {
        let out = self.compile(&[]);
        assert!(
            out.status.success(),
            "Magento compiles this store, so di compile must too:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_preference_for_a_missing_class_is_refused() {
    let fx = Fixture::new(
        "mq-reject-pref-type",
        r#"<preference for="Acme\Probe\Model\ApiInterface" type="Acme\Probe\Model\Gone"/>"#,
    );
    let stderr = fx.refused(&[
        r#"Preference declared for "Acme\Probe\Model\ApiInterface" as "Acme\Probe\Model\Gone", but the latter does not exist."#,
        "app/code/Acme/Probe/etc/di.xml:3",
    ]);
    assert!(stderr.contains("setup:di:compile"), "says Magento refuses it too:\n{stderr}");
}

#[test]
fn a_missing_plugin_class_is_refused() {
    let fx = Fixture::new(
        "mq-reject-plugin",
        r#"<type name="Acme\Probe\Model\Real"><plugin name="p" type="Acme\Probe\Plugin\Gone"/></type>"#,
    );
    fx.refused(&["Plugin class Acme\\Probe\\Plugin\\Gone doesn't exist", "app/code/Acme/Probe/etc/di.xml:3"]);
}

#[test]
fn a_plugin_through_a_virtual_type_of_a_missing_class_is_refused() {
    let fx = Fixture::new(
        "mq-reject-plugin-vtype",
        r#"<virtualType name="Acme\Probe\Plugin\Virtual" type="Acme\Probe\Plugin\Gone"/>
<type name="Acme\Probe\Model\Real"><plugin name="p" type="Acme\Probe\Plugin\Virtual"/></type>"#,
    );
    fx.refused(&["Plugin class Acme\\Probe\\Plugin\\Gone doesn't exist"]);
}

#[test]
fn a_preference_for_an_interface_is_refused() {
    // `class_exists` is false for an interface.
    let fx = Fixture::new(
        "mq-reject-pref-interface",
        r#"<preference for="Acme\Probe\Model\Real" type="Acme\Probe\Model\ApiInterface"/>"#,
    );
    fx.refused(&[r#"as "Acme\Probe\Model\ApiInterface", but the latter does not exist."#]);
}

#[test]
fn an_area_preference_is_checked_in_its_area() {
    let fx = Fixture::new("mq-reject-pref-area", "");
    fx.di(
        "etc/frontend/di.xml",
        r#"<preference for="Acme\Probe\Model\ApiInterface" type="Acme\Probe\Model\Gone"/>"#,
    );
    fx.refused(&["app/code/Acme/Probe/etc/frontend/di.xml:3", "frontend"]);
}

#[test]
fn a_preference_for_a_class_outside_the_compile_is_refused() {
    // Autoloadable, so it exists, but in no path the compile scans.
    let fx = Fixture::new(
        "mq-reject-pref-unscanned",
        r#"<preference for="Acme\Probe\Model\ApiInterface" type="Acme\Lib\Outside"/>"#,
    );
    fx.write(
        "vendor/acme/lib/src/Outside.php",
        "<?php\nnamespace Acme\\Lib;\n\nclass Outside implements \\Acme\\Probe\\Model\\ApiInterface\n{\n}\n",
    );
    fx.write(
        "vendor/composer/autoload_classmap.php",
        "<?php\n\n$vendorDir = dirname(__DIR__);\n$baseDir = dirname($vendorDir);\n\nreturn array(\n    \
         'Acme\\\\Lib\\\\Outside' => $vendorDir . '/acme/lib/src/Outside.php',\n);\n",
    );
    fx.refused(&[
        r#"Preference declared for "Acme\Probe\Model\ApiInterface" as "Acme\Lib\Outside", but the latter has not been included in dependency injection compilation."#,
    ]);
}

#[test]
fn what_magento_compiles_is_compiled() {
    let cases = [
        // Configuring a class that does not exist.
        ("pref-for", r#"<preference for="Acme\Probe\Model\GoneInterface" type="Acme\Probe\Model\Real"/>"#),
        ("type-name", r#"<type name="Acme\Probe\Model\Gone"><arguments><argument name="x" xsi:type="string">y</argument></arguments></type>"#),
        ("vtype-unused", r#"<virtualType name="Acme\Probe\Model\Virtual" type="Acme\Probe\Model\Gone"/>"#),
        // Magento resolves no class for a preference to a virtual type.
        (
            "vtype-preferred",
            r#"<virtualType name="Acme\Probe\Model\Virtual" type="Acme\Probe\Model\Gone"/>
<preference for="Acme\Probe\Model\ApiInterface" type="Acme\Probe\Model\Virtual"/>"#,
        ),
        ("arg-object", r#"<type name="Acme\Probe\Model\Real"><arguments><argument name="dep" xsi:type="object">Acme\Probe\Model\Gone</argument></arguments></type>"#),
        // A disabled plugin is never loaded.
        ("plugin-disabled", r#"<type name="Acme\Probe\Model\Real"><plugin name="p" type="Acme\Probe\Plugin\Gone" disabled="true"/></type>"#),
        // A plugin on a missing class still loads its plugin class, which exists.
        ("plugin-on-missing", r#"<type name="Acme\Probe\Model\Gone"><plugin name="p" type="Acme\Probe\Model\Real"/></type>"#),
        // The compile generates the factory the preference names.
        ("pref-factory", r#"<preference for="Acme\Probe\Model\ApiInterface" type="Acme\Probe\Model\RealFactory"/>"#),
        ("pref-real", r#"<preference for="Acme\Probe\Model\ApiInterface" type="Acme\Probe\Model\Real"/>"#),
    ];
    for (name, di) in cases {
        Fixture::new(&format!("mq-accept-{name}"), di).accepted();
    }
}

#[test]
fn a_refused_compile_is_not_up_to_date_afterwards() {
    let good = r#"<preference for="Acme\Probe\Model\ApiInterface" type="Acme\Probe\Model\Real"/>"#;
    let fx = Fixture::new("mq-reject-incremental", good);
    let incremental = |fx: &Fixture| {
        Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["di", "compile", "--incremental", "--root"])
            .arg(&fx.0)
            .output()
            .expect("run magecommand")
    };
    assert!(incremental(&fx).status.success());
    fx.di("etc/di.xml", r#"<preference for="Acme\Probe\Model\ApiInterface" type="Acme\Probe\Model\Gone"/>"#);
    assert_eq!(incremental(&fx).status.code(), Some(1));
    // Back to the config the last successful compile saw: its output is gone,
    // so this must compile again rather than report "up to date".
    fx.di("etc/di.xml", good);
    let out = incremental(&fx);
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("up to date"));
    assert!(fx.0.join("generated/metadata/global.php").is_file());
}


#[test]
fn watch_reports_a_refusal_and_compiles_once_it_is_fixed() {
    let fx = Fixture::new(
        "mq-reject-watch",
        r#"<preference for="Acme\Probe\Model\ApiInterface" type="Acme\Probe\Model\Gone"/>"#,
    );
    let child = Command::new(env!("CARGO_BIN_EXE_magecommand"))
        .args(["di", "watch", "--once", "--root"])
        .arg(&fx.0)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run magecommand");
    // The first build is refused; give the watcher time to start, then fix it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(!fx.0.join("generated/metadata/global.php").exists(), "a refused build writes nothing");
    fx.di("etc/di.xml", r#"<preference for="Acme\Probe\Model\ApiInterface" type="Acme\Probe\Model\Real"/>"#);
    let out = child.wait_with_output().expect("watch exits after one recompile");
    assert!(std::time::Instant::now() < deadline);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("but the latter does not exist"), "{stderr}");
    assert!(fx.0.join("generated/metadata/global.php").is_file(), "the fix compiles the whole tree:\n{stderr}");
}
