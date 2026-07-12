//! The data magequery returns. Every type is owned (cloned out of the index, so callers
//! never thread the `Magento` handle's lifetime through their code) and, with the default
//! `serde` feature, serializes straight to `--json`.

use crate::ids::{Area, ClassName, EventName, ModuleName};
use crate::source::Source;

/// Where a module's code lives on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleSource {
    App,
    Vendor,
    Setup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Module {
    pub name: ModuleName,
    /// As recorded in `config.php` (`1` = enabled).
    pub enabled: bool,
    pub source: ModuleSource,
    /// Module root directory.
    pub path: std::path::PathBuf,
    /// `<sequence>` dependencies from `module.xml`; they influence merge/load order.
    pub sequence: Vec<ModuleName>,
    /// 0-based position in the authoritative `config.php` load order.
    pub load_order: usize,
}

/// A module present on disk (and registered via composer/`module.xml`) but absent from
/// `config.php` — almost always means `bin/magento setup:upgrade` was not run.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct UnregisteredModule {
    pub name: ModuleName,
    pub source: ModuleSource,
    pub path: std::path::PathBuf,
}

/// Result of the `modules --check` lint: the two ways a module set can be inconsistent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ModuleCheck {
    /// On disk but not listed in `config.php` (forgot `setup:upgrade`).
    pub on_disk_not_in_config: Vec<UnregisteredModule>,
    /// Listed in `config.php` but no `module.xml` found on disk (broken/removed module).
    pub in_config_not_on_disk: Vec<ModuleName>,
}

impl ModuleCheck {
    /// True when the module set is consistent (nothing to report).
    pub fn is_clean(&self) -> bool {
        self.on_disk_not_in_config.is_empty() && self.in_config_not_on_disk.is_empty()
    }
}

/// One cross-index reference to a class (`whatis`): the role it plays and where.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ClassRef {
    /// e.g. `@resolver of Query.products`, `consumer \`x\` of queue y`.
    pub role: String,
    pub source: Source,
}

/// Everything magequery knows about one class: identity, a compressed DI summary
/// (forward + reverse), and every configuration reference to it. The aggregate view —
/// `di`/`uses` remain the focused drill-downs.
#[derive(Debug, Clone)]
#[derive(serde::Serialize)]
pub struct Whatis {
    pub class: ClassName,
    /// The PSR-4/PSR-0 source file; `None` for virtual types and generated classes.
    pub file: Option<std::path::PathBuf>,
    /// `class` / `abstract class` / `interface`, from the header parse.
    pub kind: Option<String>,
    pub is_virtual_type: bool,
    /// The module whose directory contains the file.
    pub module: Option<ModuleName>,
    /// The composer package owning the file, and its version.
    pub package: Option<String>,
    pub package_version: Option<String>,
    /// Direct parents/interfaces (full ancestry via `di`).
    pub parents: Vec<ClassName>,
    pub interfaces: Vec<ClassName>,
    /// The concrete type a preference redirects this to (global area), when it differs.
    pub resolves_to: Option<ClassName>,
    /// For a virtual type: the real class it instantiates.
    pub instantiates: Option<ClassName>,
    /// Plugins that fire on it / configured constructor arguments (global area).
    pub plugin_count: usize,
    pub argument_count: usize,
    /// The full reverse-DI result (see [`Uses`]).
    pub uses: Uses,
    /// Events it observes, cron jobs it runs, webapi routes it serves.
    pub observes: Vec<Observer>,
    pub cron_jobs: Vec<CronJob>,
    pub webapi: Vec<WebapiRoute>,
    /// The registered console command, when it is one.
    pub command: Option<ConsoleCommand>,
    /// GraphQL fields/types it resolves.
    pub graphql: Vec<ClassRef>,
    /// Message-queue handlers/consumers it backs.
    pub mq: Vec<ClassRef>,
    /// Controller URLs it serves, when it's an action.
    pub action_urls: Vec<ControllerAction>,
}

/// A `doctor` lint identifier — what kind of problem a finding is.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[derive(serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoctorLint {
    ModuleMissingOnDisk,
    ModuleNotRegistered,
    SequenceCycle,
    PreferenceTargetMissing,
    PreferenceCycle,
    VirtualTypeBaseMissing,
    VirtualTypeCycle,
    PluginClassMissing,
    DiArgumentClassMissing,
    ObserverClassMissing,
    CronInstanceMissing,
    WebapiServiceMissing,
    AclResourceUnknown,
    CommandClassMissing,
    MqHandlerMissing,
    QueueNoConsumer,
    GraphqlResolverMissing,
    CommandUnregistered,
    ObserverUnregistered,
    PluginUnregistered,
}

/// One `doctor` finding. Errors are things that break at runtime (dangling references,
/// cycles, module-set drift); warnings are probably-forgotten wiring (unregistered
/// commands/observers/plugins, unconsumed queues) that may be intentional.
#[derive(Debug, Clone)]
#[derive(serde::Serialize)]
pub struct DoctorFinding {
    pub lint: DoctorLint,
    pub severity: crate::error::Severity,
    pub message: String,
    /// The declaring file, when the finding has one.
    pub source: Option<Source>,
}

/// The `doctor` result: findings sorted errors-first.
#[derive(Debug, Clone)]
#[derive(serde::Serialize)]
pub struct DoctorReport {
    pub findings: Vec<DoctorFinding>,
}

impl DoctorReport {
    pub fn errors(&self) -> usize {
        self.findings.iter().filter(|f| f.severity == crate::error::Severity::Error).count()
    }
    pub fn warnings(&self) -> usize {
        self.findings.len() - self.errors()
    }
}

/// The everyday facts about an installation, on one screen: what/where it is and how it's
/// deployed. Config values always *try* the database (base URLs usually live only there)
/// and fall back to the static sources; every env-derived field degrades to `None` on a
/// fresh checkout that has no `env.php` yet.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct InstanceInfo {
    /// Why the database couldn't contribute (`None` = DB values included).
    pub db_error: Option<String>,
    /// Magento version, from the product package in `installed.json`.
    pub version: Option<String>,
    /// The package the version came from (`magento/product-community-edition`,
    /// `mage-os/magento2-base`, …) — also tells the distribution apart.
    pub version_package: Option<String>,
    /// Deploy mode from `env.php` `MAGE_MODE`; absent = Magento's "default" mode.
    pub mode: Option<String>,
    /// `var/.maintenance.flag` exists.
    pub maintenance: bool,
    /// Exempt IPs from `var/.maintenance.ip`.
    pub maintenance_allowed_ips: Vec<String>,
    /// `web/unsecure/base_url` / `web/secure/base_url` at the `default` scope (static
    /// sources; `{{base_url}}` means auto-detect and is shown verbatim).
    pub base_url: Option<String>,
    pub base_url_secure: Option<String>,
    /// How many non-default scopes (websites/stores) override either base URL.
    pub base_url_overrides: usize,
    /// The admin path from `env.php` `backend/frontName`.
    pub admin_front_name: Option<String>,
    /// The admin URL: the (secure, else unsecure) base URL + frontName.
    pub admin_url: Option<String>,
    /// `catalog/search/engine` (opensearch, elasticsearch7, …).
    pub search_engine: Option<String>,
    /// The engine's configured endpoint (`catalog/search/<engine>_server_hostname[:port]`).
    pub search_host: Option<String>,
    /// Full-page cache application: `built-in`, `varnish`, or the raw value.
    pub fpc: Option<String>,
    /// The first message-queue connection as `name @ host:port` (usually amqp).
    pub queue_endpoint: Option<String>,
    /// `general/locale/code` / `currency/options/base` / `general/locale/timezone`.
    pub locale: Option<String>,
    pub currency: Option<String>,
    pub timezone: Option<String>,
    /// Seconds since the last successful cron job finished (DB clock). `None` with
    /// `db_error` unset = no successful runs recorded — cron is likely not set up.
    pub cron_last_success_ago: Option<i64>,
    /// The active frontend theme path (default scope), e.g. `Hyva/default` — from
    /// `design/theme/theme_id` (resolved via the `theme` table when it's a numeric id),
    /// falling back to the DI default (`Magento\Theme\Model\View\Design` `themes` arg).
    pub theme: Option<String>,
    /// The frontend stack, classified from the theme's ancestry (or, when the active theme
    /// is unknown, from installed packages): `Hyvä`, `Breeze`, `Luma`, `Blank`.
    pub frontend: Option<String>,
    /// The frontend package's version (Hyvä/Breeze; stock themes ship with Magento).
    pub frontend_version: Option<String>,
    /// The checkout stack, from installed packages: a known solution's label (Hyvä
    /// Checkout, Firecheckout, …) or the verbatim non-core package name containing
    /// "checkout". `None` = nothing beyond the stock (Luma) checkout found.
    pub checkout: Option<String>,
    pub checkout_version: Option<String>,
    /// The installed solution's own selection setting, when it has one (Hyvä Checkout's
    /// `hyva_themes_checkout/general/checkout`): `default` = the Magento/Luma original is
    /// still selected; anything else is the chosen checkout namespace, verbatim.
    pub checkout_selected: Option<String>,
    /// The default DB connection, credentials omitted: database name and endpoint
    /// (`host:port` or a socket path).
    pub db_name: Option<String>,
    pub db_endpoint: Option<String>,
    /// Table prefix, when one is configured (non-empty).
    pub table_prefix: Option<String>,
    /// Session storage (`env.php` `session`).
    pub session: Option<SessionConfig>,
    /// Cache frontends (`default`, `page_cache`) with their backends.
    pub cache_frontends: Vec<CacheFrontend>,
    pub cache_types_enabled: usize,
    pub cache_types_total: usize,
    /// The store hierarchy — websites → stores (groups) → store views — counted from the
    /// live DB when reachable, else from `config.php`'s `scopes` node (present when the
    /// config is dumped). The synthetic admin scopes (id 0) are excluded.
    pub websites: Option<usize>,
    pub store_groups: Option<usize>,
    pub store_views: Option<usize>,
    /// `env.php` `install/date`, verbatim.
    pub installed_at: Option<String>,
    /// Modules in `config.php` (enabled + disabled).
    pub modules_total: usize,
    pub modules_enabled: usize,
    /// Modules on disk but absent from `config.php` — Magento's `module:status` counts
    /// these as *disabled* (they aren't enabled), `setup:upgrade` registers them.
    pub modules_unregistered: usize,
    /// Vendor/app split over everything seen on disk (config.php + unregistered), so the
    /// two sum to enabled + disabled.
    pub modules_vendor: usize,
    pub modules_app: usize,
    /// Named composer packages in `installed.json`.
    pub packages_total: usize,
}

