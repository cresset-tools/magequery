//! Data types for the wiring domain.

use super::*;

/// One hop in a preference chain. Preferences are followed to a fixpoint, so resolving an
/// interface may pass through several `for -> type` redirects before reaching a concrete.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PreferenceStep {
    pub from: ClassName,
    pub to: ClassName,
    pub source: Source,
}

/// The concrete type Magento instantiates for a requested name, in one area.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Preference {
    pub requested: ClassName,
    pub concrete: ClassName,
    pub chain: Vec<PreferenceStep>,
    pub area: Area,
}

/// The kind of interception a plugin method performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InterceptKind {
    Before,
    Around,
    After,
}

impl std::fmt::Display for InterceptKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            InterceptKind::Before => "before",
            InterceptKind::Around => "around",
            InterceptKind::After => "after",
        })
    }
}

/// A single interception a plugin performs: its method (e.g. `beforeSave`) and the target
/// method it intercepts on the plugged class (e.g. `save`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PluginMethod {
    pub kind: InterceptKind,
    /// The intercepted method on the target class (e.g. `save`, `getList`).
    pub target: String,
    /// The plugin's method name (e.g. `beforeSave`, `afterGetList`).
    pub plugin_method: String,
}

/// One `<plugin>` declaration *using* a class — the flip side of [`Plugin`]: given the
/// plugin's own class, the type it is attached to. `declared_on` is the type as written
/// in di.xml (often an interface or parent); resolve it with `preference` for the
/// concrete class that actually runs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PluginTarget {
    pub declared_on: ClassName,
    pub plugin_name: String,
    pub disabled: bool,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Plugin {
    /// The plugin's declaration name; identity for merge/override across modules.
    pub name: String,
    pub class: ClassName,
    pub sort_order: i32,
    /// The interceptions this plugin performs (which target methods, and how). Empty if the
    /// plugin class couldn't be read or defines no `before*/around*/after*` methods.
    pub methods: Vec<PluginMethod>,
    /// The type the `<plugin>` was declared on. May be an interface or ancestor of the
    /// concrete class rather than the concrete class itself — that's the case people miss.
    pub declared_on: ClassName,
    pub disabled: bool,
    pub source: Source,
    /// The area(s) this plugin is declared in. A single area for a per-area query; the full
    /// set for an all-areas merge (e.g. `[Global]` = base, or `[WebapiRest, WebapiSoap]`).
    pub areas: Vec<Area>,
}

/// A plugin's participation in an execution chain — enough to render the step.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ChainPluginRef {
    pub name: String,
    pub class: ClassName,
    pub plugin_method: String,
    pub sort_order: i32,
    /// The type the plugin was declared on (interface/ancestor or the concrete class).
    pub declared_on: ClassName,
    pub source: Source,
    /// Area(s) this plugin is declared in (see [`Plugin::areas`]).
    pub areas: Vec<Area>,
}

/// One step in a method's interceptor execution, in run order. `AroundEnter`/`AroundExit`
/// bracket the inner steps (the onion); `Target` is the real method implementation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum ChainStep {
    Before(ChainPluginRef),
    AroundEnter(ChainPluginRef),
    Target,
    AroundExit(ChainPluginRef),
    After(ChainPluginRef),
}

/// The full execution chain for one intercepted method: before/around in, the target,
/// around out/after — i.e. the plugin onion.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MethodChain {
    /// The intercepted method name.
    pub method: String,
    pub steps: Vec<ChainStep>,
}

/// A merged constructor `<argument>`: its name, value, and where the winning declaration is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Argument {
    pub name: String,
    pub value: ArgValue,
    pub source: Source,
}

/// A di.xml argument value. Objects are the interesting case (what gets injected).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum ArgValue {
    /// `xsi:type="object"` — an injected class or virtual type.
    Object(ClassName),
    /// Scalar value (`string`/`boolean`/`number`/`init_parameter`/`const`/…): the xsi type
    /// and its text.
    Scalar { xsi_type: String, text: String },
    /// `xsi:type="array"` — ordered items, each with its own provenance.
    Array(Vec<ArgItem>),
    /// `xsi:type="null"` or an empty value.
    Null,
}

/// One `<item>` of an array argument, with the module/file/line that declared it — so a
/// merged array (e.g. `routerList`) records which module contributed each entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ArgItem {
    pub key: String,
    pub value: ArgValue,
    pub source: Source,
}

