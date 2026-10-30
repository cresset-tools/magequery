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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
    /// An argument name or array item key declared twice in one di.xml element: the
    /// later declaration silently replaces the earlier.
    DuplicateArgumentKey,
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
    /// An installed module switched off in `config.php` (`=> 0`) rather than removed.
    ModuleDisabled,
    /// Enabled DI config pointing at a class that lives in a disabled module.
    DisabledModuleReferenced,
}

/// The kebab-case name a finding is reported under: what `doctor --deny` takes.
impl std::fmt::Display for DoctorLint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match serde_json::to_value(self) {
            Ok(serde_json::Value::String(name)) => f.write_str(&name),
            _ => write!(f, "{self:?}"),
        }
    }
}

impl DoctorLint {
    /// Every lint, in code order.
    pub const ALL: [DoctorLint; 24] = [
        Self::ModuleMissingOnDisk,
        Self::ModuleNotRegistered,
        Self::SequenceCycle,
        Self::ModuleDisabled,
        Self::DisabledModuleReferenced,
        Self::PreferenceTargetMissing,
        Self::PreferenceCycle,
        Self::VirtualTypeBaseMissing,
        Self::VirtualTypeCycle,
        Self::PluginClassMissing,
        Self::DiArgumentClassMissing,
        Self::DuplicateArgumentKey,
        Self::ObserverClassMissing,
        Self::CronInstanceMissing,
        Self::WebapiServiceMissing,
        Self::AclResourceUnknown,
        Self::CommandClassMissing,
        Self::MqHandlerMissing,
        Self::QueueNoConsumer,
        Self::GraphqlResolverMissing,
        Self::TemplateFileMissing,
        Self::CommandUnregistered,
        Self::ObserverUnregistered,
        Self::PluginUnregistered,
    ];

    /// The lint's stable code, printed with every finding (`warn[E5003]`) and accepted
    /// by `doctor --deny`. Grouped by what is checked: `E1xxx` the module set, `E2xxx` DI,
    /// `E3xxx` entry points, `E4xxx` frontend, `E5xxx` code wired nowhere. A code belongs
    /// to the lint, not the severity, and is never reused: a new lint takes the next
    /// number in its group.
    pub fn code(self) -> &'static str {
        match self {
            Self::ModuleMissingOnDisk => "E1001",
            Self::ModuleNotRegistered => "E1002",
            Self::SequenceCycle => "E1003",
            Self::ModuleDisabled => "E1004",
            Self::DisabledModuleReferenced => "E1005",
            Self::PreferenceTargetMissing => "E2001",
            Self::PreferenceCycle => "E2002",
            Self::VirtualTypeBaseMissing => "E2003",
            Self::VirtualTypeCycle => "E2004",
            Self::PluginClassMissing => "E2005",
            Self::DiArgumentClassMissing => "E2006",
            Self::DuplicateArgumentKey => "E2007",
            Self::ObserverClassMissing => "E3001",
            Self::CronInstanceMissing => "E3002",
            Self::WebapiServiceMissing => "E3003",
            Self::AclResourceUnknown => "E3004",
            Self::CommandClassMissing => "E3005",
            Self::MqHandlerMissing => "E3006",
            Self::QueueNoConsumer => "E3007",
            Self::GraphqlResolverMissing => "E3008",
            Self::TemplateFileMissing => "E4001",
            Self::CommandUnregistered => "E5001",
            Self::ObserverUnregistered => "E5002",
            Self::PluginUnregistered => "E5003",
        }
    }
}

impl std::str::FromStr for DoctorLint {
    type Err = String;

    /// Parse a lint's code (`E1004`, any case) or the kebab-case name it serializes as
    /// (`module-disabled`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(lint) = Self::ALL.into_iter().find(|l| l.code().eq_ignore_ascii_case(s)) {
            return Ok(lint);
        }
        serde_json::from_value(serde_json::Value::String(s.to_owned()))
            .map_err(|_| format!("unknown doctor lint `{s}` (a code like E1004, or a name like module-disabled)"))
    }
}

