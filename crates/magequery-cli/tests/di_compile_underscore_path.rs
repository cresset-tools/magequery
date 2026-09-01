//! `di compile` and class names containing underscores.
//!
//! Not built on Windows, for the same reason as the sibling `di compile`
//! fixture: the metadata filenames join their cache scope with `|`, which is a
//! reserved character in a Windows filename, so the command dies before it
//! writes anything.
//!
//! Magento decides a generated file's path in
//! `Magento\Framework\Code\Generator\Io::generateResultFileName`:
//!
//! ```php
//! str_replace(['\\', '_'], '/', $className) . '.php'
//! ```
//!
//! Both the namespace separator AND the underscore become directory
//! separators — PSR-0, which the generated-code autoloader still expects.
//! Splitting only on `\` put `…\Renderer\Configurable_2_1\Interceptor` at
//! `Renderer/Configurable_2_1/Interceptor.php` while Magento writes
//! `Renderer/Configurable/2/1/Interceptor.php`, so the class the compile had
//! just emitted was at a path nothing would load — silently, since the file
//! existed and only the autoloader disagreed. Three live stores carry such a
//! class (Wyomind's `Configurable_2_1` renderers).
//!
//! The class NAME keeps its underscores; only the path is split.

#![cfg(not(windows))]

use std::path::PathBuf;
use std::process::Command;

/// A throwaway Magento root with one module, an underscore-named class, and a
/// plugin on it (so the compile has a reason to emit an interceptor).
struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mq-underscore-path");
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str, content: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write("app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Repro' => 1]];\n");
        write(
            "app/code/Acme/Repro/etc/module.xml",
            "<?xml version=\"1.0\"?>\n<config><module name=\"Acme_Repro\"/></config>\n",
        );
        // The underscore-named subject, mirroring Wyomind's `Configurable_2_1`.
        write(
            "app/code/Acme/Repro/Block/Configurable_2_1.php",
            "<?php\nnamespace Acme\\Repro\\Block;\n\nclass Configurable_2_1\n{\n    \
             public function render()\n    {\n        return 'x';\n    }\n}\n",
        );
        write(
            "app/code/Acme/Repro/Plugin/Renderer.php",
            "<?php\nnamespace Acme\\Repro\\Plugin;\n\nclass Renderer\n{\n    \
             public function beforeRender(\\Acme\\Repro\\Block\\Configurable_2_1 $subject)\n    \
             {\n        return null;\n    }\n}\n",
        );
        write(
            "app/code/Acme/Repro/etc/di.xml",
            "<?xml version=\"1.0\"?>\n<config>\n    \
             <type name=\"Acme\\Repro\\Block\\Configurable_2_1\">\n        \
             <plugin name=\"acme_renderer\" type=\"Acme\\Repro\\Plugin\\Renderer\"/>\n    \
             </type>\n</config>\n",
        );
        Fixture(root)
    }

    fn compile(&self) {
        let out = Command::new(env!("CARGO_BIN_EXE_magecommand"))
            .args(["di", "compile", "--root"])
            .arg(&self.0)
            .output()
            .expect("run magecommand");
        assert!(
            out.status.success(),
            "di compile failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn generated(&self, rel: &str) -> PathBuf {
        self.0.join("generated/code").join(rel)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn underscores_in_a_class_name_become_directories() {
    let fx = Fixture::new();
    fx.compile();

    let psr0 = fx.generated("Acme/Repro/Block/Configurable/2/1/Interceptor.php");
    let flat = fx.generated("Acme/Repro/Block/Configurable_2_1/Interceptor.php");

    assert!(
        psr0.is_file(),
        "the interceptor must land where Magento's Io::generateResultFileName puts it \
         (underscores split into directories), so the generated-code autoloader finds it"
    );
    assert!(
        !flat.exists(),
        "the flat path is what the autoloader will NOT look for; emitting it too would ship \
         a duplicate class"
    );

    // The path is split; the class name itself keeps its underscores.
    let src = std::fs::read_to_string(&psr0).expect("read interceptor");
    assert!(
        src.contains("class Interceptor extends \\Acme\\Repro\\Block\\Configurable_2_1"),
        "the subject must keep its underscores in PHP; got:\n{src}"
    );
}
