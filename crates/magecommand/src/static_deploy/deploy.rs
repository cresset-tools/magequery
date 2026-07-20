//! `magecommand static deploy` — the finale: reproduce a whole
//! `setup:static-content:deploy` run over a **per-theme locale matrix**,
//! fanning out the independent `(area, theme, locale)` package builds across
//! rayon while honoring the one cross-theme dependency (the shared
//! `.min`-sibling bundle cache). Library-first: this module plans and builds;
//! the CLI in `lib.rs` renders/writes.
//!
//! ## The matrix
//!
//! Positional locales are the DEFAULT locale set. A `--theme id:loc,loc`
//! overrides that one theme's locales; a bare `--theme id` inherits the
//! defaults; no `--theme` at all means Magento's default deployable set —
//! every registered physical theme (`Options::THEME` default `['all']`,
//! `PackagePool` iterates the theme collection; a theme deploys only in the
//! area it belongs to). `--area` restricts which areas run (default: all,
//! matching `Options::AREA` default `['all']`).
//!
//! ## The work grouping (and why)
//!
//! The `.min`-sibling cache lives on the deploy's Bundle service and poisons
//! plain names for LATER themes in the same sub-run (blank-before-luma: real
//! effect on `vimeo/player.js` etc.). Every golden was captured as an
//! independent per-`(area, locale)` `scd` invocation, so the faithful — and
//! trivially deterministic — unit of work is the **`(area, locale)` group**:
//! its themes build sequentially (given order) sharing one fresh cache
//! ([`super::files::build_group`]); different groups are fully independent
//! (disjoint output paths, disjoint caches) and fan out across rayon. Thus
//! the parallel deploy is byte-identical to a forced-serial one, and the
//! blank-before-luma ordering is preserved WITHIN each group regardless of
//! completion order.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use rayon::prelude::*;

use super::files::{self, DeployInputs, PlacementOptions, ThemePackage};

/// One requested theme with its resolved locale set.
#[derive(Debug, Clone)]
pub struct ThemeSpec {
    /// Bare theme id (`Magento/luma`) or area-qualified (`frontend/Magento/luma`).
    pub id: String,
    /// `None` = inherit the default locale set; `Some` = this theme's override.
    pub locales: Option<Vec<String>>,
}

/// A planned unit of work: the themes of one area+locale, built together with
/// one shared bundle cache in the given order.
#[derive(Debug, Clone)]
pub struct Group {
    pub area: String,
    pub locale: String,
    /// Area-qualified theme ids (`frontend/Magento/blank`), in build order.
    pub theme_ids: Vec<String>,
}

/// Per-package placement stats (the deploy summary rows) — no bytes retained,
/// so a large matrix stays memory-bounded when [`execute_to_disk`] writes each
/// group as it finishes.
#[derive(Debug, Clone)]
pub struct TupleStat {
    pub area: String,
    pub theme: String,
    pub locale: String,
    pub output: PathBuf,
    pub files: usize,
    pub bytes: usize,
    pub copied: usize,
    pub css_processed: usize,
    pub less_compiled: usize,
    pub requirejs: usize,
    pub bundles: usize,
    /// Compiler warnings `(logical path, message)`.
    pub warnings: Vec<(String, String)>,
}

/// A deploy planning/execution error.
pub type DeployError = super::less::LessDeployError;

fn err(message: String) -> DeployError {
    DeployError {
        entry: None,
        module: None,
        file: None,
        message,
    }
}

/// Area of a discovered/qualified theme id — the segment before the first `/`.
fn area_of(id: &str) -> Option<&str> {
    id.split_once('/').map(|(a, _)| a)
}

