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
        Command::Compile { dry_run } => compile(cli.root, cli.json, dry_run),
        Command::Compare {
            ref archive,
            ref output,
            fail_on_diff,
            sample,
        } => compare(archive, output, cli.json, fail_on_diff, sample),
    }
}

fn compile(root: Option<PathBuf>, json: bool, dry_run: bool) -> anyhow::Result<ExitCode> {
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
        anyhow::bail!("generation is not implemented yet — run with --dry-run");
    }
    Ok(ExitCode::SUCCESS)
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