impl ArgValue {
    /// Merge a newer declaration over `self` the way Magento merges di.xml arguments:
    /// two arrays merge by item key (newer overrides same-key — taking the newer item's
    /// source — appends new keys, recursing into nested arrays); anything else is replaced.
    pub(crate) fn merged_with(&self, newer: &ArgValue) -> ArgValue {
        match (self, newer) {
            (ArgValue::Array(old), ArgValue::Array(new)) => {
                let mut items = old.clone();
                for ni in new {
                    match items.iter_mut().find(|i| i.key == ni.key) {
                        Some(ei) => {
                            ei.value = ei.value.merged_with(&ni.value);
                            ei.source = ni.source.clone();
                        }
                        None => items.push(ni.clone()),
                    }
                }
                ArgValue::Array(items)
            }
            _ => newer.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Observer {
    pub event: EventName,
    pub name: String,
    pub instance: ClassName,
    pub disabled: bool,
    /// `shared="false"` requests a fresh instance per dispatch.
    pub shared: bool,
    pub source: Source,
}

/// A cron job from `crontab.xml`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CronJob {
    pub group: String,
    pub name: String,
    pub instance: ClassName,
    pub method: String,
    /// A literal cron expression (`<schedule>`), if fixed.
    pub schedule: Option<String>,
    /// A config path the schedule is read from (`<config_path>`), if configurable.
    pub config_path: Option<String>,
    /// Live history from `cron_schedule` (`--db` only).
    pub live: Option<CronJobLive>,
    pub source: Source,
}

/// One job's live `cron_schedule` summary: the outcome of its most recent started run,
/// the next pending run, and status counts over the retained history window.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CronJobLive {
    /// Status of the most recently *started* row (`success`/`error`/`running`);
    /// `None` = the job never ran.
    pub last_status: Option<String>,
    /// When that run started (`executed_at`).
    pub last_run: Option<String>,
    /// Seconds since that run started, per the DB server's clock.
    pub last_run_secs: Option<i64>,
    /// How long it took (success runs).
    pub last_duration_secs: Option<i64>,
    /// The `messages` of the most recent error row, if any error is retained.
    pub last_error: Option<String>,
    /// The earliest pending `scheduled_at` — the next run.
    pub next_scheduled: Option<String>,
    pub pending: u32,
    pub running: u32,
    pub success: u32,
    pub error: u32,
    pub missed: u32,
}

/// One `cron_schedule` row of a job's history.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CronRun {
    pub status: String,
    pub scheduled_at: Option<String>,
    pub executed_at: Option<String>,
    pub finished_at: Option<String>,
    pub duration_secs: Option<i64>,
    pub messages: Option<String>,
}

/// The `cron` result: definitions (with live overlay via `--db`), plus job codes in
/// `cron_schedule` no crontab.xml defines — leftovers of removed modules.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CronJobs {
    pub jobs: Vec<CronJob>,
    /// Only populated with `--db` and no group filter (like `Patches::orphaned_applied`).
    pub orphaned_codes: Vec<String>,
}

/// A named di.xml declaration pointing at a class: a `<preference for=name type=X>` (the
/// name is the `for` type) or a `<virtualType name=name type=X>` (the name is the virtual
/// type built on X).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UseRef {
    pub name: ClassName,
    pub source: Source,
}

/// One place a class is wired into another type's constructor via di.xml `<arguments>`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InjectionSite {
    /// The type receiving the class.
    pub consumer: ClassName,
    /// True when `consumer` is a virtual type rather than a real class.
    pub consumer_is_virtual: bool,
    /// The constructor argument name.
    pub argument: String,
    /// Array-item keys inside the argument (e.g. `["blog", "class"]` for
    /// `routerList['blog']['class']`); empty when the class is the argument itself.
    pub item_path: Vec<String>,
    /// Exactly what di.xml declares: the class, its generated `\Proxy`, or its name as a
    /// string literal.
    pub declared: ClassName,
    /// The declaration is `xsi:type="string"` (a factory/pool-style class-name value), not
    /// an `object` injection.
    pub as_string: bool,
    pub source: Source,
}

/// Reverse DI — everything in the merged di.xml config that references a class: the
/// inverse of [`Resolution`]. "Who depends on / receives this class?"
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Uses {
    pub class: ClassName,
    /// Types whose `<preference>` resolves (directly) to the class.
    pub preferred_for: Vec<UseRef>,
    /// Virtual types built on the class.
    pub virtual_types: Vec<UseRef>,
    /// Constructor arguments (incl. nested array items) injecting the class.
    pub injections: Vec<InjectionSite>,
}

/// A console command registered on `CommandListInterface`'s `commands` array argument in
/// di.xml — what `bin/magento` picks up from modules. (Commands registered through the
/// bootstrap-time `cli_commands.php`/`CommandLocator` mechanism — a handful of framework
/// ones like `maintenance:*` — have no di.xml declaration and are not listed.)
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ConsoleCommand {
    /// The `bin/magento` command name (e.g. `indexer:reindex`), extracted from the command
    /// class (`setName`/`$defaultName`/`parent::__construct`, following `self::CONST` into
    /// ancestors). `None` when the class builds it dynamically.
    pub name: Option<String>,
    /// The command's description (`setDescription`), when stated literally.
    pub description: Option<String>,
    /// The di.xml array-item key the command is registered under (its merge identity).
    pub item_key: String,
    pub class: ClassName,
    pub source: Source,
}

/// A frontend/adminhtml route from `routes.xml` (frontName → modules handling it).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Route {
    pub area: Area,
    /// The router this route belongs to (`standard`, `admin`, …).
    pub router: String,
    pub id: String,
    pub front_name: String,
    /// Modules handling the route, in declaration order across modules.
    pub modules: Vec<ModuleName>,
    pub source: Source,
}

/// A controller action (a "subroute"): a concrete `Controller/.../Action.php` class reached
/// via a route's frontName, mapped to its URL path.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ControllerAction {
    /// `frontName/controller/action`, e.g. `catalog/product/view`.
    pub url: String,
    pub class: ClassName,
    /// `Frontend` or `Adminhtml`.
    pub area: Area,
    pub module: ModuleName,
    pub source: Source,
}
