//! Compare creatuity-format (compiled) interceptors rendered by
//! `compiled_interceptor` against a `generated/code` tree PHP filled on demand:
//! `compiled_interceptor_compare <store root> <reference generated/code>`.
//!
//! Subjects are taken in the reference files' mtime order — the order PHP
//! generated them in, which is the order its shared `AreasPluginList` was
//! asked about them. `VERBOSE=1` lists every non-identical class;
//! `DUMP=<dir>` writes `.php`/`.rust` pairs for each difference.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use magecommand_engine::compiled_interceptor::{compiled_interceptors, AreasPluginList};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.file_name().is_some_and(|x| x == "Interceptor.php") {
            out.push(p);
        }
    }
}

fn declared_class(src: &str) -> Option<String> {
    let ns = src.lines().find_map(|l| l.strip_prefix("namespace ")?.strip_suffix(';').map(str::to_owned))?;
    let name = src.lines().find_map(|l| {
        let rest = l.strip_prefix("class ")?;
        rest.split_whitespace().next().map(str::to_owned)
    })?;
    Some(format!("{ns}\\{name}"))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("store root"));
    let reference = PathBuf::from(args.next().expect("reference generated/code"));
    let verbose = std::env::var_os("VERBOSE").is_some();
    let dump = std::env::var_os("DUMP").map(PathBuf::from);

    let mut files = Vec::new();
    walk(&reference, &mut files);
    let mut timed: Vec<(std::time::SystemTime, String, String)> = files
        .iter()
        .filter_map(|f| {
            let src = std::fs::read_to_string(f).ok()?;
            let class = declared_class(&src)?;
            let mtime = std::fs::metadata(f).ok()?.modified().ok()?;
            Some((mtime, class, src))
        })
        .collect();
    timed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let want: BTreeMap<String, String> =
        timed.iter().map(|(_, c, s)| (c.trim_end_matches("\\Interceptor").to_owned(), s.clone())).collect();
    let mut subjects: Vec<String> = timed.iter().map(|(_, c, _)| c.trim_end_matches("\\Interceptor").to_owned()).collect();
    // ORDER=name|reverse: probe how much the walk's order-dependence matters.
    match std::env::var("ORDER").as_deref() {
        Ok("name") => subjects.sort(),
        Ok("reverse") => subjects.reverse(),
        _ => {}
    }

    let t = std::time::Instant::now();
    let mage = magequery_core::Magento::open(&root).unwrap();
    let mut defs = magecommand_engine::definitions::Definitions::empty();
    defs.load_closure(&mage, &root, subjects.iter().cloned());
    // A subject may itself be generated (`…InterfaceFactory\Interceptor`):
    // PHP's autoloader produced it first; render it the same way.
    let generated: Vec<&str> =
        subjects.iter().map(String::as_str).filter(|s| defs.get(s).is_none()).collect();
    magecommand_engine::codegen::generate_on_demand_closed(
        &mage,
        &mut defs,
        &root,
        &Default::default(),
        generated.iter().copied(),
    );
    loop {
        let need = AreasPluginList::required_classes(&mage, &defs, &subjects);
        if defs.load_closure(&mage, &root, need) == 0 {
            break;
        }
    }
    defs.load_constant_classes(&mage, &root);
    eprintln!("definitions loaded in {:?}", t.elapsed());

    let t = std::time::Instant::now();
    let plugins = AreasPluginList::new(&mage, &defs, &subjects);
    eprintln!("scopes {:?} computed in {:?}", plugins.scope_codes(), t.elapsed());
    let t = std::time::Instant::now();
    let rendered = compiled_interceptors(&defs, &plugins, &subjects);
    eprintln!("rendered {} interceptors in {:?}", rendered.len(), t.elapsed());

    let mut tally: BTreeMap<&str, usize> = BTreeMap::new();
    for (subject, result) in &rendered {
        let expected = &want[subject];
        let verdict = match result {
            Ok(bytes) if bytes == expected => "identical",
            Ok(_) => "differs",
            Err(_) => "not rendered",
        };
        if verdict != "identical" && verbose {
            match result {
                Err(e) => println!("{verdict}: {subject}: {e}"),
                Ok(_) => println!("{verdict}: {subject}"),
            }
        }
        if let (Some(dir), Ok(bytes), "differs") = (&dump, result, verdict) {
            let base = dir.join(subject.replace('\\', "_"));
            std::fs::write(base.with_extension("rust"), bytes).unwrap();
            std::fs::write(base.with_extension("php"), expected).unwrap();
        }
        *tally.entry(verdict).or_default() += 1;
    }
    for (verdict, n) in tally {
        println!("Interceptor          {verdict:<14} {n}");
    }
}
