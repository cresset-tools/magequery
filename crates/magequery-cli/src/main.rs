mod style;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use magequery_core::model::ModuleSource;
use magequery_core::{
    AclResource, ArgValue, Area, ChainStep, ClassName, ConfigSet, ConfigSourceKind, ConfigValue,
    DbColumn, DbTable, Error, EventName, Magento, MethodChain, Observer, Plugin, Preference,
    Resolution, Route, Source, WebapiRoute,
};

// The top-level command list, grouped, for `print_help`. Kept in sync with the `Command`
// enum and CLAUDE.md's command surface. We render the root help ourselves (rather than via a
// clap `help_template`) because clap can't head-group subcommands without nesting them — which
// would break flat `magequery <command>` invocation — and a plain template string can't carry
// the semantic colors. Rendering through `style` gives both grouping *and* color (and stays
// plain when piped / `--color never`, since `style` decides once at startup).
const HELP_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "Wiring",
        &[
            ("di", "Full DI resolution: concrete type, args, virtual type, plugins"),
            ("preference", "Concrete class for an interface/class"),
            ("plugins", "Plugin (interceptor) chain for a class, incl. inherited"),
            ("events", "Events and their observers"),
        ],
    ),
    (
        "Entry points",
        &[
            ("routes", "Frontend/adminhtml routes (frontName → modules)"),
            ("actions", "Controller actions: URL → action class"),
            ("webapi", "REST endpoints from webapi.xml"),
            ("cron", "Cron jobs, optionally by group"),
        ],
    ),
    ("Data", &[("schema", "Tables from db_schema.xml: a list, or one table's DDL")]),
    (
        "Config & admin",
        &[
            ("config", "Resolve a config path/section with its source (static, +--db)"),
            ("system-config", "Where each admin config path lives (Stores → Configuration)"),
            ("acl", "Admin ACL resource tree from acl.xml; resolve a <resource> id"),
        ],
    ),
    (
        "Runtime",
        &[
            ("db", "DB connections from env.php; info / ping"),
            ("redis", "Redis/Valkey usage from env.php; info / ping"),
            ("session", "Session storage config from env.php"),
            ("cache", "Cache backends and type flags from env.php"),
            ("lock", "Locking backend from env.php"),
            ("queue", "Message-queue (AMQP) connections from env.php"),
            ("url-rewrites", "URL rewrites from the DB (request → target)"),
        ],
    ),
    ("Project", &[("modules", "Installed modules in load order")]),
];

/// Global options, for `print_help`: (flags, placeholder, description). Mirror the `Cli`
/// fields below plus clap's auto `--help`/`--version`.
const HELP_OPTIONS: &[(&str, &str, &str)] = &[
    ("-r, --root", "<ROOT>", "Path to the Magento root [default: .]"),
    ("    --color", "<COLOR>", "When to colorize output [default: auto] [auto, always, never]"),
    ("-h, --help", "", "Print help"),
    ("-V, --version", "", "Print version"),
];

// We render the *root* `--help` / no-args screen ourselves (`print_help`, intercepted in
// `main` before clap parses) to get grouped + colored output; subcommands keep clap's native
// per-command help. The `help` subcommand stays enabled: bare `magequery help` is intercepted
// to show our grouped screen, while `magequery help <command>` is clap's per-command help.
#[derive(Parser)]
#[command(name = "magequery", version, about = "Inspect a Magento 2 codebase from the command line")]
struct Cli {
    /// Path to the Magento root.
    #[arg(long, short = 'r', global = true, default_value = ".")]
    root: PathBuf,

    /// When to colorize output.
    #[arg(long, value_enum, global = true, default_value_t)]
    color: style::ColorChoice,

    #[command(subcommand)]
    command: Command,
}

/// Does this invocation want the *root* help screen? True for a bare `magequery`, or `-h`/
/// `--help` appearing before any subcommand. Scans only our fixed global flags; anything we
/// don't recognize (including unknown flags and the subcommand itself) is handed to clap.
/// MUST be updated if a new value-taking global flag is added.
fn wants_root_help(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-h" | "--help" => return true,
            "-V" | "--version" => return false,
            // Value-taking globals: skip the flag and its value token.
            "--root" | "-r" | "--color" => i += 2,
            // `--root=…` / `--color=…`: value is attached.
            _ if a.starts_with("--root=") || a.starts_with("--color=") => i += 1,
            // Bare `magequery help` → our grouped screen; `help <command>` → clap's
            // per-command help.
            "help" => return !args[i + 1..].iter().any(|t| !t.starts_with('-')),
            // First non-flag token is the subcommand; any unknown flag → let clap handle it.
            _ => return false,
        }
    }
    // Only global flags, no subcommand → show help.
    true
}

/// The `--color` choice from raw args (we render help before clap parses). Defaults to auto.
fn color_from_args(args: &[String]) -> style::ColorChoice {
    use clap::ValueEnum;
    let mut i = 0;
    while i < args.len() {
        let v = match args[i].as_str() {
            "--color" => args.get(i + 1).map(String::as_str),
            a => a.strip_prefix("--color="),
        };
        if let Some(v) = v {
            return style::ColorChoice::from_str(v, true).unwrap_or_default();
        }
        i += 1;
    }
    style::ColorChoice::default()
}

/// Render the grouped, colored top-level help (the root `--help` / no-args screen).
fn print_help() {
    let name_w = HELP_GROUPS
        .iter()
        .flat_map(|(_, cmds)| cmds.iter())
        .map(|(n, _)| n.len())
        .max()
        .unwrap_or(0);

    println!("Inspect a Magento 2 codebase from the command line\n");
    println!("{} magequery [OPTIONS] <COMMAND>\n", style::target("Usage:"));

    for (group, cmds) in HELP_GROUPS {
        println!("{}", style::target(&format!("{group}:")));
        for (name, desc) in *cmds {
            let pad = " ".repeat(name_w - name.len());
            println!("  {}{pad}  {desc}", style::name(name));
        }
        println!();
    }

    // Pad the *plain* "flags placeholder" column, then color the parts (escapes don't count
    // toward width).
    let left_w = HELP_OPTIONS
        .iter()
        .map(|(f, p, _)| f.len() + if p.is_empty() { 0 } else { p.len() + 1 })
        .max()
        .unwrap_or(0);
    println!("{}", style::target("Options:"));
    for (flags, placeholder, desc) in HELP_OPTIONS {
        let (colored, plain_len) = if placeholder.is_empty() {
            (style::name(flags), flags.len())
        } else {
            (format!("{} {}", style::name(flags), style::dim(placeholder)), flags.len() + 1 + placeholder.len())
        };
        let pad = " ".repeat(left_w - plain_len);
        println!("  {colored}{pad}  {desc}");
    }
}

// Ordered into the seven command groups of the LOCKED CLI organization (see CLAUDE.md
// "Command surface"). Declaration order = the order clap lists them under `--help`, and it
// mirrors the `COMMAND_GROUPS` legend below. New commands slot into a group, never append.
#[derive(Subcommand)]
enum Command {
    // ── Wiring (object manager): how a class is assembled ──
    /// Full DI resolution: concrete type, arguments, virtual type, plugins.
    Di(DiArgs),
    /// Concrete class for an interface/class (a view of di).
    Preference(PreferenceArgs),
    /// Plugin (interceptor) chain for a class, incl. inherited (a view of di).
    Plugins(PluginsArgs),
    /// Events and their observers.
    Events(EventsArgs),