/// Normalize a theme id to its area-qualified form, given the discovered set.
/// A bare `Magento/luma` resolves to whichever area's theme matches (there is
/// only ever one physical theme per `Vendor/name` per area on a stock
/// install); an already-qualified id passes through if it exists.
fn qualify(id: &str, themes: &[(String, PathBuf)]) -> Result<String, DeployError> {
    if area_of(id).is_some_and(|a| a == "frontend" || a == "adminhtml") {
        // already qualified
        if themes.iter().any(|(tid, _)| tid == id) {
            return Ok(id.to_string());
        }
        return Err(err(format!("theme not found on disk: {id}")));
    }
    let matches: Vec<&String> = themes
        .iter()
        .map(|(tid, _)| tid)
        .filter(|tid| tid.strip_prefix("frontend/").is_some_and(|r| r == id)
            || tid.strip_prefix("adminhtml/").is_some_and(|r| r == id))
        .collect();
    match matches.len() {
        0 => Err(err(format!(
            "theme not found on disk: {id} (try an area-qualified id like frontend/{id})"
        ))),
        1 => Ok(matches[0].clone()),
        _ => Err(err(format!(
            "ambiguous theme {id} across areas: {}; qualify it (e.g. frontend/{id})",
            matches.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ))),
    }
}

/// Build the deploy plan (the ordered list of `(area, locale)` groups) from
/// the matrix. `default_locales` is the positional locale set; `theme_specs`
/// empty ⇒ all discovered themes with the defaults. `areas` is the include
/// filter (empty ⇒ both frontend and adminhtml).
pub fn plan(
    inputs: &DeployInputs,
    default_locales: &[String],
    theme_specs: &[ThemeSpec],
    areas: &[String],
) -> Result<Vec<Group>, DeployError> {
    let area_ok = |a: &str| areas.is_empty() || areas.iter().any(|x| x == a);

    // Resolve the requested (theme, locales) list — preserving order.
    let resolved: Vec<(String, Vec<String>)> = if theme_specs.is_empty() {
        // Default deployable set: every discovered theme, default locales.
        inputs
            .themes
            .iter()
            .map(|(id, _)| (id.clone(), default_locales.to_vec()))
            .collect()
    } else {
        let mut v = Vec::with_capacity(theme_specs.len());
        for spec in theme_specs {
            let id = qualify(&spec.id, &inputs.themes)?;
            let locales = spec.locales.clone().unwrap_or_else(|| default_locales.to_vec());
            v.push((id, locales));
        }
        v
    };

    if resolved.iter().all(|(_, l)| l.is_empty()) {
        return Err(err(
            "no locales to deploy (pass at least one positional locale, e.g. en_US)".into(),
        ));
    }

    // Group by (area, locale), preserving theme insertion order within a group.
    // BTreeMap key orders groups deterministically (area, then locale).
    let mut groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (id, locales) in &resolved {
        let Some(area) = area_of(id) else { continue };
        if !area_ok(area) {
            continue;
        }
        for locale in locales {
            let entry = groups.entry((area.to_string(), locale.clone())).or_default();
            if !entry.contains(id) {
                entry.push(id.clone());
            }
        }
    }

    Ok(groups
        .into_iter()
        .map(|((area, locale), theme_ids)| Group {
            area,
            locale,
            theme_ids,
        })
        .collect())
}

/// Pre-extract the (locale-independent) js/html phrase set for every distinct
/// area in the plan — the expensive scan, done once and shared read-only.
fn extract_all_area_phrases(
    inputs: &DeployInputs,
    groups: &[Group],
) -> BTreeMap<String, HashSet<String>> {
    let mut area_phrases: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    for g in groups {
        area_phrases.entry(g.area.clone()).or_insert_with(|| {
            let dirs = inputs.area_theme_dirs(&g.area);
            super::jstranslation::extract_area_phrases(&inputs.root, &g.area, &inputs.scan_modules, &dirs)
        });
    }
    area_phrases
}

/// Build ONE group's packages (all themes, in order, sharing a fresh cache).
fn build_group_packages(
    inputs: &DeployInputs,
    g: &Group,
    area_phrases: &BTreeMap<String, HashSet<String>>,
    opts: &PlacementOptions,
) -> Result<Vec<ThemePackage>, DeployError> {
    let phrases = area_phrases.get(&g.area).expect("area phrases pre-extracted");
    // Theme-chain i18n for the dictionary (extraction is theme-independent,
    // but a theme's own i18n csv could add entries — use the first theme's
    // chain, as the group's themes share an area).
    let chain_dirs: Vec<PathBuf> = g
        .theme_ids
        .first()
        .and_then(|id| super::less::theme_chain(&g.area, id, &inputs.themes).ok())
        .map(|chain| {
            chain
                .iter()
                .filter_map(|t| inputs.themes.iter().find(|(tid, _)| tid == &t.id).map(|(_, d)| d.clone()))
                .collect()
        })
        .unwrap_or_default();
    let js_translation = files::js_translation_for(inputs, &g.locale, &chain_dirs, phrases);
    files::build_group(inputs, &g.area, &g.theme_ids, &g.locale, &js_translation, opts)
}

