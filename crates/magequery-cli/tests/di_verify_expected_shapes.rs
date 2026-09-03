//! `di verify` and the store shapes whose artifacts a 2.4.9 compile omits.
//!
//! Not built on Windows: the metadata filenames join their cache scope with
//! `|`, a reserved character there.
//!
//! Every `di verify` regression this suite exists for had the same shape — an
//! archive produced by a Magento that compiled **every module on disk**, checked
//! against magecommand, which compiles only the ENABLED ones. The extra
//! artifacts in the archive are expected, and the classifier's job is to say so.
//! It repeatedly said so for only *part* of a component:
//!
//! - the rule keyed the owning module off the first two path segments, so it
//!   matched nothing once paths carried the `code/` prefix a whole-tree diff
//!   produces — 254 files explained in one layout, 0 in the other;
//! - it recognized `/Interceptor.php` but not the other generated kinds;
//! - it resolved a class to its source file, which a GENERATED name does not
//!   have, so a component's plain classes were explained while its interceptors
//!   and factories were not.
//!
//! Each of those reached a client pipeline. None needed a real store to catch:
//! they need an archive that has artifacts the output does not, in the shapes
//! below. The fixture therefore builds the output with a real `di compile`, then
//! synthesizes the archive as "that output, plus what the older compiler would
//! also have written".
//!
//! The negative controls matter as much as the positive ones — a rule that
//! explains everything is worthless.

#![cfg(not(windows))]

use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    /// A root with three components:
    ///
    /// - `Acme_Live` — enabled, and the subject of a plugin so it gets an
    ///   interceptor in both trees;
    /// - `Acme_Off` — on disk, `0` in config.php;
    /// - `vendor/acme/ghost` — PSR-4 autoloaded and NOT a module at all (no
    ///   entry in config.php), the shape a package takes when it registers
    ///   itself from `registration.php` without ever being enabled.
    fn new(name: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("mq-verify-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let w = |rel: &str, content: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        };

        w(
            "app/etc/config.php",
            "<?php\nreturn ['modules' => ['Acme_Live' => 1, 'Acme_Off' => 0]];\n",
        );
        for (m, enabled) in [("Live", true), ("Off", false)] {
            w(
                &format!("app/code/Acme/{m}/etc/module.xml"),
                &format!("<?xml version=\"1.0\"?>\n<config><module name=\"Acme_{m}\"/></config>\n"),
            );
            w(
                &format!("app/code/Acme/{m}/Model/Thing.php"),
                &format!(
                    "<?php\nnamespace Acme\\{m}\\Model;\n\nclass Thing\n{{\n    \
                     public function run()\n    {{\n        return 1;\n    }}\n}}\n"
                ),
            );
            if enabled {
                w(
                    &format!("app/code/Acme/{m}/Plugin/Watcher.php"),
                    &format!(
                        "<?php\nnamespace Acme\\{m}\\Plugin;\n\nclass Watcher\n{{\n    \
                         public function beforeRun(\\Acme\\{m}\\Model\\Thing $s)\n    \
                         {{\n        return null;\n    }}\n}}\n"
                    ),
                );
                w(
                    &format!("app/code/Acme/{m}/etc/di.xml"),
                    &format!(
                        "<?xml version=\"1.0\"?>\n<config>\n    \
                         <type name=\"Acme\\{m}\\Model\\Thing\">\n        \
                         <plugin name=\"acme_watcher\" type=\"Acme\\{m}\\Plugin\\Watcher\"/>\n    \
                         </type>\n</config>\n"
                    ),
                );
            }
        }

        // The ghost package: real files, PSR-4 mapped, no module entry.
        w(
            "vendor/acme/ghost/src/Model/Spirit.php",
            "<?php\nnamespace Acme\\Ghost\\Model;\n\nclass Spirit\n{\n}\n",
        );
        w(
            "vendor/composer/installed.json",
            r#"{"packages":[{"name":"acme/ghost","version":"1.0.0","type":"magento2-module",
               "install-path":"../acme/ghost","autoload":{"psr-4":{"Acme\\Ghost\\":"src/"}}}]}"#,
        );

        Fixture(root)
    }

    fn compile(&self) {
        let out = Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["di", "compile", "--root"])
            .arg(&self.0)
            .output()
            .expect("run magecommand");
        assert!(out.status.success(), "compile: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// Copy `generated/` to `archive/` — the starting point for a synthetic
    /// oracle that agrees with us everywhere except the extra artifacts.
    fn snapshot_archive(&self) {
        let archive = self.0.join("archive");
        let _ = std::fs::remove_dir_all(&archive);
        copy_tree(&self.0.join("generated"), &archive);
    }

    /// Add a file the older compiler would also have written.
    fn archive_extra(&self, rel: &str) {
        let p = self.0.join("archive").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "<?php\n// artifact the 2.4.7 compiler emitted\n").unwrap();
    }

    /// `di verify` over the WHOLE trees (the shape a pipeline uses).
    fn verify_whole(&self) -> bool {
        self.verify(&self.0.join("archive"), &self.0.join("generated"))
    }

    /// `di verify` over the `code/` halves (the shape the docs describe).
    fn verify_halves(&self) -> bool {
        self.verify(&self.0.join("archive/code"), &self.0.join("generated/code"))
    }

    fn verify(&self, archive: &Path, output: &Path) -> bool {
        Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["di", "verify", "--root"])
            .arg(&self.0)
            .arg("--archive")
            .arg(archive)
            .arg("--output")
            .arg(output)
            .arg("--fail-on-diff")
            .output()
            .expect("run magecommand")
            .status
            .success()
    }

    fn report(&self) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["di", "verify", "--root"])
            .arg(&self.0)
            .arg("--archive")
            .arg(self.0.join("archive"))
            .arg("--output")
            .arg(self.0.join("generated"))
            .output()
            .expect("run magecommand");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// A disabled module's generated artifacts are absent from a 2.4.9 compile —