    // ── Entry points: how execution starts ──
    /// Frontend/adminhtml routes (frontName → modules).
    Routes(RoutesArgs),
    /// Controller actions: URL → action class.
    Actions(ActionsArgs),
    /// REST endpoints from webapi.xml.
    Webapi(WebapiArgs),
    /// Cron jobs, optionally by group.
    Cron(CronArgs),

    // ── Data: persistence & model ──
    /// Tables from db_schema.xml: a list, or one table's DDL.
    Schema(SchemaArgs),

    // ── Config & admin: where settings & permissions live ──
    /// Resolve a config path/section with its source (static, +--db).
    Config(ConfigArgs),
    /// Where each admin config path lives (Stores → Configuration).
    SystemConfig(SystemConfigArgs),
    /// Admin ACL resource tree from acl.xml; resolve a <resource> id.
    Acl(AclArgs),

    // ── Runtime: env.php config & live connections ──
    /// DB connections from env.php; info / ping.
    Db(DbArgs),
    /// Redis/Valkey usage from env.php; info / ping.
    Redis(RedisArgs),
    /// Session storage config from env.php.
    Session(InfoArgs),
    /// Cache backends and type flags from env.php.
    Cache(InfoArgs),
    /// Locking backend from env.php.
    Lock(InfoArgs),
    /// Message-queue (AMQP) connections from env.php.
    Queue(InfoArgs),
    /// URL rewrites from the DB (request → target).
    UrlRewrites(UrlRewritesArgs),

    // ── Project: the codebase itself ──
    /// Installed modules in load order.
    Modules(ModulesArgs),
}