/// One edge of a module's dependency graph: the neighbouring module and how the
/// dependency is declared — `<sequence>` in module.xml (load order), the owning composer
/// package's `require`, or both.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct DepEdge {
    pub module: ModuleName,
    pub via_sequence: bool,
    pub via_composer: bool,
    /// False when the module is referenced but absent from `config.php` — common for a
    /// `<sequence>` entry naming an optional module that isn't installed.
    pub installed: bool,
    pub enabled: bool,
    /// The declaring file: the depending module's `etc/module.xml` or `composer.json`.
    pub source: Source,
}

/// A module's dependency graph, both directions, from the two static sources
/// (`<sequence>` + composer `require`). Composer edges carry composer's granularity: a
/// required package that bundles several modules yields an edge per module.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ModuleDeps {
    pub module: ModuleName,
    /// The composer package providing the module, when composer-managed.
    pub package: Option<String>,
    pub depends_on: Vec<DepEdge>,
    pub depended_on_by: Vec<DepEdge>,
    /// Composer requires that aren't Magento modules (framework, libraries, `php`/`ext-*`)
    /// — listed so nothing is silently dropped.
    pub other_requires: Vec<String>,
}

/// One hop in a preference chain. Preferences are followed to a fixpoint, so resolving an
/// interface may pass through several `for -> type` redirects before reaching a concrete.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct PreferenceStep {
    pub from: ClassName,
    pub to: ClassName,
    pub source: Source,
}

/// The concrete type Magento instantiates for a requested name, in one area.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Preference {
    pub requested: ClassName,
    pub concrete: ClassName,
    pub chain: Vec<PreferenceStep>,
    pub area: Area,
}

/// One `<preference for= type=>` in the merged config. A declaration, not a
/// resolution: no fixpoint is followed (that's [`Preference`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[non_exhaustive]
pub struct PreferenceDecl {
    pub for_type: ClassName,
    pub prefer: ClassName,
    pub source: Source,
}

/// One `<virtualType name= type=>` in the merged config.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[non_exhaustive]
pub struct VirtualTypeDecl {
    pub name: ClassName,
    pub base: ClassName,
    pub source: Source,
}

/// One `<plugin>` as declared on its target in the merged config. Raw: no
/// ancestor logic is applied — a consumer walking the class hierarchy (a DI
/// compiler) collects these per ancestor itself; [`Magento::plugins`] is the
/// resolved per-class view.
///
/// [`Magento::plugins`]: crate::Magento::plugins
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[non_exhaustive]
pub struct PluginDecl {
    /// The type the `<plugin>` element sits on (class, interface, or virtual type).
    pub target: ClassName,
    pub name: String,
    /// `None` when attribute-level merging never supplied a `type=` (broken config).
    pub class: Option<ClassName>,
    pub sort_order: i32,
    pub disabled: bool,
    pub source: Source,
}

/// One explicit `shared=` declaration on a `<type>`/`<virtualType>`. Absent
/// types default to shared in Magento; only written attributes are exported.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[non_exhaustive]
pub struct TypeSharedDecl {
    pub type_name: ClassName,
    pub shared: bool,
    pub source: Source,
}

/// One constructor `<argument>` on a type/virtualType in the merged config.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[non_exhaustive]
pub struct TypeArgDecl {
    /// The type (or virtual type) the `<arguments>` block sits on.
    pub type_name: ClassName,
    pub arg: String,
    pub value: ArgValue,
    pub source: Source,
}

/// The fully merged DI configuration of one area, exported wholesale — the
/// bulk primitive a DI compiler iterates, where the per-class queries
/// ([`Magento::preference`], [`Magento::plugins`]) answer one name at a time.
/// Deterministically sorted; every declaration carries provenance.
///
/// [`Magento::preference`]: crate::Magento::preference
/// [`Magento::plugins`]: crate::Magento::plugins
#[derive(Debug, Clone)]
#[derive(serde::Serialize)]
#[non_exhaustive]
pub struct DiExport {
    pub area: Area,
    /// Sorted by `for_type`.
    pub preferences: Vec<PreferenceDecl>,
    /// Sorted by `name`.
    pub virtual_types: Vec<VirtualTypeDecl>,
    /// Sorted by `target`, then Magento's execution order (`sort_order`
    /// ascending, ties by declaration order).
    pub plugins: Vec<PluginDecl>,
    /// Sorted by `type_name`, then `arg`.
    pub arguments: Vec<TypeArgDecl>,
    /// Explicit `shared=` declarations, sorted by `type_name`.
    pub shared: Vec<TypeSharedDecl>,
}

/// The kind of interception a plugin method performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct PluginMethod {
    pub kind: InterceptKind,
    /// The intercepted method on the target class (e.g. `save`, `getList`).
    pub target: String,
    /// The plugin's method name (e.g. `beforeSave`, `afterGetList`).
    pub plugin_method: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub enum ChainStep {
    Before(ChainPluginRef),
    AroundEnter(ChainPluginRef),
    Target,
    AroundExit(ChainPluginRef),
    After(ChainPluginRef),
}

/// The full execution chain for one intercepted method: before/around in, the target,
/// around out/after — i.e. the plugin onion.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct MethodChain {
    /// The intercepted method name.
    pub method: String,
    pub steps: Vec<ChainStep>,
}

/// A merged constructor `<argument>`: its name, value, and where the winning declaration is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Argument {
    pub name: String,
    pub value: ArgValue,
    pub source: Source,
}

/// An `xsi:type="object"` argument: the injected class plus the attributes
/// that shape DI compilation (`shared=` picks `_i_` vs `_ins_`; `sortOrder=`
/// drives ObjectManager's argument-merge ordering).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[non_exhaustive]
pub struct ObjectRef {
    pub class: ClassName,
    pub shared: Option<bool>,
    pub sort_order: Option<i32>,
}

/// A di.xml argument value. Objects are the interesting case (what gets injected).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub enum ArgValue {
    /// `xsi:type="object"` — an injected class or virtual type.
    Object(ObjectRef),
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ArgItem {
    pub key: String,
    pub value: ArgValue,
    /// `sortOrder=` XML attribute on the item (any xsi:type) — Magento's
    /// ArrayType interpreter stably sorts an array's items by it.
    pub sort_order: Option<i32>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CronJobs {
    pub jobs: Vec<CronJob>,
    /// Only populated with `--db` and no group filter (like `Patches::orphaned_applied`).
    pub orphaned_codes: Vec<String>,
}

/// A named di.xml declaration pointing at a class: a `<preference for=name type=X>` (the
/// name is the `for` type) or a `<virtualType name=name type=X>` (the name is the virtual
/// type built on X).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct UseRef {
    pub name: ClassName,
    pub source: Source,
}

/// One place a class is wired into another type's constructor via di.xml `<arguments>`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ControllerAction {
    /// `frontName/controller/action`, e.g. `catalog/product/view`.
    pub url: String,
    pub class: ClassName,
    /// `Frontend` or `Adminhtml`.
    pub area: Area,
    pub module: ModuleName,
    pub source: Source,
}

/// Where a resolved config value came from (its source layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSourceKind {
    /// Module `config.xml` `<default>` default.
    ConfigXml,
    /// `app/etc/config.php` `system` node (locked/dumped).
    ConfigPhp,
    /// `app/etc/env.php` `system` node.
    EnvPhp,
    /// A `CONFIG__*` environment variable.
    EnvVar,
    /// `core_config_data` (database).
    Database,
}

/// A resolved system-config value at a given scope, with its source.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ConfigValue {
    /// Config path, e.g. `web/secure/base_url`.
    pub path: String,
    /// Scope: `default`, `websites/<code>`, or `stores/<code>`.
    pub scope: String,
    pub value: String,
    pub source: ConfigSourceKind,
    /// File the value was declared in (`None` for env var / database).
    pub file: Option<std::path::PathBuf>,
    /// 1-based line, or `0` if unknown (config.php/env.php aren't line-tracked).
    pub line: u32,
}

/// A database connection from `env.php` (`db/connection/<name>`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct DbConnection {
    /// Connection name (`default`, `indexer`, split-db `checkout`/`sales`, …).
    pub name: String,
    pub host: String,
    pub port: Option<u16>,
    pub dbname: String,
    pub username: String,
    /// Raw password — callers should mask it for display unless explicitly revealing.
    pub password: String,
    /// `unix_socket` if the connection uses a socket instead of host/port.
    pub unix_socket: Option<String>,
    pub model: Option<String>,
    pub engine: Option<String>,
    pub active: bool,
}

/// The `db` section of `env.php`: the table prefix and all configured connections.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct DbConfig {
    pub table_prefix: String,
    pub connections: Vec<DbConnection>,
}

/// Result of testing a database connection.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct DbPing {
    pub connection: String,
    pub ok: bool,
    pub server_version: Option<String>,
    pub error: Option<String>,
    pub elapsed_ms: u128,
}

/// One Redis (or Valkey) instance Magento is configured to use.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct RedisInstance {
    /// What it backs: `default` (cache), `page_cache`, or `session`.
    pub purpose: String,
    /// Server — a hostname or a socket path (starts with `/`).
    pub host: String,
    pub port: Option<u16>,
    /// Redis database number.
    pub database: Option<String>,
    pub password: String,
    /// The cache backend class (for cache instances); `None` for session.
    pub backend: Option<String>,
}

/// All Redis usages found in `env.php` (cache / page cache / session).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct RedisConfig {
    pub instances: Vec<RedisInstance>,
}

/// Result of pinging one Redis instance.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct RedisPing {
    pub purpose: String,
    pub host: String,
    pub database: Option<String>,
    pub ok: bool,
    pub server_version: Option<String>,
    pub error: Option<String>,
    pub elapsed_ms: u128,
}

/// Session storage configuration (the `session` section of `env.php`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct SessionConfig {
    /// Save handler: `files`, `db`, or `redis`.
    pub handler: String,
    /// Where sessions live: the server/socket for `redis`, the save path for `files`
    /// (`None` ⇒ Magento's default `var/session`); `None` for `db`.
    pub location: Option<String>,
    /// Redis database number, when the handler is `redis`.
    pub database: Option<String>,
}

/// One configured cache frontend (`cache/frontend/<id>` in `env.php`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CacheFrontend {
    /// Frontend id: `default` (the general cache) or `page_cache` (full-page cache).
    pub id: String,
    /// Backend class, e.g. `\Magento\Framework\Cache\Backend\Redis` or a file backend.
    pub backend: String,
    /// Server/socket for a Redis backend (`host:port` or a socket path); `None` otherwise.
    pub location: Option<String>,
    pub database: Option<String>,
}

