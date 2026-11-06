//! The parsed installation. Step 1 builds the module list; later steps add the per-area
//! DI config and class resolver to this struct.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::composer;
use crate::error::{Diagnostic, Error, Result};
use crate::ids::ModuleName;
use crate::model::{DuplicateModule, Module, ModuleCheck, ModuleSource, UnregisteredModule};
use crate::parse;
use crate::engine::resolver;
use crate::engine::vfs::Vfs;

use std::sync::Arc;

pub(crate) struct Index {
    pub root: PathBuf,
    /// Unsaved-buffer overlay; every content read of a source file goes through it.
    pub vfs: Arc<Vfs>,
    pub modules: Vec<Module>,
    pub check: ModuleCheck,
    pub resolver: resolver::ClassResolver,
    pub diagnostics: Vec<Diagnostic>,
    /// Named composer packages (root + `require`), retained for the `deps` graph.
    pub packages: Vec<PackageMeta>,
    /// Library component paths (magento2-library packages), registration order.
    pub library_paths: Vec<PathBuf>,
    /// Modules read from an [`OpenOptions::extra_app_code`] directory: their source
    /// root on disk → the `app/code/<Vendor>/<Module>` path they are compiled as.
    pub mounts: HashMap<PathBuf, PathBuf>,
}

/// How [`Index::build`] decides which modules exist and in what order.
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Ignore `app/etc/config.php`: every discovered module is enabled, ordered the
    /// way Magento orders a fresh install (`ModuleList\Loader::load`). For a
    /// checkout that was never installed, such as the root an integration-test
    /// sandbox is built from.
    pub all_modules: bool,
    /// Directories laid out like `app/code` (`<Vendor>/<Module>/etc/module.xml`).
    /// Their modules are read from there but treated as if deployed under
    /// `app/code`, which is what Magento's integration-test bootstrap does with
    /// `dev/tests/integration/_files`.
    pub extra_app_code: Vec<PathBuf>,
}

/// The slice of a composer package `deps`/`info` need: who it is, where it lives (to map
/// modules to their owning package), its version, and what it requires.
pub(crate) struct PackageMeta {
    pub name: String,
    pub version: Option<String>,
    pub root: PathBuf,
    pub require: Vec<String>,
    /// `autoload.files` entries (registration.php paths, relative to `root`).
    /// Pinpoint components bundled under a subdir (e.g. Hyvä's admin theme at
    /// `src/theme/`) that `root/theme.xml` alone would miss — same trick vendor
    /// module discovery uses for `src/`-bundled modules.
    pub autoload_files: Vec<String>,
}

struct Discovered {
    source: ModuleSource,
    path: PathBuf,
    sequence: Vec<ModuleName>,
}

/// Modules found so far, by name, plus every name found in a second place.
#[derive(Default)]
struct Discoveries {
    modules: HashMap<ModuleName, Discovered>,
    duplicates: Vec<DuplicateModule>,
}

