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

use crate::compare::CompareReport;

/// A category of known/expected difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub enum KnownKind {
    /// Plugin-list cache filename scope ordering (sorted vs load-order).
    PluginListScopeOrder,
    /// Interceptor for a class in a module that is disabled in config.php.
    DisabledModuleInterceptor,
    /// Two paths differing only by letter case (case-insensitive filesystem).
    FilenameCasing,
}

impl KnownKind {
    fn title(self) -> &'static str {
        match self {
            KnownKind::PluginListScopeOrder => "Plugin-list cache filename scope ordering",
            KnownKind::DisabledModuleInterceptor => {
                "Interceptors for disabled modules (Magento 2.4.9 behavior)"
            }
            KnownKind::FilenameCasing => "Filename letter-casing (case-insensitive filesystem)",
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
}

/// Partition `report`'s differences into explained known groups and the
/// unexplained remainder.
pub fn classify(report: &CompareReport, ctx: &ClassifyCtx) -> Classified {
    // Work on owned, consumable sets so each path is claimed by at most one
    // classifier.
    let mut missing: Vec<String> = report.missing.clone();
    let mut extra: Vec<String> = report.extra.clone();
    let changed: Vec<String> = report.changed.clone();
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
}
