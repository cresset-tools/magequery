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
//!
//! ## Scope of the shared cache (a deliberate divergence)
//!
//! Real Magento shares ONE `Bundle` `$excludedCache` across the WHOLE
//! `setup:static-content:deploy` process — every area AND every locale — so in
//! a single combined `bin/magento` invocation a `.min` sibling seen in the
//! first-processed package poisons the plain name for every later package,
//! across areas and locales alike. We scope the shared cache to the
//! `(area, locale)` group instead. This is intentional: it makes fan-out
//! trivially deterministic (a run-global cache would reintroduce completion-
//! order dependence), and it matches how every golden was captured — as an
//! independent per-`(area, locale)` `scd`. The one observable consequence is
//! that a combined `static deploy en_US fr_FR` (or frontend+adminhtml at once)
//! keeps a `.min`-shadowed plain file (e.g. `vimeo/player.js`) in each group's
//! first theme, whereas one real combined `bin/magento` command keeps it in at
//! most a single package. It equals N separate per-`(area, locale)` real
//! deploys, NOT one combined command.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use super::files::{self, DeployInputs, PlacementOptions, ThemePackage};

/// Magento's allowed-locale set — the curated `Locale\Config::$_allowedLocales`
/// list (`array_keys(Setup\Lists::getLocaleList())` before the ICU-availability
/// filter, which only ever REMOVES entries, so this stays a safe superset of
/// what any given install accepts). `setup:static-content:deploy` runs every
/// `--language` value through `Validator\Locale::isValid` in `InputValidator`
/// and aborts on an unknown code before doing any work; we mirror that. Sorted
/// for `binary_search`.
const ALLOWED_LOCALES: &[&str] = &[
    "af_ZA",    "ar_DZ",    "ar_EG",    "ar_KW",    "ar_MA",    "ar_SA",    "az_Latn_AZ",    "be_BY",
    "bg_BG",    "bn_BD",    "bs_Latn_BA",    "ca_ES",    "cs_CZ",    "cy_GB",    "da_DK",    "de_AT",
    "de_CH",    "de_DE",    "de_LU",    "el_GR",    "en_AU",    "en_CA",    "en_GB",    "en_IE",
    "en_NZ",    "en_US",    "es_AR",    "es_BO",    "es_CL",    "es_CO",    "es_CR",    "es_ES",
    "es_MX",    "es_PA",    "es_PE",    "es_US",    "es_VE",    "et_EE",    "eu_ES",    "fa_IR",
    "fi_FI",    "fil_PH",    "fr_BE",    "fr_CA",    "fr_CH",    "fr_FR",    "fr_LU",    "gl_ES",
    "gu_IN",    "he_IL",    "hi_IN",    "hr_HR",    "hu_HU",    "id_ID",    "is_IS",    "it_CH",
    "it_IT",    "ja_JP",    "ka_GE",    "km_KH",    "ko_KR",    "lo_LA",    "lt_LT",    "lv_LV",
    "mk_MK",    "mn_Cyrl_MN",    "ms_Latn_MY",    "ms_MY",    "nb_NO",    "nl_BE",    "nl_NL",    "nn_NO",
    "pl_PL",    "pt_BR",    "pt_PT",    "ro_RO",    "ru_RU",    "sk_SK",    "sl_SI",    "sq_AL",
    "sr_Cyrl_RS",    "sr_Latn_RS",    "sv_FI",    "sv_SE",    "sw_KE",    "th_TH",    "tr_TR",    "uk_UA",
    "vi_VN",    "zh_Hans_CN",    "zh_Hant_HK",    "zh_Hant_TW",
];

/// Is `loc` a locale `setup:static-content:deploy` would accept? Rejecting an
/// unknown code here also blocks a locale carrying path separators / `..`,
/// which [`files::package_dir`] would otherwise splice verbatim into the output
/// path (a deploy writing outside `--out`).
pub fn is_allowed_locale(loc: &str) -> bool {
    ALLOWED_LOCALES.binary_search(&loc).is_ok()
}

/// How the shared bundle-cache `.min`-sibling ordering is resolved — the
/// `magecommand static deploy --order` choice, as a typed value for in-process
/// callers.
#[derive(Debug, Clone)]
pub enum Order {
    /// Deterministic byte-sorted ordering (no minification probe).
    Sorted,
    /// Probe real minification, using the given scratch dir (`None` = the static
    /// root itself).
    Probe(Option<PathBuf>),
}