#[derive(clap::Args)]
struct SystemConfigArgs {
    /// Filter by a config-path or label substring (e.g. `web/secure` or `base url`).
    /// Omit to list every admin setting.
    filter: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct AclArgs {
    /// An exact resource id (`Magento_Sales::actions_view`) → its tree position + children;
    /// otherwise a substring matched against id or title. Omit to print the whole tree.
    resource: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct SchemaArgs {
    /// Exact table name → full definition; otherwise a name substring to list matching tables.
    /// Omit to list every table.
    table: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct UrlRewritesArgs {
    /// Filter by a substring of the request or target path (matched in SQL).
    path: Option<String>,
    /// Only rewrites for this store-view (code).
    #[arg(long)]
    store: Option<String>,
    /// Only show redirects (301/302), not internal rewrites.
    #[arg(long)]
    redirects: bool,
    /// Max rows to return (the table can be huge).
    #[arg(long, default_value_t = 200)]
    limit: usize,
    #[arg(long)]
    json: bool,
}

/// Args for the simple env.php deployment-info commands (session/cache/lock/queue).
#[derive(clap::Args)]
struct InfoArgs {
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct ConfigArgs {
    /// Config path (`web/secure/base_url`) or section prefix (`web/secure`). Omit to list
    /// every key.
    #[arg(default_value = "")]
    path: String,
    /// Resolve a single scope (`default`, `websites/<code>`, `stores/<code>`). Default:
    /// show every scope that sets the path.
    #[arg(long)]
    scope: Option<String>,
    /// Also read admin-set values from `core_config_data` (needs a reachable database).
    #[arg(long)]
    db: bool,
    /// Decrypt encrypted values (secrets) using the crypt key from env.php.
    #[arg(long)]
    decrypt: bool,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct RedisArgs {
    #[command(subcommand)]
    command: RedisCommand,
}

#[derive(Subcommand)]
enum RedisCommand {
    /// Show Redis/Valkey instances from env.php.
    Info {
        #[arg(long)]
        json: bool,
    },
    /// Ping every configured instance.
    Ping {
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Args)]
struct DbArgs {
    #[command(subcommand)]
    command: DbCommand,
}

#[derive(Subcommand)]
enum DbCommand {
    /// Show database connections from env.php.
    Info {
        #[arg(long)]
        json: bool,
    },
    /// Test a connection (default: the `default` connection).
    Ping {
        /// Connection name (default: `default`).
        connection: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Args)]
struct ActionsArgs {
    /// Filter by URL substring (e.g. `catalog`).
    url: Option<String>,
    /// Area: frontend (default) or adminhtml.
    #[arg(long)]
    area: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct EventsArgs {
    /// An event name to show observers for. Omit to list all events with counts.
    event: Option<String>,
    /// Area (default: global).
    #[arg(long)]
    area: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct CronArgs {
    /// Restrict to one cron group.
    group: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct RoutesArgs {
    /// Area (default: frontend + adminhtml).
    #[arg(long)]
    area: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct WebapiArgs {
    /// Filter by URL substring.
    url: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct DiArgs {
    /// The interface or class to resolve (leading backslash optional).
    class: String,
    /// Area to resolve in (default: global).
    #[arg(long)]
    area: Option<String>,
    /// Emit JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct PluginsArgs {
    /// The interface or class whose plugins to list (leading backslash optional).
    class: String,
    /// Area to resolve in (default: global).
    #[arg(long, conflicts_with = "all_areas")]
    area: Option<String>,
    /// Show every area (collapsing areas identical to global).
    #[arg(long = "all-areas")]
    all_areas: bool,
    /// Show the execution chain (before/around/target/around/after onion) per method
    /// instead of the flat plugin list.
    #[arg(long)]
    chain: bool,
    /// With --chain, restrict to a single intercepted method (implies --chain).
    #[arg(long)]
    method: Option<String>,
    /// Emit JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct PreferenceArgs {
    /// The interface or class to resolve (leading backslash optional).
    class: String,
    /// Resolve in a single area (e.g. adminhtml). Default: global + per-area differences.
    #[arg(long, conflicts_with = "all_areas")]
    area: Option<String>,
    /// Show every area explicitly instead of collapsing to differences.
    #[arg(long = "all-areas")]
    all_areas: bool,
    /// Emit JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct ModulesArgs {
    /// Lint mode: verify every module on disk is registered in config.php (and vice
    /// versa). Exits non-zero if not — e.g. when `setup:upgrade` hasn't been run.
    #[arg(long)]
    check: bool,
    /// Show only enabled modules.
    #[arg(long, conflicts_with = "disabled")]
    enabled: bool,
    /// Show only disabled modules.
    #[arg(long)]
    disabled: bool,
    /// Filter by where the module lives.
    #[arg(long, value_enum)]
    source: Option<SourceFilter>,
    /// Emit JSON instead of a table.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum SourceFilter {
    App,
    Vendor,
}

fn main() -> Result<()> {
    // Take over the root `--help` / no-args screen (grouped + colored, via `print_help`)
    // before clap can render its flat one; every `magequery <command> --help` still uses
    // clap's native per-command help.
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if wants_root_help(&raw) {
        style::init(color_from_args(&raw));
        print_help();
        return Ok(());
    }

    let cli = Cli::parse();
    style::init(cli.color);
    let mage = Magento::open(&cli.root)
        .with_context(|| format!("opening Magento installation at {}", cli.root.display()))?;

    let result = match cli.command {
        Command::Modules(args) => modules(&mage, &args),
        Command::Preference(args) => preference(&mage, &args, &cli.root),
        Command::Plugins(args) => plugins(&mage, &args, &cli.root),
        Command::Di(args) => di(&mage, &args, &cli.root),
        Command::Events(args) => events(&mage, &args, &cli.root),
        Command::Cron(args) => cron(&mage, &args, &cli.root),
        Command::Routes(args) => routes(&mage, &args, &cli.root),
        Command::Webapi(args) => webapi(&mage, &args, &cli.root),
        Command::Actions(args) => actions(&mage, &args, &cli.root),
        Command::Db(args) => db(&mage, &args),
        Command::Redis(args) => match args.command {
            RedisCommand::Info { json } => redis_info(&mage, json),
            RedisCommand::Ping { json } => redis_ping(&mage, json),
        },
        Command::Session(args) => session_info(&mage, args.json),
        Command::Cache(args) => cache_info(&mage, args.json),
        Command::Lock(args) => lock_info(&mage, args.json),
        Command::Queue(args) => queue_info(&mage, args.json),
        Command::SystemConfig(args) => system_config(&mage, &args, &cli.root),
        Command::Acl(args) => acl(&mage, &args, &cli.root),
        Command::Schema(args) => schema(&mage, &args, &cli.root),
        Command::UrlRewrites(args) => url_rewrites(&mage, &args),
        Command::Config(args) => config(&mage, &args, &cli.root),
    };

    // Diagnostics are non-fatal; surface them on stderr (so stdout stays pipeable) *after*
    // the command, so any lazily-built index (di.xml) has contributed its diagnostics too.
    let diags = mage.diagnostics();
    if !diags.is_empty() {
        eprintln!("note: {} diagnostic(s) while indexing", diags.len());
        for d in &diags {
            eprintln!("  {:?}: {}", d.severity, d.message);
        }
    }

    result
}

fn config(mage: &Magento, args: &ConfigArgs, root: &Path) -> Result<()> {
    let set = mage.config(args.db).map_err(|e| anyhow!(e))?;
    let dec = if args.decrypt { Some(mage.decryptor().map_err(|e| anyhow!(e))?) } else { None };
    let dec = dec.as_ref();

    // A single scope was requested.
    if let Some(scope) = args.scope.as_deref() {
        if let Some(v) = set.get(scope, &args.path) {
            if args.json {
                println!("{}", serde_json::to_string_pretty(v)?);
                return Ok(());
            }
            let inherited = if v.scope != scope {
                format!("  {}", style::dim("(inherited)"))
            } else {
                String::new()
            };
            println!("{} = {}", style::name(&v.path), show_val(&v.value, dec));
            println!("   {}  scope={}  {}{inherited}", source_tag(v.source), style::area(&v.scope), config_loc(v, root));
            return Ok(());
        }
        return config_section_or_unset(&set, scope, &args.path, args.json, root, dec);
    }

    // Default: show the value in every scope that sets it.
    let values = set.scopes_for(&args.path);
    if !values.is_empty() {
        if args.json {
            println!("{}", serde_json::to_string_pretty(&values)?);
            return Ok(());
        }
        let width = values.iter().map(|v| v.scope.len()).max().unwrap_or(0);
        println!("{}", style::name(&args.path));
        for v in &values {
            let scope = format!("{:<width$}", v.scope);
            println!("  {}  {}   {}  {}", style::area(&scope), show_val(&v.value, dec), source_tag(v.source), config_loc(v, root));
        }
        return Ok(());
    }

    // Not a leaf anywhere — treat as a section (default scope).
    config_section_or_unset(&set, "default", &args.path, args.json, root, dec)
}

fn config_section_or_unset(
    set: &ConfigSet,
    scope: &str,
    path: &str,
    json: bool,
    root: &Path,
    dec: Option<&magequery_core::Decryptor>,
) -> Result<()> {
    let section = set.section(scope, path);
    if json {
        println!("{}", serde_json::to_string_pretty(&section)?);
        return Ok(());
    }
    if section.is_empty() {
        println!("{} {}", style::name(path), style::dim("(not set)"));
        return Ok(());
    }
    let header = if path.is_empty() {
        style::dim(&format!("all config — {} values", section.len()))
    } else {
        format!("{}{}  {}", style::name(path), style::dim("/*"), style::dim(&format!("({} values)", section.len())))
    };
    println!("{header}");
    for v in &section {
        println!("  {} = {}   {}  {}", style::name(&v.path), show_val(&v.value, dec), source_tag(v.source), config_loc(v, root));
    }
    Ok(())
}

fn val(s: &str) -> String {
    if s.is_empty() {
        style::dim("(empty)")
    } else {
        s.to_string()
    }
}

/// Render a config value, decrypting it when a decryptor is given and the value is encrypted.
fn show_val(value: &str, dec: Option<&magequery_core::Decryptor>) -> String {
    if let Some(d) = dec {
        if let Some(plain) = d.decrypt(value) {
            return format!("{} {}", style::ok(&plain), style::dim("🔓"));
        }
        if magequery_core::Decryptor::is_encrypted(value) {
            // Couldn't decrypt: either the one cipher we don't support (Blowfish), or (more
            // often) a DB imported from another environment whose crypt key isn't in env.php.
            let note = match magequery_core::Decryptor::cipher_version(value) {
                Some(0) => "(encrypted — legacy Blowfish cipher unsupported)".to_string(),
                _ => "(encrypted — crypt key mismatch?)".to_string(),
            };
            return format!("{}  {}", val(value), style::err(&note));
        }
    }
    val(value)
}

fn source_tag(kind: ConfigSourceKind) -> String {
    let label = match kind {
        ConfigSourceKind::ConfigXml => "config.xml",
        ConfigSourceKind::ConfigPhp => "config.php",
        ConfigSourceKind::EnvPhp => "env.php",
        ConfigSourceKind::EnvVar => "env-var",
        ConfigSourceKind::Database => "db",
    };
    style::kind(&format!("[{label}]"))
}

fn config_loc(v: &ConfigValue, root: &Path) -> String {
    match v.source {
        ConfigSourceKind::EnvVar => style::path(&format!("# ${}", env_var_name(&v.scope, &v.path))),
        ConfigSourceKind::Database => style::path("# core_config_data"),
        _ => match &v.file {
            Some(f) => {
                let rel = f.strip_prefix(root).unwrap_or(f);
                let loc = if v.line == 0 {
                    format!("# {}", rel.display())
                } else {
                    format!("# {}:{}", rel.display(), v.line)
                };
                style::path(&loc)
            }
            None => String::new(),
        },
    }
}

/// Reconstruct the `CONFIG__…` env var name for a (scope, path).
fn env_var_name(scope: &str, path: &str) -> String {
    let scope_part = if scope == "default" {
        "DEFAULT".to_string()
    } else if let Some(code) = scope.strip_prefix("websites/") {
        format!("WEBSITES__{}", code.to_uppercase())
    } else if let Some(code) = scope.strip_prefix("stores/") {
        format!("STORES__{}", code.to_uppercase())
    } else {
        scope.to_uppercase()
    };
    format!("CONFIG__{scope_part}__{}", path.to_uppercase().replace('/', "__"))
}

fn redis_ping(mage: &Magento, json: bool) -> Result<()> {
    let pings = mage.ping_redis().map_err(|e| anyhow!(e))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&pings)?);
        if pings.iter().any(|p| !p.ok) {
            std::process::exit(1);
        }
        return Ok(());
    }
    if pings.is_empty() {
        println!("{}", style::dim("(no Redis/Valkey configured)"));
        return Ok(());
    }
    for p in &pings {
        let db = p.database.as_deref().map(|d| format!(" db{d}")).unwrap_or_default();
        if p.ok {
            println!(
                "{}  {}{} — {} ({}ms)",
                style::ok("OK"),
                style::area(&p.purpose),
                style::dim(&db),
                style::number(p.server_version.as_deref().unwrap_or("?")),
                p.elapsed_ms,
            );
        } else {
            println!(
                "{}  {}{} — {}",
                style::err("FAIL"),
                style::area(&p.purpose),
                style::dim(&db),
                p.error.as_deref().unwrap_or("unknown error"),
            );
        }
    }
    std::io::Write::flush(&mut std::io::stdout()).ok();
    if pings.iter().any(|p| !p.ok) {
        std::process::exit(1);
    }
    Ok(())
}

fn redis_info(mage: &Magento, json: bool) -> Result<()> {
    let cfg = mage.redis_config().map_err(|e| anyhow!(e))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&cfg)?);
        return Ok(());
    }
    if cfg.instances.is_empty() {
        println!("{}", style::dim("(no Redis/Valkey configured — cache & session use other backends)"));
        return Ok(());
    }
    for r in &cfg.instances {
        let label = match r.purpose.as_str() {
            "default" => "cache (default)",
            "page_cache" => "page cache",
            "session" => "session",
            other => other,
        };
        println!("[{}]", style::area(label));
        if r.host.starts_with('/') {
            println!("  socket    {}", style::path(&r.host));
        } else {
            println!("  host      {}:{}", style::class(&r.host), style::number(&r.port.unwrap_or(6379).to_string()));
        }
        println!("  database  {}", style::number(r.database.as_deref().unwrap_or("0")));
        let pw = if r.password.is_empty() { style::dim("(empty)") } else { r.password.clone() };
        println!("  password  {pw}");
        if let Some(b) = &r.backend {
            println!("  backend   {}", style::dim(b.trim_start_matches('\\')));
        }
    }
    Ok(())
}

fn session_info(mage: &Magento, json: bool) -> Result<()> {
    let cfg = mage.session_config().map_err(|e| anyhow!(e))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&cfg)?);
        return Ok(());
    }
    println!("handler   {}", style::ok(&cfg.handler));
    if let Some(loc) = &cfg.location {
        let label = if cfg.handler == "files" { "path" } else { "location" };
        // A filesystem path (file save_path or a Redis socket) vs a host:port endpoint.
        let styled = if loc.starts_with('/') { style::path(loc) } else { style::class(loc) };
        println!("{label:<10}{styled}");
    }
    if let Some(db) = &cfg.database {
        println!("database  {}", style::number(db));
    }
    Ok(())
}

fn cache_info(mage: &Magento, json: bool) -> Result<()> {
    let cfg = mage.cache_config().map_err(|e| anyhow!(e))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&cfg)?);
        return Ok(());
    }
    for f in &cfg.frontends {
        let label = if f.id == "page_cache" { "page cache" } else { &f.id };
        println!("[{}]", style::area(label));
        println!("  backend   {}", style::dim(f.backend.trim_start_matches('\\')));
        if let Some(loc) = &f.location {
            let styled = if loc.starts_with('/') { style::path(loc) } else { style::class(loc) };
            println!("  location  {styled}");
        }
        if let Some(db) = &f.database {
            println!("  database  {}", style::number(db));
        }
    }
    if !cfg.types.is_empty() {
        let on = cfg.types.iter().filter(|t| t.enabled).count();
        println!("\n{}", style::dim(&format!("cache types ({on}/{} enabled)", cfg.types.len())));
        for t in &cfg.types {
            let flag = if t.enabled { style::ok("on ") } else { style::err("off") };
            println!("  {flag}  {}", style::name(&t.name));
        }
    }
    Ok(())
}

