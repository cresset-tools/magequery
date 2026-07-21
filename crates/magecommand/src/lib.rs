//! magecommand — the write-side companion to magequery.
//!
//! magequery reads (nouns: inspect a Magento entity); magecommand writes (acts
//! on the codebase). The grammar is `magecommand <group> <subcommand>` (two
//! tokens; see `docs/command-surface.md`). Built today: the `di` group —
//! `di compile` (generate generated/code + metadata), `di verify` (the archive
//! oracle every milestone is accepted against), `di watch`, `di digest`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};

mod static_deploy;
mod watch;

#[derive(Parser)]
#[command(
    name = "magecommand",
    version,
    about = "Act on a Magento 2 codebase: fast DI compilation.",
    arg_required_else_help = true
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

/// Top-level groups. The grammar is `magecommand <group> <subcommand>` — two
/// tokens, mirroring magequery's nouns (magequery reads a noun, magecommand
/// writes it). Bare `magecommand` and bare `magecommand <group>` print help
/// (`arg_required_else_help`). Only the `di` group is built today; the planned
/// groups (`static`, `i18n`, `make`, `module`/`mode`/…, `product`/…) are
/// documented in `docs/command-surface.md` but not yet wired.
#[derive(Subcommand)]
enum Command {
    /// DI compilation — reproduce `setup:di:compile` (generated/code + metadata).
    #[command(arg_required_else_help = true)]
    Di {
        #[command(subcommand)]
        command: DiCommand,
    },
    /// Static content — reproduce `setup:static-content:deploy` artifacts.
    #[command(name = "static", arg_required_else_help = true)]
    Static {
        #[command(subcommand)]
        command: StaticCommand,
    },
}

