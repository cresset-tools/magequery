//! Diff the computed generated-code SET against an archived compile
//! (`generated/_code`). Debugging harness for the M3 diff war.
//!
//!   cargo run -p magecommand-engine --example codegen_set --release -- /path/to/store

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use magecommand_engine::codegen::{Codegen, GenKind};

fn main() {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/jelle/mg-install-310"));
    let magento = magequery_core::Magento::open(&root).expect("open magento");
    let archive = root.join("generated/_code");

    let mut defs = magecommand_engine::definitions::Definitions::scan(&magento, &root, &archive);
    let pref_keys: Vec<String> = magecommand_engine::areaconfig::AREA_CODES
        .iter()
        .flat_map(|(area, _)| {
            magento
                .di_export(*area)
                .preferences
                .into_iter()
                .map(|p| p.for_type.as_str().to_owned())
        })
        .collect();
    defs.extend_hierarchy(&magento, &root, pref_keys);

    let mut cg = Codegen::new(&magento, &defs, root.clone());
    let t = std::time::Instant::now();
    cg.collect();
    eprintln!("collect: {:?}", t.elapsed());

    // Incidental generation: the Area operation's NonLazyTypes modifier
    // class_exists()es every candidate — arguments keys (which include
    // virtual-type names), instanceTypes values, preference values. Names
    // ending `\Proxy` short-circuit before the check; `\Interceptor` names
    // are op-5 artifacts accounted for separately.
    let t = std::time::Instant::now();
    for (area, _) in magecommand_engine::areaconfig::AREA_CODES {
        let file = magecommand_engine::areaconfig::build_area_file(&magento, &defs, area, &root);
        let names: Vec<String> = file
            .arguments
            .keys()
            .cloned()
            .chain(file.instance_types.iter().map(|(_, v)| v.clone()))
            .chain(file.preferences.iter().map(|(_, v)| v.clone()))
            .filter(|n| !n.ends_with("\\Proxy") && !n.ends_with("\\Interceptor"))
            .collect();
        cg.ensure_all(names.iter().map(String::as_str));
    }
    eprintln!("incidental (area files): {:?}", t.elapsed());

    // Byte-verify every emitted factory against the archive file.
    let mut byte_ok = 0usize;
    let mut byte_bad: Vec<&String> = Vec::new();
    for (name, kind) in &cg.emitted {
        if !matches!(kind, GenKind::Factory | GenKind::ExtensionInterfaceFactory) {
            continue;
        }
        let path = archive.join(name.replace('\\', "/")).with_extension("php");
        let Ok(expected_bytes) = std::fs::read_to_string(&path) else {
            byte_bad.push(name);
            continue;
        };
        if magecommand_engine::codegen::factory_bytes(name, *kind) == expected_bytes {
            byte_ok += 1;
        } else {
            byte_bad.push(name);
        }
    }
    println!("factory bytes: {byte_ok} identical, {} mismatched", byte_bad.len());
    for b in byte_bad.iter().take(10) {
        println!("  byte-mismatch: {b}");
    }

    // Byte-verify Extension / ExtensionInterface files.
    let ext_cfg = magecommand_engine::codegen::ExtConfig::build(&magento);
    let mut ext_ok = 0usize;
    let mut ext_bad: Vec<&String> = Vec::new();
    for (name, kind) in &cg.emitted {
        if !matches!(kind, GenKind::Extension | GenKind::ExtensionInterface) {
            continue;
        }
        let path = archive.join(name.replace('\\', "/")).with_extension("php");
        let Ok(expected_bytes) = std::fs::read_to_string(&path) else {
            ext_bad.push(name);
            continue;
        };
        if magecommand_engine::codegen::extension_bytes(name, *kind, &ext_cfg) == expected_bytes {
            ext_ok += 1;
        } else {
            ext_bad.push(name);
        }
    }
    println!("extension bytes: {ext_ok} identical, {} mismatched", ext_bad.len());
    for b in ext_bad.iter().take(10) {
        println!("  ext-mismatch: {b}");
    }

    // Byte-verify Proxy files.
    let mut px_ok = 0usize;
    let mut px_bad: Vec<&String> = Vec::new();
    for (name, kind) in &cg.emitted {
        if *kind != GenKind::Proxy {
            continue;
        }
        let source = name.trim_end_matches("\\Proxy");
        let path = archive.join(name.replace('\\', "/")).with_extension("php");
        let Ok(expected_bytes) = std::fs::read_to_string(&path) else {
            px_bad.push(name);
            continue;
        };
        match magecommand_engine::proxy::proxy_bytes(&defs, source) {
            Some(got) if got == expected_bytes => px_ok += 1,
            _ => px_bad.push(name),
        }
    }
    println!("proxy bytes: {px_ok} identical, {} mismatched", px_bad.len());
    for b in px_bad.iter().take(6) {
        println!("  proxy-mismatch: {b}");
    }

    // Byte-verify SearchResults + ProxyDeferred.
    for (name, kind) in &cg.emitted {
        let got = match kind {
            GenKind::SearchResults => Some(magecommand_engine::codegen::search_results_bytes(name)),
            GenKind::ProxyDeferred => magecommand_engine::proxy::proxy_deferred_bytes(
                &defs,
                name.trim_end_matches("\\ProxyDeferred"),
            ),
            _ => continue,
        };
        let path = archive.join(name.replace('\\', "/")).with_extension("php");
        let exp = std::fs::read_to_string(&path).unwrap_or_default();
        println!(
            "{:?} {name}: {}",
            kind,
            if got.as_deref() == Some(exp.as_str()) { "IDENTICAL" } else { "MISMATCH" }
        );
    }

    // What the archive holds, as fqcn -> kind bucket (by suffix).
    let mut expected: BTreeMap<String, &'static str> = BTreeMap::new();
    walk(&archive, &archive, &mut expected);

    // Interceptors: SET + bytes.
    let t = std::time::Instant::now();
    let has_plugins = magecommand_engine::interception::interception_map(&magento, &defs);
    let plan = magecommand_engine::interceptor::plan(&magento, &defs, &has_plugins);
    eprintln!("interceptor plan: {:?} ({} classes)", t.elapsed(), plan.methods.len());
    let arch_intc: std::collections::HashSet<String> = expected
        .iter()
        .filter(|(_, c)| **c == "interceptor")
        .map(|(n, _)| n.trim_end_matches("\\Interceptor").to_owned())
        .collect();
    let mine: std::collections::HashSet<String> = plan.methods.keys().cloned().collect();
    let missing: Vec<&String> = arch_intc.difference(&mine).collect();
    let extra: Vec<&String> = mine.difference(&arch_intc).collect();
    println!(
        "interceptor SET: archive {}  computed {}  missing {}  extra {}",
        arch_intc.len(),
        mine.len(),
        missing.len(),
        extra.len()
    );
    for m in missing.iter().take(15) {
        println!("  SET-missing: {m}");
    }
    for e in extra.iter().take(15) {
        println!("  SET-extra: {e}");
    }
    let mut ic_ok = 0usize;
    let mut ic_bad: Vec<String> = Vec::new();
    for (class, mset) in &plan.methods {
        if !arch_intc.contains(class) {
            continue;
        }
        let path = archive
            .join(class.replace('\\', "/"))
            .join("Interceptor.php");
        let Ok(exp) = std::fs::read_to_string(&path) else { continue };
        match magecommand_engine::interceptor::interceptor_bytes(&defs, class, mset) {
            Some(got) if got == exp => ic_ok += 1,
            _ => ic_bad.push(class.clone()),
        }
    }
    println!("interceptor bytes: {ic_ok} identical, {} mismatched", ic_bad.len());
    ic_bad.sort();
    for b in ic_bad.iter().take(15) {
        println!("  ic-mismatch: {b}");
    }

    let bucket = |k: GenKind| -> &'static str {
        match k {
            GenKind::Factory | GenKind::ExtensionInterfaceFactory => "factory",
            GenKind::Proxy => "proxy",
            GenKind::Interceptor => "interceptor",
            GenKind::Extension => "extension",
            GenKind::ExtensionInterface => "extensionInterface",
            GenKind::SearchResults => "searchResults",
            GenKind::ProxyDeferred => "proxyDeferred",
            _ => "other",
        }
    };
    let got: BTreeMap<String, &'static str> = cg
        .emitted
        .iter()
        .map(|(name, kind)| (name.clone(), bucket(*kind)))
        .collect();

    for cat in [
        "factory",
        "extension",
        "extensionInterface",
        "proxy",
        "searchResults",
        "proxyDeferred",
        "interceptor",
        "other",
    ] {
        let exp: Vec<&String> = expected
            .iter()
            .filter(|(_, c)| **c == cat)
            .map(|(n, _)| n)
            .collect();
        let act: Vec<&String> = got.iter().filter(|(_, c)| **c == cat).map(|(n, _)| n).collect();
        let missing: Vec<&&String> = exp.iter().filter(|n| !got.contains_key(**n)).collect();
        let extra: Vec<&&String> = act.iter().filter(|n| !expected.contains_key(**n)).collect();
        println!(
            "{cat:>18}: archive {:4}  computed {:4}  missing {:3}  extra {:3}",
            exp.len(),
            act.len(),
            missing.len(),
            extra.len()
        );
        for m in missing.iter().take(40) {
            println!("{:>18}  - {m}", "");
        }
        for e in extra.iter().take(40) {
            println!("{:>18}  + {e}", "");
        }
    }
}

/// Bucket an archive file path by its generated-artifact suffix.
fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, &'static str>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(base, &path, out);
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(stem) = name.strip_suffix(".php") else { continue };
        let fqcn = path
            .strip_prefix(base)
            .unwrap()
            .with_extension("")
            .to_string_lossy()
            .replace('/', "\\");
        let cat = if stem == "Interceptor" {
            "interceptor"
        } else if stem == "Proxy" {
            "proxy"
        } else if stem == "ProxyDeferred" {
            "proxyDeferred"
        } else if stem.ends_with("Factory") {
            "factory"
        } else if stem.ends_with("ExtensionInterface") {
            "extensionInterface"
        } else if stem.ends_with("Extension") {
            "extension"
        } else if stem.ends_with("SearchResults") {
            "searchResults"
        } else {
            "other"
        };
        out.insert(fqcn, cat);
    }
}
