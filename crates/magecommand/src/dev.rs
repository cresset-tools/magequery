//! `dev upgrade`: a development `bin/magento setup:upgrade` that skips PHP when
//! there is nothing to upgrade. The decisions live in magequery-core's
//! `devupgrade`; this runs them in `setup:upgrade`'s order and falls back to the
//! real command, with the terminal attached, whenever one of them says PHP is
//! needed.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use magequery_core::Magento;
use magequery_core::devupgrade::{ConfigPhpUpdate, DevUpgradeCheck, empty_dir};

use crate::style;

/// What `Installer::cleanupGeneratedFiles` empties, unless `--keep-generated`.
const GENERATED: &[&str] = &["generated/code", "generated/metadata", "pub/static", "var/view_preprocessed"];

pub struct Options {
    pub keep_generated: bool,
    pub php: String,
    pub full: bool,
}

pub fn upgrade(root: Option<PathBuf>, json: bool, opts: &Options) -> Result<ExitCode> {
    let start = Instant::now();
    let root = root.unwrap_or_else(|| PathBuf::from("."));
    let mut mage = Magento::open(&root).map_err(|e| anyhow!(e))?;
    if mage.info().mode.as_deref() == Some("production") {
        bail!("MAGE_MODE is production: `magecommand dev upgrade` is for development; run bin/magento setup:upgrade");
    }

    let mut needs_php: Vec<String> = Vec::new();
    let mut done: Vec<String> = Vec::new();
    if opts.full {
        needs_php.push("--full".into());
    }

    // 1. config.php, before anything reads the module list (as Installer does).
    match mage.upgraded_config_php().map_err(|e| anyhow!(e))? {
        ConfigPhpUpdate::Unchanged => {}
        ConfigPhpUpdate::Rewrite { contents, added, removed, reordered } => {
            let path = mage.root().join("app/etc/config.php");
            std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
            let mut what = Vec::new();
            if !added.is_empty() {
                what.push(format!("added {}", added.join(", ")));
            }
            if !removed.is_empty() {
                what.push(format!("removed {}", removed.join(", ")));
            }
            if reordered {
                what.push("reordered".into());
            }
            done.push(format!("config.php: {}", what.join("; ")));
            mage = Magento::open(&root).map_err(|e| anyhow!(e))?;
        }
        ConfigPhpUpdate::CannotReproduce(why) => needs_php.push(why),
    }

    // 2. Everything that would need PHP.
    let check = if needs_php.is_empty() {
        mage.dev_upgrade_check().map_err(|e| anyhow!(e))?
    } else {
        DevUpgradeCheck::default()
    };
    needs_php.extend(check.needs_php.iter().cloned());

    // 3. The fast path: caches, generated files, the recurring writes.
    if needs_php.is_empty() {
        match mage.clean_caches() {
            Ok(lines) => done.extend(lines),
            Err(e) => needs_php.push(e.to_string()),
        }
    }
    if !needs_php.is_empty() {
        return fall_back(mage.root(), json, opts, &needs_php, &done, start);
    }
    if !opts.keep_generated {
        let mut n = 0;
        for dir in GENERATED {
            let path = mage.root().join(dir);
            n += empty_dir(&path).with_context(|| format!("emptying {}", path.display()))?;
        }
        done.push(format!("generated files: emptied {} ({n} entries)", GENERATED.join(", ")));
    }
    done.extend(mage.dev_upgrade_apply(&check).map_err(|e| anyhow!(e))?);

    let ms = start.elapsed().as_millis();
    if json {
        let out = serde_json::json!({
            "php": false,
            "done": done,
            "recurring_skipped": check.recurring_skipped,
            "elapsed_ms": ms,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        for d in &done {
            println!("{d}");
        }
        println!(
            "{}",
            style::dim(&format!(
                "recurring scripts with nothing to do: {}",
                check.recurring_skipped.join(", ")
            ))
        );
        println!("{}", style::pass(&format!("Nothing to upgrade; done natively in {ms} ms.")));
    }
    Ok(ExitCode::SUCCESS)
}

/// Run the real `setup:upgrade`, terminal attached, after saying why.
fn fall_back(
    root: &Path,
    json: bool,
    opts: &Options,
    needs_php: &[String],
    done: &[String],
    start: Instant,
) -> Result<ExitCode> {
    let mut words = opts.php.split_whitespace();
    let program = words.next().context("--php is empty")?;
    let mut cmd = Command::new(program);
    cmd.args(words).arg("bin/magento").arg("setup:upgrade").current_dir(root);
    if opts.keep_generated {
        cmd.arg("--keep-generated");
    }
    // Keep stdout for the JSON report; PHP's output goes to stderr then.
    if json {
        cmd.stdout(std::io::stderr());
    } else {
        for d in done {
            println!("{d}");
        }
        eprintln!("{}", style::warn("setup:upgrade needs PHP:"));
        for r in needs_php {
            eprintln!("  {r}");
        }
    }
    let status = cmd.status().with_context(|| format!("running {} bin/magento setup:upgrade", opts.php))?;
    let ms = start.elapsed().as_millis();
    if json {
        let out = serde_json::json!({
            "php": true,
            "needs_php": needs_php,
            "done": done,
            "php_exit_code": status.code(),
            "elapsed_ms": ms,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        eprintln!("{}", style::dim(&format!("bin/magento setup:upgrade took {ms} ms")));
    }
    Ok(match status.code() {
        Some(0) => ExitCode::SUCCESS,
        Some(c) => ExitCode::from(u8::try_from(c).unwrap_or(1)),
        None => ExitCode::FAILURE,
    })
}
