//! `static deploy` and the CssUrls post-processor.
//!
//! Not built on Windows (the metadata filenames use `|`).
//!
//! `Magento\Deploy\Package\Processor\PostProcessor\CssUrls` runs after a
//! package is written and rewrites relative `url()`s that do not resolve
//! inside it — pointing them at the parent package that owns the file. It
//! exists only in the deploy pipeline, so nothing about compiling a stylesheet
//! reveals it; a real store's admin theme was one file from byte-parity
//! entirely because of this pass.
//!
//! The fixture builds a module whose stylesheet climbs out of its own
//! directory (`url(../../images/…)` from `css/source/`), which is the shape
//! that triggers it, plus a control url that resolves normally.

#![cfg(not(windows))]

use std::path::PathBuf;
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    /// Per-test root: the two tests run in parallel and `Drop` removes the
    /// tree, so a shared path makes them race.
    fn new(name: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("mq-cssurls-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let w = |rel: &str, content: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        };
        let wb = |rel: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"\x89PNG\r\n\x1a\n").unwrap();
        };

        w("app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Art' => 1]];\n");
        w(
            "app/code/Acme/Art/etc/module.xml",
            "<?xml version=\"1.0\"?>\n<config><module name=\"Acme_Art\"/></config>\n",
        );
        // A BASE-area module asset: it belongs to `base/Magento/base/default`,
        // which is the parent package the rewrite should name.
        wb("app/code/Acme/Art/view/base/web/images/logo.png");
        // …and an adminhtml one, whose parent package is the adminhtml base.
        wb("app/code/Acme/Art/view/adminhtml/web/images/icon.png");
        w(
            "app/code/Acme/Art/view/adminhtml/web/css/source/_module.less",
            // First two climb out of `css/source/` into the module's images/.
            // The third stays inside the package and must be left alone.
            ".logo { background-image: url(../../images/logo.png); }\n\
             .icon { background-image: url(../../images/icon.png); }\n\
             .here { background-image: url(./inline.png); }\n",
        );
        wb("app/code/Acme/Art/view/adminhtml/web/css/source/inline.png");

        w(
            "app/design/adminhtml/Acme/admin/theme.xml",
            "<?xml version=\"1.0\"?>\n<theme><title>Acme Admin</title></theme>\n",
        );
        w("app/design/adminhtml/Acme/admin/registration.php", "<?php\n// adminhtml/Acme/admin\n");
        w(
            "app/design/adminhtml/Acme/admin/web/css/styles.less",
            "//@magento_import 'source/_module.less';\n",
        );
        w("lib/web/mage/requirejs/mixins.js", "// mixins stub\n");

        Fixture(root)
    }

    fn deploy(&self) {
        let out = Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["static", "deploy", "--root"])
            .arg(&self.0)
            .args(["--theme", "Acme/admin", "--no-parent", "en_US", "--out"])
            .arg(self.0.join("out"))
            .output()
            .expect("run magecommand");
        assert!(
            out.status.success(),
            "static deploy failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn styles(&self) -> String {
        let p = self.0.join("out/adminhtml/Acme/admin/en_US/css/styles.css");
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A url that climbs out of the package is repointed at the parent package
/// that owns the file, with `../` × (4 + the css directory's depth) — five
/// here, for a stylesheet in `css/`. The AREA comes from where the source file
/// lives, so the two assets resolve to different parents.
#[test]
fn urls_escaping_the_package_are_repointed_at_their_parent_package() {
    let fx = Fixture::new("escaping");
    fx.deploy();
    let css = fx.styles();

    assert!(
        css.contains("url(../../../../../base/Magento/base/default/Acme_Art/images/logo.png)"),
        "a `view/base` asset resolves to the BASE-area parent package; got:\n{css}"
    );
    assert!(
        css.contains("url(../../../../../adminhtml/Magento/base/default/Acme_Art/images/icon.png)"),
        "a `view/adminhtml` asset resolves to the adminhtml parent package; got:\n{css}"
    );
}

/// The control, and the half that matters most: a url that already resolves
/// inside the package is left exactly as written. Rewriting these would break
/// every working stylesheet on a store, so it is worth its own assertion.
#[test]
fn urls_resolving_inside_the_package_are_untouched() {
    let fx = Fixture::new("inside");
    fx.deploy();
    let css = fx.styles();

    assert!(
        css.contains("url(./inline.png)"),
        "an in-package url must survive verbatim; got:\n{css}"
    );
    assert!(
        !css.contains("Magento/base/default/Acme_Art/css/source/inline.png"),
        "…and must not be repointed at a parent package; got:\n{css}"
    );
}
