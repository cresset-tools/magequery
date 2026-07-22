//! The full compile compute, assembled into an in-memory output set — the single
//! source of truth shared by `compile` (writes everything) and `watch` (diffs
//! against the previous output and writes only the delta).
//!
//! Keeping ONE compute path is what guarantees `watch` is byte-for-byte a cold
//! `compile`: both call [`compute_outputs`], which returns every file the compile
//! would write under `generated/`, keyed by its path relative to `generated/`
//! (`metadata/<name>` or `code/<rel>`). Neither the scan nor any write happens
//! here — the caller owns the scanned [`Definitions`] (so `watch` can reuse it
//! across edits) and the disk.

use std::path::Path;

use magequery_core::Magento;

use crate::areaconfig::{self, AREA_CODES};
use crate::codegen;
use crate::definitions::Definitions;
use crate::interception;
use crate::metadata;
use crate::phpexport;
use crate::pluginlist;

/// Every file a compile emits under `generated/`, in memory, plus the diagnostics
/// the run produced. `files` keys are relative to `generated/` (`metadata/global.php`,
/// `code/Magento/…/Interceptor.php`), so a writer just prepends `root/generated/`.
pub struct CompileOutputs {
    pub files: Vec<(String, String)>,
    /// Static-analysis / codegen findings, surfaced to the user (not fatal).
    pub findings: Vec<String>,
    /// Class names referenced in DI but unresolvable via any autoload map.
    pub unresolved: Vec<String>,
}

/// Run the full compute over an already-scanned `defs` and return the complete
/// output set without touching disk. `defs` is extended in place
/// (`extend_hierarchy` pulls in ancestor closures); a caller that reuses `defs`
/// across calls (the `watch` server) must pass a fresh clone each time, since the
/// extend set depends on the current DI config.
pub fn compute_outputs(magento: &Magento, defs: &mut Definitions, root: &Path) -> CompileOutputs {
    compute_outputs_opts(magento, defs, root, false)
}

/// Like [`compute_outputs`], with `fused` selecting the fused-interceptor
/// renderer (`di compile --fused`) for global-only classes.
pub fn compute_outputs_opts(
    magento: &Magento,
    defs: &mut Definitions,
    root: &Path,
    fused: bool,
) -> CompileOutputs {
    let _prof = std::env::var_os("MAGECOMMAND_PROFILE").is_some();
    macro_rules! ilap {
        ($t:expr, $label:expr) => {
            if _prof {
                eprintln!("  [profile]   · {:<26} {:>8.1} ms", $label, $t.elapsed().as_secs_f64() * 1000.0);
                $t = std::time::Instant::now();
            }
        };
    }
    let mut _it = std::time::Instant::now();

    // Reflect the ancestor closure of every scanned class, plus the DI names the
    // scan walk can't reach on its own: preference TARGETS and PLUGIN CLASSES
    // (Magento reflects a plugin class via autoload during interception, even
    // when its file sits in a scan-excluded path).
    let mut resolve_keys: Vec<String> = Vec::new();
    for (area, _) in AREA_CODES {
        let export = magento.di_export_ref(area);
        resolve_keys.extend(export.preferences.iter().map(|p| p.for_type.as_str().to_owned()));
        resolve_keys.extend(
            export.plugins.iter().filter_map(|p| p.class.as_ref()).map(|c| c.as_str().to_owned()),
        );
    }
    let unresolved = defs.extend_hierarchy(magento, root, resolve_keys);
    ilap!(_it, "extend_hierarchy");

    let mut files: Vec<(String, String)> = Vec::new();
    let mut findings: Vec<String> = Vec::new();

    // app_action_list metadata.
    let list = metadata::app_action_list(magento);
    files.push(("metadata/app_action_list.php".to_owned(), phpexport::to_php_file(&list)));

    // Magento generates code and aggregates the config in a specific order that
    // is really a fixpoint: it emits factories/proxies, then INTERCEPTORS that
    // may target them, then its ClassesScanner picks the whole `generated/code`
    // tree up before the config Reader runs — so every generated name lands in
    // the `arguments` universe (and thus interception.php's has-plugins map and
    // InterceptorSubstitution). A from-empty compile clears `generated/code`
    // before scanning (like `setup:di:compile`), so a single pass is blind to
    // the classes it is about to emit — leaving interception globally dead at
    // runtime. We reach the same fixpoint by folding each round's output back
    // into the scan universe and recomputing until it stops growing: round 1
    // sees the source classes, round 2 the generated factories/proxies (so
    // second-order interceptors on them are emitted), round 3 confirms closure.
    //
    // A reproduction/warm run whose scan already read an archived
    // `generated/code` has every generated class from the start, so
    // `add_generated_code` adds nothing and the loop body never runs — that path
    // stays byte-identical with zero extra work.
    let mut interception = interception::interception_map(magento, defs);
    let mut area_files = areaconfig::build_all_area_files(magento, defs, root);
    let mut code =
        codegen::generate_code(magento, defs, root.to_path_buf(), &area_files, &interception, fused);
    ilap!(_it, "areas + code (round 1)");
    // Bounded: the generated universe is finite and each round only adds classes,
    // so this converges in a couple of rounds; the cap is a runaway backstop.
    for _round in 0..8 {
        if defs.add_generated_code(&code.files, root) == 0 {
            break;
        }
        interception = interception::interception_map(magento, defs);
        area_files = areaconfig::build_all_area_files(magento, defs, root);
        code = codegen::generate_code(
            magento,
            defs,
            root.to_path_buf(),
            &area_files,
            &interception,
            fused,
        );
        ilap!(_it, "areas + code (fold round)");
    }

    for ca in &area_files {
        findings.extend(ca.file.findings.iter().cloned());
        files.push((format!("metadata/{}.php", ca.code), ca.rendered.clone()));
    }
    files.push(("metadata/interception.php".to_owned(), interception::render(&interception)));

    // plugin-list metadata files (over the now-complete universe).
    let plugin_lists = pluginlist::generate(magento, defs);
    for (name, content) in &plugin_lists.files {
        files.push((format!("metadata/{name}"), content.clone()));
    }
    findings.extend(plugin_lists.findings);
    ilap!(_it, "plugin-lists");

    for (rel, content) in code.files {
        files.push((format!("code/{rel}"), content));
    }
    findings.extend(code.findings);

    CompileOutputs { files, findings, unresolved }
}