/// `magecommand static <subcommand>` — the static-content group. Only the
/// LESS pipeline is built today; JS/requirejs come later.
#[derive(Subcommand)]
enum StaticCommand {
    /// Assemble and compile a theme's LESS entry points (pure Rust — no PHP,
    /// no node; theme fallback + `//@magento_import` + `Vendor_Module::`
    /// resolution handled by the orchestration layer).
    Less {
        /// Theme id, e.g. `Magento/luma` (or `frontend/Magento/luma`).
        #[arg(long, value_name = "VENDOR/NAME", required_unless_present = "file")]
        theme: Option<String>,
        /// Area of the theme (`frontend` or `adminhtml`).
        #[arg(long, default_value = "frontend",
              value_parser = ["frontend", "adminhtml"])]
        area: String,
        /// Locale for the `pub/static` placement.
        #[arg(long, default_value = "en_US")]
        locale: String,
        /// Entry point(s) to compile (`styles-m`, `styles`, or a logical
        /// path like `mage/gallery/gallery`). Default: every entry the theme
        /// chain itself provides (its top-level `web/css/*` files — blank's
        /// `styles-m`/`styles-l`/`print`/`email` set, backend's
        /// `styles`/`styles-old`).
        #[arg(long, value_name = "NAME")]
        entry: Vec<String>,
        /// Compile ONE materialized `.less` file instead of a theme's entry
        /// points — the per-file mode the Magento bridge adapter shells out
        /// to. Relative imports resolve from the file's directory (the
        /// `//@magento_import`/module-notation expansion is assumed already
        /// materialized, as in `var/view_preprocessed`). Prints the CSS to
        /// stdout unless `--out` is given.
        #[arg(long, value_name = "PATH",
              conflicts_with_all = ["theme", "area", "entry", "locale", "skip_broken_modules"])]
        file: Option<PathBuf>,
        /// Write compiled CSS under this directory instead of `pub/static`
        /// (with `--file`: as `<stem>.css`).
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Print the compiled CSS to stdout (no writes). Theme mode needs
        /// exactly one --entry; the default with `--file`.
        #[arg(long, conflicts_with = "out")]
        stdout: bool,
        /// Compress the output CSS (`Less_Parser` `compress=true` — what
        /// Magento's PHP adapter sets outside developer mode).
        #[arg(long)]
        compress: bool,
        /// Drop a broken module's LESS partial and re-splice instead of
        /// failing the entry point (default: fail loudly naming the module).
        #[arg(long)]
        skip_broken_modules: bool,
    },
    /// Emit a theme's requirejs deploy artifacts (pure Rust — no PHP, no
    /// node): `requirejs-config.js` (textual IIFE concatenation in collector
    /// order — lib → module contexts in load order → theme layers
    /// ancestor-first), `requirejs-min-resolver.js` (the fixed template with
    /// the `dev/js/minify_exclude` condition), and `mage/requirejs/mixins.js`
    /// (a verbatim copy of the lib source).
    Requirejs {
        /// Theme id, e.g. `Magento/luma` (or `frontend/Magento/luma`).
        #[arg(long, value_name = "VENDOR/NAME")]
        theme: String,
        /// Area of the theme (`frontend` or `adminhtml`).
        #[arg(long, default_value = "frontend",
              value_parser = ["frontend", "adminhtml"])]
        area: String,
        /// Locale for the `pub/static` placement.
        #[arg(long, default_value = "en_US")]
        locale: String,
        /// Write the artifacts under this directory instead of `pub/static`,
        /// at their deployed relative paths (`<DIR>/requirejs-config.js`,
        /// `<DIR>/requirejs-min-resolver.js`,
        /// `<DIR>/mage/requirejs/mixins.js`).
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Print ONLY the assembled `requirejs-config.js` to stdout (no
        /// writes; the two sibling artifacts are not emitted — backward
        /// compatible with the config-only behavior). Mutually exclusive
        /// with the global `--json` (which claims stdout for the
        /// source-list document).
        #[arg(long, conflicts_with = "out")]
        stdout: bool,
    },
    /// Generate a theme's `js/bundle/bundle<N>.js` files (SCD JS bundling,
    /// pure Rust): resolves the deployed package's js/html view from the
    /// SOURCE tree (theme fallback + module/i18n layers + the generated
    /// requirejs artifacts), applies the theme `view.xml` excludes and the
    /// `.min`-sibling rule, and splits into `bundle_size`-capped RequireJS
    /// config maps — byte-faithful to a real deploy (see `--order`).
    Bundle {
        /// Theme id, e.g. `Magento/luma` (or `frontend/Magento/luma`).
        /// Repeatable: themes are processed in the given order with the
        /// deploy's shared `.min`-sibling cache, exactly like one
        /// multi-theme `setup:static-content:deploy` run.
        #[arg(long, value_name = "VENDOR/NAME", required = true)]
        theme: Vec<String>,
        /// Area of the theme(s) (`frontend` or `adminhtml`).
        #[arg(long, default_value = "frontend",
              value_parser = ["frontend", "adminhtml"])]
        area: String,
        /// Locale of the package.
        #[arg(long, default_value = "en_US")]
        locale: String,
        /// Write bundles under this directory (as
        /// `<DIR>/<Vendor>/<name>/js/bundle/bundle<N>.js`) instead of
        /// `pub/static/<area>/<theme>/<locale>/js/bundle`.
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// In-bundle ordering: `probe` reproduces the output filesystem's
        /// readdir order (what PHP's `GLOB_NOSORT` glob over the deployed
        /// tree yields — byte-faithful on hash-ordered filesystems like
        /// ext4); `sorted` is portable lexicographic order.
        #[arg(long, value_name = "probe|sorted", default_value = "probe")]
        order: String,
        /// Scratch directory for `--order probe` (must be on the same
        /// filesystem the deploy would write to). Default: the output base
        /// directory.
        #[arg(long, value_name = "DIR")]
        probe_dir: Option<PathBuf>,
    },
    /// Deploy a theme's FULL static-file package (everything a real
    /// `setup:static-content:deploy` writes, quick strategy): the plain-copy
    /// engine over the source collectors (lib/module/theme layers + i18n
    /// overlays), LESS-compiled css (compressed — the default/production-mode
    /// output) with the VariableNotation/ModuleNotation css processors, the
    /// requirejs artifacts, `js-translation.json`, the js bundles and
    /// `sri-hashes.json` — byte-faithful to a real deploy.
    Files {
        /// Theme id, e.g. `Magento/luma` (or `frontend/Magento/luma`).
        /// Repeatable: themes are processed in the given order with the
        /// deploy's shared `.min`-sibling bundle cache, like one multi-theme
        /// `setup:static-content:deploy` run.
        #[arg(long, value_name = "VENDOR/NAME", required = true)]
        theme: Vec<String>,
        /// Area of the theme(s) (`frontend` or `adminhtml`).
        #[arg(long, default_value = "frontend",
              value_parser = ["frontend", "adminhtml"])]
        area: String,
        /// Locale of the package. (Dictionary limitation: only locales whose
        /// js dictionary is empty — no phrase translating differently, like
        /// en_US — produce a byte-faithful `js-translation.json`.)
        #[arg(long, default_value = "en_US")]
        locale: String,
        /// Write the packages under this static root (as
        /// `<DIR>/<area>/<Vendor>/<name>/<locale>/…`) instead of
        /// `pub/static`.
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Bundle ordering: `probe` reproduces the output filesystem's readdir
        /// order (byte-faithful on hash-ordered filesystems like ext4).
        /// `sorted` is a portable, filesystem-independent DETERMINISM aid, not
        /// a faithful reproduction: because `.min`-sibling exclusion is
        /// readdir-order-dependent, sorting changes which files are bundled
        /// (e.g. it wrongly bundles PageBuilder libs a real deploy excludes),
        /// so its output matches no real deploy — use it only to compare two
        /// magecommand runs. (Package deployment order itself follows the
        /// SOURCE tree's readdir order — no probe involved.)
        #[arg(long, value_name = "probe|sorted", default_value = "probe")]
        order: String,
        /// Scratch directory for `--order probe` (must be on the same
        /// filesystem the deploy would write to). Default: the output base
        /// directory.
        #[arg(long, value_name = "DIR")]
        probe_dir: Option<PathBuf>,
        /// Write `<static root>/deployed_version.txt` with this exact value
        /// (the deploy's `--content-version`). Omitted = no file: the real
        /// value is a per-run timestamp, and inventing one here would only
        /// masquerade as a run that never happened.
        #[arg(long, value_name = "VERSION")]
        deployed_version: Option<String>,
        /// Compile LESS uncompressed (developer-mode output) instead of the
        /// default/production-mode compressed css.
        #[arg(long)]
        no_compress: bool,
    },
    /// Reproduce `setup:static-content:deploy` over a MATRIX of themes ×
    /// locales × areas, fanning the independent package builds out across
    /// rayon. Positional locales are the default set; a `--theme id:loc,loc`
    /// overrides that theme's locales; a child `--theme` also deploys its
    /// parent (Magento's quick strategy; `--no-parent` opts out); no `--theme`
    /// deploys every registered theme; `--area` restricts areas (default:
    /// all). Everything a real quick-strategy deploy writes, byte-faithful
    /// (incl. a correct per-locale `js-translation.json`) plus one run-scoped
    /// `deployed_version.txt`. The `.min`-sibling bundle cache is scoped per
    /// `(area, locale)` group, so a multi-area/multi-locale run equals N
    /// separate `bin/magento` deploys — not one combined invocation (which
    /// shares a single cache across the whole run); see `--order`.
    Deploy {
        /// Default locale set (repeatable positional), e.g. `en_US fr_FR`.
        /// Empty is an error — pass at least one.
        #[arg(value_name = "LOCALE")]
        locales: Vec<String>,
        /// Theme to deploy, optionally with a per-theme locale override
        /// appended as `:loc1,loc2` (`Magento/luma:de_DE,fr_FR`). Repeatable.
        /// A bare id inherits the default locales. Omit ALL `--theme` flags to
        /// deploy every registered physical theme (Magento's default set).
        #[arg(long, value_name = "VENDOR/NAME[:loc,loc]")]
        theme: Vec<String>,
        /// Restrict to these areas (comma-separated `frontend,adminhtml`).
        /// A theme only deploys in the area it belongs to. Default: all.
        #[arg(long, value_name = "frontend,adminhtml", value_delimiter = ',')]
        area: Vec<String>,
        /// Write the packages under this static root instead of `pub/static`.
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Bundle ordering. `probe` = deployed-tree readdir order, byte-
        /// faithful on hash-ordered filesystems. `sorted` = portable
        /// lexicographic — a determinism aid only: it changes which files are
        /// bundled (`.min`-sibling exclusion is readdir-order-dependent), so it
        /// is faithful to no real deploy. Default: `probe`.
        #[arg(long, value_name = "probe|sorted", default_value = "probe")]
        order: String,
        /// Do NOT auto-deploy a `--theme`'s parent theme(s) (Magento's
        /// `--no-parent`). By default a child theme pulls in its ancestors, as
        /// a real quick-strategy deploy does.
        #[arg(long)]
        no_parent: bool,
        /// Scratch directory for `--order probe` (same filesystem as `--out`).
        #[arg(long, value_name = "DIR")]
        probe_dir: Option<PathBuf>,
        /// Write ONE `<static root>/deployed_version.txt` with this exact
        /// value (the deploy's `--content-version`). Omitted = no file: the
        /// real value is a per-run timestamp and inventing one would only
        /// masquerade as a run that never happened.
        #[arg(long, value_name = "VERSION")]
        deployed_version: Option<String>,
        /// Cap the rayon fan-out to N threads (`1` = forced serial, the
        /// determinism baseline). Default: the rayon default (num CPUs).
        #[arg(long, value_name = "N")]
        jobs: Option<usize>,
        /// Compile LESS uncompressed (developer-mode) instead of the default
        /// production-mode compressed css.
        #[arg(long)]
        no_compress: bool,
    },
    /// Minify ONE CSS or JS file (the `.min.*` building block of the future
    /// `static deploy`). Deliberately NOT byte-parity with Magento's
    /// cssmin/JShrink — semantic equivalence via lightningcss (CSS,
    /// serialization-only: no rule merging, no downleveling) and oxc (JS,
    /// ES5 output floor, AMD-safe). Errors exit non-zero on stderr.
    Minify {
        /// The CSS file to minify.
        #[arg(long, value_name = "FILE", required_unless_present = "js",
              conflicts_with = "js")]
        css: Option<PathBuf>,
        /// The JS file to minify.
        #[arg(long, value_name = "FILE")]
        js: Option<PathBuf>,
        /// Write the minified output to this exact path (default: the
        /// `.min.*` sibling of the input, Magento's `addMinifiedSign`).
        #[arg(long, value_name = "FILE", conflicts_with = "stdout")]
        out: Option<PathBuf>,
        /// Print ONLY the minified content to stdout (no writes).
        #[arg(long)]
        stdout: bool,
    },
    /// Semantic CSS diff (plan §7.7): compare a golden `.css` (real SCD
    /// output) against ours, normalizing only non-semantic formatting
    /// (whitespace, hex case/shorthand, leading zeros, comments) —
    /// order-preserving; every remaining difference is a real residual.
    Cssdiff {
        /// The golden CSS (the expected side, e.g. real SCD output).
        expected: PathBuf,
        /// Our CSS (the actual side).
        actual: PathBuf,
        /// Print at most N findings (0 = all).
        #[arg(long, default_value_t = 40, value_name = "N")]
        limit: usize,
    },
}