/// Whether one Magento cache type is enabled (`cache_types` map in `env.php`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CacheType {
    pub name: String,
    pub enabled: bool,
}

/// Cache configuration: the backend per frontend, plus the per-type enable flags.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CacheConfig {
    pub frontends: Vec<CacheFrontend>,
    pub types: Vec<CacheType>,
}

/// Locking backend configuration (the `lock` section of `env.php`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct LockConfig {
    /// Provider: `db` (default), `file`, `zookeeper`, or `cache`.
    pub provider: String,
    /// Provider-specific settings (`path`, `prefix`, `host`, …), sorted by key. NULL/empty
    /// entries are omitted.
    pub config: std::collections::BTreeMap<String, String>,
}

/// One message-queue connection (`queue/amqp` or `queue/connections/<name>` in `env.php`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct QueueConnection {
    /// Connection name, e.g. `amqp`.
    pub name: String,
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    /// Raw password (no masking, matching `db info`).
    pub password: String,
    pub virtualhost: Option<String>,
}

/// Message-queue configuration (the `queue` section of `env.php`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct QueueConfig {
    pub connections: Vec<QueueConnection>,
    /// The `consumers_wait_for_messages` flag, if set.
    pub consumers_wait_for_messages: Option<String>,
}

/// The kind of a GraphQL schema type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GqlKind {
    Object,
    Interface,
    Input,
    Enum,
    Union,
    Scalar,
}

impl std::fmt::Display for GqlKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            GqlKind::Object => "type",
            GqlKind::Interface => "interface",
            GqlKind::Input => "input",
            GqlKind::Enum => "enum",
            GqlKind::Union => "union",
            GqlKind::Scalar => "scalar",
        })
    }
}

/// One argument of a GraphQL field, as written (`pageSize: Int`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct GqlArg {
    pub name: String,
    pub ty: String,
}

/// One field of a GraphQL type, with its resolver and the module that declared it — the
/// per-field provenance is the point: `Query` is assembled from dozens of modules.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct GqlField {
    pub name: String,
    pub args: Vec<GqlArg>,
    /// Return type as written, e.g. `[ProductInterface!]!`.
    pub ty: String,
    /// `@resolver(class:)` — the class that computes the field. `None` for plain data
    /// fields served from the parent's output array.
    pub resolver: Option<ClassName>,
    /// `@doc(description:)`, or the SDL description string.
    pub description: Option<String>,
    /// `@deprecated` — the reason (possibly empty).
    pub deprecated: Option<String>,
    /// `@cache(cacheable:)`, when stated.
    pub cacheable: Option<bool>,
    pub source: Source,
}

/// A GraphQL schema type merged from every module's `schema.graphqls` (fields union by
/// name across modules, matching Magento's schema stitching).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct GqlType {
    pub name: String,
    pub kind: GqlKind,
    pub implements: Vec<String>,
    /// `@typeResolver(class:)` on interfaces/types — maps a runtime value to its concrete
    /// schema type.
    pub type_resolver: Option<ClassName>,
    pub description: Option<String>,
    pub fields: Vec<GqlField>,
    /// Enum values.
    pub values: Vec<String>,
    /// Union member types.
    pub members: Vec<String>,
    /// Where the type was first declared.
    pub source: Source,
}

/// A message-queue topic from `communication.xml`, with its handlers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct MqTopic {
    pub name: String,
    /// Request payload type (a class/interface name, or a primitive like `string`).
    pub request: Option<String>,
    pub response: Option<String>,
    /// `schema="Class::method"` — request/response derived from a service method.
    pub schema: Option<String>,
    pub handlers: Vec<MqHandler>,
    pub source: Source,
}

/// One handler of a topic (`<handler>` in `communication.xml`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct MqHandler {
    pub name: String,
    pub class: ClassName,
    pub method: String,
    pub disabled: bool,
    pub source: Source,
}

/// A queue consumer from `queue_consumer.xml`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct MqConsumer {
    pub name: String,
    pub queue: String,
    /// Declared connection; absent ⇒ Magento's runtime default (amqp, falling back to db).
    pub connection: Option<String>,
    pub consumer_instance: Option<ClassName>,
    /// `handler="Class::method"`; absent ⇒ handlers come from `communication.xml`.
    pub handler: Option<String>,
    pub max_messages: Option<String>,
    pub source: Source,
}

/// A topic's publisher from `queue_publisher.xml`, flattened to its enabled connection.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct MqPublisher {
    pub topic: String,
    /// Direct-to-queue shorthand (`queue=` attribute), bypassing exchange routing.
    pub queue: Option<String>,
    /// The enabled `<connection>`'s name/exchange, when the element form is used.
    pub connection: Option<String>,
    pub exchange: Option<String>,
    pub disabled: bool,
    pub source: Source,
}

/// How a message travels from a topic into a queue.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub enum MqVia {
    /// The topic's publisher names the queue directly (`<publisher queue=…>`).
    PublisherQueue { source: Source },
    /// An exchange binding whose topic pattern matches.
    Binding {
        exchange: String,
        /// The exchange's connection (`amqp` when not declared — the XSD default).
        connection: String,
        id: String,
        pattern: String,
        source: Source,
    },
}

/// One queue a topic's messages land in, how they get there, and who reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct MqRoute {
    pub queue: String,
    /// Every path leading here (a direct publisher queue and/or matching bindings).
    pub via: Vec<MqVia>,
    /// Consumers reading this queue (joined by queue name).
    pub consumers: Vec<MqConsumer>,
}

/// The full journey of one topic: definition + handlers (`communication.xml`), its
/// publisher (`queue_publisher.xml`), and each queue it reaches with that queue's
/// consumers (`queue_topology.xml` + `queue_consumer.xml`) — the answer to "who processes
/// a message published on this topic".
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct MqTopicRoute {
    pub topic: MqTopic,
    pub publisher: Option<MqPublisher>,
    pub routes: Vec<MqRoute>,
}

/// One URL rewrite from the `url_rewrite` table (live DB only — these are runtime data,
/// generated from products/categories/CMS pages plus manual entries; no static source).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct UrlRewrite {
    /// The public path requested, e.g. `my-product.html`.
    pub request_path: String,
    /// What it resolves to: an internal route (`catalog/product/view/id/42`) or, for a
    /// redirect, another request path.
    pub target_path: String,
    /// `product`, `category`, `cms-page`, or `custom`.
    pub entity_type: String,
    pub entity_id: u32,
    /// `0` = internal rewrite (no redirect), else the HTTP redirect code (`301`/`302`).
    pub redirect_type: u16,
    /// Store-view code the rewrite applies to, or `store/<id>` if the id is unknown.
    pub store: String,
    pub description: Option<String>,
    /// `true` for system-generated rewrites; `false` for manually-added ones.
    pub autogenerated: bool,
}

/// A page of URL rewrites, with whether more existed beyond the requested limit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct UrlRewrites {
    pub rewrites: Vec<UrlRewrite>,
    /// `true` if the `url_rewrite` table held more matching rows than `limit` (dropped).
    pub truncated: bool,
}

/// One column of a table, from declarative `db_schema.xml` (static — no DB needed).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct DbColumn {
    pub name: String,
    /// The `xsi:type`: `int`, `smallint`, `varchar`, `text`, `decimal`, `timestamp`, …
    pub col_type: String,
    pub nullable: bool,
    pub unsigned: bool,
    /// `length` for `varchar`/`char`; `None` otherwise.
    pub length: Option<String>,
    /// `precision`/`scale` for `decimal`.
    pub precision: Option<String>,
    pub scale: Option<String>,
    pub default: Option<String>,
    /// `identity="true"` — auto-increment.
    pub identity: bool,
    pub comment: Option<String>,
    pub source: Source,
}

/// An index (`<index>`), from `db_schema.xml`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct DbIndex {
    /// `referenceId`.
    pub id: String,
    /// `indexType`: `btree`, `fulltext`, `hash`.
    pub index_type: String,
    pub columns: Vec<String>,
    pub source: Source,
}

/// A constraint (`<constraint>`): primary key, unique key, or foreign key, from `db_schema.xml`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct DbConstraint {
    /// `referenceId` (`PRIMARY` for the primary key).
    pub id: String,
    /// `primary`, `unique`, or `foreign`.
    pub kind: String,
    /// Local columns covered (the referencing column for a foreign key).
    pub columns: Vec<String>,
    /// Foreign keys only: the referenced table/column and `ON DELETE` action.
    pub reference_table: Option<String>,
    pub reference_column: Option<String>,
    pub on_delete: Option<String>,
    pub source: Source,
}

/// A table merged from every module's `db_schema.xml`, in load order (a module can add columns
/// /indexes/constraints to another module's table; `disabled="true"` drops them).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct DbTable {
    pub name: String,
    pub engine: Option<String>,
    pub resource: Option<String>,
    pub comment: Option<String>,
    pub columns: Vec<DbColumn>,
    pub constraints: Vec<DbConstraint>,
    pub indexes: Vec<DbIndex>,
    /// Where the table was first declared.
    pub source: Source,
}

/// One table an indexer's materialized view subscribes to (`mview.xml`): changes to it are
/// change-logged and trigger partial reindexing when the indexer runs "by schedule".
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct MviewSubscription {
    pub table: String,
    /// The column identifying the changed entity (usually `entity_id`).
    pub entity_column: Option<String>,
    pub source: Source,
}

/// Live indexer state (`indexer_state` joined with `mview_state`), attached via `--db`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct IndexerLive {
    /// `valid` (ready) / `invalid` (reindex required) / `working` / `suspended`;
    /// `None` = no `indexer_state` row (never initialized).
    pub status: Option<String>,
    pub updated: Option<String>,
    /// Update mode: `true` = by schedule (mview enabled), `false` = on save;
    /// `None` = no `mview_state` row.
    pub by_schedule: Option<bool>,
    /// The view's own state: `idle`/`working`/`suspended`.
    pub view_status: Option<String>,
    /// Distinct pending entities in the changelog not yet applied (schedule mode;
    /// `None` = no changelog table or mode is on-save).
    pub backlog: Option<u64>,
}

