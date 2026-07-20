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
    /// The `xsi:type` this value was declared with — `Config\Dom`'s
    /// type-attribute identity, which decides replace-vs-merge on override.
    fn xsi_type(&self) -> &str {
        match self {
            ArgValue::Object(_) => "object",
            ArgValue::Array(_) => "array",
            ArgValue::Null => "null",
            ArgValue::Scalar { xsi_type, .. } => xsi_type,
        }
    }

    /// Merge a newer declaration over `self` the way Magento's `Config\Dom::
    /// _mergeNode` merges di.xml arguments. A re-declaration with a DIFFERENT
    /// `xsi:type` replaces the node wholesale — attributes included, so an
    /// `xsi:type="null"` knockout of an object item drops its `sortOrder` and
    /// the item falls to sort position 0. Same type merges: arrays item-by-item
    /// by key (newer overrides same-key — taking the newer item's source —
    /// appends new keys, recursing into nested arrays); objects override the
    /// class text but MERGE attributes, keeping `shared`/`sortOrder` the newer
    /// declaration doesn't restate; scalars take the newer text.
    pub(crate) fn merged_with(&self, newer: &ArgValue) -> ArgValue {
        match (self, newer) {
            (ArgValue::Array(old), ArgValue::Array(new)) => {
                let mut items = old.clone();
                for ni in new {
                    match items.iter_mut().find(|i| i.key == ni.key) {
                        Some(ei) => {
                            if ei.value.xsi_type() == ni.value.xsi_type() {
                                ei.value = ei.value.merged_with(&ni.value);
                                ei.sort_order = ni.sort_order.or(ei.sort_order);
                            } else {
                                ei.value = ni.value.clone();
                                ei.sort_order = ni.sort_order;
                            }
                            ei.source = ni.source.clone();
                        }
                        None => items.push(ni.clone()),
                    }
                }
                ArgValue::Array(items)
            }
            (ArgValue::Object(old), ArgValue::Object(new)) => ArgValue::Object(ObjectRef {
                // An empty override (`<item … xsi:type="object" sortOrder=…/>`)
                // merges attributes only — `_mergeNode` returns before touching
                // the value when the new node has no children.
                class: if new.class.as_str().is_empty() {
                    old.class.clone()
                } else {
                    new.class.clone()
                },
                shared: new.shared.or(old.shared),
                sort_order: new.sort_order.or(old.sort_order),
            }),
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

// ---- DI-export declarations (magecommand) ----
/// One `<preference for= type=>` in the merged config. A declaration, not a
/// resolution: no fixpoint is followed (that's [`Preference`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[non_exhaustive]
pub struct PreferenceDecl {
    pub for_type: ClassName,
    pub prefer: ClassName,
    /// First-declaration order across the merge — the position the entry
    /// holds in Magento's (insertion-ordered) preference map, which PHP
    /// preserves across overrides.
    pub decl_order: u32,
    pub source: Source,
}

/// One `<virtualType name= type=>` in the merged config.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[non_exhaustive]
pub struct VirtualTypeDecl {
    pub name: ClassName,
    pub base: ClassName,
    /// First-declaration order across the merge (see [`PreferenceDecl`]).
    pub decl_order: u32,
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
    /// `disabled=` as literally merged: `None` when no file ever wrote the
    /// attribute (compiled plugin maps carry the key only when declared).
    pub disabled_attr: Option<bool>,
    /// Config layer (0 primary, 1 module global, 2 area) where `disabled=`
    /// first appeared; `None` when never written.
    pub disabled_layer: Option<u8>,
    /// Same for `type=`.
    pub instance_layer: Option<u8>,
    /// `type=` was written with a leading backslash (the compiled plugin
    /// lists keep the raw spelling in _data).
    pub class_backslash: bool,
    /// The enclosing type node's spelling at first declaration.
    pub target_backslash: bool,
    /// First-declaration position: config layer (0 global / 1 area overlay),
    /// module load order, line — Magento's insertion order for plugin maps.
    pub decl_layer: u8,
    pub decl_load_order: u32,
    pub decl_line: u32,
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

/// First-mention position of one `<type>`/`<virtualType>` NODE per config
/// layer. The XML DOM merge pins a node's document position at its first
/// appearance; per-scope-read iteration (the compiled plugin lists) follows
/// node order within each layer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[non_exhaustive]
pub struct TypeNodePosition {
    /// RAW spelling — `\X` and `X` are distinct nodes.
    pub name: String,
    pub primary: Option<u32>,
    pub modules: Option<u32>,
    pub overlay: Option<u32>,
}

/// The fully merged DI configuration of one area, exported wholesale — the
/// bulk primitive a DI compiler iterates, where the per-class queries
/// ([`Magento::preference`], [`Magento::plugins`]) answer one name at a time.
/// Deterministically sorted; every declaration carries provenance.
///
/// [`Magento::preference`]: crate::Magento::preference
/// [`Magento::plugins`]: crate::Magento::plugins
/// Cheap declaration counts for one area — what a work-plan/summary view needs,
/// without materializing (and sorting) the full [`DiExport`]. Produced by
/// [`Magento::di_summary`](crate::Magento::di_summary).
#[derive(Debug, Clone, Copy, Default)]
#[derive(serde::Serialize)]
#[non_exhaustive]
pub struct DiSummary {
    pub preferences: usize,
    pub virtual_types: usize,
    pub plugin_declarations: usize,
    /// Distinct targets with at least one non-disabled plugin.
    pub plugged_targets: usize,
    pub arguments: usize,
}

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
    /// Node first-mention positions per layer, sorted by name.
    pub node_positions: Vec<TypeNodePosition>,
}

/// drives ObjectManager's argument-merge ordering).
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize)]
#[non_exhaustive]
pub struct ObjectRef {
    pub class: ClassName,
    pub shared: Option<bool>,
    pub sort_order: Option<i32>,
}
