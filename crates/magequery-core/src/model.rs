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

/// A di.xml argument value. Objects are the interesting case (what gets injected).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
pub enum ArgValue {
    /// `xsi:type="object"` — an injected class or virtual type.
    Object(ClassName),
    /// Scalar value (`string`/`boolean`/`number`/`init_parameter`/`const`/…): the xsi type
    /// and its text.
    Scalar { xsi_type: String, text: String },
    /// `xsi:type="array"` — ordered key → value items.
    Array(Vec<(String, ArgValue)>),
    /// `xsi:type="null"` or an empty value.
    Null,
}

impl ArgValue {
    /// Merge a newer declaration over `self` the way Magento merges di.xml arguments:
    /// two arrays merge by item key (newer overrides same-key, appends new keys, recursing
    /// into nested arrays); anything else is replaced wholesale by `newer`.
    pub(crate) fn merged_with(&self, newer: &ArgValue) -> ArgValue {
        match (self, newer) {
            (ArgValue::Array(old), ArgValue::Array(new)) => {
                let mut items = old.clone();
                for (k, nv) in new {
                    match items.iter_mut().find(|(ek, _)| ek == k) {
                        Some((_, ev)) => *ev = ev.merged_with(nv),
                        None => items.push((k.clone(), nv.clone())),
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
