//! The compile must not depend on directory enumeration order.
//!
//! `read_dir` returns entries in whatever order the filesystem keeps them:
//! APFS and ext4 both hash filenames (different hashes), overlayfs layers its
//! own, and an ext4 without `dir_index` returns creation order. So a compile
//! that leans on that order is reproducible on the machine that wrote it and
//! subtly different everywhere else — exactly the failure that a local
//! "compile twice, compare" can never surface.
//!
//! Sorting every read would bury the question instead of answering it. So the
//! order is left as the filesystem gives it and PERMUTED here instead: the
//! same tree is compiled under several shuffle seeds and the emitted files
//! must be byte-identical every time.
//!
//! Covers the enumerations on the compile path: module discovery
//! (`engine::index`), the definition scans (`definitions.rs`), the primary di
//! glob (`codegen.rs` and `engine::di`) and the app/code registration walk
//! (`arguments.rs`).
//!
//! ONE deliberate exception, which this test cannot see and must not be
//! "fixed" when you meet it: the excluded-module-path regexes in
//! `metadata/<area>.php` DO follow filesystem order for app/code modules,
//! because Magento's own `NonComposerComponentRegistration.php` globs them
//! with GLOB_NOSORT. Reproducing that order is what makes the oracle match;
//! sorting would break it. Measured on a real store, shuffling permutes
//! `app/code/(?:Vendor/(?:ModuleA|ModuleB))` inside those regexes and nothing
//! else. The fixtures below have no `vendor/` tree, so they never emit that
//! block — which is precisely why they can assert "nothing moves" at all.
//!
//! So this test's claim is narrower than it looks, and that is the useful
//! claim: everything that is supposed to be order-independent is, and a NEW
//! dependence cannot appear without turning this red.

use std::collections::BTreeMap;
use std::path::Path;

use magecommand_engine::build::compute_outputs;
use magecommand_engine::definitions::Definitions;
use magequery_core::Magento;

/// Compile a root under `seed` and return its emitted files as path → body.
/// A map, not the emitted Vec: what lands on disk is one file per path, so
/// the Vec's own order is not part of the output.
fn compile_under(root: &Path, seed: u64) -> BTreeMap<String, String> {
    magequery_core::fsutil::set_shuffle_seed(seed);
    let magento = Magento::open(root).unwrap_or_else(|e| panic!("open {}: {e}", root.display()));
    let generated_code = root.join("generated/code");
    let mut defs = Definitions::scan(&magento, root, &generated_code);
    compute_outputs(&magento, &mut defs, root).files.into_iter().collect()
}

/// Report the first few differences, so a failure names the file rather than
/// just the count.
fn assert_same(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>, what: &str) {
    if a == b {
        return;
    }
    let mut lines = Vec::new();
    for path in a.keys().chain(b.keys()).collect::<std::collections::BTreeSet<_>>() {
        match (a.get(path), b.get(path)) {
            (Some(x), Some(y)) if x != y => lines.push(format!("  contents differ: {path}")),
            (Some(_), None) => lines.push(format!("  only in the first order: {path}")),
            (None, Some(_)) => lines.push(format!("  only in the second order: {path}")),
            _ => {}
        }
    }
    let shown = lines.len().min(10);
    panic!(
        "{what}: the compile depends on directory enumeration order \
         ({} file(s) differ)\n{}",
        lines.len(),
        lines[..shown].join("\n")
    );
}

