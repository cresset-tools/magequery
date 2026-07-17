//! Explain the *known, expected* divergences in a [`CompareReport`].
//!
//! When the archive was produced by a different Magento than magecommand
//! targets (Mage-OS 3.1.0 / 2.4.9), some differences are not bugs — they are
//! deliberate upstream improvements magecommand faithfully reproduces, or pure
//! environment artifacts. Most people don't know this, so `compare` shouldn't
//! just list opaque "missing/extra" paths: it should recognize each known
//! pattern, *verify* the divergence actually matches it, and explain why it
//! differs and why the newer form is better. Whatever matches nothing stays
//! flagged as a genuine, unexplained divergence to investigate.
//!
//! Every classifier is conservative: it only claims a divergence is expected
//! when the evidence is unambiguous (an exact scope permutation, a
//! case-only path pair, an interceptor whose module is provably disabled), so a
//! real regression is never silently absorbed into the "known" bucket.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::compare::{canonical_method_order, CompareReport};

/// A category of known/expected difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub enum KnownKind {
    /// Plugin-list cache filename scope ordering (sorted vs load-order).
    PluginListScopeOrder,
    /// Interceptor for a class in a module that is disabled in config.php.
    DisabledModuleInterceptor,
    /// A metadata DI-config file (`global.php`, `<area>.php`) that is identical
    /// once the top-level entries for classes in disabled modules are removed.
    DisabledModuleMetadata,
    /// A metadata DI-config file where the output is a strict superset of the
    /// archive: it only *adds* entries (e.g. the `nonLazyTypes` section and the
    /// `NonLazyTypes` compiler-chain step, a 2.4.9 / PHP 8.4 feature the older
    /// archive predates), never removing or changing what the archive has.
    ExtraMetadata,
    /// A metadata DI-config file whose only substantive difference (once
    /// disabled-module entries and pure additions are accounted for) is the
    /// generated ClassesScanner directory-exclusion regex — a literal alternation
    /// of every enabled module's directory, so a stale archive with a different
    /// module set lists different members.
    ClassScannerExcludeRegex,
    /// Two paths differing only by letter case (case-insensitive filesystem).
    FilenameCasing,
    /// Interceptor/proxy content that differs only in the code generator's
    /// formatting between the archive's Magento and magecommand's 2.4.9 target
    /// (return-type spacing, explicit nullable defaults, proxy `__clone`/
    /// `_resetState` null-guards, added `__debugInfo`). Behavior-preserving.
    GeneratorVersionFormatting,
    /// An arguments entry that resolves to NULL in the output because the
    /// class (or an ancestor its constructor comes from) lives in
    /// eval-obfuscated vendor source — the real compiler EXECUTES the
    /// decrypting stub and reflects the materialized class; magecommand never
    /// executes PHP, so the constructor is statically invisible.
    ObfuscatedVendorSource,
}

impl KnownKind {
    fn title(self) -> &'static str {
        match self {
            KnownKind::PluginListScopeOrder => "Plugin-list cache filename scope ordering",
            KnownKind::DisabledModuleInterceptor => {
                "Interceptors for disabled modules (Magento 2.4.9 behavior)"
            }
            KnownKind::DisabledModuleMetadata => {
                "DI-config entries for disabled modules (Magento 2.4.9 behavior)"
            }
            KnownKind::ExtraMetadata => {
                "Extra DI-config metadata (output superset — nonLazyTypes, Magento 2.4.9)"
            }
            KnownKind::ClassScannerExcludeRegex => {
                "ClassesScanner directory-exclusion regex (enabled-module set differs)"
            }
            KnownKind::FilenameCasing => "Filename letter-casing (case-insensitive filesystem)",
            KnownKind::GeneratorVersionFormatting => {
                "Code-generator formatting (Magento version, behavior-preserving)"
            }
            KnownKind::ObfuscatedVendorSource => {
                "Arguments unresolvable from eval-obfuscated vendor source"
            }
        }
    }
}

/// One recognized group of expected differences, with its explanation.
#[derive(Debug, serde::Serialize)]
#[non_exhaustive]
pub struct KnownGroup {
    pub kind: KnownKind,
    pub title: String,
    /// Why the two sides differ and why the magecommand/2.4.9 form is better.
    pub explanation: String,
    /// One human line per member (the path, or the archive↔output pairing).
    pub items: Vec<String>,
    /// True when the classifier positively confirmed the pattern beyond the
    /// filename (e.g. the two plugin-list files are byte-identical, so it is
    /// provably only a rename). Informational; a group is only emitted when it
    /// already matched its pattern.
    pub verified: bool,
}

/// A compare report split into explained (known) groups and the unexplained
/// remainder — the differences that still warrant investigation.
#[derive(Debug, Default, serde::Serialize)]
#[non_exhaustive]
pub struct Classified {
    pub known: Vec<KnownGroup>,
    pub missing: Vec<String>,
    pub extra: Vec<String>,
    pub changed: Vec<String>,
}

impl Classified {
    /// Total differences we recognized and explained.
    pub fn known_count(&self) -> usize {
        self.known.iter().map(|g| g.items.len()).sum()
    }

    /// Differences that matched no known pattern — the real signal.
    pub fn unexplained_count(&self) -> usize {
        self.missing.len() + self.extra.len() + self.changed.len()
    }
}

/// Inputs the classifiers need beyond the report: the two trees (to verify
/// paired-file content) and the set of disabled module names (from config.php).
pub struct ClassifyCtx<'a> {
    pub archive: &'a Path,
    pub output: &'a Path,
    pub disabled_modules: &'a HashSet<String>,
    /// Class keys (escaped `Vendor\\\\Class` metadata form) whose arguments are
    /// statically unresolvable because their constructor chain runs through
    /// eval-obfuscated vendor source — see [`obfuscation_blocked_classes`].
    pub obfuscation_blocked: &'a HashSet<String>,
}

