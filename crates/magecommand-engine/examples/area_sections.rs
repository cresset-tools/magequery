//! Bring-up harness: compare per-area `arguments`/`preferences`/
//! `instanceTypes` sections against the archived area files, before whole
//! files can be byte-compared (`nonLazyTypes` still pending).
//!
//! ```sh
//! cargo run --release -p magecommand-engine --example area_sections [-- <root> [area]]
//! ```

use std::fs;
use std::path::PathBuf;

use magecommand_engine::areaconfig::{build_area_file, render_section, AREA_CODES};
use magecommand_engine::definitions::Definitions;
use magecommand_engine::phpexport::{PhpKey, PhpValue};

fn main() {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/jelle/mg-install-310"));
    let only_area = std::env::args().nth(2);
    let magento = magequery_core::Magento::open(&root).expect("open magento root");

    eprintln!("scanning definitions …");
    let mut defs = Definitions::scan(&magento, &root, &root.join("generated/_code"));
    eprintln!("{} scanned classes", defs.scanned.len());
    // Preference keys of ALL areas may need reflection-grade knowledge too.
    let pref_keys: Vec<String> = AREA_CODES
        .iter()
        .flat_map(|(area, _)| {
            magento
                .di_export(*area)
                .preferences
                .into_iter()
                .map(|p| p.for_type.as_str().to_owned())
        })
        .collect();
    let unresolved = defs.extend_hierarchy(&magento, &root, pref_keys);
    eprintln!(
        "{} classes after hierarchy extension ({} unresolved: {:?}…)",
        defs.classes.len(),
        unresolved.len(),
        unresolved.iter().take(5).collect::<Vec<_>>()
    );

    let mut failures = 0usize;
    for (area, code) in AREA_CODES {
        if only_area.as_deref().is_some_and(|a| a != code) {
            continue;
        }
        let archive_path = root.join(format!("generated/_metadata/{code}.php"));
        let archive = fs::read_to_string(&archive_path).expect("read archive area file");
        let file = build_area_file(&magento, &defs, area, &root, false);
        if !file.findings.is_empty() {
            eprintln!("{code}: {} findings, first 5:", file.findings.len());
            for f in file.findings.iter().take(5) {
                eprintln!("  ! {f}");
            }
        }

        let args_value = PhpValue::Array(
            file.arguments
                .iter()
                .map(|(k, v)| (PhpKey::str(k.clone()), v.clone()))
                .collect(),
        );
        let args_rendered = {
            let wrapper = PhpValue::Array(vec![(PhpKey::str("arguments"), args_value)]);
            let full = magecommand_engine::phpexport::to_php_file(&wrapper);
            let body = full
                .strip_prefix("<?php return array (\n")
                .and_then(|s| s.strip_suffix("\n);"))
                .expect("wrapper shape");
            format!("{body}\n")
        };

        let non_lazy_rendered = {
            let section = PhpValue::Array(
                file.non_lazy
                    .iter()
                    .map(|k| (PhpKey::str(k.clone()), PhpValue::Bool(true)))
                    .collect(),
            );
            let wrapper = PhpValue::Array(vec![(PhpKey::str("nonLazyTypes"), section)]);
            let full = magecommand_engine::phpexport::to_php_file(&wrapper);
            let body = full
                .strip_prefix("<?php return array (\n")
                .and_then(|s| s.strip_suffix("\n);"))
                .expect("wrapper shape");
            format!("{body}\n")
        };
        for (key, ours) in [
            ("arguments", args_rendered),
            ("preferences", render_section("preferences", &file.preferences)),
            (
                "instanceTypes",
                render_section("instanceTypes", &file.instance_types),
            ),
            ("nonLazyTypes", non_lazy_rendered),
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
                        print_diff_sample(&theirs, &ours);
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

fn print_diff_sample(theirs: &str, ours: &str) {
    let t: Vec<&str> = theirs.lines().collect();
    let o: Vec<&str> = ours.lines().collect();
    let mut shown = 0usize;
    let mut i = 0usize; // theirs
    let mut j = 0usize; // ours
    while shown < 12 && (i < t.len() || j < o.len()) {
        match (t.get(i), o.get(j)) {
            (Some(a), Some(b)) if a == b => {
                i += 1;
                j += 1;
            }
            (a, b) => {
                println!("  diff at theirs:{} ours:{}", i + 1, j + 1);
                println!("    theirs: {}", a.unwrap_or(&"<eof>"));
                println!("    ours:   {}", b.unwrap_or(&"<eof>"));
                shown += 1;
                // naive resync: skip one line on the longer-looking side
                let (ta, ob) = (a.unwrap_or(&""), b.unwrap_or(&""));
                if ta < ob {
                    i += 1;
                } else if ob < ta {
                    j += 1;
                } else {
                    i += 1;
                    j += 1;
                }
            }
        }
    }
}