/// A whole `setup:static-content:deploy` run as a linkable request. Mirrors the
/// `magecommand static deploy` CLI arguments so an in-process caller (e.g.
/// magebuild) gets byte-identical matrix orchestration without shelling out.
#[derive(Debug, Clone)]
pub struct DeployRequest {
    /// The positional/default locale set — at least one required.
    pub locales: Vec<String>,
    /// Theme matrix entries: a bare/area-qualified `id`, or `id:loc1,loc2` to
    /// override that theme's locales. Empty ⇒ every discovered theme with the
    /// default locales.
    pub themes: Vec<String>,
    /// Area include filter (`frontend` / `adminhtml`); empty ⇒ both.
    pub areas: Vec<String>,
    /// Static-root override; `None` ⇒ `<root>/pub/static`.
    pub out: Option<PathBuf>,
    /// Bundle `.min`-sibling ordering strategy.
    pub order: Order,
    /// Don't walk theme `<parent>` fallbacks when discovering the default set.
    pub no_parent: bool,
    /// `deployed_version.txt` contents written once at the static root; `None` ⇒
    /// don't write it (never an invented timestamp).
    pub deployed_version: Option<String>,
    /// rayon job cap: `None` ⇒ rayon default, `Some(1)` ⇒ serial.
    pub jobs: Option<usize>,
    /// Skip gzip/brotli pre-compression of text assets.
    pub no_compress: bool,
}

/// The result of [`deploy_to_disk`] — everything the CLI renders, so an
/// in-process caller can report identically.
#[derive(Debug, Clone)]
pub struct DeploySummary {
    /// The static root everything was written under.
    pub static_root: PathBuf,
    /// The planned `(area, locale)` groups, in build order.
    pub groups: Vec<Group>,
    /// Themes dropped for a dangling `<parent>` chain (non-fatal diagnostics).
    pub skipped: Vec<SkippedTheme>,
    /// Per-`(area, theme, locale)` placement stats.
    pub stats: Vec<TupleStat>,
    /// Wall-clock of the execute (write-to-disk) phase.
    pub elapsed: std::time::Duration,
    /// Plugins on an extension point we DO model whose effect we do not
    /// recognize — these certainly change the output, so the CLI always warns.
    pub plugin_warnings: Vec<String>,
    /// Plugins on a deploy-path type we do not model at all. Often
    /// deploy-irrelevant, so the CLI surfaces them only under `--verbose`.
    pub plugin_notices: Vec<String>,
}