impl Index {
    pub fn build(root: &Path, vfs: Arc<Vfs>, opts: &OpenOptions) -> Result<Index> {
        let config_php = root.join("app/etc/config.php");
        // Without config.php, the skeleton's app/etc/di.xml is what marks a root.
        let is_root = if opts.all_modules {
            config_php.is_file() || root.join("app/etc/di.xml").is_file()
        } else {
            config_php.is_file()
        };
        if !is_root {
            return Err(Error::NotMagentoRoot {
                path: root.to_path_buf(),
            });
        }

        let mut diagnostics = Vec::new();
        let _p = std::time::Instant::now();

        let ordered = if opts.all_modules {
            None
        } else {
            let text = vfs.read_to_string(&config_php).map_err(|source| Error::Io {
                file: config_php.clone(),
                source,
            })?;
            Some(parse::config_php_modules(&text).map_err(|detail| Error::Parse {
                file: config_php.clone(),
                detail,
            })?)
        };
        prof("config.php read+parse", &_p);

        // Parse composer metadata once — reused for vendor discovery and the class
        // resolver. Empty (with a diagnostic) if installed.json is unreadable.
        let vendor = root.join("vendor");
        let _p = std::time::Instant::now();
        let packages = if vendor.is_dir() {
            match composer::installed_packages(&vendor) {
                Ok(p) => p,
                Err(e) => {
                    diagnostics.push(Diagnostic::warning(
                        format!("composer installed.json unavailable ({e}); scanning vendor/ recursively"),
                        None,
                    ));
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        prof("installed.json read+parse", &_p);

        // Discover modules on disk. app/code is scanned first so it wins over a vendor
        // copy of the same module name, matching Magento's app/code precedence.
        let mut discovered = Discoveries::default();
        let _p = std::time::Instant::now();
        discover_app_code(root, &mut discovered, &mut diagnostics, &vfs);
        prof("app/code discovery", &_p);
        // Extra app/code trees come after the real one (a module already deployed to
        // app/code wins) and before vendor, matching where they will be registered.
        let mut mounts = HashMap::new();
        for dir in &opts.extra_app_code {
            let mut found = Discoveries::default();
            scan(dir, ModuleSource::App, 0, &mut found, &mut diagnostics, &vfs);
            discovered.duplicates.append(&mut found.duplicates);
            let mut found: Vec<_> = found.modules.into_iter().collect();
            found.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, d) in found {
                // A module already deployed to app/code wins, and its fixture copy is
                // not a duplicate: the test bootstrap copies it rather than registering
                // it where it is, so Magento only ever sees the one.
                if discovered.modules.contains_key(&name) {
                    continue;
                }
                if let Ok(rel) = d.path.strip_prefix(dir) {
                    mounts.insert(d.path.clone(), root.join("app/code").join(rel));
                }
                discovered.modules.insert(name, d);
            }
        }
        let _p = std::time::Instant::now();
        if !packages.is_empty() {
            discover_vendor(&packages, &mut discovered, &mut diagnostics, &vfs);
        } else if vendor.is_dir() {
            scan(&vendor, ModuleSource::Vendor, 0, &mut discovered, &mut diagnostics, &vfs);
        }
        prof("vendor discovery", &_p);

        // Without config.php, every module on disk is enabled, in the order Magento's
        // installer would write them.
        let ordered = match ordered {
            Some(o) => o,
            None => sort_by_sequence(discovered.modules.iter().map(|(n, d)| (n, d.sequence.as_slice())))
                .map_err(|detail| Error::Parse { file: PathBuf::from("etc/module.xml"), detail })?
                .into_iter()
                .map(|n| (n, true))
                .collect(),
        };

        // Build the module list in config.php load order, and record any config entry that
        // has no matching module on disk.
        let mut modules = Vec::with_capacity(ordered.len());
        let mut in_config_not_on_disk = Vec::new();
        for (load_order, (name, enabled)) in ordered.into_iter().enumerate() {
            match discovered.modules.remove(&name) {
                Some(d) => modules.push(Module {
                    name,
                    enabled,
                    source: d.source,
                    path: d.path,
                    sequence: d.sequence,
                    load_order,
                }),
                None => in_config_not_on_disk.push(name),
            }
        }

        // Whatever is still in `discovered` exists on disk but isn't in config.php — the
        // "forgot setup:upgrade" case. Reported structurally via `module_check`, not as
        // always-on diagnostic noise.
        let mut duplicates = discovered.duplicates;
        duplicates.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.ignored.cmp(&b.ignored)));
        let mut on_disk_not_in_config: Vec<UnregisteredModule> = discovered
            .modules
            .into_iter()
            .map(|(name, d)| UnregisteredModule { name, source: d.source, path: d.path })
            .collect();
        on_disk_not_in_config.sort_by(|a, b| a.name.cmp(&b.name));

        // The di.xml index is built lazily (see `Magento`) — it's the expensive part and
        // commands like `modules`/`events` don't need it. The resolver is cheap (PSR-4 maps
        // only; PHP parsing is lazy), so it stays eager.
        let resolver = resolver::ClassResolver::build(&packages, &modules, root, Arc::clone(&vfs));

        // Library component paths (magento2-library packages), in registration
        // order: the dirs their `autoload.files` (registration.php) live in.
        // What Magento's ComponentRegistrar would report as LIBRARY — the DI
        // compiler scans these alongside modules.
        let mut library_paths: Vec<PathBuf> = Vec::new();
        for p in &packages {
            if p.package_type.as_deref() == Some("magento2-library") {
                let mut dirs: Vec<PathBuf> = p
                    .autoload_files
                    .iter()
                    .filter_map(|f| p.root.join(f).parent().map(Path::to_path_buf))
                    .collect();
                if dirs.is_empty() {
                    dirs.push(p.root.clone());
                }
                for d in dirs {
                    if !library_paths.contains(&d) {
                        library_paths.push(d);
                    }
                }
            }
        }

        // Keep the slim package facts (already parsed) for the lazy `deps` graph.
        let packages = packages
            .into_iter()
            .filter_map(|p| {
                p.name.map(|name| PackageMeta {
                    name,
                    version: p.version,
                    root: p.root,
                    require: p.require,
                    autoload_files: p.autoload_files,
                })
            })
            .collect();

        Ok(Index {
            root: root.to_path_buf(),
            vfs,
            modules,
            check: ModuleCheck { on_disk_not_in_config, in_config_not_on_disk, duplicates },
            resolver,
            diagnostics,
            packages,
            library_paths,
            mounts,
        })
    }
}