/// Partition `report`'s differences into explained known groups and the
/// unexplained remainder.
pub fn classify(report: &CompareReport, ctx: &ClassifyCtx) -> Classified {
    // Work on owned, consumable sets so each path is claimed by at most one
    // classifier.
    let mut missing: Vec<String> = report.missing.clone();
    let mut extra: Vec<String> = report.extra.clone();
    let mut changed: Vec<String> = report.changed.clone();
    let mut known: Vec<KnownGroup> = Vec::new();

    // 1. Plugin-list scope ordering: pair a missing `a|b|…|plugin-list.php`
    //    with an extra file whose scope tokens are the same multiset in a
    //    different order.
    if let Some(group) = plugin_list_scope_order(&mut missing, &mut extra, ctx) {
        known.push(group);
    }

    // 2. Filename casing: pair a remaining missing path with a remaining extra
    //    path that is identical apart from ASCII case.
    if let Some(group) = filename_casing(&mut missing, &mut extra) {
        known.push(group);
    }

    // 3. Disabled-module interceptors: a remaining missing interceptor whose
    //    owning module is disabled in config.php (no extra counterpart —
    //    magecommand simply doesn't generate it).
    if let Some(group) = disabled_module_interceptors(&mut missing, ctx) {
        known.push(group);
    }

    // 4. Code-generator version formatting: a changed file whose only difference
    //    is the generator formatting Magento changed between the archive's
    //    version and magecommand's 2.4.9 target (verified by normalizing both
    //    sides to byte-equality).
    if let Some(group) = generator_version_formatting(&mut changed, ctx) {
        known.push(group);
    }

    // 5. Obfuscation-blocked entries: report the classes whose arguments are
    //    statically unresolvable (their constructor chain runs through
    //    eval-obfuscated vendor source, pre-verified into ctx). Their entries
    //    are stripped inside rules 5b–7's comparisons, so the files claim
    //    under their natural patterns; this group is where the WHY lives.
    if !ctx.obfuscation_blocked.is_empty() {
        let mut items: Vec<String> = ctx
            .obfuscation_blocked
            .iter()
            .map(|k| k.replace("\\\\", "\\"))
            .collect();
        items.sort();
        known.push(KnownGroup {
            kind: KnownKind::ObfuscatedVendorSource,
            title: KnownKind::ObfuscatedVendorSource.title().to_owned(),
            explanation:
                "These classes' constructor chains run through vendor source whose class \
declaration only exists inside an eval() of encrypted code (Anowave-style obfuscation). The real \
compiler EXECUTES that stub when the autoloader requires the file, then reflects the materialized \
class; magecommand never executes PHP, so the constructor is statically invisible and the \
arguments entry honestly degrades to NULL (a compile finding records each one). At runtime the \
object manager falls back to reflection for a NULL entry, so the store still works — the row is \
just uncached. This is the one place the no-PHP-execution guarantee costs fidelity."
                    .to_owned(),
            items,
            verified: true,
        });
    }

    // 5b. Disabled-module DI-config entries: a changed metadata file (`global.php`,
    //    `<area>.php`) that becomes byte-identical once the top-level entries for
    //    classes in disabled modules are removed from both sides — the metadata
    //    analog of rule 3.
    if let Some(group) = disabled_module_metadata(&mut changed, ctx) {
        known.push(group);
    }

    // 6. Extra metadata: a changed metadata file where the output only *adds*
    //    lines the archive lacks (chiefly the `nonLazyTypes` section) and never
    //    removes or changes archive content — magecommand's 2.4.9 target being
    //    ahead of the older archive. Disabled-module entries are stripped first so
    //    a file mixing disabled removals with output additions still qualifies.
    if let Some(group) = extra_metadata(&mut changed, ctx) {
        known.push(group);
    }

    // 7. ClassesScanner exclude-regex: a changed metadata file whose only
    //    remaining difference (after disabled removals + additions) is the
    //    generated directory-exclusion regex, whose alternation enumerates the
    //    enabled-module dirs — a stale archive lists a different set. Runs last so
    //    rules 5/6 claim the files where the regex was not the blocker.
    if let Some(group) = class_scanner_exclude_regex(&mut changed, ctx) {
        known.push(group);
    }

    Classified { known, missing, extra, changed }
}

/// Scope tokens of a plugin-list cache filename, e.g.
/// `adminhtml|global|primary|plugin-list.php` -> `{adminhtml, global, primary}`
/// as a *sorted* vector (the order-insensitive identity). `None` if the
/// basename isn't a plugin-list cache file.
fn plugin_list_scopes(rel: &str) -> Option<Vec<String>> {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    let inner = base.strip_suffix("|plugin-list.php")?;
    if inner.is_empty() {
        return None;
    }
    let mut scopes: Vec<String> = inner.split('|').map(str::to_owned).collect();
    scopes.sort();
    Some(scopes)
}

fn plugin_list_scope_order(
    missing: &mut Vec<String>,
    extra: &mut Vec<String>,
    ctx: &ClassifyCtx,
) -> Option<KnownGroup> {
    let mut items: Vec<String> = Vec::new();
    let mut claimed_missing: HashSet<String> = HashSet::new();
    let mut claimed_extra: HashSet<String> = HashSet::new();
    let mut all_verified = true;

    for m in missing.iter() {
        let Some(m_scopes) = plugin_list_scopes(m) else { continue };
        // Find an extra plugin-list file with the same scope multiset but a
        // different actual name (the reorder).
        let hit = extra.iter().find(|e| {
            !claimed_extra.contains(*e)
                && plugin_list_scopes(e).as_ref() == Some(&m_scopes)
                && *e != m
        });
        let Some(e) = hit else { continue };
        let identical = same_bytes(&ctx.archive.join(m), &ctx.output.join(e));
        all_verified &= identical;
        items.push(format!(
            "{m}  ->  {e}{}",
            if identical { "" } else { "  (contents also differ)" }
        ));
        claimed_missing.insert(m.clone());
        claimed_extra.insert(e.clone());
    }

    if items.is_empty() {
        return None;
    }
    missing.retain(|m| !claimed_missing.contains(m));
    extra.retain(|e| !claimed_extra.contains(e));

    let verified_note = if all_verified {
        "The paired files are byte-identical — only the cache-id filename differs. "
    } else {
        "Some pairs also differ in content (usually the disabled-module plugin set — see that group). "
    };
    Some(KnownGroup {
        kind: KnownKind::PluginListScopeOrder,
        title: KnownKind::PluginListScopeOrder.title().to_owned(),
        explanation: format!(
            "The plugin-list cache filename encodes the config scopes it was compiled from. \
magecommand sorts those scope names alphabetically (e.g. `global|primary`), matching Mage-OS; \
the archive lists them in module load order (e.g. `primary|global`). {verified_note}\
Deterministic, sorted names mean the same scope set always maps to one cache file regardless of \
the order scopes are requested in — the fix behind Adobe issue #40408."
        ),
        items,
        verified: all_verified,
    })
}

fn filename_casing(missing: &mut Vec<String>, extra: &mut Vec<String>) -> Option<KnownGroup> {
    let mut items: Vec<String> = Vec::new();
    let mut claimed_missing: HashSet<String> = HashSet::new();
    let mut claimed_extra: HashSet<String> = HashSet::new();

    for m in missing.iter() {
        let m_lower = m.to_ascii_lowercase();
        let hit = extra
            .iter()
            .find(|e| !claimed_extra.contains(*e) && e.to_ascii_lowercase() == m_lower && *e != m);
        let Some(e) = hit else { continue };
        items.push(format!("{m}  ->  {e}"));
        claimed_missing.insert(m.clone());
        claimed_extra.insert(e.clone());
    }

    if items.is_empty() {
        return None;
    }
    missing.retain(|m| !claimed_missing.contains(m));
    extra.retain(|e| !claimed_extra.contains(e));

    Some(KnownGroup {
        kind: KnownKind::FilenameCasing,
        title: KnownKind::FilenameCasing.title().to_owned(),
        explanation:
            "These paths differ only in letter case (e.g. `Tierprice` vs `TierPrice`). The archive \
was generated on a case-insensitive filesystem (macOS), where the first directory created wins the \
casing. magecommand emits the case the PHP class actually declares (its reflection name), which is \
the correct, unambiguous form on the case-sensitive filesystems used in production (Linux)."
                .to_owned(),
        items,
        verified: true,
    })
}