/// Reproduce a whole `setup:static-content:deploy` run over the
/// theme × locale × area matrix, writing every package under the static root.
/// This is the one in-process entry point: `magecommand static deploy` and
/// magebuild both call it, and the CLI adds only argument parsing and result
/// rendering on top. Fatal problems (bad root, invalid locale/area, an empty
/// plan, a write failure) return `Err`; a theme skipped for a dangling parent
/// chain is a non-fatal [`DeploySummary::skipped`] entry.
pub fn deploy_to_disk(root: &Path, req: &DeployRequest) -> anyhow::Result<DeploySummary> {
    use anyhow::Context as _;

    if req.locales.is_empty() {
        anyhow::bail!("no locales given — pass at least one, e.g. `static deploy en_US`");
    }
    for a in &req.areas {
        if a != "frontend" && a != "adminhtml" {
            anyhow::bail!("--area must be `frontend` and/or `adminhtml`, got `{a}`");
        }
    }

    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let magento = magequery_core::Magento::open(&root)
        .with_context(|| format!("not a Magento root: {}", root.display()))?;

    let static_root = match &req.out {
        Some(dir) => dir.clone(),
        None => root.join("pub").join("static"),
    };

    // Parse the theme matrix: `id` or `id:loc1,loc2`.
    let theme_specs: Vec<ThemeSpec> = req
        .themes
        .iter()
        .map(|raw| match raw.split_once(':') {
            Some((id, locs)) => ThemeSpec {
                id: id.to_string(),
                locales: Some(
                    locs.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect(),
                ),
            },
            None => ThemeSpec { id: raw.to_string(), locales: None },
        })
        .collect();

    let inputs = DeployInputs::prepare(&magento).map_err(|e| anyhow::anyhow!("{e}"))?;
    let Plan { groups, skipped } = plan(&inputs, &req.locales, &theme_specs, &req.areas, req.no_parent)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if groups.is_empty() {
        // Every candidate theme was skipped (dangling parent chain) or the area
        // filter matched nothing — fold the skip reasons into the error so the
        // diagnostic survives even without a summary to carry it.
        if skipped.is_empty() {
            anyhow::bail!("nothing to deploy (no theme matches the area filter)");
        }
        let detail = skipped
            .iter()
            .map(|s| format!("{} ({})", s.id, s.reason))
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::bail!("nothing to deploy — every candidate theme was skipped: {detail}");
    }

    let order_mode = match &req.order {
        Order::Sorted => super::bundle::OrderMode::Sorted,
        Order::Probe(scratch) => {
            let scratch = scratch.clone().unwrap_or_else(|| static_root.clone());
            std::fs::create_dir_all(&scratch)
                .with_context(|| format!("create probe scratch {}", scratch.display()))?;
            super::bundle::OrderMode::Probe(scratch)
        }
    };
    // File-set exclusions contributed by DI plugins on `Deploy\Package\Package`
    // (Hyva's tailwind drop). Read from THIS store's di.xml, never assumed.
    let plugins = files::deploy_plugin_effects(&magento);
    let (plugin_warnings, plugin_notices) = (plugins.unknown(), plugins.unmodelled_surface());
    let opts = PlacementOptions { compress: !req.no_compress, order: order_mode, plugins };

    // deployed_version.txt — ONE file at the static root, written first (only
    // with an explicit version, never an invented timestamp).
    if let Some(version) = &req.deployed_version {
        std::fs::create_dir_all(&static_root)
            .with_context(|| format!("mkdir {}", static_root.display()))?;
        let p = static_root.join(files::DEPLOYED_VERSION_FILE_NAME);
        std::fs::write(&p, version.as_bytes())
            .with_context(|| format!("write {}", p.display()))?;
    }

    let started = std::time::Instant::now();
    let stats = execute_to_disk(&inputs, &groups, &static_root, &opts, req.jobs)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let elapsed = started.elapsed();

    Ok(DeploySummary {
        static_root,
        groups,
        skipped,
        stats,
        elapsed,
        plugin_warnings,
        plugin_notices,
    })
}

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

/// A discovered theme dropped from the **default** (all-themes) deploy because
/// its `theme.xml` `<parent>` fallback chain dangles — an ancestor is absent on
/// disk (e.g. a project theme inheriting from a Hyvä/vendor theme that isn't
/// installed). Per the project's Error-vs-Diagnostic split, a single broken
/// theme is a diagnostic, not a fatal error: the rest of the run proceeds. An
/// explicitly-named `--theme` still fails loud (you named it).
#[derive(Debug, Clone)]
pub struct SkippedTheme {
    /// Area-qualified id (`adminhtml/Vendor/theme`).
    pub id: String,
    /// Why it was skipped (the theme-chain error message).
    pub reason: String,
}