/// `magecommand di <subcommand>` — the DI-compile group. `compile` generates,
/// `verify` diffs against an archived ground truth, `watch` is the long-running
/// server, `digest` fingerprints the inputs.
#[derive(Subcommand)]
enum DiCommand {
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
        /// Emit fused interceptors (inlined plugin chains) instead of stock
        /// runtime-dispatch ones — creatuity's technique. Global-only classes
        /// only for now; multi-area classes still emit stock interceptors.
        #[arg(long)]
        fused: bool,
    },
    /// Long-running compile server: build once, then keep the parsed index in
    /// memory and recompile on file changes, writing only the delta. The fast
    /// edit loop (kills the re-scan and the full re-write on each change).
    Watch {
        /// Recompile once after the first change, then exit (for testing).
        #[arg(long)]
        once: bool,
    },
    /// Verify a generated tree against an archived ground truth
    /// (`generated/_code`, `generated/_metadata`).
    Verify {
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

/// The whole `magecommand` CLI. Lives in this lib crate; the actual `[[bin]]`
/// is a shim in the `magequery` package, so both binaries ship from the one
/// dist app (single `magequery-v<version>` release tag) without two workspace
/// packages racing to produce a `magecommand` artifact.
pub fn cli_main() -> anyhow::Result<ExitCode> {
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
        Command::Di { command } => match command {
            DiCommand::Compile { dry_run, force, incremental, fused } => {
                compile(cli.root, cli.json, dry_run, force, incremental, fused)
            }
            DiCommand::Digest { stat } => digest(cli.root, stat),
            DiCommand::Watch { once } => watch::watch(cli.root, cli.json, once),
            DiCommand::Verify {
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
        },
        Command::Static { command } => match command {
            StaticCommand::Less {
                ref theme,
                ref area,
                ref locale,
                ref entry,
                ref file,
                ref out,
                stdout,
                compress,
                skip_broken_modules,
            } => match file {
                Some(f) => static_less_file(f, out.as_deref(), compress),
                None => static_less(
                    cli.root,
                    theme.as_deref().expect("clap: --theme required without --file"),
                    area,
                    locale,
                    entry,
                    out.as_deref(),
                    stdout,
                    compress,
                    skip_broken_modules,
                ),
            },
            StaticCommand::Requirejs { ref theme, ref area, ref locale, ref out, stdout } => {
                static_requirejs(cli.root, theme, area, locale, out.as_deref(), stdout, cli.json)
            }
            StaticCommand::Bundle {
                ref theme,
                ref area,
                ref locale,
                ref out,
                ref order,
                ref probe_dir,
            } => static_bundle(
                cli.root,
                theme,
                area,
                locale,
                out.as_deref(),
                order,
                probe_dir.as_deref(),
                cli.json,
            ),
            StaticCommand::Files {
                ref theme,
                ref area,
                ref locale,
                ref out,
                ref order,
                ref probe_dir,
                ref deployed_version,
                no_compress,
            } => static_files(
                cli.root,
                theme,
                area,
                locale,
                out.as_deref(),
                order,
                probe_dir.as_deref(),
                deployed_version.as_deref(),
                no_compress,
                cli.json,
            ),
            StaticCommand::Deploy {
                ref locales,
                ref theme,
                ref area,
                ref out,
                ref order,
                no_parent,
                ref probe_dir,
                ref deployed_version,
                jobs,
                no_compress,
            } => static_deploy(
                cli.root,
                locales,
                theme,
                area,
                out.as_deref(),
                order,
                no_parent,
                probe_dir.as_deref(),
                deployed_version.as_deref(),
                jobs,
                no_compress,
                cli.json,
            ),
            StaticCommand::Minify { ref css, ref js, ref out, stdout } => {
                static_minify(css.as_deref(), js.as_deref(), out.as_deref(), stdout)
            }
            StaticCommand::Cssdiff { ref expected, ref actual, limit } => {
                static_cssdiff(expected, actual, limit, cli.json)
            }
        },
    }
}

/// `magecommand static less --file` — compile ONE materialized `.less` file
/// (the interface the Magento bridge adapter shells out to). Relative imports
/// resolve from the file's directory; output goes to `--out <DIR>/<stem>.css`
/// or stdout. A compile error exits non-zero with the compiler's rendering
/// (file, line, column, source excerpt) VERBATIM on stderr — the PHP adapter
/// shows that message as-is.
fn static_less_file(file: &Path, out: Option<&Path>, compress: bool) -> anyhow::Result<ExitCode> {
    use static_deploy::less as sdless;

    let compiled = match sdless::compile_file(file, compress) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return Ok(ExitCode::FAILURE);
        }
    };
    for w in &compiled.warnings {
        eprintln!("warning: {}: {w}", compiled.file.display());
    }
    match out {
        Some(dir) => {
            let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
            let target = dir.join(format!("{stem}.css"));
            std::fs::create_dir_all(dir)
                .with_context(|| format!("mkdir {}", dir.display()))?;
            std::fs::write(&target, &compiled.css)
                .with_context(|| format!("write {}", target.display()))?;
            println!(
                "{}: {} bytes -> {}",
                compiled.file.display(),
                compiled.css.len(),
                target.display()
            );
        }
        None => print!("{}", compiled.css),
    }
    Ok(ExitCode::SUCCESS)
}