/// Magento's module load order for a fresh install: a port of
/// `Framework\Module\ModuleList\Loader::sortBySequence`, which writes `config.php`
/// on install, `setup:upgrade` and `module:enable`. Ported literally, quirks
/// included, because the order decides how di.xml merges: names sorted bytewise
/// (`ksort`), then every name *containing* `Magento_` moved first, then a pairwise
/// swap pass that is neither stable nor a topological sort in general. `Err` names
/// a `<sequence>` cycle, as Magento's LogicException does.
pub(crate) fn sort_by_sequence<'a>(
    modules: impl IntoIterator<Item = (&'a ModuleName, &'a [ModuleName])>,
) -> std::result::Result<Vec<ModuleName>, String> {
    let sequences: HashMap<&str, &[ModuleName]> =
        modules.into_iter().map(|(n, seq)| (n.as_str(), seq)).collect();
    let mut names: Vec<&str> = sequences.keys().copied().collect();
    names.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let (magento, others): (Vec<&str>, Vec<&str>) =
        names.into_iter().partition(|n| n.contains("Magento_"));

    let mut cache: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut expanded: Vec<(&str, std::collections::HashSet<&str>)> = Vec::new();
    for name in magento.into_iter().chain(others) {
        let seq = expand_sequence(&sequences, name, &mut cache, &mut Vec::new())?;
        expanded.push((name, seq.into_iter().collect()));
    }

    // The "bubble sort" from Loader, verbatim: `$j` starts at `$i`, and `$i` is
    // re-read after every swap.
    let total = expanded.len();
    for i in 0..total.saturating_sub(1) {
        for j in i..total {
            if expanded[i].1.contains(expanded[j].0) {
                expanded.swap(i, j);
            }
        }
    }
    Ok(expanded.into_iter().map(|(n, _)| ModuleName::new(n)).collect())
}

/// `Loader::expandSequence`: a module's direct and transitive `<sequence>`
/// entries. A name no module declares still counts (it just has no sequence of
/// its own).
fn expand_sequence<'a>(
    sequences: &HashMap<&'a str, &'a [ModuleName]>,
    name: &'a str,
    cache: &mut HashMap<&'a str, Vec<&'a str>>,
    stack: &mut Vec<&'a str>,
) -> std::result::Result<Vec<&'a str>, String> {
    if stack.contains(&name) {
        let parent = stack.last().copied().unwrap_or_default();
        return Err(format!("circular sequence reference from '{parent}' to '{name}'"));
    }
    if let Some(seq) = cache.get(name) {
        return Ok(seq.clone());
    }
    let direct: Vec<&'a str> = sequences
        .get(name)
        .map(|seq| seq.iter().map(ModuleName::as_str).collect())
        .unwrap_or_default();
    stack.push(name);
    let mut all: Vec<&'a str> = Vec::new();
    for dep in &direct {
        for s in expand_sequence(sequences, dep, cache, stack)? {
            if !all.contains(&s) {
                all.push(s);
            }
        }
    }
    stack.pop();
    for s in &direct {
        if !all.contains(s) {
            all.push(s);
        }
    }
    cache.insert(name, all.clone());
    Ok(all)
}

