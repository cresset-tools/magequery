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
        Command::Compile { dry_run, force } => compile(cli.root, cli.json, dry_run, force),
        Command::Compare {
            ref archive,
            ref output,
            fail_on_diff,
            sample,
        } => compare(archive, output, cli.json, fail_on_diff, sample),
    }
}

fn compile(root: Option<PathBuf>, json: bool, dry_run: bool, force: bool) -> anyhow::Result<ExitCode> {
    use magequery_core::Area;

    let root = root.unwrap_or_else(|| PathBuf::from("."));
    let magento = magequery_core::Magento::open(&root)
        .with_context(|| format!("not a Magento root: {}", root.display()))?;

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
        // dirs `_code`/`_metadata` are never touched.)
        let code_dir = root.join("generated/code");
        let meta_dir = root.join("generated/metadata");
        if !force && (dir_has_files(&code_dir) || dir_has_files(&meta_dir)) {
            anyhow::bail!(
                "generated/code or generated/metadata is non-empty; pass --force to overwrite"
            );
        }
        magecommand_engine::metadata::clear_generated_dir(&root, "code")?;
        magecommand_engine::metadata::clear_generated_dir(&root, "metadata")?;

        // Metadata files (M2). The output dir is now clean, so force-write.
        let list = magecommand_engine::metadata::app_action_list(&magento);
        let content = magecommand_engine::phpexport::to_php_file(&list);
        let path = magecommand_engine::metadata::write_metadata_file(
            &root,
            "app_action_list.php",
            &content,
            true,
        )?;
        println!("wrote {}", path.display());

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
        let unresolved = defs.extend_hierarchy(&magento, &root, pref_keys);
        if !unresolved.is_empty() {
            eprintln!(
                "note: {} class name(s) unresolvable via autoload maps (first: {})",
                unresolved.len(),
                unresolved.first().map(String::as_str).unwrap_or("")
            );
        }
        let mut finding_count = 0usize;
        for (area, code) in magecommand_engine::areaconfig::AREA_CODES {
            let file = magecommand_engine::areaconfig::build_area_file(
                &magento, &defs, area, &root,
            );
            finding_count += file.findings.len();
            let path = magecommand_engine::metadata::write_metadata_file(
                &root,
                &format!("{code}.php"),
                &file.render(),
                true,
            )?;
            println!("wrote {}", path.display());
        }
        if finding_count > 0 {
            eprintln!("note: {finding_count} static-analysis finding(s) across areas — see --json");
        }

        let interception = magecommand_engine::interception::interception_map(&magento, &defs);
        let path = magecommand_engine::metadata::write_metadata_file(
            &root,
            "interception.php",
            &magecommand_engine::interception::render(&interception),
            true,
        )?;
        println!("wrote {}", path.display());

        let plugin_lists = magecommand_engine::pluginlist::generate(&magento, &defs);
        for (name, content) in &plugin_lists.files {
            let path =
                magecommand_engine::metadata::write_metadata_file(&root, name, content, true)?;
            println!("wrote {}", path.display());
        }
        if !plugin_lists.findings.is_empty() {
            eprintln!(
                "note: {} plugin-list finding(s), first: {}",
                plugin_lists.findings.len(),
                plugin_lists.findings.first().map(String::as_str).unwrap_or("")
            );
        }

        // generated/code (M3): factories, extensions, proxies, searchResults,
        // proxyDeferred, interceptors — the full tree the compare checks
        // against `generated/_code`.
        let code = magecommand_engine::codegen::generate_code(&magento, &defs, root.clone());
        for (rel, content) in &code.files {
            magecommand_engine::metadata::write_code_file(&root, rel, content, true)?;
        }
        println!("wrote {} generated/code file(s)", code.files.len());
        if !code.findings.is_empty() {
            eprintln!(
                "note: {} generated-code finding(s), first: {}",
                code.findings.len(),
                code.findings.first().map(String::as_str).unwrap_or("")
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Whether a directory exists and contains at least one entry.
fn dir_has_files(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

fn compare(
    archive: &PathBuf,
    output: &PathBuf,
    json: bool,
    fail_on_diff: bool,
    sample: usize,
) -> anyhow::Result<ExitCode> {
    let report = magecommand_engine::compare_dirs(archive, output)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let total = report.archive_total();
        println!(
            "archive: {} file(s) · identical {} · changed {} · missing {} · extra {}",
            total,
            report.identical,
            report.changed.len(),
            report.missing.len(),
            report.extra.len()
        );
        print_bucket("changed", &report.changed, sample);
        print_bucket("missing", &report.missing, sample);
        print_bucket("extra", &report.extra, sample);
        if report.is_clean() {
            println!("output reproduces the archive exactly");
        }
    }

    if fail_on_diff && !report.is_clean() {
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
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
