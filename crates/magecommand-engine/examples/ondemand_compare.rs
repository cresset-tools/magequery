//! Compare what the Rust generators render against a `generated/code` tree PHP
//! filled on demand: `ondemand_compare <store root> <reference generated/code>`.
//! Every reference file is named by its own `namespace` + class declaration,
//! rendered from the store's sources, and compared byte for byte.
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "php") {
            out.push(p);
        }
    }
}

fn declared_class(src: &str) -> Option<String> {
    let ns = src.lines().find_map(|l| l.strip_prefix("namespace ")?.strip_suffix(';').map(str::to_owned))?;
    let name = src.lines().find_map(|l| {
        let l = l.trim_start_matches("abstract ").trim_start_matches("final ");
        let rest = l.strip_prefix("class ").or_else(|| l.strip_prefix("interface "))?;
        rest.split_whitespace().next().map(str::to_owned)
    })?;
    Some(format!("{ns}\\{name}"))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("store root"));
    let reference = PathBuf::from(args.next().expect("reference generated/code"));
    let verbose = std::env::var_os("VERBOSE").is_some();

    let mut files = Vec::new();
    walk(&reference, &mut files);
    let mut want: BTreeMap<String, String> = BTreeMap::new();
    for f in &files {
        let src = std::fs::read_to_string(f).unwrap();
        match declared_class(&src) {
            Some(c) => {
                want.insert(c, src);
            }
            None => eprintln!("no class in {}", f.display()),
        }
    }

    let t = std::time::Instant::now();
    let mage = magequery_core::Magento::open(&root).unwrap();
    let mut sources = Vec::new();
    for name in want.keys() {
        let mut current = name.clone();
        while let Some((_, source)) = magecommand_engine::codegen::classify(&current) {
            sources.push(source.clone());
            current = source;
        }
    }
    let mut defs = magecommand_engine::definitions::Definitions::empty();
    defs.load_closure(&mage, &root, sources);
    defs.load_constant_classes(&mage, &root);
    let code = magecommand_engine::codegen::generate_on_demand_closed(
        &mage,
        &mut defs,
        &root,
        &HashSet::new(),
        want.keys().map(String::as_str),
    );
    eprintln!("rendered {} files in {:?} ({} findings)", code.files.len(), t.elapsed(), code.findings.len());

    let rendered: BTreeMap<String, &String> = code
        .files
        .iter()
        .filter_map(|(_, bytes)| Some((declared_class(bytes)?, bytes)))
        .collect();
    let mut tally: BTreeMap<(String, &str), usize> = BTreeMap::new();
    for (class, bytes) in &want {
        let kind = class.rsplit('\\').next().unwrap();
        let kind = ["Interceptor", "Proxy", "Factory", "ExtensionInterface", "Extension", "SearchResults"]
            .into_iter()
            .find(|k| kind.ends_with(k))
            .unwrap_or("other")
            .to_owned();
        let verdict = match rendered.get(class) {
            None => "not rendered",
            Some(r) if r.as_str() == bytes.as_str() => "identical",
            Some(_) => "differs",
        };
        if verbose && verdict != "identical" {
            println!("{verdict}: {class}");
            if let (Some(dir), Some(r)) = (std::env::var_os("DUMP"), rendered.get(class)) {
                let base = PathBuf::from(dir).join(class.replace('\\', "_"));
                std::fs::write(base.with_extension("rust"), r).unwrap();
                std::fs::write(base.with_extension("php"), bytes).unwrap();
            }
        }
        *tally.entry((kind, verdict)).or_default() += 1;
    }
    for ((kind, verdict), n) in tally {
        println!("{kind:<20} {verdict:<14} {n}");
    }
    for f in code.findings.iter().take(10) {
        println!("finding: {f}");
    }
}
