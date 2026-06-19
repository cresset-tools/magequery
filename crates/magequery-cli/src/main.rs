mod style;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use magequery_core::model::ModuleSource;
use magequery_core::{
    ArgValue, Area, ChainStep, ClassName, Error, EventName, Magento, MethodChain, Observer,
    Plugin, Preference, Resolution, Route, Source, WebapiRoute,
};

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

#[derive(Subcommand)]
enum Command {
    /// List installed modules in config.php load order.
    Modules(ModulesArgs),
    /// Show the concrete class Magento instantiates for an interface/class.
    Preference(PreferenceArgs),
    /// Show the interceptor (plugin) chain for a class, including ancestor/interface plugins.
    Plugins(PluginsArgs),
    /// Full DI resolution: concrete type, arguments, virtual type, and plugins.
    Di(DiArgs),
    /// List events and their observers (or observers of one event).
    Events(EventsArgs),
    /// List cron jobs (optionally for one group).
    Cron(CronArgs),
    /// List frontend/adminhtml routes (frontName → modules).
    Routes(RoutesArgs),
    /// List REST endpoints from webapi.xml (optionally filtered by URL substring).
    Webapi(WebapiArgs),
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
    let cli = Cli::parse();
    style::init(cli.color);
    let mage = Magento::open(&cli.root)
        .with_context(|| format!("opening Magento installation at {}", cli.root.display()))?;

    // Diagnostics are non-fatal; surface them on stderr so stdout stays pipeable.
    let diags = mage.diagnostics();
    if !diags.is_empty() {
        eprintln!("note: {} diagnostic(s) while indexing", diags.len());
        for d in diags {
            eprintln!("  {:?}: {}", d.severity, d.message);
        }
    }

    match cli.command {
        Command::Modules(args) => modules(&mage, &args),
        Command::Preference(args) => preference(&mage, &args),
        Command::Plugins(args) => plugins(&mage, &args, &cli.root),
        Command::Di(args) => di(&mage, &args, &cli.root),
        Command::Events(args) => events(&mage, &args, &cli.root),
        Command::Cron(args) => cron(&mage, &args, &cli.root),
        Command::Routes(args) => routes(&mage, &args, &cli.root),
        Command::Webapi(args) => webapi(&mage, &args, &cli.root),
    }
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
        println!(
            "  {} = {}   {}",
            a.name,
            render_arg(&a.value),
            style::path(&short_loc(&a.source, root))
        );
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

/// Render an argument value as a PHP-like, syntax-colored literal (recursive for arrays).
fn render_arg(v: &ArgValue) -> String {
    match v {
        // Object reference — leading backslash like a FQCN, no quotes.
        ArgValue::Object(c) => style::class(&format!("\\{}", c.as_str())),
        ArgValue::Scalar { xsi_type, text } => render_scalar(xsi_type, text),
        ArgValue::Array(items) => {
            let inner: Vec<String> = items
                .iter()
                .map(|(k, val)| format!("{} => {}", style::string_lit(&format!("'{k}'")), render_arg(val)))
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

/// `file:line` with the Magento root stripped for brevity.
fn short_loc(s: &Source, root: &Path) -> String {
    let rel = s.file.strip_prefix(root).unwrap_or(&s.file);
    if s.line == 0 {
        rel.display().to_string()
    } else {
        format!("{}:{}", rel.display(), s.line)
    }
}

fn preference(mage: &Magento, args: &PreferenceArgs) -> Result<()> {
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
            .map(|step| style::path(&step.source.location()))
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