/// An indexer from `indexer.xml`, joined (on `view_id`) with its `mview.xml` view — the
/// definition plus the tables whose changes feed it. Merged across modules in load order
/// (a module can add subscriptions to another module's view).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Indexer {
    /// Indexer id, e.g. `catalog_product_price` (what `bin/magento indexer:reindex` takes).
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub class: ClassName,
    /// The `mview.xml` view this indexer subscribes through (usually equal to `id`).
    pub view_id: Option<String>,
    /// Indexers sharing one physical index; reindexing either validates both.
    pub shared_index: Option<String>,
    /// Indexer ids this one depends on (they index first).
    pub dependencies: Vec<String>,
    /// Tables the indexer's view subscribes to, from `mview.xml` (empty if no view).
    pub subscriptions: Vec<MviewSubscription>,
    /// Live status from the DB (`--db` only).
    pub live: Option<IndexerLive>,
    pub source: Source,
}

/// One admin configuration field from `adminhtml/system.xml` — the map from a config `path`
/// to **where it lives in the admin** (Stores → Configuration → tab → section → group → field)
/// and how it behaves. Static; merged across modules.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct SystemField {
    /// `section/group/field` (or the field's explicit `<config_path>` override).
    pub path: String,
    /// The field's label (what the admin shows).
    pub label: String,
    /// Field input type: `text`, `select`, `multiselect`, `password`, …
    pub field_type: String,
    /// Tab label (the top of the breadcrumb), resolved from the section's `<tab>` reference.
    pub tab: Option<String>,
    /// Section label.
    pub section: String,
    /// Group label.
    pub group: String,
    /// Scopes the field is editable at: any of `default`, `website`, `store`.
    pub scopes: Vec<String>,
    pub source_model: Option<String>,
    pub backend_model: Option<String>,
    pub source: Source,
}

/// An admin ACL resource from `acl.xml` — one node in the permission tree (Magento Admin →
/// … → this resource). The ids that `webapi` and `system-config` cite as required
/// `<resource>`s resolve here. Merged across modules: a module attaches resources under
/// another module's by re-stating the ancestor path.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct AclResource {
    /// The unique resource id, e.g. `Magento_Sales::actions_view`.
    pub id: String,
    /// Human title shown in the admin role-permissions tree (empty when the id is used only as
    /// a path anchor and never declared with a title).
    pub title: String,
    /// Parent resource id (`None` for a root, normally `Magento_Backend::admin`).
    pub parent: Option<String>,
    /// Direct child resource ids, sorted by (`sortOrder`, id).
    pub children: Vec<String>,
    pub sort_order: Option<i32>,
    pub disabled: bool,
    pub source: Source,
}

/// Where a translation row comes from, in Magento's precedence order: modules (the
/// current request's controller module wins within this layer), then language packs,
/// then themes (child over parent), then the `translation` DB table.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationLayer {
    Module(ModuleName),
    /// Language pack name (`vendor/package`).
    Pack(String),
    /// Theme id (`frontend/Magento/luma`).
    Theme(String),
    Db,
}

/// One dictionary row for a phrase.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct TranslationEntry {
    pub layer: TranslationLayer,
    pub value: String,
    /// `key == value`: Magento's loader *deletes* earlier translations of the key
    /// (reset to untranslated), it does not set anything.
    pub reset: bool,
    /// DB rows only: the store the row applies to.
    pub store_id: Option<u32>,
    pub source: Source,
}

/// Every dictionary row matching a phrase, in precedence order.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct TranslationMatch {
    pub key: String,
    pub entries: Vec<TranslationEntry>,
}

/// The result of a translation scan, with enough context to explain an empty one:
/// how many dictionaries for the locale were actually scanned, and which
/// disabled/unregistered modules ship one that therefore never loads.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Translations {
    pub locale: String,
    pub matches: Vec<TranslationMatch>,
    /// `<locale>.csv` files that exist across enabled modules, packs, and themes.
    pub dictionaries_scanned: usize,
    /// Modules that ship a dictionary for this locale but are disabled or not in
    /// config.php — their translations never load.
    pub inactive_dictionaries: Vec<ModuleName>,
}

/// One attribute in a catalog attribute group, with the module that added it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CatalogAttribute {
    pub name: String,
    pub source: Source,
}

/// A `catalog_attributes.xml` group — which attributes Magento loads in a given context
/// (`quote_item`, `wishlist_item`, `catalog_product` collections, …), merged across
/// modules (each attribute keeps the adding module's `Source`). The "why isn't my
/// attribute available on the quote item" surface.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CatalogAttributeGroup {
    pub name: String,
    /// Sorted by attribute name.
    pub attributes: Vec<CatalogAttribute>,
}

/// A theme's override of an email template file.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct EmailTemplateOverride {
    /// Theme id, e.g. `frontend/Magento/luma`.
    pub theme: String,
    pub file: std::path::PathBuf,
}

/// A transactional email template registered in `etc/email_templates.xml`. The `id` is
/// also the value config stores when the template is selected in the admin.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct EmailTemplate {
    pub id: String,
    pub label: String,
    /// File name as declared, relative to `<module>/view/<area>/email/`.
    pub file: String,
    /// `html` or `text`.
    pub kind: String,
    /// The module whose view dir holds the file (may differ from the declaring module).
    pub module: ModuleName,
    pub area: Area,
    /// The resolved module file; `None` = declared but missing on disk (broken).
    pub path: Option<std::path::PathBuf>,
    /// Theme files that override it (which one applies depends on the active theme).
    pub theme_overrides: Vec<EmailTemplateOverride>,
    pub source: Source,
}

/// One admin-configurable parameter of a widget.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct WidgetParam {
    pub name: String,
    /// The `xsi:type` (`text`, `select`, `block`, `conditions`, …).
    pub param_type: String,
    pub required: bool,
    pub label: String,
    pub source_model: Option<ClassName>,
    pub default: Option<String>,
}

/// A widget type declared in `etc/widget.xml` — what the admin's "Insert Widget" offers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Widget {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    /// The block class that renders it.
    pub class: ClassName,
    pub parameters: Vec<WidgetParam>,
    /// Layout containers it declares templates for.
    pub containers: Vec<String>,
    pub source: Source,
}

/// The kind of a layout operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutOpKind {
    Block,
    Container,
    ReferenceBlock,
    ReferenceContainer,
    Update,
    Move,
}

/// One operation a layout file performs on the page.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct LayoutOp {
    pub kind: LayoutOpKind,
    /// Block/container name; the target handle for `Update`; the element for `Move`.
    pub name: String,
    pub class: Option<ClassName>,
    pub template: Option<String>,
    /// The enclosing named element (or `Move`'s destination).
    pub parent: Option<String>,
    /// `remove="true"` on a reference.
    pub remove: bool,
    pub source: Source,
}

/// Who provides a layout or ui component file: a module (merged in load order) or a
/// theme (applied per the active theme's ancestry — reported, not resolved, since the
/// active theme is runtime state).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutLayer {
    Module(ModuleName),
    /// Theme id, e.g. `frontend/Magento/luma`.
    Theme(String),
}

/// One layout file's contribution to a handle.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct LayoutContribution {
    pub layer: LayoutLayer,
    pub file: std::path::PathBuf,
    pub ops: Vec<LayoutOp>,
}

/// Everything contributing to one layout handle in one area: module files (in load
/// order), then theme files, plus the handle-inclusion graph around it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct LayoutView {
    pub handle: String,
    pub area: Area,
    pub contributions: Vec<LayoutContribution>,
    /// Handles this one pulls in (`<update handle=…>`).
    pub includes: Vec<String>,
    /// Handles that pull this one in.
    pub included_by: Vec<String>,
}

/// One node a ui component file declares or modifies. UI component XML is
/// open-vocabulary — the element name IS the component type (`column`, `field`,
/// `dataSource`, …) — and Magento merges same-name files by matching `(element, name)`,
/// so each node is reported with both.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct UiComponentOp {
    pub element: String,
    pub name: String,
    /// PHP class (`class=`) — data providers, column/filter renderers.
    pub class: Option<ClassName>,
    /// JS component (`component=`).
    pub component: Option<String>,
    /// `<field formElement=>`.
    pub form_element: Option<String>,
    pub sort_order: Option<String>,
    /// `<settings><label>` (or a button's direct `<label>`).
    pub label: Option<String>,
    /// `<settings><disabled>true</disabled>` — removes the node on merge.
    pub disabled: bool,
    /// `<settings><visible>` when stated.
    pub visible: Option<bool>,
    /// Nearest enclosing named node.
    pub parent: Option<String>,
    /// Nesting depth below the root element (captured ancestors; for tree rendering).
    pub depth: u16,
    pub source: Source,
}

/// One file's contribution to a ui component (module files in load order, then themes).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct UiComponentContribution {
    pub layer: LayoutLayer,
    pub file: std::path::PathBuf,
    pub ops: Vec<UiComponentOp>,
}

/// Everything contributing to one ui component (an admin grid, form, …) in one area.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct UiComponentView {
    pub name: String,
    /// The root element of the first declaring file (`listing`, `form`, …).
    pub kind: String,
    pub area: Area,
    pub contributions: Vec<UiComponentContribution>,
}

/// One node of the category tree (pre-order flattened; `level` 1 = a root tree).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CategoryTreeNode {
    pub id: u32,
    pub name: String,
    pub level: u32,
    /// Directly assigned products (`catalog_category_product`).
    pub direct_products: u32,
    /// Default-scope flags; `None` = no row.
    pub active: Option<bool>,
    pub in_menu: Option<bool>,
    pub anchor: Option<bool>,
    /// Store groups using this category as their root (roots only).
    pub root_of: Vec<String>,
}

/// A category search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CategoryHit {
    pub id: u32,
    pub name: String,
    pub url_key: Option<String>,
    pub level: u32,
    pub active: Option<bool>,
}

/// One "this category is invisible" finding: the category's own scope settings, or an
/// ancestor whose inactivity hides the whole subtree.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CategoryVisibilityIssue {
    /// `None` = the category itself.
    pub ancestor_id: Option<u32>,
    pub ancestor_name: Option<String>,
    /// Scopes where it is effectively inactive (`all scopes`, `default`, `stores/<code>`).
    pub scopes: Vec<String>,
}

/// The per-store indexed product count (`catalog_category_product_index_store<N>` —
/// what the storefront lists, including anchor-inherited products).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CategoryIndexCount {
    pub store: String,
    pub products: u32,
}

/// One directly assigned product (`--products`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CategoryProduct {
    pub entity_id: u32,
    pub sku: String,
    pub name: Option<String>,
    pub position: i64,
}