/// The output of [`plan`]: the deployable `(area, locale)` groups plus any
/// discovered themes skipped for a dangling parent chain.
#[derive(Debug, Clone)]
pub struct Plan {
    pub groups: Vec<Group>,
    pub skipped: Vec<SkippedTheme>,
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
    no_parent: bool,
) -> Result<Plan, DeployError> {
    let area_ok = |a: &str| areas.is_empty() || areas.iter().any(|x| x == a);
    let mut skipped: Vec<SkippedTheme> = Vec::new();

    // Validate every requested locale against Magento's allowed set before any
    // work (real SCD's InputValidator does this) — and, as a corollary, reject
    // a locale that would escape the output root as a path segment.
    let mut to_check: Vec<&String> = default_locales.iter().collect();
    for spec in theme_specs {
        if let Some(locs) = &spec.locales {
            to_check.extend(locs.iter());
        }
    }
    for loc in to_check {
        if !is_allowed_locale(loc) {
            return Err(err(format!(
                "'{loc}' is not a valid locale — pass a supported code like en_US \
                 (Magento's info:language:list)"
            )));
        }
    }

    // Resolve the requested (theme, locales) list — preserving order.
    let resolved: Vec<(String, Vec<String>)> = if theme_specs.is_empty() {
        // Default deployable set: every discovered theme, default locales.
        // A discovered theme whose `<parent>` chain dangles (an ancestor absent
        // on disk) would abort the entire multi-theme/locale run when its
        // package is built — so validate the chain here and skip the broken
        // theme with a diagnostic instead, mirroring Magento (an unresolvable
        // parent is left unlinked; the rest still deploys). Any theme reaching a
        // missing ancestor is dropped, so no child is left orphaned.
        let mut v = Vec::with_capacity(inputs.themes.len());
        for (id, _) in &inputs.themes {
            if let Some(area) = area_of(id) {
                if let Err(e) = super::less::theme_chain(area, id, &inputs.themes) {
                    skipped.push(SkippedTheme { id: id.clone(), reason: e.message });
                    continue;
                }
            }
            v.push((id.clone(), default_locales.to_vec()));
        }
        v
    } else {
        let mut v = Vec::with_capacity(theme_specs.len());
        for spec in theme_specs {
            let id = qualify(&spec.id, &inputs.themes)?;
            let locales = match &spec.locales {
                // An explicitly-named theme resolving to no locales is a
                // footgun (`--theme id:` with a trailing colon / shell-stripped
                // list) — fail loudly instead of silently dropping it.
                Some(l) if l.is_empty() => {
                    return Err(err(format!(
                        "theme {} has an empty locale override — nothing to deploy for it",
                        spec.id
                    )));
                }
                Some(l) => l.clone(),
                None => default_locales.to_vec(),
            };
            // Real quick-strategy SCD always deploys a child theme's ancestors
            // too (PackagePool retains the parent; QuickDeploy's
            // `parentCompilationRequested` defaults on). `--no-parent` opts out
            // (Magento's NO_PARENT). Emit the chain root-first so a parent
            // poisons the child's `.min` siblings before the child is built.
            if !no_parent {
                if let Some(area) = area_of(&id) {
                    let chain = super::less::theme_chain(area, &id, &inputs.themes)?;
                    for anc in chain.iter().rev() {
                        v.push((anc.id.clone(), locales.clone()));
                    }
                    continue;
                }
            }
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

    let groups = groups
        .into_iter()
        .map(|((area, locale), theme_ids)| Group {
            area,
            locale,
            theme_ids,
        })
        .collect();
    Ok(Plan { groups, skipped })
}

/// Build ONE group's packages (all themes, in order, sharing a fresh cache).
/// The `js-translation.json` (its js/html phrase scan is the expensive part) is
/// computed here inside the parallel group task — [`files::js_translation_for`]
/// skips the scan when the locale's dictionary is empty (en_US), and otherwise
/// it overlaps across groups.
fn build_group_packages(
    inputs: &DeployInputs,
    g: &Group,
    opts: &PlacementOptions,
) -> Result<Vec<ThemePackage>, DeployError> {
    // The phrase scan is theme-independent (`getData` ignores its themePath),
    // so hoist it out of the per-theme dictionary build inside `build_group`.
    let area_phrases = files::area_phrases_for(inputs, &g.area);
    files::build_group(inputs, &g.area, &g.theme_ids, &g.locale, &area_phrases, opts)
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

/// `<static root>/<area>/sri-hashes.json` — the ONE integrity file per AREA
/// (`Csp\Model\SubresourceIntegrity\Storage\File::resolveFilePath`, whose
/// `$context` is the area name), never a per-package artifact.
pub fn area_sri_path(static_root: &std::path::Path, area: &str) -> PathBuf {
    static_root.join(area).join(files::SRI_HASHES_FILE_NAME)
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

    // Per group: the stat rows plus this group's contribution to its area's
    // `sri-hashes.json`. `par_iter().map().collect()` preserves group order, so
    // the accumulated integrity entries land in the run's deployment order —
    // the order a real deploy's successive `saveBunch` calls produce.
    type GroupOut = (Vec<TupleStat>, Vec<(String, files::PackageSri)>);
    let collected: Vec<GroupOut> = with_pool(jobs, || {
        groups
            .par_iter()
            .map(|g| {
                let packages = build_group_packages(inputs, g, opts)?;
                let mut rows = Vec::with_capacity(packages.len());
                let mut sri: Vec<(String, files::PackageSri)> =
                    Vec::with_capacity(packages.len());
                for pkg in &packages {
                    let target = files::package_dir(static_root, &g.area, &pkg.theme, &g.locale);
                    write_package(pkg, &target)?;
                    sri.push((g.area.clone(), pkg.sri.clone()));
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
                Ok((rows, sri))
            })
            .collect::<Result<Vec<_>, DeployError>>()
    })?;

    // One `sri-hashes.json` per area at the static root. `RemoveAllAssetIntegrityHashes`
    // wipes each area's file before a CLI deploy and the run then accumulates
    // into it, so writing exactly this run's entries reproduces the result.
    // Each SRI phase runs across EVERY package before the next starts, so the
    // file lists all packages' published js, THEN all their `requirejs-config.js`,
    // THEN all their bundles — not one package's three phases at a time.
    let mut by_area: Vec<(String, Vec<&files::PackageSri>)> = Vec::new();
    for (_, groups_sri) in &collected {
        for (area, pkg_sri) in groups_sri {
            match by_area.iter_mut().find(|(a, _)| a == area) {
                Some((_, acc)) => acc.push(pkg_sri),
                None => by_area.push((area.clone(), vec![pkg_sri])),
            }
        }
    }
    for (area, pkgs) in &by_area {
        let entries: Vec<(String, String)> = pkgs
            .iter()
            .flat_map(|p| p.package.iter())
            .chain(pkgs.iter().flat_map(|p| p.requirejs.iter()))
            .chain(pkgs.iter().flat_map(|p| p.bundles.iter()))
            .cloned()
            .collect();
        let path = area_sri_path(static_root, area);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| err(format!("mkdir {}: {e}", parent.display())))?;
        }
        std::fs::write(&path, files::sri_hashes_json(&entries))
            .map_err(|e| err(format!("write {}: {e}", path.display())))?;
    }

    Ok(collected.into_iter().flat_map(|(rows, _)| rows).collect())
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
    // Create every distinct parent directory ONCE up front (a package's ~1600
    // files share ~100 dirs — a per-file `create_dir_all` re-walked each path).
    // Then the writes themselves are independent (distinct paths) and I/O-bound,
    // so fan them out: writing a package is otherwise a serial syscall storm
    // (`openat`/`write`/`mkdir`) on the deploy's critical path.
    let mut dirs: HashSet<PathBuf> = HashSet::new();
    for f in &pkg.files {
        if let Some(parent) = std::path::Path::new(&f.path).parent() {
            if !parent.as_os_str().is_empty() {
                dirs.insert(target.join(parent));
            }
        }
    }
    for dir in &dirs {
        std::fs::create_dir_all(dir).map_err(|e| err(format!("mkdir {}: {e}", dir.display())))?;
    }
    pkg.files.par_iter().try_for_each(|f| {
        let path = target.join(&f.path);
        std::fs::write(&path, &f.content).map_err(|e| err(format!("write {}: {e}", path.display())))
    })
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
            language_packs: Vec::new(),
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
        let groups = plan(&inp, &["en_US".into()], &[], &[], false).unwrap().groups;
        // one frontend group (blank+luma) + one adminhtml group (backend).
        assert_eq!(groups.len(), 2);
        let fe = groups.iter().find(|g| g.area == "frontend").unwrap();
        assert_eq!(fe.theme_ids, vec!["frontend/Magento/blank", "frontend/Magento/luma"]);
        let adm = groups.iter().find(|g| g.area == "adminhtml").unwrap();
        assert_eq!(adm.theme_ids, vec!["adminhtml/Magento/backend"]);
    }

    #[test]
    fn default_skips_theme_with_dangling_parent() {
        // A discovered theme whose theme.xml `<parent>` points at a theme that
        // isn't on disk (a project theme inheriting from a vendor theme that
        // isn't installed) must NOT abort the whole default run: it's dropped
        // with a reason, and the healthy siblings still deploy.
        let td = tempfile::tempdir().unwrap();
        let child = td.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("theme.xml"), "<theme><parent>Vendor/gone</parent></theme>").unwrap();
        let ok = td.path().join("ok");
        std::fs::create_dir_all(&ok).unwrap();
        std::fs::write(ok.join("theme.xml"), "<theme><title>OK</title></theme>").unwrap();

        let inp = DeployInputs {
            root: td.path().to_path_buf(),
            themes: vec![
                ("frontend/Vendor/child".into(), child),
                ("frontend/Vendor/ok".into(), ok),
            ],
            modules: Vec::new(),
            reg_modules: Vec::new(),
            scan_modules: Vec::new(),
            language_packs: Vec::new(),
            min_resolver: String::new(),
        };
        let p = plan(&inp, &["en_US".into()], &[], &[], false).unwrap();
        assert_eq!(p.skipped.len(), 1);
        assert_eq!(p.skipped[0].id, "frontend/Vendor/child");
        assert!(
            p.skipped[0].reason.contains("parent theme 'frontend/Vendor/gone'"),
            "{}",
            p.skipped[0].reason
        );
        // The healthy theme still deploys — one broken theme never blinds the run.
        assert_eq!(p.groups.len(), 1);
        assert_eq!(p.groups[0].theme_ids, vec!["frontend/Vendor/ok"]);
    }

    #[test]
    fn area_filter_restricts() {
        let inp = inputs_with_themes(&["frontend/Magento/luma", "adminhtml/Magento/backend"]);
        let groups = plan(&inp, &["en_US".into()], &[], &["frontend".into()], false).unwrap().groups;
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
        let groups = plan(&inp, &["en_US".into()], &specs, &[], false).unwrap().groups;
        // Two groups: (frontend, en_US)=[blank], (frontend, de_DE)=[luma].
        // (Fake theme dirs have no readable theme.xml, so no parent expansion.)
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
        let groups = plan(&inp, &["en_US".into(), "nl_NL".into()], &specs, &[], false).unwrap().groups;
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
        let e = plan(&inp, &["en_US".into()], &specs, &[], false).unwrap_err();
        assert!(e.message.contains("ambiguous"), "{}", e.message);
    }

    #[test]
    fn unknown_theme_errors() {
        let inp = inputs_with_themes(&["frontend/Magento/luma"]);
        let specs = vec![ThemeSpec { id: "Magento/nope".into(), locales: None }];
        let e = plan(&inp, &["en_US".into()], &specs, &[], false).unwrap_err();
        assert!(e.message.contains("not found"), "{}", e.message);
    }

    #[test]
    fn invalid_locale_is_rejected() {
        let inp = inputs_with_themes(&["frontend/Magento/blank"]);
        // Positional locale.
        let e = plan(&inp, &["zz_ZZ".into()], &[], &[], false).unwrap_err();
        assert!(e.message.contains("not a valid locale"), "{}", e.message);
        // Per-theme override locale.
        let specs = vec![ThemeSpec { id: "Magento/blank".into(), locales: Some(vec!["zz_ZZ".into()]) }];
        let e = plan(&inp, &["en_US".into()], &specs, &[], false).unwrap_err();
        assert!(e.message.contains("not a valid locale"), "{}", e.message);
        // Path-traversal locale (the SM-4 escape) is rejected as invalid.
        let e = plan(&inp, &["../../../etc".into()], &[], &[], false).unwrap_err();
        assert!(e.message.contains("not a valid locale"), "{}", e.message);
    }

    #[test]
    fn empty_locale_override_errors_not_drops() {
        let inp = inputs_with_themes(&["frontend/Magento/blank", "frontend/Magento/luma"]);
        // luma named with an empty override, blank carries the run: must error,
        // never silently drop luma.
        let specs = vec![
            ThemeSpec { id: "Magento/blank".into(), locales: None },
            ThemeSpec { id: "Magento/luma".into(), locales: Some(vec![]) },
        ];
        let e = plan(&inp, &["en_US".into()], &specs, &[], false).unwrap_err();
        assert!(e.message.contains("empty locale override"), "{}", e.message);
    }

    #[test]
    fn allowlist_is_sorted_for_binary_search() {
        let mut sorted = ALLOWED_LOCALES.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, ALLOWED_LOCALES, "ALLOWED_LOCALES must stay sorted");
        assert!(is_allowed_locale("en_US") && is_allowed_locale("zh_Hant_TW"));
        assert!(!is_allowed_locale("zz_ZZ"));
    }
}
