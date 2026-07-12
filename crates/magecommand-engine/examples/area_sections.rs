//! Bring-up harness: compare our per-area `preferences`/`instanceTypes`
//! sections against the archived area files, section by section, before the
//! whole files can be byte-compared (arguments/nonLazyTypes still pending).
//!
//! ```sh
//! cargo run --release -p magecommand-engine --example area_sections [-- <root>]
//! ```

use std::fs;
use std::path::PathBuf;

use magecommand_engine::areaconfig::{area_sections, render_section, AREA_CODES};
use magecommand_engine::definitions::Definitions;

fn main() {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/jelle/mg-install-310"));
    let magento = magequery_core::Magento::open(&root).expect("open magento root");

    eprintln!("scanning definitions …");
    let defs = Definitions::scan(&magento, &root, &root.join("generated/_code"));
    eprintln!("{} classes in the universe", defs.classes.len());

    let mut failures = 0usize;
    for (area, code) in AREA_CODES {
        let archive_path = root.join(format!("generated/_metadata/{code}.php"));
        let archive = fs::read_to_string(&archive_path).expect("read archive area file");
        let sections = area_sections(&magento, &defs, area);

        for (key, ours) in [
            ("preferences", render_section("preferences", &sections.preferences)),
            (
                "instanceTypes",
                render_section("instanceTypes", &sections.instance_types),
            ),
        ] {
            match extract_section(&archive, key) {
                None => {
                    println!("{code}.{key}: section not found in archive");
                    failures += 1;
                }
                Some(theirs) => {
                    if theirs == ours {
                        println!("{code}.{key}: identical ({} lines)", ours.lines().count());
                    } else {
                        failures += 1;
                        println!(
                            "{code}.{key}: DIFFERS (ours {} lines, theirs {} lines)",
                            ours.lines().count(),
                            theirs.lines().count()
                        );
                        print_first_diff(&theirs, &ours);
                    }
                }
            }
        }
    }
    if failures > 0 {
        std::process::exit(1);
    }
}

/// Slice `  'key' => \n  array (\n … \n  ),\n` out of an area file.
fn extract_section(archive: &str, key: &str) -> Option<String> {
    let marker = format!("  '{key}' => ");
    let start = archive
        .lines()
        .scan(0usize, |pos, line| {
            let this = *pos;
            *pos += line.len() + 1;
            Some((this, line))
        })
        .find(|(_, line)| *line == marker)?
        .0;
    // The section ends before the next top-level key line or the final `);`.
    let rest = &archive[start..];
    let mut end = rest.len();
    let mut pos = 0usize;
    let mut first = true;
    for line in rest.lines() {
        if !first && (line == ");" || (line.starts_with("  '") && line.ends_with(" => "))) {
            end = pos;
            break;
        }
        first = false;
        pos += line.len() + 1;
    }
    Some(rest[..end].to_owned())
}

fn print_first_diff(theirs: &str, ours: &str) {
    let mut t = theirs.lines();
    let mut o = ours.lines();
    let mut line_no = 0usize;
    loop {
        line_no += 1;
        match (t.next(), o.next()) {
            (Some(a), Some(b)) if a == b => continue,
            (a, b) => {
                println!("  first diff at section line {line_no}:");
                println!("    theirs: {}", a.unwrap_or("<eof>"));
                println!("    ours:   {}", b.unwrap_or("<eof>"));
                return;
            }
        }
    }
}
