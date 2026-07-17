//! `magecommand di watch` — the long-running compile server (v1).
//!
//! The two costs a cold `compile` pays every run are the PHP scan (reading +
//! parsing thousands of files) and the write of ~10k output files. Both hurt
//! because they *touch many files*, and on APFS the filesystem-metadata lock
//! serializes that. A warm server touches only what changed:
//!
//! - the parsed [`Definitions`] (the PHP universe) stays in memory, so an edit
//!   that doesn't touch PHP (a di.xml tweak — the common case) skips the scan
//!   entirely;
//! - the OS file-watcher reports the exact delta, so there's no re-stat walk;
//! - the previous output tree stays in memory, so we diff and write only the
//!   handful of files that actually changed.
//!
//! Correctness is guaranteed by construction: each recompile runs the *same*
//! [`build::compute_outputs`] a cold compile runs, over a `Definitions` that is
//! either freshly re-scanned (a PHP file changed) or unchanged (nothing PHP
//! changed, so a re-scan would be identical). So the server's output after an
//! edit is byte-for-byte a cold `compile --force` of that same edited state —
//! the acceptance test on the oracle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Context;
use notify::event::{EventKind, ModifyKind};
use notify::{RecursiveMode, Watcher};

use magecommand_engine::build::{self, CompileOutputs};
use magecommand_engine::definitions::Definitions;
use magecommand_engine::metadata;
use magequery_core::Magento;

/// Quiet window after the last change event before recompiling. Coalesces the
/// burst an editor's save fires (write-temp + rename + attr changes) into one
/// recompile.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// What a changed path implies for the in-memory state.
enum Change {
    /// Not a compile input (our own output, `var/`, editor temp, …) — ignore.
    Irrelevant,
    /// A DI/config XML — re-open Magento (re-parse di.xml), reuse the PHP scan.
    Di,
    /// A PHP file under a scan root — re-scan the PHP universe.
    Php,
    /// config.php / module.xml / composer — the module set or autoload may have
    /// changed: re-open Magento AND re-scan.
    ModuleSet,
}