/// One `doctor` finding. Errors are things that break at runtime (dangling references
/// something live can reach, cycles, module-set drift); warnings are probably-forgotten
/// wiring (unregistered commands/observers/plugins, unconsumed queues, stale references
/// nothing can reach) that may be intentional; infos are unreachable references into a
/// package that isn't installed (optional integrations).
#[derive(Debug, Clone, serde::Serialize)]
pub struct DoctorFinding {
    pub lint: DoctorLint,
    /// The lint's stable code ([`DoctorLint::code`]).
    pub code: &'static str,
    pub severity: crate::error::Severity,
    pub message: String,
    /// The declaring file, when the finding has one.
    pub source: Option<Source>,
    /// The thing the finding is *about* — the missing/unregistered class, the module,
    /// the ACL id — structured so tooling (LSP quick fixes) never parses `message`.
    pub subject: Option<String>,
    /// Why the severity is what it is, for missing-class findings: the path by which
    /// something live builds the missing class (an error), or every dead end that keeps
    /// it from ever being built (a warning or info).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<ChainNode>,
}

/// One step of a [`DoctorFinding::chain`]: a reference to (or fact about) a class, with
/// the steps that explain it underneath.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ChainNode {
    pub message: String,
    pub source: Option<Source>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ChainNode>,
}

/// The `doctor` result: findings sorted errors-first.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DoctorReport {
    pub findings: Vec<DoctorFinding>,
}

impl DoctorReport {
    /// Treat these lints as errors: their findings are upgraded to
    /// [`Severity::Error`](crate::error::Severity), so they fail the run. How a
    /// shop makes a warning it has cleaned up blocking in its own CI.
    pub fn deny(&mut self, lints: &[DoctorLint]) {
        for f in &mut self.findings {
            if lints.contains(&f.lint) {
                f.severity = crate::error::Severity::Error;
            }
        }
        self.sort();
    }

    /// Errors first, then warnings, then infos; by message within a severity.
    pub(crate) fn sort(&mut self) {
        use crate::error::Severity;
        let rank = |s: Severity| match s {
            Severity::Error => 0,
            Severity::Warning => 1,
            Severity::Info => 2,
        };
        self.findings
            .sort_by(|a, b| rank(a.severity).cmp(&rank(b.severity)).then_with(|| a.message.cmp(&b.message)));
    }

    pub fn errors(&self) -> usize {
        self.count(crate::error::Severity::Error)
    }
    pub fn warnings(&self) -> usize {
        self.count(crate::error::Severity::Warning)
    }
    pub fn infos(&self) -> usize {
        self.count(crate::error::Severity::Info)
    }
    fn count(&self, severity: crate::error::Severity) -> usize {
        self.findings.iter().filter(|f| f.severity == severity).count()
    }

    /// Keep only the findings located in one of `scopes`. A scope is a root-relative
    /// path prefix (`app/code/Acme`, `app/design/frontend/Acme`), a module
    /// (`Acme_Widget`), a module vendor (`Acme` = every `Acme_*` module), or a theme or
    /// theme vendor (`frontend/Acme/base`, `Acme`). Findings without a source match on
    /// their subject (the module, for module-set drift).
    pub fn retain_in(&mut self, root: &std::path::Path, scopes: &[String]) {
        if scopes.is_empty() {
            return;
        }
        self.findings.retain(|f| scopes.iter().any(|scope| finding_in(f, root, scope)));
    }
}

fn finding_in(f: &DoctorFinding, root: &std::path::Path, scope: &str) -> bool {
    let scope = scope.trim_start_matches("./").trim_end_matches('/');
    let module_matches = |module: &str| {
        module == scope
            || module.strip_prefix(scope).is_some_and(|rest| rest.starts_with('_'))
            // Theme ids (`frontend/Acme/base`): the vendor or the vendor/name.
            || module.split_once('/').is_some_and(|(_, theme)| {
                theme == scope
                    || theme.strip_prefix(scope).is_some_and(|rest| rest.starts_with('/'))
            })
    };
    match &f.source {
        Some(source) => {
            let file = source.file.strip_prefix(root).unwrap_or(&source.file);
            let file = file.strip_prefix(".").unwrap_or(file);
            file.starts_with(scope) || module_matches(source.module.as_str())
        }
        None => f.subject.as_deref().is_some_and(module_matches),
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