fn prof(label: &str, since: &std::time::Instant) {
    if std::env::var_os("MQ_PROFILE").is_some() {
        eprintln!("[prof] {label}: {:.2}ms", since.elapsed().as_secs_f64() * 1000.0);
    }
}

fn discover_app_code(
    root: &Path,
    out: &mut Discoveries,
    diags: &mut Vec<Diagnostic>,
    vfs: &Vfs,
) {
    // app/code is small and not composer-managed; a pruned recursive scan is cheap here.
    let base = root.join("app/code");
    if base.is_dir() {
        scan(&base, ModuleSource::App, 0, out, diags, vfs);
    }
}

fn discover_vendor(
    packages: &[composer::ComposerPackage],
    out: &mut Discoveries,
    diags: &mut Vec<Diagnostic>,
    vfs: &Vfs,
) {
    // Probe every package's candidate module roots in parallel (the cost is ~500 file
    // reads + parses). `read_module_root` is pure, so this is a clean map; rayon preserves
    // package order in the collected result, keeping the sequential merge deterministic.
    let probes: Vec<Probe> = packages
        .par_iter()
        .flat_map_iter(|pkg| {
            candidate_roots(pkg)
                .into_iter()
                .map(|r| read_module_root(&r, ModuleSource::Vendor, &vfs))
        })
        .collect();

    for probe in probes {
        match probe {
            Probe::None => {}
            Probe::Found(name, d) => merge(out, diags, name, d),
            Probe::Bad(diag) => diags.push(diag),
        }
    }

    // Superpackage fallback. A few vendor packages register a single root
    // `registration.php` that `require_once`s many modules bundled under an
    // `app/code/` tree (e.g. `magestore/synthesized-superpackage`, 88 modules).
    // None of those module roots sit at a composer candidate root, so probe the
    // `app/code` tree recursively — exactly as we do for the project's own
    // `app/code`. The `is_dir` gate keeps this free for normal packages (a
    // vendor package with an `app/code/` dir is the rare superpackage marker).
    for pkg in packages {
        let app_code = pkg.root.join("app/code");
        if app_code.is_dir() {
            scan(&app_code, ModuleSource::Vendor, 0, out, diags, vfs);
        }
    }
}

/// Candidate module-root directories for a composer package: the dir of each
/// registration.php in `autoload.files` (covers packages bundling several modules under
/// `src/`), plus the conventional `pkg/` and `pkg/src/` for packages that don't list one.
fn candidate_roots(pkg: &composer::ComposerPackage) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = pkg
        .autoload_files
        .iter()
        .map(|f| {
            let full = pkg.root.join(f);
            full.parent().map(Path::to_path_buf).unwrap_or_else(|| pkg.root.clone())
        })
        .collect();
    roots.push(pkg.root.clone());
    roots.push(pkg.root.join("src"));
    roots.sort();
    roots.dedup();
    roots
}

/// Outcome of probing one candidate directory. Pure (no shared state) so it can run in
/// parallel; the caller merges results sequentially.
enum Probe {
    None,
    Found(ModuleName, Discovered),
    Bad(Diagnostic),
}

/// Read+parse `dir/etc/module.xml` if present. Does no insertion — returns a [`Probe`].
fn read_module_root(dir: &Path, source: ModuleSource, vfs: &Vfs) -> Probe {
    let module_xml = dir.join("etc/module.xml");
    if !module_xml.is_file() {
        return Probe::None;
    }
    match vfs.read_to_string(&module_xml) {
        Ok(text) => match parse::module_xml(&text) {
            Ok(parsed) => Probe::Found(
                parsed.name,
                Discovered { source, path: dir.to_path_buf(), sequence: parsed.sequence },
            ),
            Err(e) => Probe::Bad(Diagnostic::warning(
                format!("parsing {}: {e}", module_xml.display()),
                None,
            )),
        },
        Err(e) => Probe::Bad(Diagnostic::warning(
            format!("reading {}: {e}", module_xml.display()),
            None,
        )),
    }
}

