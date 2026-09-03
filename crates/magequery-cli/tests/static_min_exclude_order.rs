//! `requirejs-min-resolver.js` and the order of `dev/js/minify_exclude`.
//!
//! Not built on Windows (see the sibling `di compile` fixtures).
//!
//! Modules append their own JS minification excludes from a DI plugin on
//! `Magento\Framework\View\Asset\Minification::getExcludes`, and those appends
//! land inside `requirejs-min-resolver.js` — a file every JS bundle then
//! embeds. Their ORDER is therefore deployed bytes.
//!
//! The order depends on the interception kind:
//!
//! - an **`after`** plugin receives the previous result and appends to it, so a
//!   chain of them applies in execution order (ascending `sortOrder`);
//! - an **`around`** plugin appends once `$proceed` has returned, so the
//!   innermost runs first — reverse execution order — and all of those land
//!   before any `after` sees the array.
//!
//! Reversing unconditionally (right only for `around`) shipped a real store's
//! two `after` plugins backwards. The rest of the static suite is gated on an
//! external corpus and skips without it, so nothing caught this; the fixture
//! below is self-contained precisely so it runs everywhere.

#![cfg(not(windows))]

use std::path::PathBuf;
use std::process::Command;

struct Fixture(PathBuf);

impl Fixture {
    /// A root with a frontend theme and two modules that each append one
    /// exclude. `kinds` picks the interception kind per module, so the same
    /// fixture covers the after-chain, the around-chain, and a mix.
    fn new(name: &str, kinds: [&str; 2]) -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("mq-excl-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let w = |rel: &str, content: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        };

        w(
            "app/etc/config.php",
            "<?php\nreturn ['modules' => ['Acme_First' => 1, 'Acme_Second' => 1]];\n",
        );
        w(
            "app/design/frontend/Acme/theme/theme.xml",
            "<?xml version=\"1.0\"?>\n<theme><title>Acme</title></theme>\n",
        );
        w(
            "app/design/frontend/Acme/theme/registration.php",
            "<?php\n// frontend/Acme/theme\n",
        );
        // The resolver emits `mage/requirejs/mixins.js` as a verbatim copy of the
        // lib source, so the deploy needs one to read.
        w("lib/web/mage/requirejs/mixins.js", "// mixins stub\n");

        // Declaration order in config.php is the tie-break for equal sortOrder,
        // so `First` executes before `Second`.
        for (i, (module, kind)) in [("First", kinds[0]), ("Second", kinds[1])].iter().enumerate() {
            let literal = format!("acme{}.example.com", i + 1);
            w(
                &format!("app/code/Acme/{module}/etc/module.xml"),
                &format!(
                    "<?xml version=\"1.0\"?>\n<config><module name=\"Acme_{module}\"/></config>\n"
                ),
            );
            let body = if *kind == "around" {
                format!(
                    "    public function aroundGetExcludes($subject, callable $proceed, \
                     $contentType)\n    {{\n        $result = $proceed($contentType);\n        \
                     if ($contentType !== 'js') {{\n            return $result;\n        }}\n        \
                     $result[] = '{literal}';\n\n        return $result;\n    }}\n"
                )
            } else {
                format!(
                    "    public function afterGetExcludes($subject, $result, $contentType)\n    \
                     {{\n        if ($contentType !== 'js') {{\n            return $result;\n        \
                     }}\n        $result[] = '{literal}';\n\n        return $result;\n    }}\n"
                )
            };
            w(
                &format!("app/code/Acme/{module}/Plugin/Excludes.php"),
                &format!(
                    "<?php\nnamespace Acme\\{module}\\Plugin;\n\nclass Excludes\n{{\n{body}}}\n"
                ),
            );
            w(
                &format!("app/code/Acme/{module}/etc/di.xml"),
                &format!(
                    "<?xml version=\"1.0\"?>\n<config>\n    \
                     <type name=\"Magento\\Framework\\View\\Asset\\Minification\">\n        \
                     <plugin name=\"acme_{}\" type=\"Acme\\{module}\\Plugin\\Excludes\"/>\n    \
                     </type>\n</config>\n",
                    module.to_lowercase()
                ),
            );
        }
        Fixture(root)
    }

    /// The excludes as they appear in the emitted resolver, in order.
    fn exclude_order(&self) -> Vec<String> {
        let out = Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["static", "requirejs", "--root"])
            .arg(&self.0)
            .args(["--theme", "Acme/theme", "--out"])
            .arg(self.0.join("out"))
            .output()
            .expect("run magecommand");
        assert!(
            out.status.success(),
            "static requirejs failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let js = std::fs::read_to_string(self.0.join("out/requirejs-min-resolver.js"))
            .expect("resolver emitted");
        let mut found = Vec::new();
        for token in ["acme1.example.com", "acme2.example.com"] {
            if let Some(at) = js.find(token) {
                found.push((at, token.to_owned()));
            }
        }
        found.sort();
        found.into_iter().map(|(_, t)| t).collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The regression: two `after` plugins apply in EXECUTION order.
#[test]
fn after_plugins_append_in_execution_order() {
    let fx = Fixture::new("after", ["after", "after"]);
    assert_eq!(
        fx.exclude_order(),
        vec!["acme1.example.com", "acme2.example.com"],
        "an `after` chain appends in ascending order — reversing it is the bug this pins"
    );
}

/// The control: two `around` plugins unwind innermost-first, so their appends
/// come out reversed. Without this the fix could be "never reverse", which is
/// just the original bug pointing the other way.
#[test]
fn around_plugins_append_innermost_first() {
    let fx = Fixture::new("around", ["around", "around"]);
    assert_eq!(
        fx.exclude_order(),
        vec!["acme2.example.com", "acme1.example.com"],
        "an `around` chain appends as it unwinds — the innermost plugin first"
    );
}

/// Mixed: the around chain has returned before any `after` runs, so the around
/// append precedes the after one regardless of declaration order.
#[test]
fn around_appends_land_before_after_appends() {
    let fx = Fixture::new("mixed", ["after", "around"]);
    assert_eq!(
        fx.exclude_order(),
        vec!["acme2.example.com", "acme1.example.com"],
        "the around append lands while the chain unwinds, before the after plugin sees the array"
    );
}