/// One product from the category's per-store index (`--indexed`) — what the storefront
/// actually lists, including anchor-inherited products.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CategoryIndexedProduct {
    pub entity_id: u32,
    pub sku: String,
    pub name: Option<String>,
    pub position: i64,
    /// Inherited from a subcategory via anchoring (`is_parent = 0`), not assigned here.
    pub via_anchor: bool,
    /// The product's effective visibility (3 = search only — in the index but not on
    /// the category page; 1 = not visible individually).
    pub visibility: Option<i64>,
}

/// One category as the database stores it. Live DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Category {
    pub id: u32,
    pub path: String,
    pub level: u32,
    pub position: u32,
    pub parent_id: Option<u32>,
    pub parent_name: Option<String>,
    pub children: u32,
    /// Ancestor names past the two roots, joined ` > ` (admin style).
    pub breadcrumb: String,
    /// Per-scope attribute values (name, is_active, url_key, display_mode, …).
    pub values: Vec<ProductValue>,
    /// Why the category (or its subtree position) is invisible, when it is.
    pub visibility: Vec<CategoryVisibilityIssue>,
    pub direct_products: u32,
    /// Indexed count per store view; empty = no index tables found.
    pub indexed: Vec<CategoryIndexCount>,
    pub rewrites: Vec<ProductRewrite>,
    /// Store groups whose root this category is.
    pub root_of: Vec<String>,
    /// Directly assigned products (populated with `--products`).
    pub products: Vec<CategoryProduct>,
    /// The store view whose index was read (`--indexed`); `None` = not requested.
    pub indexed_store: Option<String>,
    /// `None` while `indexed_store` is set = the store's index table doesn't exist
    /// (the category:product indexer never ran for it).
    pub indexed_products: Option<Vec<CategoryIndexedProduct>>,
}

/// One tier-price row (`catalog_product_entity_tier_price`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct TierPrice {
    /// Website code, or `(all)` for website_id 0.
    pub website: String,
    /// Customer group name, or `ALL GROUPS`.
    pub customer_group: String,
    pub qty: String,
    /// Fixed price — or `None` when the row is a percentage discount.
    pub value: Option<String>,
    pub percentage: Option<String>,
}

/// One catalog-rule price row (`catalogrule_product_price` — the rule engine's
/// materialized prices, ±1 day around today).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct RulePrice {
    pub date: String,
    pub website: String,
    pub customer_group: String,
    pub rule_price: String,
}

/// One `catalog_product_index_price` row — what the storefront actually reads.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct IndexedPrice {
    pub website: String,
    pub customer_group: String,
    pub price: Option<String>,
    pub final_price: Option<String>,
    pub min_price: Option<String>,
    pub max_price: Option<String>,
    pub tier_price: Option<String>,
}

/// One component of a composite product (configurable variant or grouped associate),
/// identity-flavored: which option combination it is, whether it's enabled, its stock.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ProductChild {
    pub sku: String,
    pub entity_id: u32,
    /// `status` decoded; `None` = no status row.
    pub enabled: Option<bool>,
    /// Configurables: the child's value per super attribute, resolved to option labels,
    /// in super-attribute order (`["Blue", "32"]`; `-` for a missing value). Empty for
    /// grouped associates.
    pub options: Vec<String>,
    /// Legacy `cataloginventory_stock_item` qty; `None` = no row.
    pub qty: Option<String>,
    pub in_stock: Option<bool>,
    /// Grouped: the default add-to-cart qty from the link attributes.
    pub default_qty: Option<String>,
}

/// One selectable product inside a bundle option.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct BundleSelection {
    pub sku: String,
    pub entity_id: u32,
    pub enabled: Option<bool>,
    pub qty: String,
    pub is_default: bool,
    /// Fixed-price bundles: the selection's price adjustment.
    pub price: Option<String>,
    /// The adjustment is a percentage of the bundle price, not a fixed amount.
    pub price_percent: bool,
    pub in_stock: Option<bool>,
}

/// One bundle option (`catalog_product_bundle_option`) with its selections.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct BundleOption {
    /// Default-store title.
    pub title: String,
    pub required: bool,
    /// `select`/`radio`/`checkbox`/`multi`.
    pub input_type: String,
    pub selections: Vec<BundleSelection>,
}

/// One component's price summary — a composite product's storefront price derives from
/// its components (configurable variants, grouped associates, bundle selections), so
/// these lines explain the parent's index min/max.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ChildPrice {
    pub sku: String,
    pub entity_id: u32,
    /// `status` decoded; `None` = no status row.
    pub enabled: Option<bool>,
    /// Default-scope EAV values.
    pub price: Option<String>,
    pub special_price: Option<String>,
    /// The child's own index `final_price` range across (website, group) rows;
    /// both `None` = not indexed (excluded from the parent's price).
    pub final_min: Option<String>,
    pub final_max: Option<String>,
    /// Fixed-price bundles: this selection's price adjustment.
    pub selection_price: Option<String>,
    /// The adjustment is a percentage, not an amount.
    pub selection_percent: bool,
}

/// Every price the database stores for one product: the EAV price attributes per scope,
/// tier prices, materialized catalog-rule prices, and the price index.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ProductPrices {
    pub entity_id: u32,
    pub sku: String,
    pub type_id: String,
    /// `catalog/price/scope`: `true` = website-scoped prices, `false` = global.
    pub price_scope_website: bool,
    /// The price-ish EAV attributes (`price`, `special_price` + validity dates, `cost`,
    /// `msrp`, …), with every scope row.
    pub attributes: Vec<ProductValue>,
    pub tier_prices: Vec<TierPrice>,
    pub rule_prices: Vec<RulePrice>,
    pub index: Vec<IndexedPrice>,
    /// Component prices: configurable variants, grouped associates, or bundle
    /// selections (empty for simples).
    pub children: Vec<ChildPrice>,
    /// Bundles: `fixed` or `dynamic` (the `price_type` attribute).
    pub bundle_price_type: Option<String>,
    pub matched_by_id: bool,
}

/// CMS entry kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CmsKind {
    Page,
    Block,
}

impl std::fmt::Display for CmsKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CmsKind::Page => "cms-page",
            CmsKind::Block => "cms-block",
        })
    }
}

/// One CMS page or block row. The same identifier can exist as several rows scoped to
/// different stores — lookups return every row, never a silent pick. Live DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CmsEntry {
    pub kind: CmsKind,
    pub id: u32,
    pub identifier: String,
    pub title: String,
    pub active: bool,
    /// Store codes; `(all stores)` for the store-0 assignment.
    pub stores: Vec<String>,
    pub created: Option<String>,
    pub updated: Option<String>,
    /// Pages only.
    pub page_layout: Option<String>,
    pub meta_title: Option<String>,
    /// A layout-update XML is attached (pages; a classic invisible behavior source).
    pub has_layout_update: bool,
    pub content_len: usize,
    /// First chunk of the content, whitespace-collapsed.
    pub content_preview: String,
    /// Full content — populated only when requested.
    pub content: Option<String>,
}

/// One row of a CMS search.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CmsHit {
    pub id: u32,
    pub identifier: String,
    pub title: String,
    pub active: bool,
    pub stores: Vec<String>,
}

/// One API integration (`integration` + its oauth token state + granted ACL resources).
/// Token secrets are never exposed — only presence/revocation. Live DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Integration {
    pub id: u32,
    pub name: String,
    pub email: Option<String>,
    pub endpoint: Option<String>,
    /// `inactive` / `active` / `recreated`.
    pub status: String,
    /// `manual` (admin-created) / `config` (defined in a module's config).
    pub setup: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Access-token state: `none` / `active` / `revoked`.
    pub token: String,
    /// Granted `Magento_Backend::all`.
    pub all_resources: bool,
    /// Granted resources, titled from the static acl.xml index (like admin roles).
    pub rules: Vec<AdminRule>,
}

/// One tax rate (`tax_calculation_rate`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct TaxRate {
    pub id: u32,
    pub code: String,
    pub country: String,
    /// Region code, `*` for all.
    pub region: String,
    /// `*` for all (or a zip range collapsed to `from–to`).
    pub postcode: String,
    pub rate: String,
}

/// One tax class, with whether any rule references it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct TaxClassInfo {
    pub id: u32,
    pub name: String,
    /// `CUSTOMER` / `PRODUCT`.
    pub class_type: String,
    /// Referenced by at least one tax rule — a product class in no rule is untaxed.
    pub in_rules: bool,
}

/// One tax rule with its class combination and rates.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct TaxRule {
    pub id: u32,
    pub code: String,
    pub priority: u32,
    pub calculate_subtotal: bool,
    pub customer_classes: Vec<String>,
    pub product_classes: Vec<String>,
    pub rates: Vec<TaxRate>,
}

/// The tax picture: classes, rules, and rates no rule uses. Live DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct TaxInfo {
    pub classes: Vec<TaxClassInfo>,
    pub rules: Vec<TaxRule>,
    /// Configured but referenced by no rule.
    pub unused_rates: Vec<TaxRate>,
}

/// One catalog price rule (`catalogrule`), with its applied state. Live DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CatalogRule {
    pub rule_id: u32,
    pub name: String,
    pub description: Option<String>,
    pub active: bool,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    /// Today is inside [from_date, to_date], per the DB clock.
    pub in_window: bool,
    /// Decoded `simple_action`.
    pub action: String,
    pub sort_order: u32,
    pub stop_rules_processing: bool,
    pub websites: Vec<String>,
    pub customer_groups: Vec<String>,
    /// Raw `conditions_serialized` — displayed, not interpreted.
    pub conditions: Option<String>,
    /// Distinct products in `catalogrule_product` — the materialized matches. 0 while
    /// everything else is green = "Apply Rules"/the catalogrule indexer never ran, or
    /// the conditions match nothing.
    pub matched_products: u32,
}

/// One row of a catalog-rule list.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CatalogRuleHit {
    pub rule_id: u32,
    pub name: String,
    pub active: bool,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    pub matched_products: u32,
}

/// One coupon of a cart price rule.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct RuleCoupon {
    pub code: String,
    pub times_used: u64,
    /// `None`/0 = unlimited.
    pub usage_limit: Option<u64>,
    pub usage_per_customer: Option<u64>,
    pub expiration_date: Option<String>,
    /// Past expiration, per the DB clock.
    pub expired: bool,
}