fn lock_info(mage: &Magento, json: bool) -> Result<()> {
    let cfg = mage.lock_config().map_err(|e| anyhow!(e))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&cfg)?);
        return Ok(());
    }
    println!("provider  {}", style::ok(&cfg.provider));
    for (k, v) in &cfg.config {
        println!("{:<10}{}", k, style::class(v));
    }
    Ok(())
}

fn queue_info(mage: &Magento, json: bool) -> Result<()> {
    let cfg = mage.queue_config().map_err(|e| anyhow!(e))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&cfg)?);
        return Ok(());
    }
    if cfg.connections.is_empty() {
        println!("{}", style::dim("(no message-queue connections configured in env.php)"));
    }
    for c in &cfg.connections {
        println!("[{}]", style::area(&c.name));
        println!("  host         {}:{}", style::class(&c.host), style::number(&c.port.unwrap_or(5672).to_string()));
        println!("  user         {}", c.user);
        let pw = if c.password.is_empty() { style::dim("(empty)") } else { c.password.clone() };
        println!("  password     {pw}");
        if let Some(vh) = &c.virtualhost {
            println!("  virtualhost  {}", style::name(vh));
        }
    }
    if let Some(w) = &cfg.consumers_wait_for_messages {
        println!("\n{} {}", style::dim("consumers_wait_for_messages"), style::number(w));
    }
    Ok(())
}

fn system_config(mage: &Magento, args: &SystemConfigArgs, root: &Path) -> Result<()> {
    let fields = mage.system_config(args.filter.as_deref());
    if args.json {
        println!("{}", serde_json::to_string_pretty(&fields)?);
        return Ok(());
    }
    if fields.is_empty() {
        println!("{}", style::dim("(no admin setting matches)"));
        return Ok(());
    }

    // Greppable: `config/path   Tab > Section > Group > Field   [scopes]   # loc`. The
    // breadcrumb is exactly where to click in Stores > Configuration to find the setting.
    let width = fields.iter().map(|f| f.path.len()).max().unwrap_or(0);
    let sep = style::dim(" > ");
    for f in &fields {
        let pad = " ".repeat(width.saturating_sub(f.path.len()));
        let mut crumbs: Vec<String> = Vec::new();
        if let Some(tab) = &f.tab {
            crumbs.push(style::dim(tab));
        }
        if !f.section.is_empty() {
            crumbs.push(f.section.clone());
        }
        if !f.group.is_empty() {
            crumbs.push(f.group.clone());
        }
        // The field itself — the leaf. Hidden/config-only fields have no label; fall back to
        // the field id (the last path segment).
        let leaf = if f.label.is_empty() {
            f.path.rsplit('/').next().unwrap_or(&f.path)
        } else {
            &f.label
        };
        crumbs.push(style::target(leaf));
        let scopes = if f.scopes.is_empty() {
            String::new()
        } else {
            format!("  {}", style::dim(&format!("[{}]", f.scopes.join(", "))))
        };
        println!(
            "{}{pad}  {}{scopes}   {}",
            style::name(&f.path),
            crumbs.join(&sep),
            style::path(&short_loc(&f.source, root)),
        );
    }
    eprintln!("\n{} setting(s) {}", fields.len(), style::dim("· Stores → Configuration"));
    Ok(())
}

