//! Data types for the project domain.

use super::*;

/// Where a module's code lives on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleSource {
    App,
    Vendor,
    Setup,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UnregisteredModule {
    pub name: ModuleName,
    pub source: ModuleSource,
    pub path: std::path::PathBuf,
}

/// Result of the `modules --check` lint: the two ways a module set can be inconsistent.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ClassRef {
    /// e.g. `@resolver of Query.products`, `consumer \`x\` of queue y`.
    pub role: String,
    pub source: Source,
}

/// Everything magequery knows about one class: identity, a compressed DI summary
/// (forward + reverse), and every configuration reference to it. The aggregate view —
/// `di`/`uses` remain the focused drill-downs.
#[derive(Debug, Clone, serde::Serialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
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
    TemplateFileMissing,
}

/// One `doctor` finding. Errors are things that break at runtime (dangling references,
/// cycles, module-set drift); warnings are probably-forgotten wiring (unregistered
/// commands/observers/plugins, unconsumed queues) that may be intentional.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DoctorFinding {
    pub lint: DoctorLint,
    pub severity: crate::error::Severity,
    pub message: String,
    /// The declaring file, when the finding has one.
    pub source: Option<Source>,
    /// The thing the finding is *about* — the missing/unregistered class, the module,
    /// the ACL id — structured so tooling (LSP quick fixes) never parses `message`.
    pub subject: Option<String>,
}

/// The `doctor` result: findings sorted errors-first.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DoctorReport {
    pub findings: Vec<DoctorFinding>,
}

impl DoctorReport {
    pub fn errors(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == crate::error::Severity::Error)
            .count()
    }
    pub fn warnings(&self) -> usize {
        self.findings.len() - self.errors()
    }
}

/// The everyday facts about an installation, on one screen: what/where it is and how it's
/// deployed. Config values always *try* the database (base URLs usually live only there)
/// and fall back to the static sources; every env-derived field degrades to `None` on a
/// fresh checkout that has no `env.php` yet.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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