/// Wrap a fan-out closure in the requested thread pool. `--jobs 1` forces
/// serial (the determinism baseline); `None`/0 uses the global rayon pool.
fn with_pool<F, T>(jobs: Option<usize>, run: F) -> Result<T, DeployError>
where
    F: FnOnce() -> Result<T, DeployError> + Send,
    T: Send,
{
    match jobs {
        Some(n) if n >= 1 => {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(n)
                .build()
                .map_err(|e| err(format!("thread pool: {e}")))?;
            pool.install(run)
        }
        _ => run(),
    }
}

/// Execute the plan and WRITE each group's packages under `static_root` as it
/// finishes — the memory-bounded path (one group's bytes live at a time per
/// worker) the CLI uses. Fan-out over groups; writes are to disjoint package
/// dirs so they never race. Returns per-`(area, theme, locale)` stats.
pub fn execute_to_disk(
    inputs: &DeployInputs,
    groups: &[Group],
    static_root: &std::path::Path,
    opts: &PlacementOptions,
    jobs: Option<usize>,
) -> Result<Vec<TupleStat>, DeployError> {
    use files::PlacedKind as K;
    let area_phrases = extract_all_area_phrases(inputs, groups);

    let stats: Vec<Vec<TupleStat>> = with_pool(jobs, || {
        groups
            .par_iter()
            .map(|g| {
                let packages = build_group_packages(inputs, g, &area_phrases, opts)?;
                let mut rows = Vec::with_capacity(packages.len());
                for pkg in &packages {
                    let target = files::package_dir(static_root, &g.area, &pkg.theme, &g.locale);
                    write_package(pkg, &target)?;
                    rows.push(TupleStat {
                        area: g.area.clone(),
                        // Bare `Vendor/name` (the area is a separate field).
                        theme: pkg.theme_path.clone(),
                        locale: g.locale.clone(),
                        output: target,
                        files: pkg.files.len(),
                        bytes: pkg.bytes(),
                        copied: pkg.count(K::Copy),
                        css_processed: pkg.count(K::CssProcessed),
                        less_compiled: pkg.count(K::LessCompiled),
                        requirejs: pkg.count(K::RequireJs),
                        bundles: pkg.count(K::Bundle),
                        warnings: pkg.warnings.clone(),
                    });
                }
                Ok(rows)
            })
            .collect::<Result<Vec<_>, DeployError>>()
    })?;

    Ok(stats.into_iter().flatten().collect())
}

