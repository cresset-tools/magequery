//! The upgrade gates: `db status`, `config status`, `theme status` and `queue
//! status`. Native versions of `bin/magento setup:db:status`,
//! `app:config:status`, and Mage-OS's `theme:registration:status` and
//! `queue:config:status`, so a deploy can decide whether `setup:upgrade` or
//! `app:config:import` has work to do without bootstrapping Magento. The checks
//! themselves live in magequery-core; this is the CLI around them.
//!
//! Each keeps its `bin/magento` counterpart's exit codes: 0 when there is
//! nothing to do, 2 when the upgrade or import has work, 1 on an error.
//! Deployer's magento2 recipe upgrades on exactly 2 and fails the deploy on any
//! other non-zero code.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Result, anyhow};
use magequery_core::Magento;
use magequery_core::model::DbStatus;

/// `DbStatusCommand::EXIT_CODE_UPGRADE_REQUIRED`,
/// `ConfigStatusCommand::EXIT_CODE_CONFIG_IMPORT_REQUIRED`, and Mage-OS's
/// registration-status commands' "missing" code: all 2.
const EXIT_WORK_TO_DO: u8 = 2;

fn exit(nothing_to_do: bool) -> ExitCode {
    if nothing_to_do { ExitCode::SUCCESS } else { ExitCode::from(EXIT_WORK_TO_DO) }
}

fn open(root: Option<PathBuf>) -> Result<Magento> {
    let root = root.unwrap_or_else(|| PathBuf::from("."));
    Magento::open(&root).map_err(|e| anyhow!(e))
}

/// `db status`: exits 2 when an upgrade is required (including an unreachable
/// database, as Magento does), and 1 on an error such as a store with no
/// install date.
pub fn db(root: Option<PathBuf>, json: bool) -> Result<ExitCode> {
    let mage = open(root)?;
    let status = mage.db_status().map_err(|e| anyhow!(e))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        for n in &status.notes {
            eprintln!("note: {n}");
        }
        print!("{}", render_db(&status, mage.root()));
    }
    Ok(exit(status.is_up_to_date()))
}

fn render_db(status: &DbStatus, root: &Path) -> String {
    let mut out = String::new();
    let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();

    if !status.schema_changes.is_empty() {
        let _ = writeln!(
            out,
            "Declarative schema is not up to date ({})",
            status.schema_changes.len()
        );
        for c in &status.schema_changes {
            let target = match &c.element {
                Some(e) => format!("{}.{e}", c.table),
                None => c.table.clone(),
            };
            let _ = writeln!(out, "  {:<20} {target}", c.operation);
            for d in &c.detail {
                let _ = writeln!(out, "  {:<20} {d}", "");
            }
            let rows = c.rows.map(|n| format!(", ~{n} rows")).unwrap_or_default();
            let _ = writeln!(out, "  {:<20} {}: {}{rows}", "", c.online, c.why);
        }
        out.push('\n');
    }
    for (title, patches) in [
        ("Schema patches are not up to date", &status.schema_patches),
        ("Data patches are not up to date", &status.data_patches),
    ] {
        if patches.is_empty() {
            continue;
        }
        let _ = writeln!(out, "{title} ({})", patches.len());
        for p in patches {
            let _ = writeln!(out, "  {}  # {}", p.class.as_str(), rel(&p.file));
        }
        out.push('\n');
    }
    if !status.module_versions.is_empty() {
        let _ = writeln!(
            out,
            "Module versions differ from setup_module ({})",
            status.module_versions.len()
        );
        for v in &status.module_versions {
            let _ = writeln!(
                out,
                "  {}  {:<6}  {} → {}",
                v.module.as_str(),
                v.kind.to_string(),
                v.current.as_deref().unwrap_or("none"),
                v.required,
            );
        }
        out.push('\n');
    }
    for e in &status.errors {
        let _ = writeln!(out, "check failed: {e}");
    }
    if status.is_up_to_date() {
        out.push_str("All modules are up to date.\n");
    } else {
        let reasons = status.maintenance_reasons();
        if reasons.is_empty() {
            out.push_str(
                "The upgrade could run without maintenance mode (recurring setup scripts not counted).\n",
            );
        } else {
            out.push_str("The upgrade needs maintenance mode:\n");
            for r in &reasons {
                let _ = writeln!(out, "  {r}");
            }
        }
        out.push_str("Run 'setup:upgrade' to update your DB schema and data.\n");
    }
    out
}

/// `config status`: exits 1 on a config this port can't hash exactly (run
/// `bin/magento app:config:status` then).
pub fn config(root: Option<PathBuf>, json: bool) -> Result<ExitCode> {
    let mage = open(root)?;
    let status = mage.config_status().map_err(|e| anyhow!(e))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        for s in status.sections.iter().filter(|s| s.changed) {
            println!("{}: changed since the last import", s.name);
        }
        if status.import_required() {
            println!(
                "Config files have changed. Run app:config:import or setup:upgrade command to synchronize configuration."
            );
        } else {
            println!("Config files are up to date.");
        }
    }
    Ok(exit(!status.import_required()))
}

/// `theme status`: the themes a `registration.php` declares that the `theme`
/// table does not have yet.
pub fn theme(root: Option<PathBuf>, json: bool) -> Result<ExitCode> {
    let mage = open(root)?;
    let missing = mage.unregistered_themes().map_err(|e| anyhow!(e))?;
    if json {
        let registered: Vec<String> =
            mage.registered_themes().into_iter().map(|(id, _)| id).collect();
        let out = serde_json::json!({ "themes": registered, "unregistered": missing });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if missing.is_empty() {
        println!("All themes are registered.");
    } else {
        for t in &missing {
            println!("{t}: not registered");
        }
    }
    Ok(exit(missing.is_empty()))
}

/// `queue status`: database queues only. RabbitMQ's state lives on the broker,
/// so for it this prints the topology fingerprint to compare between releases.
pub fn queue(root: Option<PathBuf>, json: bool) -> Result<ExitCode> {
    let mage = open(root)?;
    let missing = mage.missing_db_queues().map_err(|e| anyhow!(e))?;
    let fingerprint = mage.amqp_topology_fingerprint().map_err(|e| anyhow!(e))?;
    if json {
        let configured = mage.topology_queues().map_err(|e| anyhow!(e))?;
        let out = serde_json::json!({
            "queues": configured,
            "missing": missing,
            "amqp_topology_fingerprint": fingerprint,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        if missing.is_empty() {
            println!("All queues are in the queue table.");
        }
        for q in &missing {
            println!("{q}: not in the queue table");
        }
        if let Some(f) = &fingerprint {
            println!("AMQP topology fingerprint: {f}");
        }
    }
    Ok(exit(missing.is_empty()))
}