/// Several vendors and several modules per vendor under `app/code` (the shape
/// the registration walk in `arguments.rs` iterates — one module can never
/// expose an order dependence), plus three primary `app/etc` di files that
/// each declare the SAME preference differently, so the glob order decides
/// the winner and any order dependence lands in `metadata/global.php`.
fn multi_module_root(root: &Path) {
    let put = |rel: &str, body: &str| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
    };
    const XSI: &str = "xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"";

    let mut modules = Vec::new();
    for vendor in ["Alpha", "Beta", "Gamma"] {
        for name in ["One", "Two", "Three"] {
            let module = format!("{vendor}_{name}");
            let dir = format!("app/code/{vendor}/{name}");
            put(
                &format!("{dir}/etc/module.xml"),
                &format!("<?xml version=\"1.0\"?>\n<config><module name=\"{module}\"/></config>\n"),
            );
            put(
                &format!("{dir}/registration.php"),
                &format!(
                    "<?php\n\\Magento\\Framework\\Component\\ComponentRegistrar::register(\n    \\Magento\\Framework\\Component\\ComponentRegistrar::MODULE,\n    '{module}',\n    __DIR__\n);\n"
                ),
            );
            // Constructor asks for a Factory that does not exist on disk, so
            // the compile has to generate one: every module contributes to
            // `code/` as well as to the metadata.
            put(
                &format!("{dir}/Model/Thing.php"),
                &format!(
                    "<?php\n\nnamespace {vendor}\\{name}\\Model;\n\nclass Thing implements \\Alpha\\One\\Api\\ThingInterface\n{{\n    public function __construct(private \\{vendor}\\{name}\\Model\\DepFactory $factory)\n    {{\n    }}\n}}\n"
                ),
            );
            put(
                &format!("{dir}/Model/Dep.php"),
                &format!("<?php\n\nnamespace {vendor}\\{name}\\Model;\n\nclass Dep\n{{\n}}\n"),
            );
            modules.push(module);
        }
    }
    put(
        "app/code/Alpha/One/Api/ThingInterface.php",
        "<?php\n\nnamespace Alpha\\One\\Api;\n\ninterface ThingInterface\n{\n}\n",
    );

    let entries: Vec<String> = modules.iter().map(|m| format!("'{m}' => 1")).collect();
    put("app/etc/config.php", &format!("<?php\nreturn ['modules' => [{}]];\n", entries.join(", ")));

    // The `{*di.xml,*/*di.xml}` glob: top-level sorted, then one level down.
    // Three declarations of one preference — only the order decides which of
    // the three survives into the compiled config.
    let pref = |target: &str| {
        format!(
            "<?xml version=\"1.0\"?>\n<config {XSI}>\n    <preference for=\"Alpha\\One\\Api\\ThingInterface\" type=\"{target}\"/>\n</config>\n"
        )
    };
    put("app/etc/di.xml", &pref("Alpha\\One\\Model\\Thing"));
    put("app/etc/zz_di.xml", &pref("Beta\\Two\\Model\\Thing"));
    // The subdirectory level always merges after the top level whatever the
    // order, so it must NOT re-declare the contested preference — it would
    // win every permutation and mask the thing under test.
    put(
        "app/etc/sub/di.xml",
        &format!(
            "<?xml version=\"1.0\"?>\n<config {XSI}>\n    <preference for=\"Alpha\\\\One\\\\Api\\\\OtherInterface\" type=\"Gamma\\\\Three\\\\Model\\\\Thing\"/>\n</config>\n"
        ),
    );
}

/// Every seed must produce the same bytes — on the fixtures AND on a root
/// with enough modules for the order to vary.
///
/// One test function on purpose: the shuffle seed is process-global, so two
/// of these running in parallel would read each other's seed.
#[test]
fn compile_output_is_independent_of_directory_order() {
    const SEEDS: [u64; 3] = [1, 7, 4242];

    let tmp = tempfile::tempdir().unwrap();
    multi_module_root(tmp.path());
    let baseline = compile_under(tmp.path(), 0);
    assert!(
        baseline.len() > 9,
        "the synthetic root should emit something to compare ({} files)",
        baseline.len()
    );
    for seed in SEEDS {
        assert_same(&baseline, &compile_under(tmp.path(), seed), &format!("multi-module root, seed {seed}"));
    }

    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/compile-fixtures");
    let mut cases: Vec<_> = std::fs::read_dir(&fixtures)
        .expect("fixtures dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("in/app/etc/config.php").is_file())
        .collect();
    cases.sort();
    assert!(cases.len() > 20, "expected the fixture corpus, found {}", cases.len());

    for case in &cases {
        let name = case.file_name().unwrap().to_string_lossy().into_owned();
        let root = case.join("in");
        let baseline = compile_under(&root, 0);
        for seed in SEEDS {
            assert_same(&baseline, &compile_under(&root, seed), &format!("fixture {name}, seed {seed}"));
        }
    }

    magequery_core::fsutil::set_shuffle_seed(0);
}