fn acl(mage: &Magento, args: &AclArgs, root: &Path) -> Result<()> {
    // An exact resource id → detail (tree position + children); otherwise a substring list.
    if let Some(id) = &args.resource {
        if let Some(res) = mage.acl_resource(id) {
            if args.json {
                println!("{}", serde_json::to_string_pretty(&res)?);
                return Ok(());
            }
            render_acl_detail(mage, &res, root);
            return Ok(());
        }
    }

    let list = mage.acl(args.resource.as_deref());
    if args.json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }
    if list.is_empty() {
        println!("{}", style::dim("(no ACL resource matches)"));
        return Ok(());
    }

    // No filter → render the permission tree (indent by depth). A substring filter → a flat,
    // aligned list (its ancestors may not be in the result, so indentation would mislead).
    let index: std::collections::HashMap<&str, &AclResource> =
        list.iter().map(|r| (r.id.as_str(), r)).collect();
    if args.resource.is_none() {
        for r in &list {
            let indent = "  ".repeat(acl_depth(r, &index));
            println!("{indent}{}", acl_line(r, root));
        }
    } else {
        let w = list.iter().map(|r| r.id.len()).max().unwrap_or(0);
        for r in &list {
            let pad = " ".repeat(w - r.id.len());
            println!("{}{pad}  {}", style::name(&r.id), acl_title_loc(r, root));
        }
    }
    eprintln!("\n{} resource(s)", list.len());
    Ok(())
}

/// A tree line: `id  Title  # loc` (id only — depth is shown by the caller's indentation).
fn acl_line(r: &AclResource, root: &Path) -> String {
    format!("{}  {}", style::name(&r.id), acl_title_loc(r, root))
}

/// The `Title [disabled]  # loc` tail shared by the tree and flat renderings.
fn acl_title_loc(r: &AclResource, root: &Path) -> String {
    let title = if r.title.is_empty() {
        style::dim("(anchor)")
    } else {
        style::target(&r.title)
    };
    let disabled = if r.disabled { format!("  {}", style::err("[disabled]")) } else { String::new() };
    format!("{title}{disabled}   {}", style::path(&short_loc(&r.source, root)))
}

/// Depth of `r` within the result set (number of ancestors present), for tree indentation.
fn acl_depth(r: &AclResource, index: &std::collections::HashMap<&str, &AclResource>) -> usize {
    let mut depth = 0;
    let mut cur = r.parent.as_deref();
    while let Some(pid) = cur {
        let Some(p) = index.get(pid) else { break };
        depth += 1;
        cur = p.parent.as_deref();
        if depth > 64 {
            break; // malformed-cycle guard
        }
    }
    depth
}

fn render_acl_detail(mage: &Magento, res: &AclResource, root: &Path) {
    println!("{}", acl_line(res, root));

    // Breadcrumb: where this resource sits in the admin permission tree.
    let ancestors = mage.acl_ancestors(&res.id);
    if !ancestors.is_empty() {
        let crumbs: Vec<String> = ancestors
            .iter()
            .map(|a| if a.title.is_empty() { a.id.clone() } else { a.title.clone() })
            .collect();
        println!("  {} {}", style::dim("path:"), crumbs.join(&style::dim(" → ")));
    }

    // Children = the sub-permissions this resource grants.
    let children = mage.acl_children(&res.id);
    if children.is_empty() {
        println!("  {}", style::dim("(leaf — grants no sub-resources)"));
    } else {
        println!("  {}", style::dim(&format!("grants ({}):", children.len())));
        let w = children.iter().map(|c| c.id.len()).max().unwrap_or(0);
        for c in &children {
            let pad = " ".repeat(w - c.id.len());
            println!("    {}{pad}  {}", style::name(&c.id), acl_title_loc(c, root));
        }
    }
}

fn schema(mage: &Magento, args: &SchemaArgs, root: &Path) -> Result<()> {
    // An exact table name shows the full definition; anything else is a substring filter.
    if let Some(name) = &args.table {
        if let Some(table) = mage.table(name) {
            if args.json {
                println!("{}", serde_json::to_string_pretty(&table)?);
                return Ok(());
            }
            render_table(&table, root);
            return Ok(());
        }
    }

    let tables = mage.schema(args.table.as_deref());
    if args.json {
        println!("{}", serde_json::to_string_pretty(&tables)?);
        return Ok(());
    }
    if tables.is_empty() {
        println!("{}", style::dim("(no table matches)"));
        return Ok(());
    }
    let width = tables.iter().map(|t| t.name.len()).max().unwrap_or(0);
    for t in &tables {
        let pad = " ".repeat(width.saturating_sub(t.name.len()));
        println!(
            "{}{pad}  {}  {}",
            style::class(&t.name),
            style::dim(&format!("{} cols", t.columns.len())),
            style::path(&short_loc(&t.source, root)),
        );
    }
    eprintln!("\n{} table(s)", tables.len());
    Ok(())
}

fn render_table(t: &DbTable, root: &Path) {
    let engine = t.engine.as_deref().map(|e| format!("  engine={e}")).unwrap_or_default();
    println!("{}{}  {}", style::class(&t.name), style::dim(&engine), style::path(&short_loc(&t.source, root)));
    if let Some(c) = &t.comment {
        println!("  {}", style::dim(c));
    }

    let table_module = t.source.module.as_str();
    let width = t.columns.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for c in &t.columns {
        let pad = " ".repeat(width.saturating_sub(c.name.len()));
        let null = if c.nullable { style::dim("NULL") } else { style::dim("NOT NULL") };
        let identity = if c.identity { format!("  {}", style::keyword("auto_increment")) } else { String::new() };
        let default = c.default.as_deref().map(|d| format!("  default {}", style::number(d))).unwrap_or_default();
        // Flag columns added by a *different* module than the one that declared the table —
        // the whole point of merging db_schema.xml across modules.
        let from = if c.source.module.as_str() != table_module {
            format!("  {}", style::module(&format!("← {}", c.source.module.as_str())))
        } else {
            String::new()
        };
        let comment = c.comment.as_deref().map(|c| format!("   {}", style::dim(&format!("# {c}")))).unwrap_or_default();
        println!("  {}{pad}  {}  {null}{identity}{default}{from}{comment}", style::name(&c.name), style::keyword(&col_type(c)));
    }

    if !t.constraints.is_empty() {
        println!("  {}", style::dim("constraints:"));
        for con in &t.constraints {
            let cols = con.columns.join(", ");
            let detail = if con.kind == "foreign" {
                let rt = con.reference_table.as_deref().unwrap_or("?");
                let rc = con.reference_column.as_deref().unwrap_or("?");
                let on_del = con.on_delete.as_deref().map(|d| format!(" ON DELETE {d}")).unwrap_or_default();
                format!("({cols}) → {}({rc}){on_del}", style::class(rt))
            } else {
                format!("({cols})")
            };
            println!("    {}  {}  {detail}", style::name(&con.id), style::kind(&con.kind));
        }
    }

    if !t.indexes.is_empty() {
        println!("  {}", style::dim("indexes:"));
        for idx in &t.indexes {
            println!("    {}  {}  ({})", style::name(&idx.id), style::dim(&idx.index_type), idx.columns.join(", "));
        }
    }
}

/// A SQL-ish type string: `varchar(32)`, `decimal(12,4)`, `int unsigned`.
fn col_type(c: &DbColumn) -> String {
    let mut s = c.col_type.clone();
    if let Some(len) = &c.length {
        s.push_str(&format!("({len})"));
    } else if let (Some(p), Some(sc)) = (&c.precision, &c.scale) {
        s.push_str(&format!("({p},{sc})"));
    }
    if c.unsigned {
        s.push_str(" unsigned");
    }
    s
}

