//! `di compile --area-config` and Mage-OS 4.0's area files written as
//! differences from `global`.
//!
//! Not built on Windows, for the same reason as the sibling `di compile`
//! fixtures: the metadata filenames join their cache scope with `|`, a reserved
//! character in a Windows filename.
//!
//! Mage-OS 4.0's `setup:di:compile` writes `global.php` in full and every other
//! area as `['_extends' => 'global', …]` holding only the entries that differ
//! (`Area::extractDiff`); its `ConfigLoader\Compiled` merges the two back. The
//! runtime still reads a full file, so both layouts are valid there, and only
//! there: an older loader would read `_extends` as a section and lose global.

#![cfg(not(windows))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The class whose source makes a checkout's compiler write diffs.
const AREA_OPERATION: &str = "setup/src/Magento/Setup/Module/Di/App/Task/Operation/Area.php";
const COMPILER: &str = "setup/src/Magento/Setup/Module/Di/Compiler";

struct Fixture(PathBuf);

impl Fixture {
    /// A store with one global preference and one frontend-only preference.
    /// `writes_diffs` gives it Mage-OS 4.0's `Area` operation; the compiler
    /// files pick the lazy pass, as in `di_compile_lazy_types.rs`.
    fn new(name: &str, writes_diffs: bool, compiler_files: &[&str]) -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = std::fs::remove_dir_all(&root);
        let fx = Fixture(root);
        fx.write("app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Repro' => 1]];\n");
        fx.write(
            "app/code/Acme/Repro/etc/module.xml",
            "<?xml version=\"1.0\"?>\n<config><module name=\"Acme_Repro\"/></config>\n",
        );
        for class in ["Api", "Everywhere", "Storefront"] {
            fx.write(
                &format!("app/code/Acme/Repro/Model/{class}.php"),
                &format!("<?php\nnamespace Acme\\Repro\\Model;\n\nclass {class}\n{{\n}}\n"),
            );
        }
        fx.write(
            "app/code/Acme/Repro/etc/di.xml",
            "<?xml version=\"1.0\"?>\n<config>\n    \
             <preference for=\"Acme\\Repro\\Model\\Api\" type=\"Acme\\Repro\\Model\\Everywhere\"/>\n\
             </config>\n",
        );
        fx.write(
            "app/code/Acme/Repro/etc/frontend/di.xml",
            "<?xml version=\"1.0\"?>\n<config>\n    \
             <preference for=\"Acme\\Repro\\Model\\Api\" type=\"Acme\\Repro\\Model\\Storefront\"/>\n\
             </config>\n",
        );
        if writes_diffs {
            fx.write(
                AREA_OPERATION,
                "<?php\nclass Area\n{\n    private function extractDiff(array $config, array $globalConfig)\n    {\n    }\n}\n",
            );
        }
        for f in compiler_files {
            fx.write(&format!("{COMPILER}/{f}"), "<?php\n");
        }
        fx
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn run(&self, extra: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["di", "compile", "--force", "--root"])
            .arg(&self.0)
            .args(extra)
            .output()
            .expect("run magecommand")
    }

    /// `di compile --incremental`, which skips the compile when no input
    /// changed. Returns stdout.
    fn incremental(&self, extra: &[&str]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["di", "compile", "--incremental", "--root"])
            .arg(&self.0)
            .args(extra)
            .output()
            .expect("run magecommand");
        assert!(out.status.success(), "di compile failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn compile(&self, extra: &[&str]) {
        let out = self.run(extra);
        assert!(out.status.success(), "di compile failed: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn metadata(&self, area: &str) -> String {
        let path = self.0.join(format!("generated/metadata/{area}.php"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// Copy `generated/metadata` aside, to compare one compile with another.
    fn keep_metadata(&self, as_name: &str) -> PathBuf {
        let to = self.0.join(as_name);
        let _ = std::fs::remove_dir_all(&to);
        copy_dir(&self.0.join("generated/metadata"), &to);
        to
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
    }
}

const DIFF_HEADER: &str = "<?php return array (\n  '_extends' => 'global',\n";
const STOREFRONT: &str = "    'Acme\\\\Repro\\\\Model\\\\Api' => 'Acme\\\\Repro\\\\Model\\\\Storefront',\n";
const EVERYWHERE: &str = "    'Acme\\\\Repro\\\\Model\\\\Api' => 'Acme\\\\Repro\\\\Model\\\\Everywhere',\n";

#[test]
fn mageos_4_0_writes_areas_as_differences_from_global() {
    let fx = Fixture::new("mq-area-diff", true, &[]);
    fx.compile(&[]);

    let global = fx.metadata("global");
    assert!(!global.contains("'_extends'"), "global is always written in full:\n{global}");
    assert!(global.contains(EVERYWHERE), "{global}");

    let frontend = fx.metadata("frontend");
    assert!(frontend.starts_with(DIFF_HEADER), "frontend extends global:\n{frontend}");
    assert!(frontend.contains(STOREFRONT), "the frontend preference differs, so it stays:\n{frontend}");
    assert!(
        !frontend.contains("'Acme\\\\Repro\\\\Model\\\\Everywhere' =>"),
        "an argument row identical to global's is left out:\n{frontend}"
    );

    let crontab = fx.metadata("crontab");
    assert!(crontab.starts_with(DIFF_HEADER), "{crontab}");
    assert!(!crontab.contains(EVERYWHERE), "crontab repeats nothing from global:\n{crontab}");
}

#[test]
fn area_config_full_keeps_every_area_whole() {
    let fx = Fixture::new("mq-area-full", true, &[]);
    fx.compile(&["--area-config", "full"]);

    let frontend = fx.metadata("frontend");
    assert!(!frontend.contains("'_extends'"), "{frontend}");
    assert!(frontend.contains(STOREFRONT), "{frontend}");
    assert!(fx.metadata("crontab").contains(EVERYWHERE));
}

#[test]
fn the_target_platform_picks_the_default_layout() {
    let fx = Fixture::new("mq-area-platform", false, &["Config/Chain/LazyTypes.php"]);
    fx.compile(&[]);
    assert!(!fx.metadata("frontend").contains("'_extends'"), "a 3.3 checkout writes full files");

    fx.compile(&["--target-platform", "mageos-4.0"]);
    assert!(fx.metadata("frontend").starts_with(DIFF_HEADER), "4.0 writes diffs");

    fx.compile(&["--target-platform", "mageos-4.0", "--area-config", "full"]);
    assert!(!fx.metadata("frontend").contains("'_extends'"), "the flag overrides the platform");
}

#[test]
fn a_platform_that_cannot_read_diffs_refuses_them() {
    let fx = Fixture::new("mq-area-refuse", false, &["Config/Chain/LazyTypes.php"]);
    for extra in [&["--area-config", "diff"][..], &["--area-config", "diff", "--target-platform", "mageos-3.3"]] {
        let out = fx.run(extra);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{extra:?} must fail: {stderr}");
        assert!(stderr.contains("--area-config diff"), "the error names the flag: {stderr}");
    }
}

fn verify(archive: &Path, output: &Path) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_magecommand"))
        .args(["di", "verify", "--no-explain", "--fail-on-diff", "--color", "never", "--archive"])
        .arg(archive)
        .arg("--output")
        .arg(output)
        .output()
        .expect("run magecommand");
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn verify_compares_the_configuration_not_the_layout() {
    let fx = Fixture::new("mq-area-verify", true, &[]);
    fx.compile(&["--area-config", "full"]);
    let full = fx.keep_metadata("full");
    fx.compile(&["--area-config", "diff"]);
    let diff = fx.keep_metadata("diff");

    for (archive, output) in [(&full, &diff), (&diff, &full), (&diff, &diff)] {
        let (clean, report) = verify(archive, output);
        assert!(clean, "{} vs {}:\n{report}", archive.display(), output.display());
    }

    // A real difference inside a diff file is still one.
    let frontend = diff.join("frontend.php");
    let text = std::fs::read_to_string(&frontend).unwrap();
    std::fs::write(&frontend, text.replace("Model\\\\Storefront'", "Model\\\\Elsewhere'")).unwrap();
    let (clean, report) = verify(&full, &diff);
    assert!(!clean, "a changed frontend preference must be reported:\n{report}");
    assert!(report.contains("frontend.php"), "{report}");
}

#[test]
fn incremental_recompiles_when_the_settings_change() {
    let fx = Fixture::new("mq-area-incremental", true, &[]);
    // A generated factory, so there is a `generated/code` to keep.
    fx.write(
        "app/code/Acme/Repro/Model/Uses.php",
        "<?php\nnamespace Acme\\Repro\\Model;\n\nclass Uses\n{\n    \
         public function __construct(EverywhereFactory $factory)\n    {\n    }\n}\n",
    );
    fx.incremental(&[]);
    assert!(fx.0.join("generated/code/Acme/Repro/Model/EverywhereFactory.php").is_file());
    assert!(fx.incremental(&[]).contains("up to date"), "nothing changed, so nothing to do");

    let out = fx.incremental(&["--area-config", "full"]);
    assert!(!out.contains("up to date"), "a new layout is a new output: {out}");
    assert!(!fx.metadata("frontend").contains("'_extends'"));
    assert!(fx.incremental(&["--area-config", "full"]).contains("up to date"));

    for settings in [&["--area-config", "full", "--target-php", "8.3"][..], &["--area-config", "full", "--fused"]] {
        let out = fx.incremental(settings);
        assert!(!out.contains("up to date"), "{settings:?} changes the output: {out}");
    }
}

#[test]
fn the_digest_covers_the_settings() {
    let fx = Fixture::new("mq-area-digest", true, &[]);
    let digest = |extra: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["di", "digest", "--root"])
            .arg(&fx.0)
            .args(extra)
            .output()
            .expect("run magecommand");
        assert!(out.status.success(), "di digest failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let default = digest(&[]);
    assert_eq!(digest(&["--area-config", "diff"]), default, "restating the default is the same output");
    for settings in [&["--area-config", "full"][..], &["--fused"], &["--target-php", "8.3"]] {
        assert_ne!(digest(settings), default, "{settings:?} is another output");
    }
}