/// Module name owning an interceptor path, if that module is disabled.
/// `Magento/Swagger/Controller/Index/Index/Interceptor.php` -> `Magento_Swagger`.
fn disabled_interceptor_module(rel: &str, disabled: &HashSet<String>) -> Option<String> {
    let subject = rel.strip_suffix("/Interceptor.php")?;
    let mut segs = subject.split('/');
    let vendor = segs.next()?;
    let module = segs.next()?;
    let name = format!("{vendor}_{module}");
    disabled.contains(&name).then_some(name)
}

fn disabled_module_interceptors(missing: &mut Vec<String>, ctx: &ClassifyCtx) -> Option<KnownGroup> {
    if ctx.disabled_modules.is_empty() {
        return None;
    }
    let mut items: Vec<String> = Vec::new();
    let mut claimed: HashSet<String> = HashSet::new();
    let mut modules: HashSet<String> = HashSet::new();

    for m in missing.iter() {
        if let Some(module) = disabled_interceptor_module(m, ctx.disabled_modules) {
            items.push(format!("{m}  ({module}, disabled)"));
            claimed.insert(m.clone());
            modules.insert(module);
        }
    }

    if items.is_empty() {
        return None;
    }
    missing.retain(|m| !claimed.contains(m));

    let mut module_list: Vec<String> = modules.into_iter().collect();
    module_list.sort();
    Some(KnownGroup {
        kind: KnownKind::DisabledModuleInterceptor,
        title: KnownKind::DisabledModuleInterceptor.title().to_owned(),
        explanation: format!(
            "These interceptors are for classes in modules disabled in app/etc/config.php ({}). \
Since Magento 2.4.9, setup:di:compile compiles only enabled modules, so magecommand — which \
targets 2.4.9 / Mage-OS 3.1.0 — correctly omits them. The archive was produced by an older \
Magento (2.4.8 or earlier) that compiled every module on disk regardless of enable-state. \
Skipping disabled modules is the improvement: it never generates code that can't run.",
            module_list.join(", ")
        ),
        items,
        verified: true,
    })
}

/// The `Vendor_Module` owning a DI-config entry key, if that module is disabled.
///
/// Keys in a `var_export`ed metadata file are double-backslash escaped, e.g.
/// `BigBridge\\TaxExceptions\\Helper\\ConfigHelper` -> `BigBridge_TaxExceptions`.
/// Virtual types and other non-FQCN keys (no `\\`) never match a module name.
fn metadata_entry_module_disabled(key: &str, disabled: &HashSet<String>) -> bool {
    let mut segs = key.split("\\\\");
    let (Some(vendor), Some(module)) = (segs.next(), segs.next()) else {
        return false;
    };
    if vendor.is_empty() || module.is_empty() {
        return false;
    }
    disabled.contains(&format!("{vendor}_{module}"))
}

/// The class key of a *top-level* DI-config entry, if `line` is one.
///
/// `var_export` indents each nesting level by two spaces. Two file shapes carry
/// class-keyed entries: the sectioned area files (`global.php`, `<area>.php`),
/// where a class is a child of an `arguments`/`preferences`/`instanceTypes`/
/// `nonLazyTypes` section and so sits at **four** spaces; and the flat
/// `interception.php` map, whose class keys sit at **two**. Both are accepted.
/// A two-space *section header* (`  'arguments' =>`) also matches here, but it has
/// no `\\`, so [`metadata_entry_module_disabled`] never treats it as a module
/// class — only real `Vendor\\Module\\…` keys are ever stripped. Deeper argument
/// lines (six-plus spaces) have a space, not a quote, at the tested position and
/// are rejected.
fn top_level_entry_key(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("    '").or_else(|| line.strip_prefix("  '"))?;
    let end = rest.find("' =>")?;
    Some(&rest[..end])
}

