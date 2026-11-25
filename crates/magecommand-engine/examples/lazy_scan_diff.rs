//! Differential harness for the Mage-OS 4.0 lazy-ghost rules
//! ([`magecommand_engine::sideeffect`]): prints `class\t0|1` for every class
//! the scan knows, to diff against Mage-OS's own `Chain\LazyTypes` run under
//! PHP 8.4 over the same root.
//!
//! ```sh
//! cargo run --release -p magecommand-engine --example lazy_scan_diff -- <root> [untrusted]
//! ```
//!
//! `untrusted` trusts no namespace, so every constructor is scanned (pair it
//! with a PHP run whose `TRUSTED_NAMESPACES` is emptied).

use std::path::PathBuf;

use magecommand_engine::areaconfig::{is_lazy_eligible, AREA_CODES};
use magecommand_engine::definitions::Definitions;
use magecommand_engine::sideeffect::Scanner;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("usage: lazy_scan_diff <root> [untrusted]"));
    let untrusted = args.next().as_deref() == Some("untrusted");
    let magento = magequery_core::Magento::open(&root).expect("open magento root");
    let mut defs = Definitions::scan(&magento, &root, &root.join("generated/code"));
    let pref_keys: Vec<String> = AREA_CODES
        .iter()
        .flat_map(|(area, _)| {
            magento.di_export(*area).preferences.into_iter().map(|p| p.for_type.as_str().to_owned())
        })
        .collect();
    defs.extend_hierarchy(&magento, &root, pref_keys);

    let mut scanner = Scanner::new(&defs);
    if untrusted {
        scanner = scanner.trusting(&[]);
    }
    let mut names: Vec<&str> = defs
        .classes
        .values()
        .filter(|r| r.meta.kind == magecommand_php::ClassKind::Class && !r.meta.conditional)
        .map(|r| r.meta.fqcn.as_str())
        .collect();
    names.sort_unstable();
    for name in names {
        let lazy = is_lazy_eligible(&defs, name) && scanner.is_lazy_eligible(name);
        println!("{name}\t{}", u8::from(lazy));
    }
}
