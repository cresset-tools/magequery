//! magecommand — the write-side companion to magequery.
//!
//! magequery reads (nouns: inspect a Magento entity); magecommand writes
//! (verbs: act on the codebase). M0 surface: `compile --dry-run` (work-plan
//! preview; generation itself lands milestone by milestone) and `compare`
//! (the archive oracle every milestone is accepted against).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};

mod watch;

#[derive(Parser)]
#[command(
    name = "magecommand",
    version,
    about = "Act on a Magento 2 codebase: fast DI compilation."
)]
struct Cli {
    /// Path to the Magento root (defaults to the current directory).
    #[arg(long, global = true, value_name = "PATH")]
    root: Option<PathBuf>,

    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compile DI configuration and generated code.
    Compile {
        /// Report what would be generated without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing generated files.
        #[arg(long)]
        force: bool,
        /// Skip the compile entirely when no compile input changed since the
        /// last run (a stat-fingerprint of the input tree). On any change, does a
        /// normal full compile. The fast path for a tight edit-compile loop.
        #[arg(long)]
        incremental: bool,
    },
    /// Long-running compile server: build once, then keep the parsed index in
    /// memory and recompile on file changes, writing only the delta. The fast
    /// edit loop (kills the re-scan and the full re-write on each change).
    Watch {
        /// Recompile once after the first change, then exit (for testing).
        #[arg(long)]
        once: bool,
    },
    /// Compare a generated tree against an archived ground truth
    /// (`generated/_code`, `generated/_metadata`).
    Compare {
        /// The archived ground-truth directory.
        #[arg(long, value_name = "DIR")]
        archive: PathBuf,
        /// The generated output directory to check.
        #[arg(long, value_name = "DIR")]
        output: PathBuf,
        /// Exit non-zero unless the trees are identical.
        #[arg(long)]
        fail_on_diff: bool,
        /// How many paths to list per difference bucket (text output).
        #[arg(long, default_value_t = 10, value_name = "N")]
        sample: usize,
        /// Don't recognize or explain known/expected differences; list every
        /// divergence as a raw bucket (the pre-classification behavior).
        #[arg(long)]
        no_explain: bool,
        /// Require exact bytes: count interceptors that differ only in method
        /// order as `changed`. By default such files (behaviorally identical;
        /// PHP 8.4 vs 8.5 reflection order) are reported as `reordered`.
        #[arg(long)]
        strict_ordering: bool,
        /// Diagnostic: for one changed metadata file (relative path, e.g.
        /// `frontend.php`), apply the classifier normalizations (strip
        /// disabled-module entries, canonicalize the ClassesScanner regex) to both
        /// sides and print the first genuine divergence with context. Pinpoints why
        /// a lone file stays unexplained.
        #[arg(long, value_name = "FILE")]
        show_residual: Option<PathBuf>,
    },
    /// Print a digest of the compile inputs — a content-addressed key for the
    /// generated output. Since the compile is a pure function of the source
    /// tree, an unchanged digest means the last compile is still valid: use it
    /// as a CI cache key for `generated/` (see docs/incremental-compile.md).
    Digest {
        /// Fingerprint by mtime+size instead of file contents — fast, but mtime
        /// is unreliable after a fresh `git checkout`, so this matches the local
        /// `--incremental` short-circuit and is NOT a portable CI key.
        #[arg(long)]
        stat: bool,
    },
}

fn main() -> anyhow::Result<ExitCode> {
    // Restore the default SIGPIPE disposition: piping into `head`/`less` and
    // quitting early should terminate us cleanly like any Unix tool. Rust
    // otherwise ignores SIGPIPE, turning a closed reader into a write error
    // that `println!` unwraps into a panic. (Same fix as magequery's main.)
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();
    match cli.command {
        Command::Compile { dry_run, force, incremental } => {
            compile(cli.root, cli.json, dry_run, force, incremental)
        }
        Command::Digest { stat } => digest(cli.root, stat),
        Command::Watch { once } => watch::watch(cli.root, cli.json, once),
        Command::Compare {
            ref archive,
            ref output,
            fail_on_diff,
            sample,
            no_explain,
            strict_ordering,
            ref show_residual,
        } => compare(
            cli.root,
            archive,
            output,
            cli.json,
            fail_on_diff,
            sample,
            no_explain,
            strict_ordering,
            show_residual.as_deref(),
        ),
    }
}