pub fn watch(root: Option<PathBuf>, _json: bool, once: bool) -> anyhow::Result<ExitCode> {
    // Absolutize like `compile` — BP is baked into generated output.
    let root = root.unwrap_or_else(|| PathBuf::from("."));
    let root = std::path::absolute(&root).unwrap_or(root);

    let mut magento = Magento::open(&root)
        .with_context(|| format!("not a Magento root: {}", root.display()))?;

    // The generated-code scan target, fixed for the server's lifetime: the
    // frozen archive in reproduction mode, else a path that never exists in live
    // mode. A cold compile scans a just-cleared (empty) generated/code; passing
    // a non-existent path is equivalent (an empty dir contributes no classes) and
    // avoids re-scanning our own freshly-written output on a later PHP change.
    let scan_generated = if root.join("generated/_code").is_dir() {
        root.join("generated/_code")
    } else {
        root.join("generated/.mqwatch-never")
    };

    // Initial full build — exactly `compile --force`, kept in memory.
    let t0 = Instant::now();
    metadata::clear_generated_dir(&root, "code")?;
    metadata::clear_generated_dir(&root, "metadata")?;
    let mut pristine = Definitions::scan(&magento, &root, &scan_generated);
    let out = compute(&magento, &pristine, &root);
    let written = metadata::write_generated(&root, &out.files)?;
    let mut prev: HashMap<String, String> = out.files.iter().cloned().collect();
    eprintln!(
        "watch: initial build — {} files in {:.1}s",
        written,
        t0.elapsed().as_secs_f64()
    );
    report_findings(&out);

    // Watcher → channel. The closure only captures the sender.
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let _ = tx.send(event);
        }
    })
    .context("failed to create a file watcher")?;
    watch_roots(&mut watcher, &magento, &root)?;
    eprintln!("watch: ready — edit a di.xml or a PHP file (Ctrl-C to stop)");

    let debug = std::env::var_os("MQ_WATCH_DEBUG").is_some();
    loop {
        // Block for the first write event, then drain the debounce window,
        // collecting UNIQUE changed paths. Skipping read/metadata events is
        // essential: the recompile's own scan reads thousands of files under the
        // watched roots, and inotify reports those as Access events — processing
        // them would feed back into an endless recompile loop.
        let mut paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let Ok(first) = rx.recv() else { break };
        collect_paths(&first, &mut paths);
        while let Ok(e) = rx.recv_timeout(DEBOUNCE) {
            collect_paths(&e, &mut paths);
        }

        // Classify the unique changed paths into the coarsest action required.
        let (mut relevant, mut rescan, mut reopen) = (false, false, false);
        for path in &paths {
            let change = classify(path, &root);
            if debug {
                let tag = match change {
                    Change::Irrelevant => "irrelevant",
                    Change::Di => "di",
                    Change::Php => "php",
                    Change::ModuleSet => "moduleset",
                };
                eprintln!("  [watch-debug] {tag:<10} {}", path.display());
            }
            match change {
                Change::Irrelevant => {}
                Change::Di => {
                    relevant = true;
                    reopen = true;
                }
                Change::Php => {
                    relevant = true;
                    rescan = true;
                }
                Change::ModuleSet => {
                    relevant = true;
                    reopen = true;
                    rescan = true;
                }
            }
        }
        if !relevant {
            continue;
        }

        let t = Instant::now();
        if reopen {
            magento = Magento::open(&root)
                .with_context(|| format!("re-open failed: {}", root.display()))?;
        }
        if rescan {
            pristine = Definitions::scan(&magento, &root, &scan_generated);
        }
        let out = compute(&magento, &pristine, &root);
        let stats = metadata::write_generated_delta(&root, &out.files, &prev)?;
        prev = out.files.iter().cloned().collect();
        eprintln!(
            "watch: recompiled in {:.0}ms — wrote {} · deleted {} · unchanged {}{}",
            t.elapsed().as_secs_f64() * 1000.0,
            stats.written,
            stats.deleted,
            stats.unchanged,
            if rescan { " (rescanned PHP)" } else { "" },
        );
        report_findings(&out);
        if once {
            break;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// One recompile from an already-scanned `pristine` index. Clones it first so the
/// pristine scan is never mutated (`extend_hierarchy` runs on the copy) and can
/// be reused across edits.
fn compute(magento: &Magento, pristine: &Definitions, root: &Path) -> CompileOutputs {
    let mut defs = pristine.clone();
    build::compute_outputs(magento, &mut defs, root)
}

fn report_findings(out: &CompileOutputs) {
    if !out.unresolved.is_empty() {
        eprintln!(
            "  note: {} unresolvable class name(s) (first: {})",
            out.unresolved.len(),
            out.unresolved.first().map(String::as_str).unwrap_or("")
        );
    }
    if !out.findings.is_empty() {
        eprintln!(
            "  note: {} compile finding(s), first: {}",
            out.findings.len(),
            out.findings.first().map(String::as_str).unwrap_or("")
        );
    }
}

/// Add an event's paths to `out`, but only for events that actually *change file
/// content* — Create, Remove, and Modify(Data/Name/Any). Access (reads — the
/// recompile's own scan generates these) and Modify(Metadata) (atime/chmod) are
/// dropped, so the server never recompiles in response to its own reads.
fn collect_paths(event: &notify::Event, out: &mut std::collections::HashSet<PathBuf>) {
    let interesting = match &event.kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Modify(_) => true,
        // Access, Any, Other: not a content change we act on.
        _ => false,
    };
    if interesting {
        out.extend(event.paths.iter().cloned());
    }
}

/// Map a changed path to the action it forces. Order matters: the module-set
/// files are named specifically, then extension decides.
fn classify(path: &Path, root: &Path) -> Change {
    // Never react to our own output or known noise dirs.
    if let Ok(rel) = path.strip_prefix(root) {
        let s = rel.to_string_lossy();
        if s.starts_with("generated/")
            || s.starts_with("var/")
            || s.starts_with("pub/")
            || s.starts_with(".git/")
        {
            return Change::Irrelevant;
        }
    }
    // Test fixtures the compile scan itself excludes (`**/_files/**`) are not
    // real inputs — a change there can't affect output, so never react to it.
    if path.components().any(|c| c.as_os_str() == "_files") {
        return Change::Irrelevant;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if matches!(name, "config.php" | "env.php" | "composer.json" | "composer.lock" | "module.xml")
    {
        return Change::ModuleSet;
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some("php") => Change::Php,
        // di.xml, events.xml, and every other config XML Magento merges.
        Some("xml") => Change::Di,
        _ => Change::Irrelevant,
    }
}

/// Recursively watch the compile's own input roots (enabled module dirs, library
/// paths, setup/src, app/etc). Nested-under-another roots are dropped to cut the
/// watch count. On some Linux hosts the inotify watch limit is too low for a full
/// Magento tree — surfaced with the sysctl to raise.
fn watch_roots(
    watcher: &mut notify::RecommendedWatcher,
    magento: &Magento,
    root: &Path,
) -> anyhow::Result<()> {
    let mut roots: Vec<PathBuf> = vec![root.join("app/etc")];
    for m in magento.modules() {
        if m.enabled {
            roots.push(m.path.clone());
        }
    }
    roots.extend(magento.library_paths().iter().cloned());
    let setup = root.join("setup/src");
    if setup.is_dir() {
        roots.push(setup);
    }
    roots.sort();
    roots.dedup();
    // Drop any root nested under one already kept (a recursive watch covers it).
    let mut kept: Vec<PathBuf> = Vec::with_capacity(roots.len());
    for r in roots {
        if kept.last().is_some_and(|prev| r.starts_with(prev)) {
            continue;
        }
        kept.push(r);
    }

    let mut watched = 0usize;
    let mut first_err: Option<notify::Error> = None;
    for r in &kept {
        if !r.is_dir() {
            continue;
        }
        match watcher.watch(r, RecursiveMode::Recursive) {
            Ok(()) => watched += 1,
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    if watched == 0 {
        if let Some(e) = first_err {
            anyhow::bail!(
                "could not watch any input root ({e}). If this is an inotify limit, raise it: \
                 sudo sysctl fs.inotify.max_user_watches=524288"
            );
        }
        anyhow::bail!("no input roots to watch under {}", root.display());
    }
    if let Some(e) = first_err {
        eprintln!("watch: note — some roots could not be watched ({e})");
    }
    eprintln!("watch: watching {watched} input roots");
    Ok(())
}