/// One cart price rule (`salesrule`), with the why-won't-it-apply facts. Live DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct SalesRule {
    pub rule_id: u32,
    pub name: String,
    pub description: Option<String>,
    pub active: bool,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    /// Today is inside [from_date, to_date], per the DB clock.
    pub in_window: bool,
    /// Decoded `coupon_type`: no coupon / specific coupon / auto-generated.
    pub coupon_type: String,
    /// Decoded `simple_action` + amount (`10% off`, `5.00 off cart`, …).
    pub action: String,
    pub apply_to_shipping: bool,
    pub free_shipping: bool,
    /// Later (higher sort_order) rules don't run after this one matches.
    pub stop_rules_processing: bool,
    pub sort_order: u32,
    /// 0 = unlimited.
    pub uses_per_customer: u64,
    pub uses_per_coupon: u64,
    /// Rule-level usage counter.
    pub times_used: u64,
    pub websites: Vec<String>,
    pub customer_groups: Vec<String>,
    /// Raw `conditions_serialized` — displayed, not interpreted.
    pub conditions: Option<String>,
    pub coupon_count: u32,
    /// First few coupons (auto-generated rules can have thousands).
    pub coupons: Vec<RuleCoupon>,
    /// Set when the lookup came in via a coupon code.
    pub matched_coupon: Option<RuleCoupon>,
}

/// One row of a sales-rule search.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct SalesRuleHit {
    pub rule_id: u32,
    pub name: String,
    pub active: bool,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
}

/// One store view in the scope tree.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct StoreViewNode {
    pub id: u32,
    pub code: String,
    pub name: String,
    pub active: bool,
    /// The group's default view.
    pub is_default: bool,
}

/// One store group in the scope tree.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct StoreGroupNode {
    pub id: u32,
    pub name: String,
    pub root_category_id: u32,
    pub root_category: Option<String>,
    /// The website's default group.
    pub is_default: bool,
    pub views: Vec<StoreViewNode>,
}

/// One website in the scope tree.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct WebsiteNode {
    pub id: u32,
    pub code: String,
    pub name: String,
    pub is_default: bool,
    pub groups: Vec<StoreGroupNode>,
}

/// The full scope tree (websites → groups → views, admin scopes excluded) plus the
/// currency rate table.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct StoreTree {
    pub websites: Vec<WebsiteNode>,
    /// `(from, to, rate)` from `directory_currency_rate`.
    pub currency_rates: Vec<(String, String, String)>,
}

/// One state assignment of an order status.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct OrderStatusState {
    pub state: String,
    /// The default status for that state.
    pub is_default: bool,
    pub visible_on_front: bool,
}

/// One order status with its state mapping(s). A status mapped to no state is
/// assignable manually but never set by Magento itself.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct OrderStatus {
    pub status: String,
    pub label: String,
    pub states: Vec<OrderStatusState>,
}

/// One customer group with its tax class and how many accounts belong to it. The
/// built-in `NOT LOGGED IN` group (id 0) is the guest bucket: it has no stored
/// members, so `members` is 0 by design. Live DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CustomerGroup {
    pub id: i64,
    pub code: String,
    pub tax_class_id: u32,
    /// The tax class name (`tax_class.class_name`); `None` if the id resolves to no row.
    pub tax_class: Option<String>,
    /// Accounts in `customer_entity` whose `group_id` is this group.
    pub members: u64,
}

/// One sales increment sequence (per entity type × store).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct SalesSequence {
    /// `order` / `invoice` / `shipment` / `creditmemo` (+ custom types).
    pub entity_type: String,
    pub store: String,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    pub step: u64,
    pub active: bool,
    /// Highest issued sequence value; `None` = nothing issued yet.
    pub current: Option<u64>,
    /// The next increment id, computed with Magento's default pattern
    /// (`prefix + 9-digit zero-padded value + suffix`) — custom patterns not modeled.
    pub next_increment: String,
    pub max_value: Option<u64>,
    pub warning_value: Option<u64>,
}

/// Which sales document to look up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SalesDocKind {
    Invoice,
    Shipment,
    Creditmemo,
}

impl std::fmt::Display for SalesDocKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SalesDocKind::Invoice => "invoice",
            SalesDocKind::Shipment => "shipment",
            SalesDocKind::Creditmemo => "creditmemo",
        })
    }
}

/// One line of a sales document (price/row_total absent on shipments).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct SalesDocumentItem {
    pub sku: String,
    pub name: Option<String>,
    pub qty: String,
    pub price: Option<String>,
    pub row_total: Option<String>,
}

/// One invoice / shipment / credit memo, with its order cross-link. Live DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct SalesDocument {
    pub kind: SalesDocKind,
    pub entity_id: u32,
    pub increment_id: String,
    /// Decoded state (invoices: open/paid/canceled; memos: open/refunded/canceled;
    /// shipments have none).
    pub state: Option<String>,
    pub order_increment: Option<String>,
    pub order_status: Option<String>,
    pub created_at: Option<String>,
    pub currency: Option<String>,
    /// subtotal/tax/shipping/grand_total (+ memo adjustments); empty for shipments.
    pub totals: Vec<OrderTotal>,
    /// Invoices: the payment transaction id.
    pub transaction_id: Option<String>,
    /// Shipments: total packed qty.
    pub total_qty: Option<String>,
    pub items: Vec<SalesDocumentItem>,
    /// Shipments: `(carrier, title, number)`.
    pub tracks: Vec<(String, String, String)>,
}

/// One row of a document search.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct SalesDocumentHit {
    pub increment_id: String,
    pub order_increment: Option<String>,
    pub created_at: Option<String>,
    /// Kind-specific: grand total (invoice/memo) or packed qty (shipment).
    pub amount: Option<String>,
}

/// One cart line.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct QuoteItem {
    pub sku: String,
    pub name: Option<String>,
    pub product_type: String,
    pub is_child: bool,
    pub qty: String,
    pub price: Option<String>,
    pub row_total: Option<String>,
    pub discount: Option<String>,
}

/// A quote address; the shipping one carries the chosen shipping method.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct QuoteAddress {
    pub kind: String,
    pub name: String,
    pub company: Option<String>,
    pub street: Option<String>,
    pub postcode: Option<String>,
    pub city: Option<String>,
    pub country: Option<String>,
    pub shipping_method: Option<String>,
    pub shipping_description: Option<String>,
}

/// One quote (cart) as checkout computed it. Live DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Quote {
    pub entity_id: u64,
    pub active: bool,
    pub store: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Seconds since the last update, DB-server clock — cart age.
    pub updated_secs: Option<i64>,
    pub converted_at: Option<String>,
    pub customer_id: Option<u32>,
    pub customer_email: Option<String>,
    pub customer_name: Option<String>,
    pub guest: bool,
    pub checkout_method: Option<String>,
    pub quote_currency: Option<String>,
    pub base_currency: Option<String>,
    pub items_qty: Option<String>,
    pub is_virtual: bool,
    pub coupon: Option<String>,
    pub applied_rule_ids: Option<String>,
    /// The order increment reserved at checkout start.
    pub reserved_order_id: Option<String>,
    /// The order this quote became, if any (`sales_order.quote_id`).
    pub order_increment: Option<String>,
    /// subtotal/shipping/tax/discount/grand_total — subtotal + grand total from the
    /// quote row, the rest from the shipping address (where checkout collects them).
    pub totals: Vec<OrderTotal>,
    pub items: Vec<QuoteItem>,
    pub addresses: Vec<QuoteAddress>,
    pub payment: Option<OrderPayment>,
}

/// One row of a quote search.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct QuoteHit {
    pub entity_id: u64,
    pub active: bool,
    pub customer_email: Option<String>,
    pub items_qty: Option<String>,
    pub grand_total: Option<String>,
    pub currency: Option<String>,
    pub updated_at: Option<String>,
}

/// One customer address, with default-billing/shipping tags.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CustomerAddress {
    pub id: u32,
    pub name: String,
    pub company: Option<String>,
    pub street: Option<String>,
    pub postcode: Option<String>,
    pub city: Option<String>,
    pub region: Option<String>,
    pub country: Option<String>,
    pub telephone: Option<String>,
    pub default_billing: bool,
    pub default_shipping: bool,
}

/// A newsletter subscription row (per store).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CustomerNewsletter {
    pub store: String,
    /// Decoded `subscriber_status`.
    pub status: String,
}

/// The customer's order summary.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CustomerOrders {
    pub count: u32,
    /// Lifetime `base_grand_total` sum (base currency).
    pub lifetime: Option<String>,
    pub first_at: Option<String>,
    pub last_at: Option<String>,
    pub last_increment: Option<String>,
    pub last_status: Option<String>,
}

/// One customer as the database stores it. Live DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Customer {
    pub entity_id: u32,
    pub email: String,
    pub name: String,
    pub group: Option<String>,
    pub website: Option<String>,
    /// The `created_in` store-name snapshot.
    pub created_in: Option<String>,
    pub created_at: Option<String>,
    pub active: bool,
    /// `false` = a confirmation token is pending — the account can't log in yet.
    pub confirmed: bool,
    pub locked: bool,
    pub lock_expires: Option<String>,
    pub failures: u32,
    pub last_login: Option<String>,
    pub last_logout: Option<String>,
    pub dob: Option<String>,
    pub taxvat: Option<String>,
    pub addresses: Vec<CustomerAddress>,
    pub newsletter: Vec<CustomerNewsletter>,
    /// Custom (non-static) customer EAV attribute values.
    pub values: Vec<ProductValue>,
    pub orders: CustomerOrders,
    /// Guest orders placed with the same email (not linked to the account).
    pub guest_orders: u32,
    pub matched_by_id: bool,
}

/// One row of a customer search.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct CustomerHit {
    pub entity_id: u32,
    pub email: String,
    pub name: String,
    pub group: Option<String>,
    pub created_at: Option<String>,
}

/// One order total line (order currency; `base` shown when the currencies differ).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct OrderTotal {
    pub key: String,
    pub amount: Option<String>,
    pub base_amount: Option<String>,
}

/// One order line, with the full quantity lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct OrderItem {
    pub sku: String,
    pub name: Option<String>,
    pub product_type: String,
    /// A composite's child row (qty bookkeeping lives on the parent).
    pub is_child: bool,
    pub qty_ordered: String,
    pub qty_invoiced: String,
    pub qty_shipped: String,
    pub qty_refunded: String,
    pub qty_canceled: String,
    pub price: Option<String>,
    pub row_total: Option<String>,
}

/// A billing or shipping address snapshot on the order.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct OrderAddress {
    /// `billing` / `shipping`.
    pub kind: String,
    pub name: String,
    pub company: Option<String>,
    pub street: Option<String>,
    pub postcode: Option<String>,
    pub city: Option<String>,
    pub country: Option<String>,
    pub telephone: Option<String>,
}