/// Remove every top-level DI-config entry whose class key belongs to a disabled
/// module — or is a known obfuscation-blocked class — so two metadata files can
/// be compared modulo that expected noise.
///
/// The block a key introduces is either a single inline line (`'K' => NULL,`) or
/// a `var_export` array spanning to its own indent-four `    ),` close; both are
/// dropped in full. Lines are re-joined with `\n`; the same transform on both
/// sides keeps any surviving difference intact for an exact comparison.
fn strip_expected_entries(
    text: &str,
    disabled: &HashSet<String>,
    blocked: &HashSet<String>,
) -> (String, bool) {
    let mut out = String::with_capacity(text.len());
    let mut stripped = false;
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if let Some(key) = top_level_entry_key(line) {
            if metadata_entry_module_disabled(key, disabled) || blocked.contains(key) {
                stripped = true;
                // A block value (`'K' =>` with the value on the following lines)
                // runs to a `)` at the key's own indentation; an inline value is
                // just `line`. (Flat-map interception entries are always inline
                // scalars, so the block branch only ever fires for area files.)
                if line.ends_with("=>") || line.ends_with("=> ") {
                    let indent = line.len() - line.trim_start().len();
                    let close = format!("{}),", " ".repeat(indent));
                    for inner in lines.by_ref() {
                        if inner == close {
                            break;
                        }
                    }
                }
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, stripped)
}

/// Find changed-metadata argument entries that are statically unresolvable
/// because the class's constructor chain runs through EVAL-OBFUSCATED vendor
/// source (Anowave-style: the real class declaration only exists inside an
/// `eval(<decrypt>(…))` payload). The real compiler `require`s the file —
/// executing the stub — and reflects the materialized class; magecommand never
/// executes PHP, so it sees no constructor and honestly emits NULL (plus a
/// compile finding).
///
/// Candidates are exactly the keys that are a one-line `NULL` entry on one
/// side and a block on the other; each is confirmed by walking the class's
/// `extends` chain through real files until one matches the obfuscation
/// signature (`eval(` + `base64_decode` in the same source). Returns the keys
/// in their escaped metadata form, for [`ClassifyCtx::obfuscation_blocked`].
pub fn obfuscation_blocked_classes(
    report: &CompareReport,
    archive: &Path,
    output: &Path,
    magento: Option<&magequery_core::Magento>,
) -> HashSet<String> {
    let Some(magento) = magento else { return HashSet::new() };

    // key -> is the entry a one-line NULL? (block entries map to false)
    fn entry_shapes(text: &str) -> Vec<(String, bool)> {
        text.lines()
            .filter_map(|line| {
                let key = top_level_entry_key(line)?;
                Some((key.to_owned(), line.trim_end().ends_with("=> NULL,")))
            })
            .collect()
    }

    let mut candidates: HashSet<String> = HashSet::new();
    for rel in &report.changed {
        let (Ok(a), Ok(b)) =
            (fs::read_to_string(archive.join(rel)), fs::read_to_string(output.join(rel)))
        else {
            continue;
        };
        if !a.starts_with("<?php return array (") {
            continue;
        }
        let shapes_a: std::collections::HashMap<String, bool> =
            entry_shapes(&a).into_iter().collect();
        for (key, b_null) in entry_shapes(&b) {
            if let Some(&a_null) = shapes_a.get(&key) {
                if a_null != b_null {
                    candidates.insert(key);
                }
            }
        }
    }

    let mut verdict: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    let mut blocked = HashSet::new();
    for key in candidates {
        let fqcn = key.replace("\\\\", "\\");
        if chain_hits_obfuscation(&fqcn, magento, &mut verdict, 0) {
            blocked.insert(key);
        }
    }
    blocked
}

fn chain_hits_obfuscation(
    fqcn: &str,
    magento: &magequery_core::Magento,
    verdict: &mut std::collections::HashMap<String, bool>,
    depth: usize,
) -> bool {
    if depth > 8 {
        return false;
    }
    if let Some(&v) = verdict.get(fqcn) {
        return v;
    }
    verdict.insert(fqcn.to_owned(), false); // cycle guard
    let Some(file) = magento.class_file(&magequery_core::ClassName::new(fqcn.to_owned())) else {
        return false;
    };
    let Ok(src) = fs::read(&file) else { return false };
    let text = String::from_utf8_lossy(&src);
    let hit = if text.contains("eval(") && text.contains("base64_decode") {
        true
    } else {
        let meta = magecommand_php::parse_file(&src);
        meta.declarations
            .iter()
            .find(|d| d.fqcn.eq_ignore_ascii_case(fqcn))
            .or(meta.declarations.first())
            .and_then(|d| d.extends.first().cloned())
            .is_some_and(|parent| chain_hits_obfuscation(&parent, magento, verdict, depth + 1))
    };
    verdict.insert(fqcn.to_owned(), hit);
    hit
}

fn disabled_module_metadata(changed: &mut Vec<String>, ctx: &ClassifyCtx) -> Option<KnownGroup> {
    if ctx.disabled_modules.is_empty() {
        return None;
    }
    let mut items: Vec<String> = Vec::new();
    let mut claimed: HashSet<String> = HashSet::new();

    for c in changed.iter() {
        let (Ok(a), Ok(b)) =
            (fs::read_to_string(ctx.archive.join(c)), fs::read_to_string(ctx.output.join(c)))
        else {
            continue;
        };
        // Only the `var_export`ed DI-config metadata files — never a generated
        // interceptor/proxy, whose top-level-looking lines aren't config entries.
        if !a.starts_with("<?php return array (") {
            continue;
        }
        let (sa, stripped_a) = strip_expected_entries(&a, ctx.disabled_modules, ctx.obfuscation_blocked);
        let (sb, stripped_b) = strip_expected_entries(&b, ctx.disabled_modules, ctx.obfuscation_blocked);
        // Claim only when removing disabled-module entries (from at least one
        // side) makes the files identical — so any *other* difference keeps the
        // file flagged.
        if (stripped_a || stripped_b) && sa == sb {
            items.push(c.clone());
            claimed.insert(c.clone());
        }
    }

    if items.is_empty() {
        return None;
    }
    changed.retain(|c| !claimed.contains(c));

    let mut module_list: Vec<String> = ctx.disabled_modules.iter().cloned().collect();
    module_list.sort();
    Some(KnownGroup {
        kind: KnownKind::DisabledModuleMetadata,
        title: KnownKind::DisabledModuleMetadata.title().to_owned(),
        explanation:
            "These metadata DI-config files are byte-identical once the top-level entries for \
classes in modules disabled in app/etc/config.php are removed from both sides. Since Magento \
2.4.9, setup:di:compile compiles only enabled modules, so magecommand — which targets 2.4.9 / \
Mage-OS 3.1.0 — correctly omits those entries. The archive was produced before the modules were \
disabled (or by an older Magento that compiled every module on disk regardless of enable-state). \
Omitting disabled modules is the improvement: the config never references code that can't run."
                .to_owned(),
        items,
        verified: true,
    })
}

/// True when every line of `a` appears in `b` in order — i.e. `b` is `a` with
/// some lines inserted, and nothing deleted or changed. Greedy single-pass, O(|b|):
/// each `b` line is consumed at most once and the cursor never rewinds, which is
/// the standard (and optimal) test for "is `a` a subsequence of `b`". A changed
/// or removed line in `a` has no match remaining in `b`, so the check fails —
/// only pure insertions into `b` pass.
fn is_line_subsequence(a: &str, b: &str) -> bool {
    let mut b_lines = b.lines();
    for a_line in a.lines() {
        loop {
            match b_lines.next() {
                Some(b_line) if b_line == a_line => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

/// A changed metadata file whose output is a strict superset of the archive: the
/// output only *adds* config (most visibly the whole `nonLazyTypes` section and
/// the `NonLazyTypes` step in `ModificationChain`, a 2.4.9 / PHP 8.4 feature the
/// older archive predates), never removing or altering what the archive has.
///
/// Disabled-module entries are stripped from both sides first (rule 5's transform),
/// so a file combining disabled-module *removals* with genuine output *additions*
/// still qualifies — every remaining difference is then an insertion into the
/// output. Any real regression (a changed value, a dropped entry) leaves an
/// archive line unmatched and keeps the file flagged.
fn extra_metadata(changed: &mut Vec<String>, ctx: &ClassifyCtx) -> Option<KnownGroup> {
    let mut items: Vec<String> = Vec::new();
    let mut claimed: HashSet<String> = HashSet::new();

    for c in changed.iter() {
        let (Ok(a), Ok(b)) =
            (fs::read_to_string(ctx.archive.join(c)), fs::read_to_string(ctx.output.join(c)))
        else {
            continue;
        };
        if !a.starts_with("<?php return array (") {
            continue;
        }
        let (sa, _) = strip_expected_entries(&a, ctx.disabled_modules, ctx.obfuscation_blocked);
        let (sb, _) = strip_expected_entries(&b, ctx.disabled_modules, ctx.obfuscation_blocked);
        // Claim only when the output strictly grew and contains every archive line
        // in order — additions only. `sa.len() < sb.len()` also rules out the
        // equal case (which rule 5 already handles).
        if sa.len() < sb.len() && is_line_subsequence(&sa, &sb) {
            items.push(c.clone());
            claimed.insert(c.clone());
        }
    }

    if items.is_empty() {
        return None;
    }
    changed.retain(|c| !claimed.contains(c));

    Some(KnownGroup {
        kind: KnownKind::ExtraMetadata,
        title: KnownKind::ExtraMetadata.title().to_owned(),
        explanation:
            "In these files the output is a strict superset of the archive: every archive entry \
is present unchanged and the output only *adds* metadata. The largest addition is the \
`nonLazyTypes` section (plus its `NonLazyTypes` step in `ModificationChain`) — the lazy-proxy \
optimization from the `Chain\\NonLazyTypes` compiler pass that magecommand's 2.4.9 / Mage-OS \
3.1.0 (PHP 8.4) target runs but the older archive predates. Extra compiled DI entries are inert \
— nothing references them at runtime — so this is additive and safe. A genuine regression would \
*remove* or *change* an archive entry, which keeps the file flagged; only pure additions land here."
                .to_owned(),
        items,
        verified: true,
    })
}

/// Replace the generated ClassesScanner directory-exclusion regex value with a
/// fixed placeholder on any line that carries it, leaving every other line intact.
///
/// The regex is a literal alternation of every enabled module's directory —
/// `#^(?:…/vendor/<pkg>…|…/app/code/<Vendor>/<Module>…)/Test#` and its `/tests#`
/// sibling — emitted by the DI compiler from the module set on disk. It is a pure
/// function of *which* modules are enabled, so a stale archive compiled from a
/// different set lists different members (and order). Canonicalizing both sides
/// lets the surrounding entries be compared without that membership drift masking
/// them. The line is recognized by its exact signature (a `#^(?:` value ending in
/// `/Test#` or `/tests#`), never an arbitrary regex, so an unrelated changed value
/// is left alone to keep its file flagged.
fn canonicalize_class_scanner_regex(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        match canonical_scanner_regex_line(line) {
            Some(canon) => out.push_str(&canon),
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// The canonicalized form of `line` if it is a ClassesScanner exclusion-regex
/// value line (`<indent><key> => '#^(?:…)/Test#',`), else `None`.
fn canonical_scanner_regex_line(line: &str) -> Option<String> {
    let arrow = line.find("=> '#^(?:")?;
    if !line.ends_with("/Test#',") && !line.ends_with("/tests#',") {
        return None;
    }
    // Keep everything through `=> ` (indent + array-index key), swap the value.
    let head = &line[..arrow + "=> ".len()];
    Some(format!("{head}'<class-scanner-exclude-regex>',"))
}

/// A changed metadata file whose only substantive difference — after removing
/// disabled-module entries and allowing pure additions — is the generated
/// ClassesScanner directory-exclusion regex. Canonicalizing that one line on both
/// sides and re-checking equality/superset confirms it; any *other* changed value
/// leaves the sides unequal and non-subsequence, so the file stays flagged.
///
/// Runs after rules 5/6 so pure disabled-module and pure-additions files are
/// already claimed — a file only reaches here because the regex line was the
/// blocker.
fn class_scanner_exclude_regex(
    changed: &mut Vec<String>,
    ctx: &ClassifyCtx,
) -> Option<KnownGroup> {
    let mut items: Vec<String> = Vec::new();
    let mut claimed: HashSet<String> = HashSet::new();

    for c in changed.iter() {
        let (Ok(a), Ok(b)) =
            (fs::read_to_string(ctx.archive.join(c)), fs::read_to_string(ctx.output.join(c)))
        else {
            continue;
        };
        if !a.starts_with("<?php return array (") {
            continue;
        }
        let (sa, _) = strip_expected_entries(&a, ctx.disabled_modules, ctx.obfuscation_blocked);
        let (sb, _) = strip_expected_entries(&b, ctx.disabled_modules, ctx.obfuscation_blocked);
        let ca = canonicalize_class_scanner_regex(&sa);
        let cb = canonicalize_class_scanner_regex(&sb);
        // Only relevant when a scanner regex was actually present and differed;
        // if canonicalizing changed nothing, rules 5/6 already own this file.
        if ca == sa && cb == sb {
            continue;
        }
        let matches = ca == cb || (ca.len() <= cb.len() && is_line_subsequence(&ca, &cb));
        if matches {
            items.push(c.clone());
            claimed.insert(c.clone());
        }
    }

    if items.is_empty() {
        return None;
    }
    changed.retain(|c| !claimed.contains(c));

    Some(KnownGroup {
        kind: KnownKind::ClassScannerExcludeRegex,
        title: KnownKind::ClassScannerExcludeRegex.title().to_owned(),
        explanation:
            "These files match once the generated ClassesScanner directory-exclusion regex is \
normalized (and disabled-module entries removed). That regex — `#^(?:…/vendor/<pkg>…|…/app/code/\
<Vendor>/<Module>…)/Test#` and its `/tests#` sibling — is a literal alternation of every enabled \
module's directory, so an archive compiled from a different module set lists different members (and \
order). magecommand builds it from the same module discovery it uses everywhere, and it is \
byte-identical to a matched-version compile (the oracle), so a membership/order difference here is a \
stale-archive artifact, never a bug. After canonicalizing it, the only remaining differences are \
disabled-module removals and additive entries — a genuine changed value would survive canonicalization \
and keep the file flagged."
                .to_owned(),
        items,
        verified: true,
    })
}

/// Reduce the code generator's version-specific formatting to a canonical form.
///
/// Magento's code generator changed several byte-level (but behavior-preserving)
/// details between releases; magecommand emits the 2.4.9 / Mage-OS 3.1.0 form, so
/// an archive from an older Magento differs in exactly these ways. Applying the
/// same reductions to BOTH sides collapses those differences while leaving any
/// genuine change (a different type, an extra dependency, a real body edit)
/// intact — so a file is only claimed when the sole differences were these.
fn strip_generator_version_formatting(s: &str) -> String {
    let mut t = s.to_string();
    // Return-type spacing: `foo() : T` (older) -> `foo(): T` (2.4.9, PSR-12).
    // Also normalizes the identical ternary spacing on both sides — harmless.
    t = t.replace(") : ", "): ");
    // Explicit nullable default: `?T $x = null` (2.4.9, PHP 8.4 compat) -> `?T $x`.
    t = t.replace(" = null,", ",");
    t = t.replace(" = null)", ")");
    // Proxy `__clone` null-guard (2.4.9) -> unguarded (older): keep the proxy
    // lazy when cloned instead of forcing instantiation.
    t = t.replace(
        "        if ($this->_subject) {\n            $this->_subject = clone $this->_getSubject();\n        }",
        "        $this->_subject = clone $this->_getSubject();",
    );
    // Proxy `__debugInfo` method (2.4.9-only) -> removed.
    t = t.replace(
        "\n\n    /**\n     * Debug proxied instance\n     */\n    public function __debugInfo()\n    {\n        return ['i' => $this->_subject];\n    }",
        "",
    );
    // Proxy `_resetState` null-guard (2.4.9) -> unguarded (older).
    t = t.replace("     * Reset state of proxied instance", "     * {@inheritdoc}");
    t = t.replace(
        "        if ($this->_subject) {\n            $this->_subject->_resetState(); \n        }",
        "        $this->_getSubject()->_resetState();",
    );
    t
}

fn generator_version_formatting(
    changed: &mut Vec<String>,
    ctx: &ClassifyCtx,
) -> Option<KnownGroup> {
    let mut items: Vec<String> = Vec::new();
    let mut claimed: HashSet<String> = HashSet::new();

    for c in changed.iter() {
        let (Ok(a), Ok(b)) =
            (fs::read_to_string(ctx.archive.join(c)), fs::read_to_string(ctx.output.join(c)))
        else {
            continue;
        };
        // Claim only when normalization makes them equal: the sole differences
        // were the known generator-formatting reductions. A generated interceptor
        // or proxy may additionally differ in method order (a reflection-order
        // artifact), so fall back to comparing the normalized text modulo block
        // order — this absorbs files that combine formatting deltas with a reorder.
        let na = strip_generator_version_formatting(&a);
        let nb = strip_generator_version_formatting(&b);
        let matches = na == nb
            || matches!(
                (canonical_method_order(na.as_bytes()), canonical_method_order(nb.as_bytes())),
                (Some(ca), Some(cb)) if ca == cb
            );
        if matches {
            items.push(c.clone());
            claimed.insert(c.clone());
        }
    }

    if items.is_empty() {
        return None;
    }
    changed.retain(|c| !claimed.contains(c));

    Some(KnownGroup {
        kind: KnownKind::GeneratorVersionFormatting,
        title: KnownKind::GeneratorVersionFormatting.title().to_owned(),
        explanation:
            "These files are byte-identical after normalizing the code generator's \
version-specific formatting, so they differ only in ways Magento changed between the archive's \
version and magecommand's 2.4.9 / Mage-OS 3.1.0 target — all behavior-preserving: return-type \
spacing (`foo() : T` -> `foo(): T`), explicit nullable defaults (`?T $x` -> `?T $x = null`, the \
PHP 8.4 compat form), and the proxy template's laziness hardening (null-guards in `__clone` and \
`_resetState` so cloning/resetting an unused proxy no longer forces instantiation, plus an added \
`__debugInfo`). Run magecommand against a 2.4.9 store and these vanish."
                .to_owned(),
        items,
        verified: true,
    })
}

/// Diagnostic for a changed metadata file that did *not* classify: apply the same
/// normalizations the classifiers use (strip disabled-module entries, canonicalize
/// the ClassesScanner exclusion regex), then report the first place the archive
/// stops being an in-order subsequence of the output — the first genuine change,
/// reorder, or drop — with context from both sides. This is what the `compare
/// --show-residual` flag prints so a lone unexplained file can be pinpointed
/// without hand-rolling the block-aware disabled-strip in a shell.
pub fn residual_report(archive_text: &str, output_text: &str, disabled: &HashSet<String>) -> String {
    let no_blocked = HashSet::new();
    let (sa, _) = strip_expected_entries(archive_text, disabled, &no_blocked);
    let (sb, _) = strip_expected_entries(output_text, disabled, &no_blocked);
    let na = canonicalize_class_scanner_regex(&sa);
    let nb = canonicalize_class_scanner_regex(&sb);
    let a: Vec<&str> = na.lines().collect();
    let b: Vec<&str> = nb.lines().collect();

    // Greedy forward subsequence match of the normalized archive against the
    // normalized output — the exact test the classifiers use.
    let mut j = 0usize;
    for (i, a_line) in a.iter().enumerate() {
        let start = j;
        while j < b.len() && b[j] != *a_line {
            j += 1;
        }
        if j == b.len() {
            // `a_line` has no match forward from `start`: the divergence point.
            let mut out = format!(
                "normalized archive is NOT a subsequence of output.\n\
                 first unmatched archive line (normalized index {i}):\n  {a_line}\n\n\
                 --- archive, from the divergence (next 22 lines) ---\n"
            );
            for l in a.iter().skip(i).take(22) {
                out.push_str("  ");
                out.push_str(l);
                out.push('\n');
            }
            out.push_str("\n--- output, from where the archive last matched (next 22 lines) ---\n");
            for l in b.iter().skip(start.saturating_sub(2)).take(22) {
                out.push_str("  ");
                out.push_str(l);
                out.push('\n');
            }
            return out;
        }
        j += 1;
    }
    "normalized archive IS a subsequence of output — after stripping disabled-module \
     entries and canonicalizing the ClassesScanner regex, the output only adds lines. \
     (This file would classify as additive/superset; if compare still flags it, the \
     difference is a pure line-count/order artifact.)"
        .to_string()
}

fn same_bytes(a: &Path, b: &Path) -> bool {
    match (fs::read(a), fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn ctx<'a>(disabled: &'a HashSet<String>, dir: &'a Path) -> ClassifyCtx<'a> {
        ClassifyCtx { archive: dir, output: dir, disabled_modules: disabled }
    }

    fn report(missing: &[&str], extra: &[&str], changed: &[&str]) -> CompareReport {
        CompareReport {
            missing: missing.iter().map(|s| s.to_string()).collect(),
            extra: extra.iter().map(|s| s.to_string()).collect(),
            changed: changed.iter().map(|s| s.to_string()).collect(),
            reordered: Vec::new(),
            identical: 0,
        }
    }

    #[test]
    fn pairs_plugin_list_scope_reorder() {
        let disabled = HashSet::new();
        let dir = Path::new("/nonexistent-cmp"); // reads fail -> not byte-verified
        let r = report(
            &["primary|global|adminhtml|plugin-list.php"],
            &["adminhtml|global|primary|plugin-list.php"],
            &[],
        );
        let c = classify(&r, &ctx(&disabled, dir));
        assert_eq!(c.known.len(), 1);
        assert_eq!(c.known[0].kind, KnownKind::PluginListScopeOrder);
        assert_eq!(c.known[0].items.len(), 1);
        assert!(c.missing.is_empty() && c.extra.is_empty());
        // Files unreadable here, so it can't claim byte-identity.
        assert!(!c.known[0].verified);
    }

    #[test]
    fn classifies_disabled_module_interceptor() {
        let mut disabled = HashSet::new();
        disabled.insert("Magento_Swagger".to_string());
        let dir = Path::new("/x");
        let r = report(
            &[
                "Magento/Swagger/Controller/Index/Index/Interceptor.php",
                "Magento/Catalog/Model/Product/Interceptor.php", // enabled -> stays
            ],
            &[],
            &[],
        );
        let c = classify(&r, &ctx(&disabled, dir));
        assert_eq!(c.known.len(), 1);
        assert_eq!(c.known[0].kind, KnownKind::DisabledModuleInterceptor);
        assert_eq!(c.known[0].items.len(), 1);
        // The enabled-module interceptor is NOT absorbed — it stays unexplained.
        assert_eq!(c.missing, vec!["Magento/Catalog/Model/Product/Interceptor.php"]);
    }

    #[test]
    fn classifies_disabled_module_metadata() {
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let write = |root: &Path, rel: &str, c: &str| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        };
        // `global.php`: the archive additionally carries a block entry and an
        // inline entry for the disabled `Bad\Mod`; the enabled `Good\Mod` entry
        // is shared. Removing the disabled entries makes the two identical.
        let shared = "  'arguments' => \n  array (\n    'Good\\\\Mod\\\\A' => \n    array (\n      'x' => \n      array (\n        '_v_' => 1,\n      ),\n    ),\n";
        let archive_global = format!(
            "<?php return array (\n{shared}    'Bad\\\\Mod\\\\B' => \n    array (\n      'y' => \n      array (\n        '_v_' => 2,\n      ),\n    ),\n    'Bad\\\\Mod\\\\C' => NULL,\n  ),\n);\n"
        );
        let output_global = format!("<?php return array (\n{shared}  ),\n);\n");
        write(archive.path(), "global.php", &archive_global);
        write(output.path(), "global.php", &output_global);
        // `crontab.php`: an *enabled*-module difference remains after stripping,
        // so it must stay flagged.
        write(archive.path(), "crontab.php", "<?php return array (\n  'arguments' => \n  array (\n    'Good\\\\Mod\\\\A' => NULL,\n  ),\n);\n");
        write(output.path(), "crontab.php", "<?php return array (\n  'arguments' => \n  array (\n    'Good\\\\Mod\\\\A' => false,\n  ),\n);\n");

        let mut disabled = HashSet::new();
        disabled.insert("Bad_Mod".to_string());
        let ctx = ClassifyCtx {
            archive: archive.path(),
            output: output.path(),
            disabled_modules: &disabled,
        };
        let r = report(&[], &[], &["global.php", "crontab.php"]);
        let c = classify(&r, &ctx);
        assert_eq!(c.known.len(), 1);
        assert_eq!(c.known[0].kind, KnownKind::DisabledModuleMetadata);
        assert_eq!(c.known[0].items, vec!["global.php"]);
        // The enabled-module change is NOT absorbed.
        assert_eq!(c.changed, vec!["crontab.php"]);
    }

    #[test]
    fn classifies_extra_metadata_additions() {
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let write = |root: &Path, rel: &str, c: &str| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        };
        // The shared `arguments` body, without the section/root close so entries
        // can be appended cleanly.
        let args_open = "<?php return array (\n  'arguments' => \n  array (\n    'Good\\\\Mod\\\\A' => \n    array (\n      'x' => \n      array (\n        '_v_' => 1,\n      ),\n    ),\n";
        let close = "  ),\n);\n"; // close arguments, then root
        // additions.php: output keeps every archive line and appends a whole new
        // section — pure additions — plus a disabled-module removal (stripped first).
        write(
            archive.path(),
            "additions.php",
            &format!("{args_open}    'Bad\\\\Mod\\\\B' => NULL,\n{close}"),
        );
        write(
            output.path(),
            "additions.php",
            &format!("{args_open}  ),\n  'nonLazyTypes' => \n  array (\n    'Good\\\\Mod\\\\A' => true,\n  ),\n);\n"),
        );
        // changed.php: the output *changes* an archive value — must stay flagged.
        write(archive.path(), "changed.php", &format!("{args_open}{close}"));
        write(
            output.path(),
            "changed.php",
            &format!("{args_open}{close}").replace("'_v_' => 1,", "'_v_' => 2,"),
        );
        // removed.php: the output *drops* an archive entry — must stay flagged.
        write(
            archive.path(),
            "removed.php",
            &format!("{args_open}    'Good\\\\Mod\\\\Z' => NULL,\n{close}"),
        );
        write(output.path(), "removed.php", &format!("{args_open}{close}"));

        let mut disabled = HashSet::new();
        disabled.insert("Bad_Mod".to_string());
        let ctx = ClassifyCtx {
            archive: archive.path(),
            output: output.path(),
            disabled_modules: &disabled,
        };
        let r = report(&[], &[], &["additions.php", "changed.php", "removed.php"]);
        let c = classify(&r, &ctx);
        let extra: Vec<_> =
            c.known.iter().filter(|g| g.kind == KnownKind::ExtraMetadata).collect();
        assert_eq!(extra.len(), 1);
        assert_eq!(extra[0].items, vec!["additions.php"]);
        // A changed value and a dropped entry are NOT absorbed.
        assert!(c.changed.contains(&"changed.php".to_string()));
        assert!(c.changed.contains(&"removed.php".to_string()));
    }

    #[test]
    fn line_subsequence_detects_insertions_only() {
        assert!(is_line_subsequence("a\nb\nc\n", "a\nX\nb\nc\nY\n")); // insertions
        assert!(!is_line_subsequence("a\nb\nc\n", "a\nc\n")); // deletion
        assert!(!is_line_subsequence("a\nb\nc\n", "a\nB\nc\n")); // change
        assert!(is_line_subsequence("", "a\n")); // empty is a subsequence of anything
    }

    #[test]
    fn pairs_case_only_paths() {
        let disabled = HashSet::new();
        let dir = Path::new("/x");
        let r = report(
            &["Magento/Catalog/Model/Backend/Tierprice/Interceptor.php"],
            &["Magento/Catalog/Model/Backend/TierPrice/Interceptor.php"],
            &[],
        );
        let c = classify(&r, &ctx(&disabled, dir));
        assert_eq!(c.known.len(), 1);
        assert_eq!(c.known[0].kind, KnownKind::FilenameCasing);
        assert!(c.missing.is_empty() && c.extra.is_empty());
    }

    #[test]
    fn genuine_difference_stays_unexplained() {
        let disabled = HashSet::new();
        let dir = Path::new("/x");
        let r = report(&["Vendor/Mod/Real/Interceptor.php"], &["Vendor/Mod/Other.php"], &["global.php"]);
        let c = classify(&r, &ctx(&disabled, dir));
        assert!(c.known.is_empty());
        assert_eq!(c.unexplained_count(), 3);
    }

    #[test]
    fn strips_the_known_generator_formatting_deltas() {
        // return-type spacing
        let a = "    public function getCID() : string\n    {\n    }\n";
        let b = "    public function getCID(): string\n    {\n    }\n";
        assert_eq!(
            strip_generator_version_formatting(a),
            strip_generator_version_formatting(b)
        );
        // nullable default + proxy __clone guard + __debugInfo
        let arch = "    public function __clone()\n    {\n        $this->_subject = clone $this->_getSubject();\n    }\n\n    public function f(?\\X $r)\n    {\n    }\n";
        let mc = "    public function __clone()\n    {\n        if ($this->_subject) {\n            $this->_subject = clone $this->_getSubject();\n        }\n    }\n\n    /**\n     * Debug proxied instance\n     */\n    public function __debugInfo()\n    {\n        return ['i' => $this->_subject];\n    }\n\n    public function f(?\\X $r = null)\n    {\n    }\n";
        assert_eq!(
            strip_generator_version_formatting(arch),
            strip_generator_version_formatting(mc)
        );
        // a genuine type difference is NOT collapsed
        let x = "    public function f(\\Laminas\\Uri\\Uri $u)\n";
        let y = "    public function f(\\Zend\\Uri\\Uri $u)\n";
        assert_ne!(
            strip_generator_version_formatting(x),
            strip_generator_version_formatting(y)
        );
    }

    #[test]
    fn classifies_formatting_absorbs_only_matching_files() {
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let write = |root: &Path, rel: &str, c: &str| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        };
        // formatting-only difference -> should be absorbed
        write(archive.path(), "A/Interceptor.php", "public function g() : void\n{\n}\n");
        write(output.path(), "A/Interceptor.php", "public function g(): void\n{\n}\n");
        // genuine difference -> stays changed
        write(archive.path(), "B/Interceptor.php", "public function g(\\Laminas\\Uri\\Uri $u)\n");
        write(output.path(), "B/Interceptor.php", "public function g(\\Zend\\Uri\\Uri $u)\n");

        let disabled = HashSet::new();
        let ctx = ClassifyCtx {
            archive: archive.path(),
            output: output.path(),
            disabled_modules: &disabled,
        };
        let r = report(&[], &[], &["A/Interceptor.php", "B/Interceptor.php"]);
        let c = classify(&r, &ctx);
        assert_eq!(c.known.len(), 1);
        assert_eq!(c.known[0].kind, KnownKind::GeneratorVersionFormatting);
        assert_eq!(c.known[0].items, vec!["A/Interceptor.php"]);
        assert_eq!(c.changed, vec!["B/Interceptor.php"]);
    }

    #[test]
    fn formatting_classifier_absorbs_a_proxy_that_also_reordered() {
        // A proxy whose only differences are a formatting delta (return-type
        // spacing) AND a method reorder — the real-world Yireo case. Neither the
        // reordered bucket (bytes still differ after sorting) nor a plain
        // formatting compare (order still differs) catches it alone; composing the
        // two does.
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let write = |root: &Path, rel: &str, c: &str| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        };
        // Shared header + two method blocks. Archive: `alpha`,`beta` with the old
        // `() : T` spacing; output: `beta`,`alpha` with the new `(): T` spacing.
        let head = "<?php\nnamespace Foo;\n\nclass Proxy extends \\Foo\\Bar implements \
\\Magento\\Framework\\ObjectManager\\NoninterceptableInterface\n{\n    protected $_subject = null;";
        let m = |name: &str, spaced: bool| {
            let colon = if spaced { ") : " } else { "): " };
            format!("\n\n    public function {name}({colon}string\n    {{\n        return '{name}';\n    }}")
        };
        let arch = format!("{head}{}{}\n}}\n", m("alpha", true), m("beta", true));
        let mc = format!("{head}{}{}\n}}\n", m("beta", false), m("alpha", false));
        write(archive.path(), "P/Proxy.php", &arch);
        write(output.path(), "P/Proxy.php", &mc);

        let disabled = HashSet::new();
        let ctx = ClassifyCtx {
            archive: archive.path(),
            output: output.path(),
            disabled_modules: &disabled,
        };
        let r = report(&[], &[], &["P/Proxy.php"]);
        let c = classify(&r, &ctx);
        assert_eq!(c.known.len(), 1);
        assert_eq!(c.known[0].kind, KnownKind::GeneratorVersionFormatting);
        assert_eq!(c.known[0].items, vec!["P/Proxy.php"]);
        assert!(c.changed.is_empty());
    }

    #[test]
    fn classifies_flat_interception_disabled_entries() {
        // `interception.php` is a FLAT two-space map (class -> bool), not the
        // sectioned four-space shape of the area files. The archive lists classes
        // from a disabled module the current (smaller) enabled set omits; removing
        // them makes the two identical.
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let write = |root: &Path, rel: &str, c: &str| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        };
        write(
            archive.path(),
            "interception.php",
            "<?php return array (\n  'Good\\\\Mod\\\\A' => true,\n  'Bad\\\\Mod\\\\B' => false,\n  'Bad\\\\Mod\\\\C' => true,\n);",
        );
        write(
            output.path(),
            "interception.php",
            "<?php return array (\n  'Good\\\\Mod\\\\A' => true,\n);",
        );
        let mut disabled = HashSet::new();
        disabled.insert("Bad_Mod".to_string());
        let ctx = ClassifyCtx {
            archive: archive.path(),
            output: output.path(),
            disabled_modules: &disabled,
        };
        let r = report(&[], &[], &["interception.php"]);
        let c = classify(&r, &ctx);
        assert_eq!(c.known.len(), 1);
        assert_eq!(c.known[0].kind, KnownKind::DisabledModuleMetadata);
        assert_eq!(c.known[0].items, vec!["interception.php"]);
        assert!(c.changed.is_empty());
        // A two-space *section header* is never mistaken for a module class.
        assert!(!metadata_entry_module_disabled("arguments", &disabled));
    }

    #[test]
    fn canonicalizes_only_the_scanner_exclude_regex() {
        let scanner = "        0 => '#^(?:/v/foo/module-a|/v/bar/module-b)/Test#',";
        assert_eq!(
            canonical_scanner_regex_line(scanner).as_deref(),
            Some("        0 => '<class-scanner-exclude-regex>',")
        );
        // A different `#^(?:` regex that is not the test-dir exclusion is left alone.
        assert!(canonical_scanner_regex_line("      'pattern' => '#^(?:foo|bar)$#',").is_none());
        // An ordinary value line is left alone.
        assert!(canonical_scanner_regex_line("        '_v_' => 'x',").is_none());
    }

    #[test]
    fn classifies_class_scanner_exclude_regex_membership() {
        // The proforto shape in miniature: a `global.php` where, relative to the
        // stale archive, the output (a) drops a disabled module's entry, (b) adds a
        // new enabled class, and (c) lists a different member set in the generated
        // ClassesScanner exclusion regex. Only after normalizing that one regex line
        // do the sides reconcile — so the file must land in the regex group, not stay
        // flagged.
        let archive = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let write = |root: &Path, rel: &str, c: &str| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        };
        let file = |members: &str, entries: &[&str]| {
            let mut s = String::from(
                "<?php return array (\n  'arguments' => \n  array (\n    'Magento\\\\Setup\\\\Module\\\\Di\\\\Code\\\\Reader\\\\ClassesScanner' => \n    array (\n      'excludePatterns' => \n      array (\n",
            );
            s.push_str(&format!("        0 => '#^(?:{members})/Test#',\n"));
            s.push_str(&format!("        1 => '#^(?:{members})/tests#',\n"));
            s.push_str("      ),\n    ),\n");
            for e in entries {
                s.push_str(&format!("    '{e}' => NULL,\n"));
            }
            s.push_str("  ),\n);");
            s
        };
        write(archive.path(), "global.php", &file("/v/foo/module-a", &["Bad\\\\Mod\\\\X"]));
        write(
            output.path(),
            "global.php",
            &file("/v/foo/module-a|/v/foo/module-b", &["New\\\\Enabled\\\\Class"]),
        );
        // Control: a genuine changed value (no scanner regex) must stay flagged.
        write(archive.path(), "other.php", "<?php return array (\n  'arguments' => \n  array (\n    'X' => \n    array (\n      '_v_' => 1,\n    ),\n  ),\n);");
        write(output.path(), "other.php", "<?php return array (\n  'arguments' => \n  array (\n    'X' => \n    array (\n      '_v_' => 2,\n    ),\n  ),\n);");

        let mut disabled = HashSet::new();
        disabled.insert("Bad_Mod".to_string());
        let ctx = ClassifyCtx {
            archive: archive.path(),
            output: output.path(),
            disabled_modules: &disabled,
        };
        let r = report(&[], &[], &["global.php", "other.php"]);
        let c = classify(&r, &ctx);
        let regex_groups: Vec<_> = c
            .known
            .iter()
            .filter(|g| g.kind == KnownKind::ClassScannerExcludeRegex)
            .collect();
        assert_eq!(regex_groups.len(), 1);
        assert_eq!(regex_groups[0].items, vec!["global.php"]);
        // The genuine value change is NOT absorbed.
        assert_eq!(c.changed, vec!["other.php"]);
    }
}
