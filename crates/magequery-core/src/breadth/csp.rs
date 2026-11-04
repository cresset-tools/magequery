//! The `csp_whitelist.xml` whitelist (Magento_Csp).
//!
//! Magento reads it through a scoped config reader: one DOM merge of every module's
//! `etc/csp_whitelist.xml` (the global tier) and one of every `etc/<area>/csp_whitelist.xml`
//! (the area tier), each keyed on policy id and value id. Its converter then keys hosts
//! by value id but hashes by the digest itself, and the area tier is laid over the global
//! one with `array_replace_recursive` on those keys. This index reproduces both steps.

use super::*;

pub(crate) struct CspIndex {
    /// Per-area tiers, each already DOM-merged and converted. `Global` is the global tier.
    tiers: HashMap<Area, Vec<CspPolicy>>,
    /// Every theme's own whitelist; `whitelist()` picks the ones for its area.
    themes: Vec<CspThemeWhitelist>,
}

/// The converter's key for a value: hosts by id, hashes by digest.
fn converted_key(v: &CspValue) -> (CspValueKind, &str) {
    match v.kind {
        CspValueKind::Host => (CspValueKind::Host, v.id.as_str()),
        CspValueKind::Hash => (CspValueKind::Hash, v.value.as_str()),
    }
}

/// Lay `value` over `values`, replacing the entry with the same `key`. The replaced
/// entry's sources move into the new one's `overrides`.
fn replace_or_push(
    values: &mut Vec<CspValue>,
    mut value: CspValue,
    same: impl Fn(&CspValue, &CspValue) -> bool,
) {
    match values.iter_mut().find(|v| same(v, &value)) {
        Some(existing) => {
            let mut prior = std::mem::take(&mut existing.overrides);
            prior.push(existing.source.clone());
            prior.append(&mut value.overrides);
            value.overrides = prior;
            *existing = value;
        }
        None => values.push(value),
    }
}

fn policy_mut<'a>(policies: &'a mut Vec<CspPolicy>, id: &str) -> &'a mut CspPolicy {
    match policies.iter().position(|p| p.id == id) {
        Some(i) => &mut policies[i],
        None => {
            policies.push(CspPolicy { id: id.to_string(), values: Vec::new() });
            policies.last_mut().unwrap()
        }
    }
}

/// Raw parsed policies → [`CspPolicy`]s with sources, merged into `into` the way the
/// DOM merger does (same policy id, same value id → later replaces earlier).
fn merge_raw(into: &mut Vec<CspPolicy>, raws: Vec<parse::RawCspPolicy>, module: &ModuleName, file: &Path, area: Area) {
    for p in raws {
        let policy = policy_mut(into, &p.id);
        for v in p.values {
            let kind = if v.kind == "hash" { CspValueKind::Hash } else { CspValueKind::Host };
            let value = CspValue {
                id: v.id,
                kind,
                value: v.value,
                algorithm: if kind == CspValueKind::Hash { v.algorithm } else { None },
                source: Source { module: module.clone(), file: file.to_path_buf(), line: v.line, area },
                overrides: Vec::new(),
            };
            replace_or_push(&mut policy.values, value, |a, b| a.id == b.id);
        }
    }
}

/// Apply the converter: within one tier, two hash entries with the same digest collapse
/// into one (the later wins).
fn convert(tier: Vec<CspPolicy>) -> Vec<CspPolicy> {
    tier.into_iter()
        .map(|p| {
            let mut values: Vec<CspValue> = Vec::new();
            for v in p.values {
                replace_or_push(&mut values, v, |a, b| converted_key(a) == converted_key(b));
            }
            CspPolicy { id: p.id, values }
        })
        .collect()
}

impl CspIndex {
    pub fn build(modules: &[Module], vfs: &Vfs, themes: &[(String, PathBuf)]) -> Self {
        let mut tiers = HashMap::new();
        for area in std::iter::once(Area::Global).chain(REAL_AREAS) {
            let mut tier: Vec<CspPolicy> = Vec::new();
            for (i, path, raws) in read_parse(modules, vfs, area, "csp_whitelist.xml", parse::csp_whitelist_xml) {
                merge_raw(&mut tier, raws, &modules[i].name, &path, area);
            }
            tiers.insert(area, convert(tier));
        }

        let mut theme_lists = Vec::new();
        for (id, dir) in themes {
            let area = if id.starts_with("adminhtml/") { Area::Adminhtml } else { Area::Frontend };
            let file = dir.join("etc").join("csp_whitelist.xml");
            let Ok(xml) = vfs.read_to_string(&file) else { continue };
            let mut policies = Vec::new();
            merge_raw(&mut policies, parse::csp_whitelist_xml(&xml), &ModuleName::new(id.as_str()), &file, area);
            let mut policies = convert(policies);
            policies.sort_by(|a, b| a.id.cmp(&b.id));
            theme_lists.push(CspThemeWhitelist { theme: id.clone(), file, policies });
        }

        Self { tiers, themes: theme_lists }
    }

    /// The merged whitelist for `area` (the global tier alone for `Area::Global`).
    pub fn whitelist(&self, area: Area) -> CspWhitelist {
        let mut policies = self.tiers.get(&Area::Global).cloned().unwrap_or_default();
        if area != Area::Global {
            for p in self.tiers.get(&area).cloned().unwrap_or_default() {
                let policy = policy_mut(&mut policies, &p.id);
                for v in p.values {
                    replace_or_push(&mut policy.values, v, |a, b| converted_key(a) == converted_key(b));
                }
            }
        }
        policies.sort_by(|a, b| a.id.cmp(&b.id));
        let themes = self
            .themes
            .iter()
            .filter(|t| match area {
                Area::Frontend => t.theme.starts_with("frontend/"),
                Area::Adminhtml => t.theme.starts_with("adminhtml/"),
                _ => false,
            })
            .cloned()
            .collect();
        CspWhitelist { area, policies, themes }
    }
}
