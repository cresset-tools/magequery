//! The parsed installation. Step 1 builds the module list; later steps add the per-area
//! DI config and class resolver to this struct.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::composer;
use crate::error::{Diagnostic, Error, Result};
use crate::ids::ModuleName;
use crate::model::{Module, ModuleCheck, ModuleSource, UnregisteredModule};
use crate::parse;
use crate::resolver;
use crate::vfs::Vfs;

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
}

/// The slice of a composer package `deps`/`info` need: who it is, where it lives (to map
/// modules to their owning package), its version, and what it requires.
pub(crate) struct PackageMeta {
    pub name: String,
    pub version: Option<String>,
    pub root: PathBuf,
    pub require: Vec<String>,
}

struct Discovered {
    source: ModuleSource,
    path: PathBuf,
    sequence: Vec<ModuleName>,
}

impl Index {
    pub fn build(root: &Path, vfs: Arc<Vfs>) -> Result<Index> {
        let config_php = root.join("app/etc/config.php");
        if !config_php.is_file() {
            return Err(Error::NotMagentoRoot {
                path: root.to_path_buf(),
            });
        }

        let mut diagnostics = Vec::new();
        let _p = std::time::Instant::now();

        let text = vfs.read_to_string(&config_php).map_err(|source| Error::Io {
            file: config_php.clone(),
            source,
        })?;
        let ordered = parse::config_php_modules(&text).map_err(|detail| Error::Parse {
            file: config_php.clone(),
            detail,
        })?;
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
        let mut discovered: HashMap<ModuleName, Discovered> = HashMap::new();
        let _p = std::time::Instant::now();
        discover_app_code(root, &mut discovered, &mut diagnostics, &vfs);
        prof("app/code discovery", &_p);
        let _p = std::time::Instant::now();
        if !packages.is_empty() {
            discover_vendor(&packages, &mut discovered, &mut diagnostics, &vfs);
        } else if vendor.is_dir() {
            scan(&vendor, ModuleSource::Vendor, 0, &mut discovered, &mut diagnostics, &vfs);
        }
        prof("vendor discovery", &_p);

        // Build the module list in config.php load order, and record any config entry that
        // has no matching module on disk.
        let mut modules = Vec::with_capacity(ordered.len());
        let mut in_config_not_on_disk = Vec::new();
        for (load_order, (name, enabled)) in ordered.into_iter().enumerate() {
            match discovered.remove(&name) {
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
        let mut on_disk_not_in_config: Vec<UnregisteredModule> = discovered
            .into_iter()
            .map(|(name, d)| UnregisteredModule { name, source: d.source, path: d.path })
            .collect();
        on_disk_not_in_config.sort_by(|a, b| a.name.cmp(&b.name));

        // The di.xml index is built lazily (see `Magento`) — it's the expensive part and
        // commands like `modules`/`events` don't need it. The resolver is cheap (PSR-4 maps
        // only; PHP parsing is lazy), so it stays eager.
        let resolver = resolver::ClassResolver::build(&packages, &modules, root, Arc::clone(&vfs));

        // Keep the slim package facts (already parsed) for the lazy `deps` graph.
        let packages = packages
            .into_iter()
            .filter_map(|p| {
                p.name.map(|name| PackageMeta {
                    name,
                    version: p.version,
                    root: p.root,
                    require: p.require,
                })
            })
            .collect();

        Ok(Index {
            root: root.to_path_buf(),
            vfs,
            modules,
            check: ModuleCheck { on_disk_not_in_config, in_config_not_on_disk },
            resolver,
            diagnostics,
            packages,
        })
    }
}

fn prof(label: &str, since: &std::time::Instant) {
    if std::env::var_os("MQ_PROFILE").is_some() {
        eprintln!("[prof] {label}: {:.2}ms", since.elapsed().as_secs_f64() * 1000.0);
    }
}

fn discover_app_code(
    root: &Path,
    out: &mut HashMap<ModuleName, Discovered>,
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
    out: &mut HashMap<ModuleName, Discovered>,
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

/// Insert a discovered module, keeping the first occurrence on a name clash.
fn merge(
    out: &mut HashMap<ModuleName, Discovered>,
    diags: &mut Vec<Diagnostic>,
    name: ModuleName,
    d: Discovered,
) {
    if out.contains_key(&name) {
        diags.push(Diagnostic::warning(
            format!("module {name} declared in multiple locations; keeping the first"),
            None,
        ));
    } else {
        out.insert(name, d);
    }
}

/// Sequential variant for the recursive `scan`: probe `dir` and record any module found.
/// Returns whether a `module.xml` was present (regardless of parse success).
fn try_module_root(
    dir: &Path,
    source: ModuleSource,
    out: &mut HashMap<ModuleName, Discovered>,
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
    out: &mut HashMap<ModuleName, Discovered>,
    diags: &mut Vec<Diagnostic>,
    vfs: &Vfs,
) {
    if depth > 6 {
        return;
    }
    if try_module_root(dir, source, out, diags, vfs) {
        return; // a module root does not contain nested modules
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
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
