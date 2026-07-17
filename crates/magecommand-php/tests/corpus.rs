//! Corpus smoke test: parse every `.php` file under a real Magento checkout.
//! Zero panics required; issues are counted and the worst offenders printed.
//!
//! Ignored by default (machine-specific). Run with:
//! ```sh
//! MAGECOMMAND_CORPUS=/home/jelle/mg-install-310 \
//!   cargo test -p magecommand-php --test corpus -- --ignored --nocapture
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn collect_php(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Skip VCS internals and the generated trees (they're output,
            // not input — though parsing them should work too).
            if name == ".git" || name == "node_modules" {
                continue;
            }
            collect_php(&path, out);
        } else if path.extension().is_some_and(|e| e == "php") {
            out.push(path);
        }
    }
}

#[test]
#[ignore = "needs a real Magento checkout; set MAGECOMMAND_CORPUS"]
fn parse_entire_corpus() {
    let root = std::env::var("MAGECOMMAND_CORPUS")
        .unwrap_or_else(|_| "/home/jelle/mg-install-310".to_owned());
    let root = PathBuf::from(root);
    assert!(root.is_dir(), "corpus root {} not found", root.display());

    let mut files = Vec::new();
    for sub in ["vendor", "app", "lib", "generated"] {
        collect_php(&root.join(sub), &mut files);
    }
    assert!(!files.is_empty(), "no PHP files found under {}", root.display());

    let started = Instant::now();
    let mut bytes = 0usize;
    let mut declarations = 0usize;
    let mut files_with_issues: Vec<(PathBuf, usize, String)> = Vec::new();
    let mut total_issues = 0usize;

    for path in &files {
        let Ok(src) = fs::read(path) else { continue };
        bytes += src.len();
        let meta = magecommand_php::parse_file(&src);
        declarations += meta.declarations.len();
        if !meta.issues.is_empty() {
            total_issues += meta.issues.len();
            let first = &meta.issues[0];
            files_with_issues.push((
                path.clone(),
                meta.issues.len(),
                format!("@{}: {}", first.offset, first.message),
            ));
        }
    }
    let elapsed = started.elapsed();

    println!(
        "corpus: {} files · {:.1} MiB · {} declarations · {:.0} MiB/s · {:?}",
        files.len(),
        bytes as f64 / (1024.0 * 1024.0),
        declarations,
        bytes as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64(),
        elapsed
    );
    println!(
        "issues: {} across {} files",
        total_issues,
        files_with_issues.len()
    );
    files_with_issues.sort_by_key(|(_, n, _)| std::cmp::Reverse(*n));
    for (path, n, first) in files_with_issues.iter().take(25) {
        println!("  {n:>3} {} — {first}", path.display());
    }

    // The M1 acceptance bar is zero; while building toward it, the test
    // fails when parsing goes badly wrong rather than on every stray issue.
    let pct = files_with_issues.len() as f64 / files.len() as f64 * 100.0;
    assert!(
        pct < 1.0,
        "{:.2}% of files have parse issues — parser has a systemic gap",
        pct
    );
}