fn url_rewrites(mage: &Magento, args: &UrlRewritesArgs) -> Result<()> {
    let set = mage
        .url_rewrites(args.path.as_deref(), args.store.as_deref(), args.redirects, args.limit)
        .map_err(|e| anyhow!(e))?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&set)?);
        return Ok(());
    }
    if set.rewrites.is_empty() {
        println!("{}", style::dim("(no URL rewrites match)"));
        return Ok(());
    }

    let width = set.rewrites.iter().map(|r| r.request_path.len()).max().unwrap_or(0);
    for r in &set.rewrites {
        let pad = " ".repeat(width.saturating_sub(r.request_path.len()));
        // Internal rewrite vs an HTTP redirect (301/302).
        let arrow = if r.redirect_type == 0 {
            style::dim("→")
        } else {
            style::err(&format!("⇒{}", r.redirect_type))
        };
        let manual = if r.autogenerated { "" } else { " manual" };
        let meta = format!("# {}:{} · store={}{manual}", r.entity_type, r.entity_id, r.store);
        println!("{}{pad}  {arrow}  {}   {}", style::name(&r.request_path), r.target_path, style::dim(&meta));
    }

    let shown = set.rewrites.len();
    if set.truncated {
        eprintln!(
            "\n{}",
            style::dim(&format!("showing first {shown} (more exist — narrow with a filter or raise --limit)"))
        );
    } else {
        eprintln!("\n{shown} rewrite(s)");
    }
    Ok(())
}

fn db(mage: &Magento, args: &DbArgs) -> Result<()> {
    match &args.command {
        DbCommand::Info { json } => db_info(mage, *json),
        DbCommand::Ping { connection, json } => db_ping(mage, connection.as_deref(), *json),
    }
}

fn db_info(mage: &Magento, json: bool) -> Result<()> {
    let cfg = mage.db_config().map_err(|e| anyhow!(e))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&cfg)?);
        return Ok(());
    }

    let prefix = if cfg.table_prefix.is_empty() {
        style::dim("(none)")
    } else {
        style::string_lit(&format!("\"{}\"", cfg.table_prefix))
    };
    println!("table_prefix: {prefix}");
    for c in &cfg.connections {
        let inactive = if c.active { String::new() } else { format!("  {}", style::err("(inactive)")) };
        println!("\n[{}]{inactive}", style::area(&c.name));
        match &c.unix_socket {
            Some(sock) => println!("  socket    {}", style::path(sock)),
            None => {
                let port = c.port.unwrap_or(3306);
                println!("  host      {}:{}", style::class(&c.host), style::number(&port.to_string()));
            }
        }
        println!("  dbname    {}", style::class(&c.dbname));
        println!("  username  {}", c.username);
        let pw = if c.password.is_empty() { style::dim("(empty)") } else { c.password.clone() };
        println!("  password  {pw}");
        if let Some(m) = &c.model {
            println!("  model     {}", style::dim(m));
        }
    }
    Ok(())
}

fn db_ping(mage: &Magento, connection: Option<&str>, json: bool) -> Result<()> {
    let ping = mage.ping_db(connection).map_err(|e| anyhow!(e))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&ping)?);
        if !ping.ok {
            std::process::exit(1);
        }
        return Ok(());
    }

    if ping.ok {
        println!(
            "{}  {} — server {} ({}ms)",
            style::ok("OK"),
            style::area(&ping.connection),
            style::number(ping.server_version.as_deref().unwrap_or("?")),
            ping.elapsed_ms,
        );
    } else {
        println!(
            "{}  {} — {}",
            style::err("FAIL"),
            style::area(&ping.connection),
            ping.error.as_deref().unwrap_or("unknown error"),
        );
        std::io::Write::flush(&mut std::io::stdout()).ok();
        std::process::exit(1);
    }
    Ok(())
}

fn actions(mage: &Magento, args: &ActionsArgs, root: &Path) -> Result<()> {
    let area = match &args.area {
        Some(a) => a.parse::<Area>().map_err(|e| anyhow!("{e}"))?,
        None => Area::Frontend,
    };
    let actions = mage.actions(area, args.url.as_deref());

    if args.json {
        println!("{}", serde_json::to_string_pretty(&actions)?);
        return Ok(());
    }

    // Greppable: one line per action — `url  class  file`.
    let width = actions.iter().map(|a| a.url.len()).max().unwrap_or(0);
    for a in &actions {
        let pad = " ".repeat(width.saturating_sub(a.url.len()));
        println!(
            "{}{pad}  {}  {}",
            a.url,
            style::class(a.class.as_str()),
            style::path(&short_loc(&a.source, root)),
        );
    }
    eprintln!("\n{} action(s)", actions.len());
    Ok(())
}

fn events(mage: &Magento, args: &EventsArgs, root: &Path) -> Result<()> {
    let area = match &args.area {
        Some(a) => a.parse::<Area>().map_err(|e| anyhow!("{e}"))?,
        None => Area::Global,
    };

    if let Some(ev) = &args.event {
        let event = EventName::new(ev.as_str());
        let observers: Vec<Observer> = mage.observers(&event, area);
        if args.json {
            println!("{}", serde_json::to_string_pretty(&observers)?);
            return Ok(());
        }
        println!("{}  ({})", style::name(ev), style::area(&area_label(&[area])));
        if observers.is_empty() {
            println!("  (no observers)");
        }
        for o in &observers {
            let dis = if o.disabled { format!("  {}", style::err("[DISABLED]")) } else { String::new() };
            let sh = if !o.shared { format!("  {}", style::dim("(not shared)")) } else { String::new() };
            println!("  {}{dis}{sh}", style::name(&o.name));
            println!("    {}  {}", style::class(o.instance.as_str()), style::path(&short_loc(&o.source, root)));
        }
    } else {
        let events = mage.events(area);
        if args.json {
            let arr: Vec<_> =
                events.iter().map(|(e, c)| serde_json::json!({"event": e.as_str(), "observers": c})).collect();
            println!("{}", serde_json::to_string_pretty(&arr)?);
            return Ok(());
        }
        for (e, count) in &events {
            println!("{}  {}", style::dim(&format!("{count:>3}")), style::name(e.as_str()));
        }
        eprintln!("\n{} event(s)", events.len());
    }
    Ok(())
}

fn cron(mage: &Magento, args: &CronArgs, root: &Path) -> Result<()> {
    let jobs = mage.cron_jobs(args.group.as_deref());
    if args.json {
        println!("{}", serde_json::to_string_pretty(&jobs)?);
        return Ok(());
    }
    let mut group = "";
    for j in &jobs {
        if j.group != group {
            println!("\n[{}]", style::area(&j.group));
            group = &j.group;
        }
        let when = j
            .schedule
            .as_deref()
            .map(|s| style::number(s))
            .or_else(|| j.config_path.as_deref().map(|c| style::dim(&format!("config:{c}"))))
            .unwrap_or_else(|| style::dim("(no schedule)"));
        println!(
            "  {}  {}::{}  {}  {}",
            style::name(&j.name),
            style::class(j.instance.as_str()),
            j.method,
            when,
            style::path(&short_loc(&j.source, root)),
        );
    }
    eprintln!("\n{} job(s)", jobs.len());
    Ok(())
}

fn routes(mage: &Magento, args: &RoutesArgs, root: &Path) -> Result<()> {
    let areas: Vec<Area> = match &args.area {
        Some(a) => vec![a.parse::<Area>().map_err(|e| anyhow!("{e}"))?],
        None => vec![Area::Frontend, Area::Adminhtml],
    };

    if args.json {
        let all: Vec<Route> = areas.iter().flat_map(|&a| mage.routes(a)).collect();
        println!("{}", serde_json::to_string_pretty(&all)?);
        return Ok(());
    }

    for &area in &areas {
        let routes = mage.routes(area);
        if routes.is_empty() {
            continue;
        }
        println!("[{}]", style::area(&area_label(&[area])));
        for r in &routes {
            let mods: Vec<String> = r.modules.iter().map(|m| style::module(m.as_str())).collect();
            println!(
                "  {}  {}  →  {}   {}",
                style::area(&r.front_name),
                style::dim(&format!("({}/{})", r.router, r.id)),
                mods.join(", "),
                style::path(&short_loc(&r.source, root)),
            );
        }
    }
    Ok(())
}

