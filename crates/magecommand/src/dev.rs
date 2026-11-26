//! `dev upgrade`: a development `bin/magento setup:upgrade` done natively. The
//! decisions live in magequery-core's `devupgrade`; this runs them in
//! `setup:upgrade`'s order. PHP runs only for what is PHP code: the recurring
//! scripts with no native port (piped to PHP, see `dev_recurring.php`), or the
//! whole `setup:upgrade` for a schema change, patch or config import. With no PHP
//! to run, it says what is left and exits 2.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use magequery_core::Magento;
use magequery_core::devupgrade::{ConfigPhpUpdate, DevUpgradeCheck, EnvPhpUpdate, RecurringNeed, empty_dir};

use crate::style;

/// What `Installer::cleanupGeneratedFiles` empties, unless `--keep-generated`.
const GENERATED: &[&str] = &["generated/code", "generated/metadata", "pub/static", "var/view_preprocessed"];

/// Runs the given modules' recurring scripts the way the Installer does.
const RECURRING_RUNNER: &str = include_str!("dev_recurring.php");

pub struct Options {
    pub keep_generated: bool,
    pub php_command: Option<String>,
    pub full: bool,
}

pub fn upgrade(root: Option<PathBuf>, json: bool, opts: &Options) -> Result<ExitCode> {
    let start = Instant::now();
    let root = root.unwrap_or_else(|| PathBuf::from("."));
    let mut mage = Magento::open(&root).map_err(|e| anyhow!(e))?;
    if mage.info().mode.as_deref() == Some("production") {
        bail!("MAGE_MODE is production: `magecommand dev upgrade` is for development; run bin/magento setup:upgrade");
    }
    let php = php_command(mage.root(), opts.php_command.as_deref())?;

    let mut needs_php: Vec<String> = Vec::new();
    let mut done: Vec<String> = Vec::new();
    if opts.full {
        needs_php.push("--full".into());
    }

    // `checkFilePermissionsForDbUpgrade`: Magento fails the upgrade on this, so
    // fail before touching anything.
    check_writable(mage.root())?;

    // 1. env.php: `DbInitStatementsCleanup`.
    match mage.cleaned_env_php().map_err(|e| anyhow!(e))? {
        EnvPhpUpdate::Unchanged => {}
        EnvPhpUpdate::Rewrite(contents) => {
            let path = mage.root().join("app/etc/env.php");
            std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
            done.push("env.php: removed the deprecated SET NAMES utf8".into());
            mage = Magento::open(&root).map_err(|e| anyhow!(e))?;
        }
        EnvPhpUpdate::CannotReproduce(why) => needs_php.push(why),
    }

    // 2. config.php, before anything reads the module list (as Installer does).
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

    // 3. Everything that would need PHP.
    let check = if needs_php.is_empty() {
        mage.dev_upgrade_check().map_err(|e| anyhow!(e))?
    } else {
        DevUpgradeCheck::default()
    };
    needs_php.extend(check.needs_php.iter().cloned());

    // 4. Caches; then the whole setup:upgrade when anything outside the
    //    recurring scripts needs PHP.
    if needs_php.is_empty() {
        match mage.clean_caches() {
            Ok(lines) => done.extend(lines),
            Err(e) => needs_php.push(e.to_string()),
        }
    }
    if !needs_php.is_empty() {
        needs_php.extend(check.recurring_php.iter().map(|n| format!("{}: {}", n.module, n.reason)));
        let report = Report { json, done: &done, skipped: &check.recurring_skipped, start };
        return whole_upgrade(mage.root(), php.as_deref(), opts, &needs_php, &report);
    }

    // 5. The native steps: generated files, the core recurring writes.
    if !opts.keep_generated {
        let mut n = 0;
        for dir in GENERATED {
            let path = mage.root().join(dir);
            n += empty_dir(&path).with_context(|| format!("emptying {}", path.display()))?;
        }
        done.push(format!("generated files: emptied {} ({n} entries)", GENERATED.join(", ")));
    }
    done.extend(mage.dev_upgrade_apply(&check).map_err(|e| anyhow!(e))?);

    // 6. The recurring scripts only PHP can run.
    let report = Report { json, done: &done, skipped: &check.recurring_skipped, start };
    if !check.recurring_php.is_empty() {
        return recurring_scripts(mage.root(), php.as_deref(), &check, &report);
    }
    let ms = start.elapsed().as_millis();
    if json {
        let out = serde_json::json!({
            "php": false,
            "php_work": "none",
            "done": done,
            "recurring_skipped": check.recurring_skipped,
            "elapsed_ms": ms,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        report.print_done();
        println!("{}", style::pass(&format!("Nothing to upgrade; done natively in {ms} ms.")));
    }
    Ok(ExitCode::SUCCESS)
}

struct Report<'a> {
    json: bool,
    done: &'a [String],
    skipped: &'a [String],
    start: Instant,
}

impl Report<'_> {
    fn print_done(&self) {
        for d in self.done {
            println!("{d}");
        }
        if !self.skipped.is_empty() {
            println!("{}", style::dim(&format!("recurring scripts with nothing to do: {}", self.skipped.join(", "))));
        }
    }
}