/// `magecommand static minify` — minify one CSS or JS file. Bare content on
/// stdout with `--stdout`; otherwise written to `--out` or the input's
/// `.min.*` sibling ([`static_deploy::minify::min_path`]). CSS recovery
/// warnings go to stderr; any error exits non-zero.
fn static_minify(
    css: Option<&Path>,
    js: Option<&Path>,
    out: Option<&Path>,
    to_stdout: bool,
) -> anyhow::Result<ExitCode> {
    use static_deploy::minify as sdmin;

    let input = css.or(js).expect("clap: --css or --js required");
    let source = std::fs::read_to_string(input)
        .with_context(|| format!("read {}", input.display()))?;
    let minified = if css.is_some() {
        match sdmin::minify_css(&source) {
            Ok(m) => {
                for w in &m.warnings {
                    eprintln!("warning: {}: {w}", input.display());
                }
                m.css
            }
            Err(e) => {
                eprintln!("error: {}: {e}", input.display());
                return Ok(ExitCode::FAILURE);
            }
        }
    } else {
        match sdmin::minify_js(&source, &input.display().to_string()) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: {e}");
                return Ok(ExitCode::FAILURE);
            }
        }
    };

    if to_stdout {
        print!("{minified}");
        return Ok(ExitCode::SUCCESS);
    }
    let target = match out {
        Some(p) => p.to_path_buf(),
        None => {
            let t = sdmin::min_path(input);
            if t == input {
                anyhow::bail!(
                    "{} already carries the .min sign; pass --out or --stdout",
                    input.display()
                );
            }
            t
        }
    };
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
    }
    std::fs::write(&target, &minified)
        .with_context(|| format!("write {}", target.display()))?;
    println!(
        "{}: {} bytes -> {} bytes -> {}",
        input.display(),
        source.len(),
        minified.len(),
        target.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// `magecommand static less` — compile a theme's LESS entry points into
/// `pub/static/<area>/<theme>/<locale>/css/` (or `--out`/`--stdout`).
#[allow(clippy::too_many_arguments)]
fn static_less(
    root: Option<PathBuf>,
    theme: &str,
    area: &str,
    locale: &str,
    entries: &[String],
    out: Option<&Path>,
    to_stdout: bool,
    compress: bool,
    skip_broken_modules: bool,
) -> anyhow::Result<ExitCode> {
    use static_deploy::less as sdless;

    let root = root.unwrap_or_else(|| PathBuf::from("."));
    let root = std::path::absolute(&root).unwrap_or(root);
    let magento = magequery_core::Magento::open(&root)
        .with_context(|| format!("not a Magento root: {}", root.display()))?;
    let orch = sdless::LessOrchestrator::from_magento(&magento, area, theme)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    eprintln!(
        "theme fallback chain: {}",
        orch.chain()
            .iter()
            .map(|t| t.id.as_str())
            .collect::<Vec<_>>()
            .join(" -> ")
    );

    let opts = sdless::LessDeployOptions {
        skip_broken_modules,
        compress,
    };
    let names: Vec<String> = if entries.is_empty() {
        // Every entry the theme chain itself provides (its top-level
        // `web/css/*` files — blank's standard six + luma's `critical.css`,
        // backend's `styles`/`styles-old`). A chain with no discoverable
        // entries falls back to the standard frontend list, whose members
        // are each skipped-with-a-note when absent.
        let discovered = sdless::discover_entries(orch.chain());
        if discovered.is_empty() {
            sdless::ENTRY_POINTS
                .iter()
                .map(|n| n.to_string())
                .collect()
        } else {
            discovered
        }
    } else {
        entries.to_vec()
    };
    if to_stdout && names.len() != 1 {
        anyhow::bail!("--stdout needs exactly one --entry");
    }

    let mut failed = false;
    for name in &names {
        match orch.compile_entry(name, &opts) {
            Ok(compiled) => {
                for (module, err) in &compiled.skipped_modules {
                    eprintln!("warning: skipped broken module {module}: {err}");
                }
                for w in &compiled.warnings {
                    eprintln!("warning: {}: {w}", compiled.entry);
                }
                if to_stdout {
                    print!("{}", compiled.css);
                    continue;
                }
                let target = match out {
                    Some(dir) => dir.join(format!("{name}.css")),
                    None => {
                        let full = if theme.contains('/') && !theme.starts_with(area) {
                            format!("{area}/{theme}")
                        } else {
                            theme.to_string()
                        };
                        sdless::output_path(&root, area, &full, locale, name)
                    }
                };
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("mkdir {}", parent.display()))?;
                }
                std::fs::write(&target, &compiled.css)
                    .with_context(|| format!("write {}", target.display()))?;
                println!(
                    "{name}: {} ({} bytes) -> {}",
                    compiled.entry_file.display(),
                    compiled.css.len(),
                    target.display()
                );
            }
            Err(e) => {
                // A theme legitimately lacking an entry (no default requested
                // file) is a hard error only when explicitly requested.
                let missing = e.message.starts_with("entry point not found");
                if missing && entries.is_empty() {
                    eprintln!("{name}: skipped (not provided by the theme chain)");
                    continue;
                }
                eprintln!("error: {e}");
                failed = true;
            }
        }
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// `magecommand static requirejs` — emit a theme's requirejs deploy artifacts
/// into `pub/static/<area>/<theme>/<locale>/` (or `--out`): the assembled
/// `requirejs-config.js`, the generated `requirejs-min-resolver.js`, and the
/// verbatim `mage/requirejs/mixins.js` copy, each at its deployed relative
/// path. `--stdout` prints ONLY the config (no writes, config-only — the
/// original behavior). `--json` renders the ordered source list plus the
/// sibling artifacts instead of the summary lines — the "which module
/// contributed what, in what order" view.
fn static_requirejs(
    root: Option<PathBuf>,
    theme: &str,
    area: &str,
    locale: &str,
    out: Option<&Path>,
    to_stdout: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    use static_deploy::requirejs as sdrjs;

    let root = root.unwrap_or_else(|| PathBuf::from("."));
    let root = std::path::absolute(&root).unwrap_or(root);
    let magento = magequery_core::Magento::open(&root)
        .with_context(|| format!("not a Magento root: {}", root.display()))?;
    let cfg = match sdrjs::build_from_magento(&magento, area, theme) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(ExitCode::FAILURE);
        }
    };

    eprintln!(
        "theme fallback chain: {}",
        cfg.chain
            .iter()
            .map(|t| t.id.as_str())
            .collect::<Vec<_>>()
            .join(" -> ")
    );

    if to_stdout && !json {
        print!("{}", cfg.js);
        return Ok(ExitCode::SUCCESS);
    }

    // The two sibling artifacts (write modes and --json only; plain --stdout
    // above stays config-only). The excludes come from the static ConfigSet.
    let excludes = match sdrjs::min_resolver_excludes_from_magento(&magento) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(ExitCode::FAILURE);
        }
    };
    let resolver = sdrjs::min_resolver_code(&excludes);
    let mixins_src = sdrjs::mixins_source_path(&root);
    // Required to write; merely reported (null bytes) in `--json --stdout`.
    let mixins = std::fs::read(&mixins_src).ok();

    let targets = if to_stdout {
        None
    } else {
        let mixins = mixins
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("read {}", mixins_src.display()))?;
        let base = match out {
            Some(dir) => dir.to_path_buf(),
            None => {
                let full = if theme.contains('/') && !theme.starts_with(area) {
                    format!("{area}/{theme}")
                } else {
                    theme.to_string()
                };
                sdrjs::output_path(&root, area, &full, locale)
                    .parent()
                    .expect("output path has a parent")
                    .to_path_buf()
            }
        };
        let config_t = base.join(sdrjs::CONFIG_FILE_NAME);
        let resolver_t = base.join(sdrjs::MIN_RESOLVER_FILE_NAME);
        let mixins_t = base.join(sdrjs::MIXINS_FILE_NAME);
        for (t, bytes) in [
            (&config_t, cfg.js.as_bytes()),
            (&resolver_t, resolver.as_bytes()),
            (&mixins_t, mixins),
        ] {
            if let Some(parent) = t.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("mkdir {}", parent.display()))?;
            }
            std::fs::write(t, bytes)
                .with_context(|| format!("write {}", t.display()))?;
        }
        Some((config_t, resolver_t, mixins_t))
    };

    if json {
        let (config_t, resolver_t, mixins_t) = match &targets {
            Some((c, r, m)) => (Some(c), Some(r), Some(m)),
            None => (None, None, None),
        };
        let doc = serde_json::json!({
            "theme_chain": cfg.chain.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
            "sources": cfg.sources.iter().map(|s| serde_json::json!({
                "file":   s.file.display().to_string(),
                "module": s.module,
                "theme":  s.theme,
                "origin": s.origin.tag(),
            })).collect::<Vec<_>>(),
            "bytes":  cfg.js.len(),
            "output": config_t.map(|t| t.display().to_string()),
            "min_resolver": {
                "excludes": excludes,
                "bytes":    resolver.len(),
                "output":   resolver_t.map(|t| t.display().to_string()),
            },
            "mixins": {
                "source": mixins_src.display().to_string(),
                "bytes":  mixins.as_ref().map(|m| m.len()),
                "output": mixins_t.map(|t| t.display().to_string()),
            },
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(ExitCode::SUCCESS);
    }
    let (config_t, resolver_t, mixins_t) =
        targets.expect("non-stdout mode always writes");
    println!(
        "requirejs-config.js: {} source file(s) ({} bytes) -> {}",
        cfg.sources.len(),
        cfg.js.len(),
        config_t.display()
    );
    println!(
        "requirejs-min-resolver.js: {} exclude(s) ({} bytes) -> {}",
        excludes.len(),
        resolver.len(),
        resolver_t.display()
    );
    println!(
        "mage/requirejs/mixins.js: verbatim copy ({} bytes) -> {}",
        mixins.map(|m| m.len()).expect("write mode read the mixins source"),
        mixins_t.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// `magecommand static bundle` — generate `js/bundle/bundle<N>.js` per theme,
/// exactly as one multi-theme deploy run would (shared `.min`-sibling cache,
/// theme order as given). Existing bundle dirs are cleared first (the real
/// deploy's `clear()`).
#[allow(clippy::too_many_arguments)]
fn static_bundle(
    root: Option<PathBuf>,
    themes: &[String],
    area: &str,
    locale: &str,
    out: Option<&Path>,
    order: &str,
    probe_dir: Option<&Path>,
    json: bool,
) -> anyhow::Result<ExitCode> {
    use static_deploy::bundle as sdb;

    let root = root.unwrap_or_else(|| PathBuf::from("."));
    let root = std::path::absolute(&root).unwrap_or(root);
    let magento = magequery_core::Magento::open(&root)
        .with_context(|| format!("not a Magento root: {}", root.display()))?;

    // Per-theme target dirs, decided up front so the probe can default to
    // the output filesystem (order must match what a deploy THERE produces).
    let targets: Vec<PathBuf> = themes
        .iter()
        .map(|t| match out {
            Some(dir) => {
                let theme_path = t.strip_prefix(&format!("{area}/")).unwrap_or(t);
                dir.join(theme_path).join(sdb::BUNDLE_JS_DIR)
            }
            None => sdb::output_dir(&root, area, t, locale),
        })
        .collect();

    let order_mode = match order {
        "sorted" => sdb::OrderMode::Sorted,
        "probe" => {
            let scratch = match probe_dir {
                Some(d) => d.to_path_buf(),
                None => match out {
                    Some(dir) => dir.to_path_buf(),
                    None => root.join("pub").join("static"),
                },
            };
            std::fs::create_dir_all(&scratch)
                .with_context(|| format!("create probe scratch {}", scratch.display()))?;
            sdb::OrderMode::Probe(scratch)
        }
        other => anyhow::bail!("--order must be `probe` or `sorted`, got `{other}`"),
    };

    let bundles = match sdb::build_from_magento(&magento, area, themes, locale, &order_mode) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(ExitCode::FAILURE);
        }
    };

    for (tb, target) in bundles.iter().zip(&targets) {
        // The real deploy's clear(): delete the bundle dir before writing.
        if target.is_dir() {
            std::fs::remove_dir_all(target)
                .with_context(|| format!("clear {}", target.display()))?;
        }
        std::fs::create_dir_all(target)
            .with_context(|| format!("mkdir {}", target.display()))?;
        for f in &tb.files {
            std::fs::write(target.join(&f.name), f.content.as_bytes())
                .with_context(|| format!("write {}", target.join(&f.name).display()))?;
        }
    }

    if json {
        let doc = serde_json::json!({
            "themes": bundles.iter().zip(&targets).map(|(tb, target)| {
                serde_json::json!({
                    "theme": tb.theme,
                    "chain": tb.chain.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
                    "max_size_kb": tb.max_size_kb,
                    "tree_files": tb.tree_files,
                    "pooled": { "jsbuild": tb.pooled.0, "text": tb.pooled.1 },
                    "output": target.display().to_string(),
                    "bundles": tb.files.iter().map(|f| serde_json::json!({
                        "name": f.name,
                        "pool": f.pool,
                        "entries": f.keys.len(),
                        "bytes": f.content.len(),
                    })).collect::<Vec<_>>(),
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(ExitCode::SUCCESS);
    }

    for (tb, target) in bundles.iter().zip(&targets) {
        eprintln!(
            "theme fallback chain: {}",
            tb.chain
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>()
                .join(" -> ")
        );
        println!(
            "{}: {} js/html file(s) -> {} jsbuild + {} text -> {} bundle(s) (max {} KB) -> {}",
            tb.theme,
            tb.tree_files,
            tb.pooled.0,
            tb.pooled.1,
            tb.files.len(),
            tb.max_size_kb,
            target.display()
        );
        for f in &tb.files {
            println!(
                "  {}  {}  {} entr{}  {} bytes",
                f.name,
                f.pool,
                f.keys.len(),
                if f.keys.len() == 1 { "y" } else { "ies" },
                f.content.len()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `magecommand static files` — deploy the FULL static-file package per
/// theme, exactly as one multi-theme quick-strategy deploy run would (shared
/// `.min`-sibling bundle cache, theme order as given). Existing files are
/// overwritten in place; the bundle dirs are cleared (the real deploy's
/// `clear()`); nothing else is deleted.
#[allow(clippy::too_many_arguments)]
fn static_files(
    root: Option<PathBuf>,
    themes: &[String],
    area: &str,
    locale: &str,
    out: Option<&Path>,
    order: &str,
    probe_dir: Option<&Path>,
    deployed_version: Option<&str>,
    no_compress: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    use static_deploy::bundle as sdb;
    use static_deploy::files as sdf;

    let root = root.unwrap_or_else(|| PathBuf::from("."));
    let root = std::path::absolute(&root).unwrap_or(root);
    let magento = magequery_core::Magento::open(&root)
        .with_context(|| format!("not a Magento root: {}", root.display()))?;

    let static_root = match out {
        Some(dir) => dir.to_path_buf(),
        None => root.join("pub").join("static"),
    };

    let order_mode = match order {
        "sorted" => sdb::OrderMode::Sorted,
        "probe" => {
            let scratch = match probe_dir {
                Some(d) => d.to_path_buf(),
                None => static_root.clone(),
            };
            std::fs::create_dir_all(&scratch)
                .with_context(|| format!("create probe scratch {}", scratch.display()))?;
            sdb::OrderMode::Probe(scratch)
        }
        other => anyhow::bail!("--order must be `probe` or `sorted`, got `{other}`"),
    };

    let opts = sdf::PlacementOptions {
        compress: !no_compress,
        order: order_mode,
    };
    let packages = match sdf::build_from_magento(&magento, area, themes, locale, &opts) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(ExitCode::FAILURE);
        }
    };

    // deployed_version.txt is written FIRST on a real run — only with an
    // explicit version (never an invented timestamp).
    if let Some(version) = deployed_version {
        std::fs::create_dir_all(&static_root)
            .with_context(|| format!("mkdir {}", static_root.display()))?;
        let p = static_root.join(sdf::DEPLOYED_VERSION_FILE_NAME);
        std::fs::write(&p, version.as_bytes())
            .with_context(|| format!("write {}", p.display()))?;
    }

    for pkg in &packages {
        let target = sdf::package_dir(&static_root, area, &pkg.theme, locale);
        // The real deploy's bundle clear().
        let bundle_dir = target.join(sdb::BUNDLE_JS_DIR);
        if bundle_dir.is_dir() {
            std::fs::remove_dir_all(&bundle_dir)
                .with_context(|| format!("clear {}", bundle_dir.display()))?;
        }
        for f in &pkg.files {
            let path = target.join(&f.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("mkdir {}", parent.display()))?;
            }
            std::fs::write(&path, &f.content)
                .with_context(|| format!("write {}", path.display()))?;
        }
        for (logical, warning) in &pkg.warnings {
            eprintln!("warning: {logical}: {warning}");
        }
    }

    if json {
        use static_deploy::files::PlacedKind as K;
        let doc = serde_json::json!({
            "deployed_version": deployed_version,
            "themes": packages.iter().map(|pkg| {
                let target = sdf::package_dir(&static_root, area, &pkg.theme, locale);
                serde_json::json!({
                    "theme": pkg.theme,
                    "theme_path": pkg.theme_path,
                    "chain": pkg.chain.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
                    "output": target.display().to_string(),
                    "files": pkg.files.len(),
                    "bytes": pkg.bytes(),
                    "copied": pkg.count(K::Copy),
                    "css_processed": pkg.count(K::CssProcessed),
                    "less_compiled": pkg.count(K::LessCompiled),
                    "requirejs": pkg.count(K::RequireJs),
                    "bundles": pkg.count(K::Bundle),
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(ExitCode::SUCCESS);
    }

    for pkg in &packages {
        use static_deploy::files::PlacedKind as K;
        let target = sdf::package_dir(&static_root, area, &pkg.theme, locale);
        eprintln!(
            "theme fallback chain: {}",
            pkg.chain
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>()
                .join(" -> ")
        );
        println!(
            "{}: {} file(s) ({} copied, {} css-processed, {} less-compiled, \
             {} requirejs, {} bundle(s), js-translation + sri-hashes) \
             {:.1} MB -> {}",
            pkg.theme,
            pkg.files.len(),
            pkg.count(K::Copy),
            pkg.count(K::CssProcessed),
            pkg.count(K::LessCompiled),
            pkg.count(K::RequireJs),
            pkg.count(K::Bundle),
            pkg.bytes() as f64 / (1024.0 * 1024.0),
            target.display()
        );
    }
    if let Some(version) = deployed_version {
        println!(
            "deployed_version.txt: {version} -> {}",
            static_root.join(sdf::DEPLOYED_VERSION_FILE_NAME).display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `magecommand static deploy` — reproduce a whole `setup:static-content:deploy`
/// run over a per-theme locale × area matrix, fanning the independent package
/// builds out across rayon (grouped by `(area, locale)` so the shared
/// `.min`-sibling bundle-cache ordering is honored). One run-scoped
/// `deployed_version.txt`.
#[allow(clippy::too_many_arguments)]
fn static_deploy(
    root: Option<PathBuf>,
    locales: &[String],
    theme_args: &[String],
    areas: &[String],
    out: Option<&Path>,
    order: &str,
    no_parent: bool,
    probe_dir: Option<&Path>,
    deployed_version: Option<&str>,
    jobs: Option<usize>,
    no_compress: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    use static_deploy::bundle as sdb;
    use static_deploy::deploy as sdd;
    use static_deploy::files as sdf;

    if locales.is_empty() {
        anyhow::bail!("no locales given — pass at least one, e.g. `static deploy en_US`");
    }
    for a in areas {
        if a != "frontend" && a != "adminhtml" {
            anyhow::bail!("--area must be `frontend` and/or `adminhtml`, got `{a}`");
        }
    }

    let root = root.unwrap_or_else(|| PathBuf::from("."));
    let root = std::path::absolute(&root).unwrap_or(root);
    let magento = magequery_core::Magento::open(&root)
        .with_context(|| format!("not a Magento root: {}", root.display()))?;

    let static_root = match out {
        Some(dir) => dir.to_path_buf(),
        None => root.join("pub").join("static"),
    };

    // Parse the theme matrix: `id` or `id:loc1,loc2`.
    let theme_specs: Vec<sdd::ThemeSpec> = theme_args
        .iter()
        .map(|raw| match raw.split_once(':') {
            Some((id, locs)) => sdd::ThemeSpec {
                id: id.to_string(),
                locales: Some(
                    locs.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect(),
                ),
            },
            None => sdd::ThemeSpec { id: raw.to_string(), locales: None },
        })
        .collect();

    let inputs = sdf::DeployInputs::prepare(&magento).map_err(|e| anyhow::anyhow!("{e}"))?;
    let plan = match sdd::plan(&inputs, locales, &theme_specs, areas, no_parent) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(ExitCode::FAILURE);
        }
    };
    // A discovered theme with a dangling `<parent>` is dropped from the default
    // all-themes run (not fatal) — surface each so the exclusion is never
    // silent; name the theme with `--theme` to force it (and fail loud).
    for s in &plan.skipped {
        eprintln!("warning: skipping theme {} — {} (use --theme {} to force)", s.id, s.reason, s.id);
    }
    let groups = plan.groups;
    if groups.is_empty() {
        eprintln!("error: nothing to deploy (no theme matches the area filter)");
        return Ok(ExitCode::FAILURE);
    }

    let order_mode = match order {
        "sorted" => sdb::OrderMode::Sorted,
        "probe" => {
            let scratch = match probe_dir {
                Some(d) => d.to_path_buf(),
                None => static_root.clone(),
            };
            std::fs::create_dir_all(&scratch)
                .with_context(|| format!("create probe scratch {}", scratch.display()))?;
            sdb::OrderMode::Probe(scratch)
        }
        other => anyhow::bail!("--order must be `probe` or `sorted`, got `{other}`"),
    };
    let opts = sdf::PlacementOptions { compress: !no_compress, order: order_mode };

    // deployed_version.txt — ONE file at the static root, written first (only
    // with an explicit version, never an invented timestamp).
    if let Some(version) = deployed_version {
        std::fs::create_dir_all(&static_root)
            .with_context(|| format!("mkdir {}", static_root.display()))?;
        let p = static_root.join(sdf::DEPLOYED_VERSION_FILE_NAME);
        std::fs::write(&p, version.as_bytes())
            .with_context(|| format!("write {}", p.display()))?;
    }

    let started = std::time::Instant::now();
    let stats = match sdd::execute_to_disk(&inputs, &groups, &static_root, &opts, jobs) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(ExitCode::FAILURE);
        }
    };
    let elapsed = started.elapsed();

    if json {
        let doc = serde_json::json!({
            "deployed_version": deployed_version,
            "jobs": jobs,
            "elapsed_ms": elapsed.as_millis(),
            "groups": groups.iter().map(|g| serde_json::json!({
                "area": g.area,
                "locale": g.locale,
                "themes": g.theme_ids,
            })).collect::<Vec<_>>(),
            "skipped": plan.skipped.iter().map(|s| serde_json::json!({
                "theme": s.id,
                "reason": s.reason,
            })).collect::<Vec<_>>(),
            "packages": stats.iter().map(|s| serde_json::json!({
                "area": s.area,
                "theme": s.theme,
                "locale": s.locale,
                "output": s.output.display().to_string(),
                "files": s.files,
                "bytes": s.bytes,
                "copied": s.copied,
                "css_processed": s.css_processed,
                "less_compiled": s.less_compiled,
                "requirejs": s.requirejs,
                "bundles": s.bundles,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(ExitCode::SUCCESS);
    }

    let mut total_files = 0usize;
    let mut total_bytes = 0usize;
    for s in &stats {
        for (logical, warning) in &s.warnings {
            eprintln!("warning: {}/{} [{}]: {logical}: {warning}", s.area, s.theme, s.locale);
        }
        println!(
            "{}/{} [{}]: {} file(s) ({} copied, {} css-processed, {} less-compiled, \
             {} requirejs, {} bundle(s), js-translation + sri-hashes) {:.1} MB -> {}",
            s.area,
            s.theme,
            s.locale,
            s.files,
            s.copied,
            s.css_processed,
            s.less_compiled,
            s.requirejs,
            s.bundles,
            s.bytes as f64 / (1024.0 * 1024.0),
            s.output.display()
        );
        total_files += s.files;
        total_bytes += s.bytes;
    }
    if let Some(version) = deployed_version {
        println!(
            "deployed_version.txt: {version} -> {}",
            static_root.join(sdf::DEPLOYED_VERSION_FILE_NAME).display()
        );
    }
    println!(
        "total: {} package(s), {} file(s), {:.1} MB in {:.2}s ({})",
        stats.len(),
        total_files,
        total_bytes as f64 / (1024.0 * 1024.0),
        elapsed.as_secs_f64(),
        match jobs {
            Some(1) => "serial".to_string(),
            Some(n) if n > 1 => format!("{n} jobs"),
            _ => "rayon default".to_string(),
        }
    );
    Ok(ExitCode::SUCCESS)
}

/// `magecommand static cssdiff` — the §7.7 semantic differ. Exit 0 when the
/// two files are semantically identical, 1 when any residual remains.
fn static_cssdiff(
    expected: &Path,
    actual: &Path,
    limit: usize,
    json: bool,
) -> anyhow::Result<ExitCode> {
    use static_deploy::cssdiff;

    let exp = std::fs::read_to_string(expected)
        .with_context(|| format!("read {}", expected.display()))?;
    let act = std::fs::read_to_string(actual)
        .with_context(|| format!("read {}", actual.display()))?;
    let d = cssdiff::diff(&exp, &act);

    if json {
        let findings: Vec<String> = d.findings.iter().map(|f| f.to_string()).collect();
        let doc = serde_json::json!({
            "expected":       expected.display().to_string(),
            "actual":         actual.display().to_string(),
            "rules_expected": d.rules_expected,
            "rules_actual":   d.rules_actual,
            "findings":       findings,
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
    } else {
        let shown = if limit == 0 { d.findings.len() } else { limit.min(d.findings.len()) };
        for f in &d.findings[..shown] {
            println!("{f}");
        }
        if shown < d.findings.len() {
            println!("… and {} more finding(s)", d.findings.len() - shown);
        }
        println!(
            "{} rule(s) expected, {} actual — {} finding(s)",
            d.rules_expected,
            d.rules_actual,
            d.findings.len()
        );
    }
    Ok(if d.is_clean() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
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
    fused: bool,
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
    // The work-plan summary needs only declaration COUNTS, so it uses the cheap
    // `di_summary` (counts straight off the merged config) rather than seven full
    // `di_export`s (clone + multi-key sort of the whole config, ~40 ms wall here).
    // The real exports are materialized — and memoized — inside `compute_outputs`.
    let summaries: Vec<_> = AREAS.iter().map(|&a| (a, magento.di_summary(a))).collect();
    let extended_types = magento.extension_attributes(None).len();
    lap!("di_summary x7 (work plan)");

    if json {
        let plan = serde_json::json!({
            "root": root.display().to_string(),
            "modules": modules.len(),
            "enabled": enabled,
            "extension_attribute_types": extended_types,
            "areas": summaries.iter().map(|(area, s)| {
                serde_json::json!({
                    "area": area,
                    "preferences": s.preferences,
                    "virtual_types": s.virtual_types,
                    "plugin_declarations": s.plugin_declarations,
                    "plugged_targets": s.plugged_targets,
                    "argument_declarations": s.arguments,
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
        for (area, s) in &summaries {
            println!(
                "  {:<12} {:>4} preferences · {:>3} virtual types · {:>4} plugin decls on {:>3} targets · {:>4} argument decls",
                format!("{:?}", area).to_lowercase(),
                s.preferences,
                s.virtual_types,
                s.plugin_declarations,
                s.plugged_targets,
                s.arguments
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
        let out = magecommand_engine::build::compute_outputs_opts(&magento, &mut defs, &root, fused);
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

/// `magecommand di digest [--stat]` — print the compile-input digest and exit.
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

    // The disabled-module and obfuscation explanations need the checkout.
    // Best-effort: if the root isn't a Magento checkout (comparing two loose
    // trees), skip them — those differences simply stay unexplained rather
    // than misclassified.
    let magento: Option<magequery_core::Magento> = if no_explain {
        None
    } else {
        let root = std::path::absolute(root.unwrap_or_else(|| PathBuf::from("."))).unwrap_or_default();
        magequery_core::Magento::open(&root).ok()
    };
    let disabled_modules: std::collections::HashSet<String> = magento
        .as_ref()
        .map(|m| {
            m.modules()
                .iter()
                .filter(|md| !md.enabled)
                .map(|md| md.name.to_string())
                .collect()
        })
        .unwrap_or_default();

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

    let obfuscation_blocked = magecommand_engine::obfuscation_blocked_classes(
        &report,
        archive,
        output,
        magento.as_ref(),
    );
    let ctx = magecommand_engine::ClassifyCtx {
        archive,
        output,
        disabled_modules: &disabled_modules,
        obfuscation_blocked: &obfuscation_blocked,
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