/// Write one theme package to `target` (the real deploy's per-package
/// `bundle` clear, then every file in write order). Mirrors the CLI's
/// `static files` writer.
fn write_package(pkg: &ThemePackage, target: &std::path::Path) -> Result<(), DeployError> {
    let bundle_dir = target.join(super::bundle::BUNDLE_JS_DIR);
    if bundle_dir.is_dir() {
        std::fs::remove_dir_all(&bundle_dir)
            .map_err(|e| err(format!("clear {}: {e}", bundle_dir.display())))?;
    }
    for f in &pkg.files {
        let path = target.join(&f.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| err(format!("mkdir {}: {e}", parent.display())))?;
        }
        std::fs::write(&path, &f.content).map_err(|e| err(format!("write {}: {e}", path.display())))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs_with_themes(themes: &[&str]) -> DeployInputs {
        DeployInputs {
            root: PathBuf::from("/nonexistent"),
            themes: themes.iter().map(|t| (t.to_string(), PathBuf::from("/x"))).collect(),
            modules: Vec::new(),
            reg_modules: Vec::new(),
            scan_modules: Vec::new(),
            min_resolver: String::new(),
        }
    }

    #[test]
    fn default_theme_set_all_areas() {
        let inp = inputs_with_themes(&[
            "frontend/Magento/blank",
            "frontend/Magento/luma",
            "adminhtml/Magento/backend",
        ]);
        let groups = plan(&inp, &["en_US".into()], &[], &[]).unwrap();
        // one frontend group (blank+luma) + one adminhtml group (backend).
        assert_eq!(groups.len(), 2);
        let fe = groups.iter().find(|g| g.area == "frontend").unwrap();
        assert_eq!(fe.theme_ids, vec!["frontend/Magento/blank", "frontend/Magento/luma"]);
        let adm = groups.iter().find(|g| g.area == "adminhtml").unwrap();
        assert_eq!(adm.theme_ids, vec!["adminhtml/Magento/backend"]);
    }

    #[test]
    fn area_filter_restricts() {
        let inp = inputs_with_themes(&["frontend/Magento/luma", "adminhtml/Magento/backend"]);
        let groups = plan(&inp, &["en_US".into()], &[], &["frontend".into()]).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].area, "frontend");
    }

    #[test]
    fn per_theme_locale_override() {
        let inp = inputs_with_themes(&["frontend/Magento/blank", "frontend/Magento/luma"]);
        let specs = vec![
            ThemeSpec { id: "Magento/blank".into(), locales: Some(vec!["en_US".into()]) },
            ThemeSpec { id: "Magento/luma".into(), locales: Some(vec!["de_DE".into()]) },
        ];
        let groups = plan(&inp, &["en_US".into()], &specs, &[]).unwrap();
        // Two groups: (frontend, en_US)=[blank], (frontend, de_DE)=[luma].
        assert_eq!(groups.len(), 2);
        let en = groups.iter().find(|g| g.locale == "en_US").unwrap();
        assert_eq!(en.theme_ids, vec!["frontend/Magento/blank"]);
        let de = groups.iter().find(|g| g.locale == "de_DE").unwrap();
        assert_eq!(de.theme_ids, vec!["frontend/Magento/luma"]);
    }

    #[test]
    fn bare_theme_inherits_defaults() {
        let inp = inputs_with_themes(&["frontend/Magento/blank", "frontend/Magento/luma"]);
        let specs = vec![
            ThemeSpec { id: "Magento/blank".into(), locales: None },
            ThemeSpec { id: "Magento/luma".into(), locales: Some(vec!["fr_FR".into()]) },
        ];
        let groups = plan(&inp, &["en_US".into(), "nl_NL".into()], &specs, &[]).unwrap();
        // blank in en_US and nl_NL; luma only fr_FR → 3 groups.
        assert_eq!(groups.len(), 3);
        let en = groups.iter().find(|g| g.locale == "en_US").unwrap();
        assert_eq!(en.theme_ids, vec!["frontend/Magento/blank"]);
        let nl = groups.iter().find(|g| g.locale == "nl_NL").unwrap();
        assert_eq!(nl.theme_ids, vec!["frontend/Magento/blank"]);
        let fr = groups.iter().find(|g| g.locale == "fr_FR").unwrap();
        assert_eq!(fr.theme_ids, vec!["frontend/Magento/luma"]);
    }

    #[test]
    fn ambiguous_bare_theme_errors() {
        let inp = inputs_with_themes(&["frontend/Vendor/x", "adminhtml/Vendor/x"]);
        let specs = vec![ThemeSpec { id: "Vendor/x".into(), locales: None }];
        let e = plan(&inp, &["en_US".into()], &specs, &[]).unwrap_err();
        assert!(e.message.contains("ambiguous"), "{}", e.message);
    }

    #[test]
    fn unknown_theme_errors() {
        let inp = inputs_with_themes(&["frontend/Magento/luma"]);
        let specs = vec![ThemeSpec { id: "Magento/nope".into(), locales: None }];
        let e = plan(&inp, &["en_US".into()], &specs, &[]).unwrap_err();
        assert!(e.message.contains("not found"), "{}", e.message);
    }
}