/// The order's payment: method plus the PSP's `additional_information` blob.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct OrderPayment {
    pub method: Option<String>,
    pub last_trans_id: Option<String>,
    /// Flattened top-level entries of `additional_information` (nested values as JSON).
    pub additional: Vec<(String, String)>,
}

/// One `sales_payment_transaction` row.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct OrderTransaction {
    pub txn_id: String,
    pub kind: String,
    pub closed: bool,
    pub created_at: Option<String>,
}

/// An invoice or credit memo attached to the order.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct OrderDocument {
    pub increment_id: String,
    /// Decoded state (`open`/`paid`/`refunded`/`canceled`).
    pub state: Option<String>,
    pub total: Option<String>,
    pub created_at: Option<String>,
}

/// One shipment with its tracking numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct OrderShipment {
    pub increment_id: String,
    pub qty: Option<String>,
    pub created_at: Option<String>,
    /// `(carrier, title, number)`.
    pub tracks: Vec<(String, String, String)>,
}

/// One status-history/comment row.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct OrderComment {
    pub status: Option<String>,
    pub comment: Option<String>,
    pub created_at: Option<String>,
    pub notified: bool,
}

/// One order as the database stores it (`sales_order` + its satellites). Live DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Order {
    pub entity_id: u32,
    pub increment_id: String,
    pub state: Option<String>,
    pub status: Option<String>,
    /// The status's admin label (`sales_order_status`).
    pub status_label: Option<String>,
    pub store: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub customer_id: Option<u32>,
    pub customer_email: Option<String>,
    pub customer_name: Option<String>,
    pub guest: bool,
    pub order_currency: Option<String>,
    pub base_currency: Option<String>,
    pub total_qty: Option<String>,
    pub coupon: Option<String>,
    pub applied_rule_ids: Option<String>,
    pub shipping_method: Option<String>,
    pub shipping_description: Option<String>,
    pub totals: Vec<OrderTotal>,
    pub items: Vec<OrderItem>,
    pub addresses: Vec<OrderAddress>,
    pub payment: Option<OrderPayment>,
    pub transactions: Vec<OrderTransaction>,
    pub invoices: Vec<OrderDocument>,
    pub shipments: Vec<OrderShipment>,
    pub creditmemos: Vec<OrderDocument>,
    pub history: Vec<OrderComment>,
    /// Present in `sales_order_grid` — `false` = the admin grid can't see this order
    /// (grid indexer behind).
    pub in_grid: bool,
    pub quote_id: Option<u64>,
    pub matched_by_id: bool,
}

/// One row of an order search.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct OrderHit {
    pub entity_id: u32,
    pub increment_id: String,
    pub status: Option<String>,
    pub grand_total: Option<String>,
    pub currency: Option<String>,
    pub customer_email: Option<String>,
    pub created_at: Option<String>,
}

/// One scope's value of a product attribute: the raw stored value plus the resolved
/// human label when the attribute's options make that possible.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ProductScopeValue {
    /// `default` (store_id 0) or the store view code.
    pub store: String,
    pub value: String,
    /// Option/source label (`1` → `Enabled`, option id → admin label).
    pub label: Option<String>,
}

/// One product attribute with every scope value the database stores for it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ProductValue {
    pub attribute: String,
    /// `varchar`/`int`/`decimal`/`text`/`datetime` — which value table the rows live in.
    pub backend_type: String,
    pub input: Option<String>,
    /// Default-scope row first, then store overrides sorted by store code.
    pub scopes: Vec<ProductScopeValue>,
}

/// One MSI source's stock for a product (`inventory_source_item`, keyed by SKU).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ProductSourceStock {
    pub source: String,
    pub quantity: String,
    pub in_stock: bool,
}

/// The legacy `cataloginventory_stock_item` row — what pre-MSI extensions still read.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ProductLegacyStock {
    pub qty: String,
    pub in_stock: bool,
    pub manage_stock: bool,
}

/// A category assignment, with the admin-style breadcrumb (root levels skipped).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ProductCategory {
    pub id: u32,
    pub breadcrumb: String,
}

/// One `url_rewrite` row pointing at the product.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ProductRewrite {
    pub request_path: String,
    pub store: String,
    /// 0 = internal rewrite, else the HTTP redirect code.
    pub redirect: u16,
}

/// One entry of a product's media gallery (`catalog_product_entity_media_gallery`),
/// with the store-0 label/position and the image roles it fills.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ProductMedia {
    /// The stored file path (e.g. `/a/b/shoe.jpg`) or the external-video url.
    pub file: String,
    /// `image` or `external-video`.
    pub media_type: String,
    /// The store-0 (default) label, if any.
    pub label: Option<String>,
    pub position: i32,
    /// Hidden from the gallery (`disabled` on the gallery row or its store-0 value).
    pub disabled: bool,
    /// Image roles this file fills, from the product's role attributes: any of
    /// `base` (`image`), `small` (`small_image`), `thumbnail`, `swatch` (`swatch_image`).
    pub roles: Vec<String>,
}

/// One product as the database stores it: identity, per-scope EAV values, stock (MSI +
/// legacy), categories, rewrites, and configurable links. Live DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Product {
    pub entity_id: u32,
    pub sku: String,
    pub type_id: String,
    pub attribute_set: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Website codes the product is assigned to.
    pub websites: Vec<String>,
    pub values: Vec<ProductValue>,
    /// MSI per-source stock (empty when MSI tables are absent).
    pub stock: Vec<ProductSourceStock>,
    pub legacy_stock: Option<ProductLegacyStock>,
    pub categories: Vec<ProductCategory>,
    pub rewrites: Vec<ProductRewrite>,
    /// Media gallery entries, in position order.
    pub media: Vec<ProductMedia>,
    /// Configurable parents this product is a variant of (SKUs).
    pub parents: Vec<String>,
    /// The attributes a configurable is configured by (`catalog_product_super_attribute`).
    pub super_attributes: Vec<String>,
    /// Configurable variants / grouped associates, with their identity essentials.
    pub children: Vec<ProductChild>,
    /// Bundle options with their selections (bundles only).
    pub bundle_options: Vec<BundleOption>,
    /// The lookup resolved via entity_id, not SKU (numeric query, no SKU match).
    pub matched_by_id: bool,
}

/// One row of a product search (`sku` substring).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ProductHit {
    pub entity_id: u32,
    pub sku: String,
    pub type_id: String,
    pub name: Option<String>,
    /// `status` decoded; `None` = no status row.
    pub enabled: Option<bool>,
}

/// One product a link points at (or, in a reverse view, that points at this product),
/// enriched with the state that decides whether it actually renders in the block.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ProductLinkTarget {
    /// The `position` link attribute (0 when unset); the block's display order.
    pub position: i32,
    pub sku: String,
    pub name: Option<String>,
    /// `status` decoded; `None` = no status row (treated as enabled by Magento).
    pub enabled: Option<bool>,
    /// Decoded visibility label (e.g. `Catalog, Search`).
    pub visibility: Option<String>,
    /// Legacy `is_in_stock`; `None` = no stock row.
    pub in_stock: Option<bool>,
    /// The product won't render in the block: disabled, or Not Visible Individually.
    pub hidden: bool,
}

/// The merchandising links of one product: related, up-sells, cross-sells
/// (`catalog_product_link` types 1/4/5). Live DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ProductLinks {
    pub entity_id: u32,
    pub sku: String,
    pub type_id: String,
    pub name: Option<String>,
    /// `true` = the reverse view: products that link *to* this one.
    pub reverse: bool,
    pub related: Vec<ProductLinkTarget>,
    pub up_sells: Vec<ProductLinkTarget>,
    pub cross_sells: Vec<ProductLinkTarget>,
    /// The lookup resolved via entity_id, not SKU (numeric query, no SKU match).
    pub matched_by_id: bool,
}

/// One queue's live backlog (MysqlMq driver tables) joined with the static topology.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct QueueBacklog {
    pub queue: String,
    /// Consumers reading it (`queue_consumer.xml`).
    pub consumers: Vec<String>,
    /// The queue exists in the db driver's `queue` table. `false` = the static config
    /// names it but the db driver doesn't know it (amqp-only, or setup:upgrade pending) —
    /// its broker-side backlog isn't inspectable from here.
    pub in_db: bool,
    /// In the DB but no static config references it (a removed module's leftover).
    pub orphaned: bool,
    /// Waiting messages (status `new`).
    pub new: u32,
    pub in_progress: u32,
    /// Failed, will be retried.
    pub retry: u32,
    pub error: u32,
    /// Complete/to-be-deleted rows awaiting cleanup.
    pub done: u32,
    /// Age of the oldest waiting (new/retry) message, DB-server clock.
    pub oldest_waiting_secs: Option<i64>,
}

/// One admin user (`admin_user` joined with its `authorization_role` group). Live DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct AdminUser {
    pub id: u32,
    pub username: String,
    pub firstname: String,
    pub lastname: String,
    pub email: String,
    pub active: bool,
    /// The role (group) name; `None` = no role assigned (can't log in usefully).
    pub role: Option<String>,
    pub created: Option<String>,
    /// Last login timestamp; `None` = never logged in.
    pub last_login: Option<String>,
    /// Seconds since the last login, per the DB server's clock.
    pub last_login_secs: Option<i64>,
    pub logins: u32,
    pub failures: u32,
    /// Account is currently locked (`lock_expires` in the future).
    pub locked: bool,
    pub lock_expires: Option<String>,
    pub locale: Option<String>,
}

/// One permission rule of an admin role: an ACL resource id, allowed or denied.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct AdminRule {
    /// ACL resource id (`Magento_Sales::actions_view`) — resolvable via `magequery acl`.
    pub resource: String,
    pub allow: bool,
    /// Title from the static acl.xml index; `None` = no module declares it (stale rule).
    pub title: Option<String>,
}

/// One admin role (`authorization_role` group) with its members and permissions. Live DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct AdminRole {
    pub id: u32,
    pub name: String,
    /// Usernames of the admin users in this role.
    pub users: Vec<String>,
    /// The role grants everything (`Magento_Backend::all` allowed).
    pub all_resources: bool,
    pub rules: Vec<AdminRule>,
}

/// What a setup-script call does to an attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EavSetupKind {
    Add,
    Update,
    Remove,
}

/// The literal kind of a setup-property value (for PHP-style rendering).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EavValueKind {
    Str,
    Num,
    Bool,
    Null,
    Class,
    /// Non-literal (nested array, variable, concatenation) — display text only.
    Other,
}

