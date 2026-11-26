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

/// A compile result Magento accepts that breaks the store at runtime, which a
/// CI gate can fail on (`di compile --fail-on`, magebuild's `fail_on`).
/// What `setup:di:compile` itself refuses is refused always, not listed here
/// ([`crate::rejects`]); missing classes elsewhere in di.xml are doctor's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailOn {
    /// A type whose di.xml arguments the compiled config leaves out
    /// ([`CompileOutputs::lost_arguments`]).
    LostArguments,
}

impl FailOn {
    pub const ALL: [(FailOn, &'static str); 1] = [(FailOn::LostArguments, "lost-arguments")];

    pub fn name(self) -> &'static str {
        Self::ALL.iter().find(|(k, _)| *k == self).map(|(_, n)| *n).expect("every kind is named")
    }

    pub fn names() -> Vec<&'static str> {
        Self::ALL.iter().map(|(_, n)| *n).collect()
    }
}

impl std::str::FromStr for FailOn {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Self::ALL
            .iter()
            .find(|(_, n)| *n == s)
            .map(|(k, _)| *k)
            .ok_or_else(|| format!("unknown kind {s:?}, expected one of: {}", Self::names().join(", ")))
    }
}

/// Every file a compile emits under `generated/`, in memory, plus the diagnostics
/// the run produced. `files` keys are relative to `generated/` (`metadata/global.php`,
/// `code/Magento/…/Interceptor.php`), so a writer just prepends `root/generated/`.
pub struct CompileOutputs {
    pub files: Vec<(String, String)>,
    /// Static-analysis / codegen findings, surfaced to the user (not fatal).
    pub findings: Vec<String>,
    /// Class names referenced in DI but unresolvable via any autoload map.
    pub unresolved: Vec<String>,
    /// Types left without an `arguments` row although di.xml configures
    /// arguments for them, sorted and deduped across areas. Magento builds
    /// them by runtime reflection, WITHOUT that configuration — see
    /// `AreaFile::lost_arguments`.
    /// Each entry is `(type, why its key went)`.
    pub lost_arguments: Vec<(String, String)>,
    /// What `setup:di:compile` would refuse to compile. Non-empty means the
    /// real compile stops, so `files` is not a tree Magento would write.
    pub rejections: Vec<crate::rejects::Rejection>,
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
/// Class names referenced by `xsi:type="const"` values anywhere in an argument
/// tree, so they can be resolved for constant evaluation.
///
/// `\Foo\Bar::BAZ` -> `Foo\Bar`. A bare `SOME_CONST` (no `::`) names a global
/// constant, not a class, and is skipped.
fn collect_const_classes(value: &magequery_core::ArgValue, out: &mut Vec<String>) {
    match value {
        magequery_core::ArgValue::Scalar { xsi_type, text } if xsi_type == "const" => {
            if let Some((class, _)) = text.rsplit_once("::") {
                let class = class.trim().trim_start_matches('\\');
                if !class.is_empty() {
                    out.push(class.to_owned());
                }
            }
        }
        magequery_core::ArgValue::Array(items) => {
            for item in items {
                collect_const_classes(&item.value, out);
            }
        }
        _ => {}
    }
}

pub fn compute_outputs_opts(
    magento: &Magento,
    defs: &mut Definitions,
    root: &Path,
    fused: bool,
) -> CompileOutputs {
    compute_outputs_with(magento, defs, root, fused, &crate::lazytypes::Target::default())
}

/// Like [`compute_outputs_opts`], for the store `target` describes: its PHP
/// version and platform release, where they differ from what the checkout says
/// — see [`crate::lazytypes`].
pub fn compute_outputs_with(
    magento: &Magento,
    defs: &mut Definitions,
    root: &Path,
    fused: bool,
    target: &crate::lazytypes::Target,
) -> CompileOutputs {
    let lazy = crate::lazytypes::LazyPass::detect(root, target);
    // A caller that asked for a layout the platform cannot read was refused
    // before this; anything else gets the platform's own.
    let layout = crate::arealayout::AreaLayout::resolve(root, target)
        .unwrap_or(crate::arealayout::AreaLayout::Full);
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
    // scan walk can't reach on its own: preference TARGETS, PLUGIN CLASSES
    // (Magento reflects a plugin class via autoload during interception, even
    // when its file sits in a scan-excluded path) and PLUGIN TARGETS. The
    // plugin-list generator asks `Relations` for a target's parents through the
    // autoloader too, which does not care whether the owning module is enabled:
    // an enabled module's plugin on a DISABLED module's class still inherits
    // every plugin declared on that class's interfaces. Unseeded, the target
    // had no record, so no parents — on one store an Amasty router plugged by
    // an enabled module lost the Csp plugin it inherits from RouterInterface.
    // Each area's merged DI export is built on first use; everything below
    // uses all of them, so build them side by side instead of one at a time.
    // The global one first and alone: it also builds the index they all share,
    // behind one lock, and a pool thread waiting inside that build must not be
    // handed another area that then waits on the same lock.
    {
        use rayon::prelude::*;
        magento.di_export_ref(magequery_core::Area::Global);
        AREA_CODES.par_iter().for_each(|(area, _)| {
            magento.di_export_ref(*area);
        });
    }
    let mut resolve_keys: Vec<String> = Vec::new();
    for (area, _) in AREA_CODES {
        let export = magento.di_export_ref(area);
        resolve_keys.extend(export.preferences.iter().map(|p| p.for_type.as_str().to_owned()));
        resolve_keys.extend(
            export.plugins.iter().filter_map(|p| p.class.as_ref()).map(|c| c.as_str().to_owned()),
        );
        resolve_keys.extend(export.plugins.iter().map(|p| p.target.as_str().to_owned()));
    }
    // …and the classes named by `xsi:type="const"` argument values. PHP evaluates
    // `\Magento\Paypal\Model\Config::METHOD_WPP_EXPRESS` through the AUTOLOADER, which
    // does not care whether the owning module is enabled — a composer-installed
    // file resolves either way. Without seeding these, a const whose class sits
    // outside the scan universe (a disabled module, or a classmap-only package)
    // evaluated to NULL: on one store an enabled GraphQL module's
    // `allowedPaymentMethodCodes` came out `'paypal_express' => NULL` where the
    // oracle has `'paypal_express' => 'paypal_express'`.
    for (area, _) in AREA_CODES {
        for decl in &magento.di_export_ref(area).arguments {
            collect_const_classes(&decl.value, &mut resolve_keys);
        }
    }

    // …and the classes named by a CLASS CONSTANT in a CONSTRUCTOR DEFAULT.
    // Exactly the reasoning of the di.xml `const` seeding above, one level
    // deeper: PHP evaluates `Foo::BAR` through the autoloader when it reflects
    // the default, and the autoloader does not care whether the owning module
    // is enabled. Unseeded, `DefsLookup::class_const` cannot find the owning
    // class, the default fails to fold, and the whole class's arguments row is
    // left out — losing the cached rows for every OTHER argument too.
    //
    // Cheap: only a default that mentions `::` can name a class constant, and
    // the parse is the same one `eval_default` performs later anyway.
    for record in defs.classes.values() {
        let Some(ctor) =
            record.meta.methods.iter().find(|m| m.name.eq_ignore_ascii_case("__construct"))
        else {
            continue;
        };
        let ns = record.meta.fqcn.rsplit_once('\\').map(|(n, _)| n).unwrap_or("");
        for param in &ctor.params {
            let Some(default) = &param.default else { continue };
            if !default.contains("::") {
                continue;
            }
            resolve_keys.extend(
                magecommand_php::constexpr::parse_const_expr(default, ns, &record.meta.uses)
                    .classes,
            );
        }
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
    ilap!(_it, "interception map");
    let mut area_files = areaconfig::build_all_area_files(magento, defs, root, lazy);
    ilap!(_it, "area files");
    let mut code =
        codegen::generate_code(magento, defs, root.to_path_buf(), &area_files, &interception, fused);
    ilap!(_it, "code (round 1)");
    // Bounded: the generated universe is finite and each round only adds classes,
    // so this converges in a couple of rounds; the cap is a runaway backstop.
    for _round in 0..8 {
        let added = defs.add_generated_code(&code.files, root);
        ilap!(_it, "fold (re-parse)");
        if added == 0 {
            break;
        }
        // A full round costs the area files, and most of what the next one
        // would discover needs none of them: a proxy of a factory the last
        // round generated, an interceptor on a generated class. Find those
        // with one area-free round first, so the full round below usually runs
        // over the closed universe and is the last. It is a head start only:
        // each file is what the full round would emit over these definitions,
        // and the full round still decides whether the universe is closed
        // (anything only the area sweep asks for sends the loop round again).
        // This round rebuilds both, and what the last one built is folded in
        // already: let it go now, so the rebuild below reuses its memory
        // instead of asking for as much again while it is still held.
        drop_off_thread(std::mem::take(&mut area_files));
        drop_off_thread(std::mem::replace(
            &mut code,
            codegen::GeneratedCode { files: Vec::new(), findings: Vec::new() },
        ));
        interception = interception::interception_map(magento, defs);
        let new = codegen::generate_new_without_areas(
            magento,
            defs,
            root.to_path_buf(),
            &interception,
            fused,
        );
        if defs.add_generated_code(&new.files, root) > 0 {
            interception = interception::interception_map(magento, defs);
        }
        ilap!(_it, "area-free round");
        area_files = areaconfig::build_all_area_files(magento, defs, root, lazy);
        ilap!(_it, "area files");
        code = codegen::generate_code(
            magento,
            defs,
            root.to_path_buf(),
            &area_files,
            &interception,
            fused,
        );
        ilap!(_it, "code (fold round)");
    }

    // Rendered only now, once: the rounds above rebuild the areas, and a
    // diff needs global's final file.
    let rendered: Vec<String> = {
        use rayon::prelude::*;
        let global = area_files.iter().find(|ca| ca.code == "global").map(|ca| &ca.file);
        area_files
            .par_iter()
            .map(|ca| match (layout, global) {
                (crate::arealayout::AreaLayout::Diff, Some(global)) if ca.code != "global" => {
                    crate::arealayout::render_diff(&ca.file, global)
                }
                _ => ca.file.render(),
            })
            .collect()
    };
    let mut lost_arguments: std::collections::BTreeSet<(String, String)> = Default::default();
    for (ca, rendered) in area_files.into_iter().zip(rendered) {
        findings.extend(ca.file.findings.iter().cloned());
        lost_arguments.extend(ca.file.lost_arguments.iter().cloned());
        files.push((format!("metadata/{}.php", ca.code), rendered));
        drop_off_thread(ca.file);
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

    // `extend_hierarchy` ran in round 1, when `generated/code` was still empty,
    // so it counted every not-yet-emitted factory and proxy as unresolvable. The
    // fold loop has since generated them. Report only what is STILL unknown —
    // a from-empty compile otherwise ends with a four-digit "unresolvable" note
    // that is almost entirely its own output (1139 of them on a real store),
    // which reads like a catastrophe in a build log and buries the handful of
    // names that genuinely have no autoload entry.
    let unresolved: Vec<String> =
        unresolved.into_iter().filter(|name| !defs.contains(name)).collect();

    let lost_arguments = lost_arguments.into_iter().collect();
    let rejections = crate::rejects::check(magento, defs, root);
    ilap!(_it, "rejections");
    CompileOutputs { files, findings, unresolved, lost_arguments, rejections }
}

/// Free a large value without making the caller wait for it. The area files
/// and the per-scope chains are hundreds of thousands of small allocations
/// each; freeing them between rounds was a quarter second of the compile spent
/// on the one thread everything else waits for.
pub(crate) fn drop_off_thread<T: Send + 'static>(value: T) {
    rayon::spawn(move || drop(value));
}
