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

    // What the archive holds, as fqcn -> kind bucket (by suffix).
    let mut expected: BTreeMap<String, &'static str> = BTreeMap::new();
    walk(&archive, &archive, &mut expected);

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