/// One scalar property from an `addAttribute` array argument.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct EavSetupProp {
    pub key: String,
    pub value: String,
    pub kind: EavValueKind,
}

/// One `addAttribute`/`updateAttribute`/`removeAttribute` call found in a setup script —
/// the static "who created/touched this attribute" join.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct EavSetupRef {
    pub kind: EavSetupKind,
    /// Resolved entity-type code (`catalog_product`) when recognizable, else the raw
    /// expression as written.
    pub entity: String,
    pub entity_known: bool,
    pub code: String,
    pub props: Vec<EavSetupProp>,
    pub source: Source,
}

/// One row of `eav_entity_type`, with its attribute count.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct EavEntityType {
    pub code: String,
    pub entity_table: Option<String>,
    pub attributes: usize,
}

/// An attribute's scope (`catalog_eav_attribute.is_global` decoded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EavScope {
    Store,
    Global,
    Website,
}

impl std::fmt::Display for EavScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EavScope::Store => "store",
            EavScope::Global => "global",
            EavScope::Website => "website",
        })
    }
}

/// Catalog-specific attribute behavior (`catalog_eav_attribute`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct EavCatalogFlags {
    pub scope: EavScope,
    pub searchable: bool,
    pub filterable: bool,
    pub filterable_in_search: bool,
    pub comparable: bool,
    pub used_in_listing: bool,
    pub used_for_sort_by: bool,
    pub visible_on_front: bool,
    /// Product types the attribute applies to (empty = all).
    pub apply_to: Vec<String>,
}

/// One EAV attribute from the live database.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct EavAttribute {
    pub code: String,
    pub entity_type: String,
    pub attribute_id: u32,
    pub label: Option<String>,
    /// `varchar`/`int`/`decimal`/`text`/`datetime`/`static`.
    pub backend_type: String,
    pub frontend_input: Option<String>,
    pub required: bool,
    pub unique: bool,
    /// Created through the admin (vs installed by code).
    pub user_defined: bool,
    pub default_value: Option<String>,
    pub source_model: Option<ClassName>,
    pub backend_model: Option<ClassName>,
    pub frontend_model: Option<ClassName>,
    /// Present when a `catalog_eav_attribute` row exists.
    pub catalog: Option<EavCatalogFlags>,
    /// The table its values live in; `None` for `static` (a column on the entity table).
    pub value_table: Option<String>,
}

/// An attribute-set membership: which set, in which group.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct EavSetMembership {
    pub set: String,
    pub group: String,
}

/// The full "what IS this attribute" card: the live row plus set membership, options,
/// and the static setup-script join (who created/modified it).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct EavAttributeCard {
    pub attribute: EavAttribute,
    /// The entity's base table (for the `static` backend-type case).
    pub entity_table: Option<String>,
    pub sets: Vec<EavSetMembership>,
    /// How many attribute sets the entity has in total.
    pub total_sets: usize,
    /// Admin-scope option labels (table-source attributes).
    pub options: Vec<String>,
    pub setup_refs: Vec<EavSetupRef>,
}

/// A `(table, column)` pair in a schema drift report.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct TableColumn {
    pub table: String,
    pub column: String,
}

/// Presence-level drift between the declared schema (`db_schema.xml`) and the live
/// database — the schema half of "is this environment in sync with the code".
/// Runtime-managed tables (mview `*_cl` changelogs, `sequence_*`, setup bookkeeping) are
/// excluded from the undeclared side; `runtime_tables_skipped` counts them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct SchemaDrift {
    /// Declared but absent live — what `setup:upgrade` would create.
    pub missing_tables: Vec<String>,
    pub missing_columns: Vec<TableColumn>,
    /// Live, no longer declared, but **whitelisted** (`db_schema_whitelist.json`) — the
    /// declarative system owns these, so `setup:upgrade` would DROP them.
    pub would_drop_tables: Vec<String>,
    pub would_drop_columns: Vec<TableColumn>,
    /// Declared but missing from every `db_schema_whitelist.json` — someone forgot
    /// `setup:db-declaration:generate-whitelist` (removals of these won't be applied).
    pub not_whitelisted_tables: Vec<String>,
    pub not_whitelisted_columns: Vec<TableColumn>,
    /// Live but declared nowhere and unwhitelisted: unmanaged (legacy install scripts,
    /// non-declarative modules) — declarative schema won't touch these.
    pub undeclared_tables: Vec<String>,
    pub undeclared_columns: Vec<TableColumn>,
    pub runtime_tables_skipped: usize,
}

impl SchemaDrift {
    pub fn is_clean(&self) -> bool {
        self.missing_tables.is_empty()
            && self.missing_columns.is_empty()
            && self.would_drop_tables.is_empty()
            && self.would_drop_columns.is_empty()
            && self.not_whitelisted_tables.is_empty()
            && self.not_whitelisted_columns.is_empty()
            && self.undeclared_tables.is_empty()
            && self.undeclared_columns.is_empty()
    }
}

/// The kind of a setup patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchKind {
    Data,
    Schema,
}

impl std::fmt::Display for PatchKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PatchKind::Data => "data",
            PatchKind::Schema => "schema",
        })
    }
}

/// One setup patch class (`Setup/Patch/Data|Schema`), optionally with its applied state
/// from the `patch_list` table.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Patch {
    pub class: ClassName,
    pub kind: PatchKind,
    pub module: ModuleName,
    /// Whether `patch_list` records it as applied; `None` = the DB wasn't consulted.
    pub applied: Option<bool>,
    pub source: Source,
}

/// All patches on disk, plus (with the DB) the applied entries no class on disk explains
/// — patches of removed modules, kept forever by `patch_list`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Patches {
    pub patches: Vec<Patch>,
    pub orphaned_applied: Vec<String>,
}

/// The auto-join a repository performs to load an extension attribute (`<join>`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ExtensionJoin {
    pub reference_table: String,
    pub reference_field: Option<String>,
    pub join_on_field: Option<String>,
    pub fields: Vec<String>,
}

/// One extension attribute bolted onto an API data interface via
/// `extension_attributes.xml` — the mechanism behind the generated `…Extension` classes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ExtensionAttribute {
    pub code: String,
    /// Declared type: a class/interface or a scalar, possibly `[]`-suffixed.
    pub ty: String,
    /// ACL resources gating the attribute in webapi responses.
    pub resources: Vec<String>,
    pub join: Option<ExtensionJoin>,
    pub source: Source,
}

/// An API data interface with every extension attribute modules add to it, merged in load
/// order (attributes keyed by code, last wins; each keeps the adding module's `Source` —
/// the point: `ProductInterface` is extended by inventory, gift-message, bundling, …).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct ExtendedType {
    pub for_type: ClassName,
    /// Sorted by code.
    pub attributes: Vec<ExtensionAttribute>,
}

/// One admin menu item from `adminhtml/menu.xml`, merged across modules in load order
/// (`<add>`/`<update>` upsert attribute-level; `<remove>` deletes). The tree the admin
/// sidebar renders — parents come from the `parent` attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct MenuItem {
    /// e.g. `Magento_Catalog::catalog_products`.
    pub id: String,
    pub title: String,
    pub parent: Option<String>,
    /// The admin route (`catalog/product/`); `None` for a grouping node.
    pub action: Option<String>,
    /// The ACL resource guarding it (resolve with `magequery acl <id>`).
    pub resource: Option<String>,
    pub sort_order: Option<i32>,
    /// Only shown when this module / config flag is active.
    pub depends_on_module: Option<String>,
    pub depends_on_config: Option<String>,
    /// Direct child ids, sorted by (`sortOrder`, id).
    pub children: Vec<String>,
    pub source: Source,
}

/// A REST endpoint from `webapi.xml`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct WebapiRoute {
    /// HTTP method (`GET`/`POST`/…).
    pub method: String,
    pub url: String,
    pub service_class: ClassName,
    pub service_method: String,
    /// ACL resources required (`<resource ref="…"/>`), or `anonymous`/`self`.
    pub resources: Vec<String>,
    pub source: Source,
}

/// The flagship result: everything Magento needs to build `requested` in `area`, with a
/// `Source` on every contributing line.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub struct Resolution {
    pub requested: ClassName,
    pub area: Area,
    pub concrete: ClassName,
    pub preference_chain: Vec<PreferenceStep>,
    /// If `concrete` is a virtual type, the real class it actually instantiates (following
    /// the `virtualType` `type=` chain to a non-virtual class). `None` for a real class.
    pub instantiates: Option<ClassName>,
    /// Plugins that fire on the concrete, sorted by `sort_order`. Includes plugins
    /// declared on ancestors/interfaces (see [`Plugin::declared_on`]).
    pub plugins: Vec<Plugin>,
    pub arguments: Vec<Argument>,
    /// Ancestor/interface types that contributed plugins or arguments to this resolution,
    /// nearest-first. Explains *why* an inherited plugin applies.
    pub contributing_ancestors: Vec<ClassName>,
}

/// A value computed independently for each [`Area`]. The natural representation of an
/// "all areas" answer: `Global` is the base, and the collapsed view shows only the areas
/// whose value differs from it.
#[derive(Debug, Clone)]
#[derive(serde::Serialize)]
#[serde(transparent)]
pub struct ByArea<T> {
    entries: Vec<(Area, T)>,
}

impl<T> ByArea<T> {
    pub fn new(entries: Vec<(Area, T)>) -> Self {
        Self { entries }
    }
    pub fn get(&self, area: Area) -> Option<&T> {
        self.entries.iter().find(|(a, _)| *a == area).map(|(_, v)| v)
    }
    pub fn iter(&self) -> impl Iterator<Item = (Area, &T)> {
        self.entries.iter().map(|(a, v)| (*a, v))
    }
}

impl<T: PartialEq> ByArea<T> {
    /// The `Global` value, if present, plus the areas whose value differs from it. This is
    /// the data behind the CLI's default collapsed-diff rendering — kept in core so library
    /// users get the same "what actually differs per area" computation for free.
    pub fn deltas(&self) -> Deltas<'_, T> {
        let base = self.get(Area::Global);
        let differing = self
            .entries
            .iter()
            .filter(|(a, v)| *a != Area::Global && base != Some(v))
            .map(|(a, v)| (*a, v))
            .collect();
        Deltas { base, differing }
    }
}

/// Result of [`ByArea::deltas`].
pub struct Deltas<'a, T> {
    pub base: Option<&'a T>,
    /// Areas (excluding `Global`) whose value differs from `base`.
    pub differing: Vec<(Area, &'a T)>,
}