/// every KIND of them, not just interceptors, and in BOTH directory layouts.
///
/// The layout half is the regression that reached a pipeline: the rule read the
/// owning module from the first two path segments, so `code/Acme/Off/…` resolved
/// to a module called `code_Acme` and matched nothing.
#[test]
fn disabled_module_artifacts_are_explained_in_both_layouts() {
    let fx = Fixture::new("disabled");
    fx.compile();
    fx.snapshot_archive();
    for rel in [
        "code/Acme/Off/Model/Thing/Interceptor.php",
        "code/Acme/Off/Model/ThingFactory.php",
        "code/Acme/Off/Model/Thing/Proxy.php",
        "code/Acme/Off/Api/Data/ThingExtension.php",
        "code/Acme/Off/Api/Data/ThingExtensionInterface.php",
    ] {
        fx.archive_extra(rel);
    }

    assert!(fx.verify_whole(), "whole-tree layout must explain them:\n{}", fx.report());
    assert!(fx.verify_halves(), "split-halves layout must explain them too:\n{}", fx.report());
}

/// A component that is not a module at all: its files resolve, but under none of
/// the compile's scan roots. Its GENERATED artifacts have no source file of
/// their own, so they must be resolved through the dispatch subject — the bug
/// that left three artifacts failing a build while 84 sibling entries were
/// explained.
#[test]
fn an_unregistered_components_artifacts_are_explained() {
    let fx = Fixture::new("ghost");
    fx.compile();
    fx.snapshot_archive();
    fx.archive_extra("code/Acme/Ghost/Model/Spirit/Interceptor.php");
    fx.archive_extra("code/Acme/Ghost/Model/SpiritFactory.php");

    assert!(
        fx.verify_whole(),
        "an interceptor and a factory over a class outside every scan root must be \
         explained, not just the plain class:\n{}",
        fx.report()
    );
}

/// The negative control. An artifact belonging to an ENABLED module is a real
/// miss — the compile should have written it — and no amount of
/// disabled/outside-scan reasoning may explain it away.
#[test]
fn an_enabled_modules_missing_artifact_still_fails() {
    let fx = Fixture::new("enabled");
    fx.compile();
    fx.snapshot_archive();
    fx.archive_extra("code/Acme/Live/Model/Other/Interceptor.php");

    assert!(
        !fx.verify_whole(),
        "a missing artifact for an enabled module must fail the diff:\n{}",
        fx.report()
    );
}

/// The other negative control: with nothing added, the two trees are the same
/// tree, so the verdict must be clean in both layouts. Guards against a
/// classifier that only ever *adds* explanations.
#[test]
fn an_identical_tree_verifies_clean() {
    let fx = Fixture::new("identical");
    fx.compile();
    fx.snapshot_archive();

    assert!(fx.verify_whole(), "identical trees must verify:\n{}", fx.report());
    assert!(fx.verify_halves(), "identical halves must verify:\n{}", fx.report());
}