// The final `lap!` resets `t` without a subsequent read — expected for a
// timing macro whose last call closes the sequence.
#[allow(unused_assignments)]
fn compile(
    root: Option<PathBuf>,
    json: bool,
    dry_run: bool,
    force: bool,
    incremental: bool,
) -> anyhow::Result<ExitCode> {
    use magequery_core::Area;

    // Magento's `BP` is always an absolute path, and it is baked verbatim into
    // several generated arguments (the dev/test path-exclusion regexes in the
    // area files). Absolutize a relative or defaulted root so `compile` is
    // byte-identical regardless of the working directory it is invoked from
    // (`.` from inside the store would otherwise emit `./setup/src` and empty
    // module-path regexes). `absolute` (not `canonicalize`) mirrors `BP`: it
    // does not resolve symlinks.
    let root = root.unwrap_or_else(|| PathBuf::from("."));
    let root = std::path::absolute(&root).unwrap_or(root);

    // Phase timing (stderr) when MAGECOMMAND_PROFILE is set — `lap!("label")`
    // prints the time since the previous lap and resets the clock.
    let prof = std::env::var_os("MAGECOMMAND_PROFILE").is_some();
    let mut t = std::time::Instant::now();
    macro_rules! lap {
        ($label:expr) => {
            if prof {
                eprintln!(
                    "  [profile] {:<26} {:>8.1} ms",
                    $label,
                    t.elapsed().as_secs_f64() * 1000.0
                );
                t = std::time::Instant::now();
            }
        };
    }

    let magento = magequery_core::Magento::open(&root)
        .with_context(|| format!("not a Magento root: {}", root.display()))?;
    lap!("open + discovery");

    // Incremental short-circuit (the CAS design — see manifest.rs). Load the
    // previous manifest; if `--incremental` and no compile INPUT changed since it
    // was written (stat-fingerprint match), the current output is already correct
    // — skip the entire compile (scan + compute + write) before doing any of it.
    // On ANY input change we fall through to a FULL compile below: a partial
    // reconcile is a net loss on APFS (rename ≈ write, and the compute needs the
    // old tree absent), so the win is entirely in this no-op skip. The digest is
    // computed ONCE here and reused as the new manifest's digest on a miss, so
    // the input tree is never fingerprinted twice. `--force` bypasses; a dry-run
    // always reports.
    let bp = root.to_string_lossy().to_string();
    let code_dir = root.join("generated/code");
    let prev_manifest = if incremental && !force && dir_has_files(&code_dir) {
        magecommand_engine::manifest::Manifest::load(&root, &bp)
    } else {
        None
    };
    // The change-detection walk's result, reused for the manifest on a miss.
    let mut input_digest: Option<String> = None;
    if !dry_run {
        if let Some(prev) = prev_manifest.as_ref() {
            let current = magecommand_engine::manifest::input_digest(
                &magento,
                &root,
                magecommand_engine::manifest::FingerprintMode::Stat,
            );
            lap!("input digest (short-circuit check)");
            if current == prev.inputs_digest {
                if json {
                    println!("{{\"status\":\"up-to-date\",\"reason\":\"no input changed\"}}");
                } else {
                    println!("up to date — no compile input changed (use --force to rebuild)");
                }
                return Ok(ExitCode::SUCCESS);
            }
            input_digest = Some(current);
        }
    }

    let modules = magento.modules();
    let enabled = modules.iter().filter(|m| m.enabled).count();

    // The config-derived half of the work plan. The PHP-scan-derived half
    // (factories/proxies referenced in code) arrives with the extractor (M1).
    const AREAS: [Area; 7] = [
        Area::Global,
        Area::Frontend,
        Area::Adminhtml,
        Area::Crontab,
        Area::WebapiRest,
        Area::WebapiSoap,
        Area::Graphql,
    ];
    let exports: Vec<_> = AREAS.iter().map(|&a| magento.di_export(a)).collect();
    let extended_types = magento.extension_attributes(None).len();
    lap!("di_export x7 (work plan)");

    if json {
        let plan = serde_json::json!({
            "root": root.display().to_string(),
            "modules": modules.len(),
            "enabled": enabled,
            "extension_attribute_types": extended_types,
            "areas": exports.iter().map(|e| {
                let targets: std::collections::BTreeSet<_> =
                    e.plugins.iter().filter(|p| !p.disabled).map(|p| &p.target).collect();
                serde_json::json!({
                    "area": e.area,
                    "preferences": e.preferences.len(),
                    "virtual_types": e.virtual_types.len(),
                    "plugin_declarations": e.plugins.len(),
                    "plugged_targets": targets.len(),
                    "argument_declarations": e.arguments.len(),
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        println!(
            "magento root: {} · {} modules ({} enabled)",
            root.display(),
            modules.len(),
            enabled
        );
        println!("work plan (config-derived; PHP-scan half lands with the extractor):");
        for e in &exports {
            let targets: std::collections::BTreeSet<_> = e
                .plugins
                .iter()
                .filter(|p| !p.disabled)
                .map(|p| &p.target)
                .collect();
            println!(
                "  {:<12} {:>4} preferences · {:>3} virtual types · {:>4} plugin decls on {:>3} targets · {:>4} argument decls",
                format!("{:?}", e.area).to_lowercase(),
                e.preferences.len(),
                e.virtual_types.len(),
                e.plugins.len(),
                targets.len(),
                e.arguments.len()
            );
        }
        println!("  extension-attribute types: {extended_types}");
    }

    if !dry_run {
        // A clean compile, like `setup:di:compile`: refuse to clobber an
        // existing output tree unless --force, then wipe generated/code +
        // generated/metadata and regenerate both from scratch. (The archive
        // dirs `_code`/`_metadata` are never touched.) `--incremental` is an
        // intentional overwrite (it only reaches here on a detected change), so
        // it skips the guard. `bp` and `code_dir` were computed above.
        let meta_dir = root.join("generated/metadata");
        if !force && !incremental && (dir_has_files(&code_dir) || dir_has_files(&meta_dir)) {
            anyhow::bail!(
                "generated/code or generated/metadata is non-empty; pass --force to overwrite"
            );
        }

        // The compute must see generated/code ABSENT: stale generated artifacts
        // otherwise leak into the scan universe AND the class resolver,
        // perturbing which factories/interceptors get emitted. Clearing first
        // (as `setup:di:compile` does) is what makes every path — full, forced,
        // or incremental-on-change — produce a byte-identical tree.
        magecommand_engine::metadata::clear_generated_dir(&root, "code")?;
        magecommand_engine::metadata::clear_generated_dir(&root, "metadata")?;
        lap!("clear generated dirs");

        // The class universe scans a generated-code tree so reflection over
        // generated artifacts (a proxy's source, an interceptor's subject)
        // resolves. Reproduction mode prefers the frozen archive `_code` (the
        // oracle's own output) over the live `generated/code` we just cleared:
        // a self-consistent bring-up. A true from-empty compile would scan our
        // freshly generated tree instead; that swap is a later refinement.
        let generated_code = if root.join("generated/_code").is_dir() {
            root.join("generated/_code")
        } else {
            root.join("generated/code")
        };
        let mut defs = magecommand_engine::definitions::Definitions::scan(
            &magento,
            &root,
            &generated_code,
        );
        lap!("scan php universe");
        // The whole compile compute (extend_hierarchy → area files →
        // interception → plugin-lists → generated/code), as one in-memory output
        // set. This is the exact path `watch` runs, so the two emit identical
        // bytes. Per-phase timings print under MAGECOMMAND_PROFILE from inside.
        let out = magecommand_engine::build::compute_outputs(&magento, &mut defs, &root);
        if !out.unresolved.is_empty() {
            eprintln!(
                "note: {} class name(s) unresolvable via autoload maps (first: {})",
                out.unresolved.len(),
                out.unresolved.first().map(String::as_str).unwrap_or("")
            );
        }
        lap!("compute outputs (in memory)");
        // Write the whole tree (the compute cleared it first). Direct parallel
        // write, no reconcile — on APFS reusing unchanged files costs more than
        // rewriting them (see manifest.rs).
        let written = magecommand_engine::metadata::write_generated(&root, &out.files)?;
        lap!("write generated (disk)");
        // Record the input fingerprint so the next --incremental run can
        // short-circuit when nothing changed. Reuse the digest computed by the
        // short-circuit check (the inputs didn't change during the compile — it
        // only writes generated/, which is not an input); compute it fresh only
        // for a --force / first run that skipped that check. Stat-based to match.
        let digest = match input_digest {
            Some(d) => d,
            None => magecommand_engine::manifest::input_digest(
                &magento,
                &root,
                magecommand_engine::manifest::FingerprintMode::Stat,
            ),
        };
        magecommand_engine::manifest::Manifest::new(&bp, digest).save(&root)?;
        lap!("write manifest + input digest");
        println!("wrote {written} generated/ file(s)");
        warn_case_collapse(&root, &out.files);
        if !out.findings.is_empty() {
            eprintln!(
                "note: {} compile finding(s), first: {}",
                out.findings.len(),
                out.findings.first().map(String::as_str).unwrap_or("")
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// After a bulk write, surface case-collision hazards in the output tree.
///
/// FILE-level collisions (two artifacts whose full paths differ only in
/// case) warn on EVERY platform: PHP class names are case-insensitive, so
/// the two generated classes are the same class to PHP — the source declares
/// case-variant duplicates, only one can ever load per request (whichever
/// spelling is referenced first), and on a case-insensitive volume one
/// artifact silently overwrote the other.
///
/// DIRECTORY-level collisions warn only when the output volume is actually
/// case-insensitive: on Linux both spellings coexist (Magento core ships
/// deliberate pairs like Backend\Tierprice vs Backend\TierPrice), but here
/// they collapsed into one physical dir (first writer wins), leaving files
/// at paths PSR-4 can't resolve on a case-sensitive host. Real
/// `setup:di:compile` on macOS collapses identically — a build-host
/// warning, not a compile error: fine for local inspection, wrong to deploy.
fn warn_case_collapse(root: &std::path::Path, files: &[(String, String)]) {
    let collisions = magecommand_engine::metadata::case_collisions(files);
    if !collisions.files.is_empty() {
        eprintln!();
        eprintln!(
            "WARNING: {} generated file group(s) differ only in letter case — PHP treats\n\
             their classes as THE SAME class (class names are case-insensitive), so the\n\
             source likely declares case-variant duplicates of one class. Which file\n\
             autoloads depends on the first reference's spelling, on every platform.",
            collisions.files.len()
        );
        for group in &collisions.files {
            eprintln!("  same class: {}", group.join("  <->  "));
        }
    }
    let collapsed_dirs = !collisions.dirs.is_empty()
        && files.first().is_some_and(|(sample, _)| {
            magecommand_engine::metadata::output_fs_is_case_insensitive(root, sample)
        });
    if collapsed_dirs {
        eprintln!();
        eprintln!(
            "WARNING: this filesystem is case-insensitive, and {} output path group(s) that\n\
             differ only in letter case collapsed into single directories (first writer wins).\n\
             Some files now sit at a path whose case does not match their class namespace, so\n\
             PSR-4 CANNOT AUTOLOAD them on a case-sensitive host. Do NOT deploy this generated/\n\
             tree to Linux — compile on the target host (or a case-sensitive volume) instead.",
            collisions.dirs.len()
        );
        for group in &collisions.dirs {
            eprintln!("  collided: {}", group.join("  <->  "));
        }
    }
    if !collisions.files.is_empty() || collapsed_dirs {
        eprintln!();
    }
}

/// `magecommand digest [--stat]` — print the compile-input digest and exit.
fn digest(root: Option<PathBuf>, stat: bool) -> anyhow::Result<ExitCode> {
    let root = root.unwrap_or_else(|| PathBuf::from("."));
    let root = std::path::absolute(&root).unwrap_or(root);
    let magento = magequery_core::Magento::open(&root)
        .with_context(|| format!("not a Magento root: {}", root.display()))?;
    let mode = if stat {
        magecommand_engine::manifest::FingerprintMode::Stat
    } else {
        magecommand_engine::manifest::FingerprintMode::Content
    };
    println!("{}", magecommand_engine::manifest::input_digest(&magento, &root, mode));
    Ok(ExitCode::SUCCESS)
}

/// Whether a directory exists and contains at least one entry.
fn dir_has_files(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

#[allow(clippy::too_many_arguments)]
fn compare(
    root: Option<PathBuf>,
    archive: &PathBuf,
    output: &PathBuf,
    json: bool,
    fail_on_diff: bool,
    sample: usize,
    no_explain: bool,
    strict_ordering: bool,
    show_residual: Option<&std::path::Path>,
) -> anyhow::Result<ExitCode> {
    let report = magecommand_engine::compare_dirs(archive, output, strict_ordering)?;

    // The disabled-module explanation needs config.php. Best-effort: if the
    // root isn't a Magento checkout (comparing two loose trees), skip it —
    // those interceptors simply stay unexplained rather than misclassified.
    let disabled_modules: std::collections::HashSet<String> = if no_explain {
        std::collections::HashSet::new()
    } else {
        let root = std::path::absolute(root.unwrap_or_else(|| PathBuf::from("."))).unwrap_or_default();
        magequery_core::Magento::open(&root)
            .map(|m| {
                m.modules()
                    .iter()
                    .filter(|md| !md.enabled)
                    .map(|md| md.name.to_string())
                    .collect()
            })
            .unwrap_or_default()
    };

    // Diagnostic mode: pinpoint one file's residual difference and stop.
    if let Some(rel) = show_residual {
        let a = std::fs::read_to_string(archive.join(rel))
            .with_context(|| format!("reading archive {}", archive.join(rel).display()))?;
        let b = std::fs::read_to_string(output.join(rel))
            .with_context(|| format!("reading output {}", output.join(rel).display()))?;
        print!("{}", magecommand_engine::residual_report(&a, &b, &disabled_modules));
        return Ok(ExitCode::SUCCESS);
    }

    let total = report.archive_total();
    let mut summary = format!(
        "archive: {} file(s) · identical {} · changed {} · missing {} · extra {}",
        total,
        report.identical,
        report.changed.len(),
        report.missing.len(),
        report.extra.len()
    );
    if !report.reordered.is_empty() {
        summary.push_str(&format!(" · reordered {}", report.reordered.len()));
    }

    // Raw mode (or --json, which stays the machine-readable byte-level report):
    // no classification.
    if no_explain {
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!("{summary}");
            print_bucket("changed", &report.changed, sample);
            print_bucket("missing", &report.missing, sample);
            print_bucket("extra", &report.extra, sample);
            print_bucket("reordered", &report.reordered, sample);
            if report.is_clean() && report.reordered.is_empty() {
                println!("output reproduces the archive exactly");
            } else if report.is_clean() {
                println!(
                    "output matches the archive; {} file(s) differ only in method order",
                    report.reordered.len()
                );
            }
        }
        return Ok(fail_exit(fail_on_diff, !report.is_clean()));
    }

    let ctx = magecommand_engine::ClassifyCtx {
        archive,
        output,
        disabled_modules: &disabled_modules,
    };
    let classified = magecommand_engine::classify(&report, &ctx);

    if json {
        let out = serde_json::json!({ "report": report, "classified": classified });
        println!("{}", serde_json::to_string_pretty(&out)?);
        // Under --json, "clean" ignores explained differences too, so scripts
        // can gate on genuine regressions only.
        return Ok(fail_exit(fail_on_diff, classified.unexplained_count() > 0));
    }

    println!("{summary}");

    // The genuine, unexplained differences first — the signal.
    print_bucket("changed", &classified.changed, sample);
    print_bucket("missing", &classified.missing, sample);
    print_bucket("extra", &classified.extra, sample);

    // Method-order-only differences: behaviorally identical, so grouped with the
    // known/expected divergences rather than the signal above.
    if !report.reordered.is_empty() {
        println!(
            "\n  ▸ Interceptor method order (PHP-version reflection order) ({} file(s))",
            report.reordered.len()
        );
        let explanation = "Same method set, byte-identical bodies, different order. PHP's \
            getMethods() order — which the interceptor generator follows — differs across PHP \
            versions (8.4 vs 8.5) for trait-using classes. Method order in a PHP class is \
            behaviorally irrelevant, so these are equivalent. Use --strict-ordering to treat \
            them as `changed`.";
        for line in wrap_indent(explanation, "    ", 92) {
            println!("{line}");
        }
        for item in report.reordered.iter().take(sample) {
            println!("      · {item}");
        }
        if report.reordered.len() > sample {
            println!("      · … {} more", report.reordered.len() - sample);
        }
    }

    // Then the known/expected differences, each with its explanation.
    if !classified.known.is_empty() {
        println!(
            "\nknown & expected differences ({} file(s)) — magecommand targets Mage-OS 3.1.0 / Magento 2.4.9:",
            classified.known_count()
        );
        for group in &classified.known {
            println!("\n  ▸ {} ({} file(s))", group.title, group.items.len());
            for line in wrap_indent(&group.explanation, "    ", 92) {
                println!("{line}");
            }
            for item in group.items.iter().take(sample) {
                println!("      · {item}");
            }
            if group.items.len() > sample {
                println!("      · … {} more", group.items.len() - sample);
            }
        }
    }

    // Verdict. Method-order-only differences (`reordered`) are behaviorally
    // benign, so they never count as unexplained but are surfaced for honesty.
    let reordered = report.reordered.len();
    let reordered_note = if reordered > 0 {
        format!(", {reordered} method-order")
    } else {
        String::new()
    };
    if report.is_clean() && reordered == 0 {
        println!("\noutput reproduces the archive exactly");
    } else if classified.unexplained_count() == 0 {
        println!(
            "\noutput matches the archive except for {} known/expected{reordered_note} difference(s) explained above",
            classified.known_count()
        );
    } else {
        println!(
            "\n{} unexplained difference(s) to investigate; {} known/expected{reordered_note}",
            classified.unexplained_count(),
            classified.known_count()
        );
    }

    Ok(fail_exit(fail_on_diff, classified.unexplained_count() > 0))
}

fn fail_exit(fail_on_diff: bool, has_diff: bool) -> ExitCode {
    if fail_on_diff && has_diff {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn print_bucket(label: &str, paths: &[String], sample: usize) {
    if paths.is_empty() {
        return;
    }
    for path in paths.iter().take(sample) {
        println!("  {label}: {path}");
    }
    if paths.len() > sample {
        println!("  {label}: … {} more", paths.len() - sample);
    }
}

/// Word-wrap `text` to `width` columns, each line prefixed with `indent`.
fn wrap_indent(text: &str, indent: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            lines.push(format!("{indent}{line}"));
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(format!("{indent}{line}"));
    }
    lines
}