/// Insert a discovered module, keeping the first occurrence on a name clash and
/// recording the clash. Magento refuses the second registration of a module name
/// (`ComponentRegistrar::register` throws), so a clash means a store that won't boot.
/// The same directory found twice (a superpackage module that is also an
/// `autoload.files` root) is one module, not a clash.
fn merge(out: &mut Discoveries, diags: &mut Vec<Diagnostic>, name: ModuleName, d: Discovered) {
    match out.modules.get(&name) {
        Some(kept) if kept.path == d.path => {}
        Some(kept) => {
            diags.push(Diagnostic::warning(
                format!(
                    "module {name} is defined in both {} and {}; Magento refuses to register it twice, magequery keeps the first",
                    kept.path.display(),
                    d.path.display()
                ),
                None,
            ));
            out.duplicates.push(DuplicateModule {
                name,
                kept: kept.path.clone(),
                ignored: d.path,
            });
        }
        None => {
            out.modules.insert(name, d);
        }
    }
}

/// Sequential variant for the recursive `scan`: probe `dir` and record any module found.
/// Returns whether a `module.xml` was present (regardless of parse success).
fn try_module_root(
    dir: &Path,
    source: ModuleSource,
    out: &mut Discoveries,
    diags: &mut Vec<Diagnostic>,
    vfs: &Vfs,
) -> bool {
    match read_module_root(dir, source, vfs) {
        Probe::None => false,
        Probe::Found(name, d) => {
            merge(out, diags, name, d);
            true
        }
        Probe::Bad(diag) => {
            diags.push(diag);
            true
        }
    }
}

/// Recursive walk that treats any directory containing `etc/module.xml` as a module root
/// and stops descending there. Used for app/code and as the no-composer vendor fallback.
fn scan(
    dir: &Path,
    source: ModuleSource,
    depth: usize,
    out: &mut Discoveries,
    diags: &mut Vec<Diagnostic>,
    vfs: &Vfs,
) {
    if depth > 6 {
        return;
    }
    if try_module_root(dir, source, out, diags, vfs) {
        return; // a module root does not contain nested modules
    }

    let entries = match crate::fsutil::read_dir_entries(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries {
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(
            name.as_ref(),
            "node_modules" | ".git" | "Test" | "_files" | "var" | "generated" | "pub"
        ) {
            continue;
        }
        scan(&entry.path(), source, depth + 1, out, diags, vfs);
    }
}

#[cfg(test)]
mod sort_tests {
    use super::sort_by_sequence;
    use crate::ids::ModuleName;

    /// `sort_by_sequence` against goldens produced by Magento's own
    /// `ModuleList\Loader` (scripts/module-order-goldens.php runs its sorting
    /// methods verbatim): 200 random graphs aimed at the quirks, and the 359
    /// modules of the bougie testing demo's integration sandbox.
    #[test]
    fn order_matches_magentos_loader_goldens() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/module-order");
        let mut checked = 0;
        for file in ["random.json", "real.json"] {
            let text = std::fs::read_to_string(dir.join(file)).unwrap();
            let cases: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
            for case in &cases {
                let name = case["name"].as_str().unwrap();
                let modules: Vec<(ModuleName, Vec<ModuleName>)> = case["modules"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(n, seq)| {
                        let seq = seq.as_array().unwrap().iter();
                        (ModuleName::new(n), seq.map(|s| ModuleName::new(s.as_str().unwrap())).collect())
                    })
                    .collect();
                let got = sort_by_sequence(modules.iter().map(|(n, s)| (n, s.as_slice())));
                match case["expected"].as_array() {
                    Some(expected) => {
                        let expected: Vec<&str> = expected.iter().map(|v| v.as_str().unwrap()).collect();
                        let got = got.unwrap_or_else(|e| panic!("{file} {name}: {e}"));
                        let got: Vec<&str> = got.iter().map(ModuleName::as_str).collect();
                        assert_eq!(got, expected, "{file} {name}");
                    }
                    None => assert!(got.is_err(), "{file} {name}: Magento rejects a cycle"),
                }
                checked += 1;
            }
        }
        assert_eq!(checked, 201);
    }
}