/// How to run PHP, when something has to: `--php-command`, else
/// `MAGECOMMAND_PHP_COMMAND`, else `bougie run -- php` in a bougie project,
/// else `php` from PATH. `None` when there is none.
fn php_command(root: &Path, explicit: Option<&str>) -> Result<Option<Vec<String>>> {
    let explicit = explicit
        .map(str::to_owned)
        .or_else(|| std::env::var("MAGECOMMAND_PHP_COMMAND").ok().filter(|v| !v.trim().is_empty()));
    if let Some(cmd) = explicit {
        let words: Vec<String> = cmd.split_whitespace().map(str::to_owned).collect();
        if words.is_empty() {
            bail!("--php-command is empty");
        }
        return Ok(Some(words));
    }
    if root.join("bougie.toml").is_file() && on_path("bougie") {
        return Ok(Some(["bougie", "run", "--", "php"].map(str::to_owned).to_vec()));
    }
    Ok(on_path("php").then(|| vec!["php".to_owned()]))
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Without PHP: say what is left, exit 2 (work to do).
fn no_php(report: &Report, work: &str, reasons: &[String], recurring: &[RecurringNeed]) -> Result<ExitCode> {
    let ms = report.start.elapsed().as_millis();
    if report.json {
        let out = serde_json::json!({
            "php": false,
            "php_work": work,
            "php_missing": true,
            "needs_php": reasons,
            "recurring_php": recurring,
            "done": report.done,
            "recurring_skipped": report.skipped,
            "elapsed_ms": ms,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        report.print_done();
        eprintln!("{}", style::warn(&format!("PHP is needed for {work}, and none was found:")));
        for r in reasons {
            eprintln!("  {r}");
        }
        eprintln!(
            "{}",
            style::dim(
                "Pass --php-command (or MAGECOMMAND_PHP_COMMAND), work in a bougie project, or put php on PATH; \
                 or run bin/magento setup:upgrade yourself."
            )
        );
    }
    Ok(ExitCode::from(2))
}

/// Run the given modules' recurring scripts in PHP (the rest is done).
fn recurring_scripts(root: &Path, php: Option<&[String]>, check: &DevUpgradeCheck, report: &Report) -> Result<ExitCode> {
    let reasons: Vec<String> = check.recurring_php.iter().map(|n| format!("{}: {}", n.module, n.reason)).collect();
    let Some(php) = php else {
        return no_php(report, "recurring setup scripts", &reasons, &check.recurring_php);
    };
    if !report.json {
        report.print_done();
        eprintln!("{}", style::warn("recurring setup scripts need PHP:"));
        for r in &reasons {
            eprintln!("  {r}");
        }
    }
    let modules = serde_json::to_string(&check.recurring_modules())?;
    let mut cmd = Command::new(&php[0]);
    cmd.args(&php[1..]).arg("--").arg(&modules).current_dir(root).stdin(Stdio::piped());
    // Keep stdout for the JSON report; PHP's output goes to stderr then.
    if report.json {
        cmd.stdout(std::io::stderr());
    }
    let mut child = cmd.spawn().with_context(|| format!("running {}", php.join(" ")))?;
    child
        .stdin
        .take()
        .context("PHP's stdin")?
        .write_all(RECURRING_RUNNER.as_bytes())
        .context("piping the recurring runner to PHP")?;
    let status = child.wait()?;
    finish(report, "recurring setup scripts", &reasons, Some(&check.recurring_php), status)
}

/// The whole `setup:upgrade`, terminal attached, after saying why.
fn whole_upgrade(
    root: &Path,
    php: Option<&[String]>,
    opts: &Options,
    reasons: &[String],
    report: &Report,
) -> Result<ExitCode> {
    let Some(php) = php else {
        return no_php(report, "setup:upgrade", reasons, &[]);
    };
    let mut cmd = Command::new(&php[0]);
    cmd.args(&php[1..]).arg("bin/magento").arg("setup:upgrade").current_dir(root);
    if opts.keep_generated {
        cmd.arg("--keep-generated");
    }
    if report.json {
        cmd.stdout(std::io::stderr());
    } else {
        report.print_done();
        eprintln!("{}", style::warn("setup:upgrade needs PHP:"));
        for r in reasons {
            eprintln!("  {r}");
        }
    }
    let status = cmd.status().with_context(|| format!("running {} bin/magento setup:upgrade", php.join(" ")))?;
    finish(report, "setup:upgrade", reasons, None, status)
}

fn finish(
    report: &Report,
    work: &str,
    reasons: &[String],
    recurring: Option<&[RecurringNeed]>,
    status: std::process::ExitStatus,
) -> Result<ExitCode> {
    let ms = report.start.elapsed().as_millis();
    if report.json {
        let out = serde_json::json!({
            "php": true,
            "php_work": work,
            "needs_php": reasons,
            "recurring_php": recurring.unwrap_or_default(),
            "done": report.done,
            "recurring_skipped": report.skipped,
            "php_exit_code": status.code(),
            "elapsed_ms": ms,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if status.success() {
        eprintln!("{}", style::pass(&format!("Done in {ms} ms ({work} in PHP).")));
    } else {
        eprintln!("{}", style::warn(&format!("{work} failed in PHP after {ms} ms")));
    }
    Ok(match status.code() {
        Some(0) => ExitCode::SUCCESS,
        Some(c) => ExitCode::from(u8::try_from(c).unwrap_or(1)),
        None => ExitCode::FAILURE,
    })
}

/// `FilePermissions::getMissingWritableDirectoriesForDbUpgrade`: app/etc and var
/// must be writable. Only when one is not does Magento look inside it, two levels
/// deep (var/session excluded, symlinks ignored), and only report it when something
/// there is not writable either.
fn check_writable(root: &Path) -> Result<()> {
    let mut missing = Vec::new();
    for dir in ["app/etc", "var"] {
        let path = root.join(dir);
        if is_writable_dir(&path) {
            continue;
        }
        if !children_writable(&path, &root.join("var/session"), 0) {
            missing.push(path.display().to_string());
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        bail!("Missing write permissions to the following paths:\n{}", missing.join("\n"))
    }
}

fn is_writable_dir(path: &Path) -> bool {
    path.is_dir() && access(path, libc_flags::R_OK) && access(path, libc_flags::W_OK)
}

fn children_writable(dir: &Path, excluded: &Path, depth: usize) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    let mut ok = true;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.starts_with(excluded) {
            continue;
        }
        let is_link = entry.file_type().is_ok_and(|t| t.is_symlink());
        if !is_link && !access(&path, libc_flags::W_OK) {
            ok = false;
        }
        if depth < 1 && !is_link && path.is_dir() && !children_writable(&path, excluded, depth + 1) {
            ok = false;
        }
    }
    ok
}

#[cfg(unix)]
mod libc_flags {
    pub const R_OK: i32 = libc::R_OK;
    pub const W_OK: i32 = libc::W_OK;
}
#[cfg(not(unix))]
mod libc_flags {
    pub const R_OK: i32 = 4;
    pub const W_OK: i32 = 2;
}

/// PHP's is_writable()/is_readable(): access(2).
#[cfg(unix)]
fn access(path: &Path, mode: i32) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
}

#[cfg(not(unix))]
fn access(path: &Path, mode: i32) -> bool {
    std::fs::metadata(path).is_ok_and(|m| mode != libc_flags::W_OK || !m.permissions().readonly())
}