fn webapi(mage: &Magento, args: &WebapiArgs, root: &Path) -> Result<()> {
    let routes: Vec<WebapiRoute> = mage.webapi(args.url.as_deref());
    if args.json {
        println!("{}", serde_json::to_string_pretty(&routes)?);
        return Ok(());
    }
    for r in &routes {
        println!("  {:<6} {}", style::kind(&r.method), r.url);
        println!(
            "    {}::{}  {}  {}",
            style::class(r.service_class.as_str()),
            r.service_method,
            style::dim(&format!("[{}]", r.resources.join(", "))),
            style::path(&short_loc(&r.source, root)),
        );
    }
    eprintln!("\n{} endpoint(s)", routes.len());
    Ok(())
}

fn di(mage: &Magento, args: &DiArgs, root: &Path) -> Result<()> {
    let class = ClassName::new(args.class.trim_start_matches('\\'));
    let area = match &args.area {
        Some(a) => a.parse::<Area>().map_err(|e| anyhow!("{e}"))?,
        None => Area::Global,
    };
    let res = mage.resolve(&class, area).map_err(|e| match e {
        Error::ClassNotFound(c) => anyhow!(
            "class not found: {c}\n  No preference is declared for it and it has no PSR-4 \
             source file. Check the namespace and spelling."
        ),
        other => anyhow!(other),
    })?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&res)?);
        return Ok(());
    }
    render_resolution(&res, root);
    Ok(())
}

fn render_resolution(res: &Resolution, root: &Path) {
    println!(
        "{}  ({})",
        style::class(res.requested.as_str()),
        style::area(&area_label(&[res.area]))
    );
    let pref = res
        .preference_chain
        .last()
        .map(|s| style::path(&short_loc(&s.source, root)))
        .unwrap_or_else(|| style::dim("(no preference — concrete class)"));
    println!("  → {}   {}", style::class(res.concrete.as_str()), pref);
    if let Some(base) = &res.instantiates {
        println!(
            "    {} {}",
            style::dim("[virtual type] instantiates"),
            style::class(&format!("\\{}", base.as_str()))
        );
    }

    println!("\n{}", style::dim(&format!("arguments ({})", res.arguments.len())));
    for a in &res.arguments {
        match &a.value {
            // Expand array arguments one item per line, each with the module/file that
            // declared it — so e.g. routerList shows which module added each entry.
            ArgValue::Array(items) if !items.is_empty() => {
                println!("  {} = {}", a.name, style::dim("["));
                for i in items {
                    println!(
                        "    {} => {}   {}",
                        style::string_lit(&format!("'{}'", i.key)),
                        render_arg(&i.value),
                        style::path(&short_loc(&i.source, root)),
                    );
                }
                println!("  {}", style::dim("]"));
            }
            _ => println!(
                "  {} = {}   {}",
                a.name,
                render_arg(&a.value),
                style::path(&short_loc(&a.source, root))
            ),
        }
    }

    println!("\n{}", style::dim(&format!("plugins ({})  — run order", res.plugins.len())));
    for p in &res.plugins {
        let kinds: Vec<String> =
            p.methods.iter().map(|m| format!("{} {}", style::kind(&m.kind.to_string()), m.target)).collect();
        let dis = if p.disabled { format!("  {}", style::err("[DISABLED]")) } else { String::new() };
        println!(
            "  {} {}  [{}]{dis}",
            style::dim(&format!("{:>3}", p.sort_order)),
            style::name(&p.name),
            kinds.join(", ")
        );
    }

    if !res.contributing_ancestors.is_empty() {
        let list: Vec<String> =
            res.contributing_ancestors.iter().map(|c| style::class(c.as_str())).collect();
        println!("\n{} {}", style::dim("contributing types:"), list.join(", "));
    }
}

/// Render an argument value as a PHP-like, syntax-colored literal, inline (recursive for
/// nested arrays). Top-level array arguments are expanded with provenance by the caller.
fn render_arg(v: &ArgValue) -> String {
    match v {
        // Object reference — leading backslash like a FQCN, no quotes.
        ArgValue::Object(c) => style::class(&format!("\\{}", c.as_str())),
        ArgValue::Scalar { xsi_type, text } => render_scalar(xsi_type, text),
        ArgValue::Array(items) => {
            let inner: Vec<String> = items
                .iter()
                .map(|i| format!("{} => {}", style::string_lit(&format!("'{}'", i.key)), render_arg(&i.value)))
                .collect();
            format!("[{}]", inner.join(", "))
        }
        ArgValue::Null => style::keyword("null"),
    }
}

fn render_scalar(xsi_type: &str, text: &str) -> String {
    match xsi_type {
        "string" => style::string_lit(&format!("\"{text}\"")),
        "boolean" => style::keyword(text),
        "number" => style::number(text),
        // init_parameter/const reference a class constant (Class::CONST).
        "init_parameter" | "const" => style::class(text),
        // Unknown/other types: quote-less, dim type hint.
        "" => style::keyword("null"),
        other => format!("{} {text}", style::dim(other)),
    }
}

fn plugins(mage: &Magento, args: &PluginsArgs, root: &Path) -> Result<()> {
    let class = ClassName::new(args.class.trim_start_matches('\\'));

    // The area whose preference resolves the concrete (and validates a not-found class).
    // For --all-areas we merge across areas but resolve the concrete from global.
    let base_area = match &args.area {
        Some(a) => a.parse::<Area>().map_err(|e| anyhow!("{e}"))?,
        None => Area::Global,
    };
    let concrete = resolve_pref(mage, &class, base_area)?.concrete.to_string();
    let scope = if args.all_areas { "all areas".to_string() } else { base_area.to_string() };

    if args.chain || args.method.is_some() {
        let chains = if args.all_areas {
            mage.plugin_chains_all_areas(&class, args.method.as_deref())
        } else {
            mage.plugin_chains(&class, base_area, args.method.as_deref())
        }
        .map_err(|e| anyhow!(e))?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&chains)?);
            return Ok(());
        }
        return render_chains(&concrete, &chains);
    }

    let plugins: Vec<Plugin> = if args.all_areas {
        mage.plugins_all_areas(&class)
    } else {
        mage.plugins(&class, base_area)
    }
    .map_err(|e| anyhow!(e))?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&plugins)?);
        return Ok(());
    }

    println!("{concrete}  ({scope})");
    if plugins.is_empty() {
        println!("  (no plugins)");
    }
    for p in &plugins {
        print_plugin(p, &concrete, root);
    }
    Ok(())
}

/// The inline `area=` tag: `base` for global, else the area name(s), comma-joined.
fn area_label(areas: &[Area]) -> String {
    if areas == [Area::Global] {
        return "base".to_string();
    }
    areas
        .iter()
        .map(|a| if *a == Area::Global { "base".to_string() } else { a.to_string() })
        .collect::<Vec<_>>()
        .join(",")
}

/// Compact 2-line plugin rendering: header (sort order, name, intercepts, origin) then
/// class · area · source.
fn print_plugin(p: &Plugin, concrete: &str, root: &Path) {
    let intercepts = if p.methods.is_empty() {
        style::dim("?")
    } else {
        p.methods
            .iter()
            .map(|m| format!("{} {}", style::kind(&m.kind.to_string()), m.target))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let inherited = if p.declared_on.to_string() != concrete {
        format!("  {} {}", style::dim("←"), style::class(p.declared_on.as_str()))
    } else {
        String::new()
    };
    let disabled = if p.disabled { format!("  {}", style::err("[DISABLED]")) } else { String::new() };
    let so = style::dim(&format!("{:>3}", p.sort_order));
    println!("{so}  {}  [{intercepts}]{inherited}{disabled}", style::name(&p.name));
    println!(
        "     {}  {} {}  {}",
        style::class(p.class.as_str()),
        style::dim("· area="),
        style::area(&area_label(&p.areas)),
        style::path(&short_loc(&p.source, root)),
    );
}

/// `# file:line` with the Magento root stripped for brevity. The leading `#` makes the
/// location a trailing comment, so a rendered line can be copy-pasted without it breaking.
fn short_loc(s: &Source, root: &Path) -> String {
    let rel = s.file.strip_prefix(root).unwrap_or(&s.file);
    if s.line == 0 {
        format!("# {}", rel.display())
    } else {
        format!("# {}:{}", rel.display(), s.line)
    }
}

fn preference(mage: &Magento, args: &PreferenceArgs, root: &Path) -> Result<()> {
    let class = ClassName::new(args.class.trim_start_matches('\\'));

    // Decide which areas to report.
    let results: Vec<Preference> = if let Some(a) = &args.area {
        let area = a.parse::<Area>().map_err(|e| anyhow!("{e}"))?;
        vec![resolve_pref(mage, &class, area)?]
    } else if args.all_areas {
        Area::ALL
            .iter()
            .map(|&a| resolve_pref(mage, &class, a))
            .collect::<Result<_>>()?
    } else {
        // Collapsed: global, then only the areas whose concrete differs from global.
        let global = resolve_pref(mage, &class, Area::Global)?;
        let mut out = vec![global.clone()];
        for &a in Area::ALL.iter().filter(|&&a| a != Area::Global) {
            let p = resolve_pref(mage, &class, a)?;
            if p.concrete != global.concrete {
                out.push(p);
            }
        }
        out
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
        return Ok(());
    }

    for p in &results {
        let loc = p
            .chain
            .last()
            .map(|step| style::path(&short_loc(&step.source, root)))
            .unwrap_or_else(|| style::dim("(no preference — concrete class)"));
        let area = format!("{:<11}", p.area.to_string());
        println!("{} {}\n            {}", style::area(&area), style::class(p.concrete.as_str()), loc);
    }
    Ok(())
}

fn render_chains(concrete: &str, chains: &[MethodChain]) -> Result<()> {
    if chains.is_empty() {
        println!("{}\n  {}", style::class(concrete), style::dim("(no intercepted methods)"));
        return Ok(());
    }
    for chain in chains {
        println!("\n{}::{}()", style::class(concrete), style::target(&chain.method));
        let mut depth: usize = 0;
        for step in &chain.steps {
            // around-exit and the target sit one level deeper than the surrounding frame.
            let ind = |d: usize| "  ".repeat(d + 1);
            match step {
                ChainStep::Before(p) => {
                    println!("{}{} {}  [{}]", ind(depth), style::kind("before "), style::name(&p.name), plugin_loc(p));
                }
                ChainStep::AroundEnter(p) => {
                    println!("{}{} {}  [{}]", ind(depth), style::kind("around↘"), style::name(&p.name), plugin_loc(p));
                    depth += 1;
                }
                ChainStep::Target => {
                    println!("{}{}", ind(depth), style::target(&format!("▶ {concrete}::{}()", chain.method)));
                }
                ChainStep::AroundExit(p) => {
                    depth = depth.saturating_sub(1);
                    println!("{}{} {}", ind(depth), style::kind("around↖"), style::name(&p.name));
                }
                ChainStep::After(p) => {
                    println!("{}{} {}  [{}]", ind(depth), style::kind("after  "), style::name(&p.name), plugin_loc(p));
                }
            }
        }
    }
    Ok(())
}

fn plugin_loc(p: &magequery_core::ChainPluginRef) -> String {
    format!(
        "{}::{}  {}  {}",
        style::class(p.class.as_str()),
        p.plugin_method,
        style::dim(&format!("so={}", p.sort_order)),
        style::dim("area=") + &style::area(&area_label(&p.areas)),
    )
}

/// Resolve a preference, turning a not-found class into a clean message instead of a
/// generic error dump.
fn resolve_pref(mage: &Magento, class: &ClassName, area: Area) -> Result<Preference> {
    mage.preference(class, area).map_err(|e| match e {
        Error::ClassNotFound(c) => anyhow!(
            "class not found: {c}\n  No preference is declared for it and it has no PSR-4 \
             source file. Check the namespace and spelling (leading backslash is optional)."
        ),
        other => anyhow!(other),
    })
}

fn modules(mage: &Magento, args: &ModulesArgs) -> Result<()> {
    if args.check {
        return modules_check(mage, args.json);
    }

    let selected: Vec<_> = mage
        .modules()
        .iter()
        .filter(|m| !(args.enabled && !m.enabled))
        .filter(|m| !(args.disabled && m.enabled))
        .filter(|m| match args.source {
            Some(SourceFilter::App) => m.source == ModuleSource::App,
            Some(SourceFilter::Vendor) => m.source == ModuleSource::Vendor,
            None => true,
        })
        .collect();

    if args.json {
        println!("{}", serde_json::to_string_pretty(&selected)?);
        return Ok(());
    }

    let name_w = selected
        .iter()
        .map(|m| m.name.as_str().len())
        .max()
        .unwrap_or(0)
        .max(4);

    for m in &selected {
        let status = if m.enabled { style::ok("on ") } else { style::err("off") };
        let src = match m.source {
            ModuleSource::App => "app",
            ModuleSource::Vendor => "vendor",
            ModuleSource::Setup => "setup",
        };
        let name = m.name.as_str();
        let pad = " ".repeat(name_w.saturating_sub(name.len()));
        println!(
            "{status} {src:<7} {}{pad} {}",
            style::module(name),
            style::path(&m.path.display().to_string()),
        );
    }
    eprintln!("\n{} module(s)", selected.len());
    Ok(())
}

fn modules_check(mage: &Magento, json: bool) -> Result<()> {
    let check = mage.module_check();

    if json {
        println!("{}", serde_json::to_string_pretty(check)?);
    } else if check.is_clean() {
        println!(
            "OK: all {} modules on disk are registered in config.php",
            mage.modules().len()
        );
    } else {
        for m in &check.on_disk_not_in_config {
            println!(
                "unregistered  {} ({}) — on disk but not in config.php; run `bin/magento setup:upgrade`\n              {}",
                m.name,
                match m.source {
                    ModuleSource::App => "app",
                    ModuleSource::Vendor => "vendor",
                    ModuleSource::Setup => "setup",
                },
                m.path.display(),
            );
        }
        for name in &check.in_config_not_on_disk {
            println!("missing       {name} — listed in config.php but no module.xml found on disk");
        }
    }

    // Flush before exiting so piped output isn't lost, then signal lint failure.
    use std::io::Write;
    std::io::stdout().flush().ok();
    if !check.is_clean() {
        std::process::exit(1);
    }
    Ok(())
}
