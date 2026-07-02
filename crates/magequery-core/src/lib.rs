//! magequery-core: static analysis of a Magento 2 codebase.
//!
//! The library never prints, never exits, and never reads ambient state — it takes a
//! filesystem root, builds an index, and returns owned, structured data. The `magequery`
//! CLI is a thin renderer on top.
//!
//! ```no_run
//! use magequery_core::{Magento, ids::{ClassName, Area}};
//!
//! let mage = Magento::open("/path/to/magento")?;
//! for d in mage.diagnostics() {
//!     eprintln!("warning: {}", d.message);
//! }
//! let res = mage.resolve(&ClassName::new("Magento\\Quote\\Api\\CartManagementInterface"), Area::Global)?;
//! println!("instantiates {}", res.concrete);
//! # Ok::<(), magequery_core::Error>(())
//! ```

pub mod error;
pub mod ids;
pub mod model;
pub mod source;

mod breadth;
mod composer;
#[cfg(feature = "db")]
mod db;
mod decrypt;
mod deploy;
mod di;
mod graphql;
mod index;
mod parse;
mod php;
mod phparray;
mod redis;
mod resolver;
mod sysconfig;

pub use error::{Diagnostic, Error, Result, Severity};
pub use ids::{Area, ClassName, ConfigPath, EventName, ModuleName};
pub use model::{
    AclResource, ArgItem, ArgValue, Argument, ByArea, ChainPluginRef, ChainStep, ConfigSourceKind, ConfigValue,
    ConsoleCommand, ControllerAction, CronJob, DbColumn, DbConfig, DbConnection, DbConstraint, DbIndex, DbPing,
    DbTable, DepEdge, GqlArg, GqlField, GqlKind, GqlType, Indexer, InstanceInfo, InterceptKind,
    MethodChain, Module, ModuleCheck, ModuleDeps,
    MviewSubscription, Observer,
    Preference, PreferenceStep, Plugin, PluginMethod, RedisConfig, RedisInstance, RedisPing,
    Resolution, Route, UnregisteredModule, WebapiRoute,
};
pub use model::{
    CacheConfig, CacheFrontend, CacheType, InjectionSite, LockConfig, MqConsumer, MqHandler,
    MqPublisher, MqRoute, MqTopic, MqTopicRoute, MqVia, QueueConfig, QueueConnection,
    SessionConfig, SystemField, UrlRewrite, UrlRewrites, UseRef, Uses,
};
pub use decrypt::Decryptor;
pub use sysconfig::ConfigSet;
pub use source::Source;

use std::path::Path;
use std::sync::OnceLock;

/// An opened Magento installation. Holds the parsed module index and merged per-area DI
/// config; every query method is a projection over it. Cheap to query, built once on
/// [`open`](Magento::open).
pub struct Magento {
    // Filled in by the indexer (steps 1–3). Kept private so the internal representation
    // can evolve without breaking the public, struct-returning API.
    index: index::Index,
    // The di.xml index — the expensive parse — is built lazily so `modules`/`events`/etc.
    // don't pay for it. Carries its own diagnostics, merged into `diagnostics()` once built.
    di: OnceLock<DiBuilt>,
    // Breadth indexes, built lazily on first use so they don't slow the core commands.
    events: OnceLock<breadth::EventIndex>,
    cron: OnceLock<breadth::CronIndex>,
    routes: OnceLock<breadth::RouteIndex>,
    webapi: OnceLock<breadth::WebapiIndex>,
    schema: OnceLock<breadth::SchemaIndex>,
    system_config: OnceLock<breadth::SystemConfigIndex>,
    acl: OnceLock<breadth::AclIndex>,
    indexers: OnceLock<breadth::IndexerIndex>,
    mq: OnceLock<breadth::MqIndex>,
    gql: OnceLock<breadth::GqlIndex>,
}

struct DiBuilt {
    index: di::DiIndex,
    diagnostics: Vec<Diagnostic>,
}

impl Magento {
    /// Build the index for the installation rooted at `root`.
    ///
    /// Fails only on problems that prevent any meaningful answer (no Magento root,
    /// unreadable `config.php`). Per-module problems are recorded as [`Diagnostic`]s and
    /// retrieved via [`diagnostics`](Magento::diagnostics) — a single broken file does not
    /// fail the build.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let index = index::Index::build(root.as_ref())?;
        Ok(Self {
            index,
            di: OnceLock::new(),
            events: OnceLock::new(),
            cron: OnceLock::new(),
            routes: OnceLock::new(),
            webapi: OnceLock::new(),
            schema: OnceLock::new(),
            system_config: OnceLock::new(),
            acl: OnceLock::new(),
            indexers: OnceLock::new(),
            mq: OnceLock::new(),
            gql: OnceLock::new(),
        })
    }

    /// The merged DI config, built (and its diagnostics collected) on first DI query.
    fn di_index(&self) -> &di::DiIndex {
        &self
            .di
            .get_or_init(|| {
                let mut diagnostics = Vec::new();
                let index = di::build(&self.index.root, &self.index.modules, &mut diagnostics);
                DiBuilt { index, diagnostics }
            })
            .index
    }

    /// Non-fatal problems found while indexing. Includes di.xml parse problems once the DI
    /// index has been built (i.e. after a DI query); call after running a DI command to see
    /// them all.
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        let mut all = self.index.diagnostics.clone();
        if let Some(di) = self.di.get() {
            all.extend(di.diagnostics.iter().cloned());
        }
        all
    }

    /// All modules, in `config.php` load order.
    pub fn modules(&self) -> &[Module] {
        &self.index.modules
    }

    /// Consistency between the modules on disk and those listed in `config.php`. A
    /// non-clean result usually means `bin/magento setup:upgrade` was not run.
    pub fn module_check(&self) -> &ModuleCheck {
        &self.index.check
    }

    /// The concrete type Magento instantiates for `class` in `area`, with the full
    /// preference chain. If no preference applies, the class is its own concrete type
    /// (empty chain) — matching Magento, which instantiates the requested class directly.
    pub fn preference(&self, class: &ClassName, area: Area) -> Result<Preference> {
        let cfg = self.di_index().config(area);
        let mut current = class.clone();
        let mut chain = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // Follow `for -> type` redirects to a fixpoint, guarding against cycles.
        while seen.insert(current.clone()) {
            match cfg.preferences.get(&current) {
                Some(located) => {
                    let to = located.value.clone();
                    chain.push(PreferenceStep {
                        from: current.clone(),
                        to: to.clone(),
                        source: located.source.clone(),
                    });
                    if to == current {
                        break;
                    }
                    current = to;
                }
                None => break,
            }
        }

        // With no preference, the class is its own concrete type — but only if it actually
        // exists. Otherwise the user gave a name that resolves to nothing; say so.
        if chain.is_empty() && !self.class_known(class, area) {
            return Err(Error::ClassNotFound(class.clone()));
        }

        Ok(Preference { requested: class.clone(), concrete: current, chain, area })
    }

    /// Whether `class` is something we can see: a real source file (PSR-4), a virtualType,
    /// or a type referenced by DI config. Used to distinguish a real concrete class from a
    /// name that resolves to nothing. Checks the area's config plus the global base.
    fn class_known(&self, class: &ClassName, area: Area) -> bool {
        if self.index.resolver.exists(class) {
            return true;
        }
        let referenced = |cfg: &di::AreaConfig| {
            cfg.virtual_types.contains_key(class) || cfg.plugins.contains_key(class)
        };
        referenced(self.di_index().config(area)) || referenced(self.di_index().config(Area::Global))
    }

    /// Plugins that fire on `class` in `area`, in execution order. The preference is
    /// resolved first, then plugins are collected from the concrete type **and every
    /// ancestor/interface** — so plugins declared on an interface or parent are included
    /// (each tagged with `declared_on`). Disabled plugins are included but flagged.
    ///
    /// A plugin *name* is unique across the resolved type's hierarchy: if the same name is
    /// declared on both the concrete class and an ancestor, the nearest one wins (Magento
    /// merges by name). Order is Magento's: ascending `sort_order`, ties broken by
    /// declaration order (module load order, then position in file) — not by name.
    pub fn plugins(&self, class: &ClassName, area: Area) -> Result<Vec<Plugin>> {
        let concrete = self.preference(class, area)?.concrete;
        let targets = self.plugin_targets(&concrete);
        let mut collected = self.collect_plugins(area, &targets);
        // Execution order: sort_order, then declaration order (load order, then line).
        collected.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(collected.into_iter().map(|(_, p)| p).collect())
    }

    /// Plugins across **all areas**, merged into one set: each plugin appears once, tagged
    /// (via its `source.area`) with where it's declared — `base` (global) or a specific
    /// area. Base plugins win a name clash over an area override. Ordered like `plugins()`.
    /// Targets are taken from the global concrete (preference rarely differs per area).
    pub fn plugins_all_areas(&self, class: &ClassName) -> Result<Vec<Plugin>> {
        let concrete = self.preference(class, Area::Global)?.concrete;
        let targets = self.plugin_targets(&concrete);

        use std::collections::{BTreeSet, HashMap};
        let mut best: HashMap<String, ((i32, (u8, u32, u32)), Plugin)> = HashMap::new();
        // Every area a given plugin name is declared in (across the merge).
        let mut areas_of: HashMap<String, BTreeSet<Area>> = HashMap::new();
        for area in Area::ALL {
            for (key, plugin) in self.collect_plugins(area, &targets) {
                areas_of.entry(plugin.name.clone()).or_default().insert(plugin.source.area);
                match best.get(&plugin.name) {
                    // Keep the lowest area_rank (base, rank 0) over an area overlay (rank 1).
                    Some((bk, _)) if bk.1 .0 <= key.1 .0 => {}
                    _ => {
                        best.insert(plugin.name.clone(), (key, plugin));
                    }
                }
            }
        }
        let mut v: Vec<_> = best.into_values().collect();
        for (_, p) in &mut v {
            p.areas = areas_of[&p.name].iter().copied().collect();
        }
        v.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(v.into_iter().map(|(_, p)| p).collect())
    }

    /// The concrete type plus its ancestors/interfaces — the set of types whose plugins
    /// apply to the concrete.
    fn plugin_targets(&self, concrete: &ClassName) -> Vec<ClassName> {
        let mut targets = vec![concrete.clone()];
        targets.extend(self.index.resolver.ancestors(concrete));
        targets
    }

    /// Collect plugins for `targets` in one area, keyed sort-order + declaration order.
    /// Dedups by name nearest-target-first (concrete wins over an ancestor).
    fn collect_plugins(
        &self,
        area: Area,
        targets: &[ClassName],
    ) -> Vec<((i32, (u8, u32, u32)), Plugin)> {
        let cfg = self.di_index().config(area);
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for target in targets {
            let Some(by_name) = cfg.plugins.get(target) else { continue };
            for (name, lp) in by_name {
                if !seen.insert(name.clone()) {
                    continue;
                }
                let Some(plugin_class) = lp.class.clone() else { continue };
                let methods = self.index.resolver.plugin_methods(&plugin_class);
                out.push((
                    (lp.sort_order, lp.order_key),
                    Plugin {
                        name: name.clone(),
                        class: plugin_class,
                        sort_order: lp.sort_order,
                        methods,
                        declared_on: target.clone(),
                        disabled: lp.disabled,
                        areas: vec![lp.source.area],
                        source: lp.source.clone(),
                    },
                ));
            }
        }
        out
    }

    /// The interceptor execution chain (the "onion") for each method intercepted on `class`
    /// in `area`: before plugins (ascending `sort_order`), around plugins nested
    /// (ascending = outer), the target method, then around unwinding and after plugins
    /// (descending). Disabled plugins are excluded (they don't run). `only` restricts to a
    /// single method name.
    ///
    /// Note: this is the standard onion. Magento's exact segmentation when `around` plugins
    /// interleave with the before/after of *other* plugins across sort orders is simplified
    /// here (all befores, then all arounds, etc.) — accurate for the common case.
    pub fn plugin_chains(
        &self,
        class: &ClassName,
        area: Area,
        only: Option<&str>,
    ) -> Result<Vec<MethodChain>> {
        Ok(chains_from(&self.plugins(class, area)?, only))
    }

    /// Like [`plugin_chains`](Magento::plugin_chains) but over the merged all-areas plugin
    /// set (see [`plugins_all_areas`](Magento::plugins_all_areas)) — one onion per method
    /// showing every plugin that can intercept it across areas, each tagged by its area.
    pub fn plugin_chains_all_areas(
        &self,
        class: &ClassName,
        only: Option<&str>,
    ) -> Result<Vec<MethodChain>> {
        Ok(chains_from(&self.plugins_all_areas(class)?, only))
    }

    /// Observers bound to `event` in `area`.
    pub fn observers(&self, event: &EventName, area: Area) -> Vec<Observer> {
        self.events_index().observers(event, area)
    }

    /// All events in `area` with their observer counts.
    pub fn events(&self, area: Area) -> Vec<(EventName, usize)> {
        self.events_index().events(area)
    }

    /// Cron jobs, optionally restricted to one group.
    pub fn cron_jobs(&self, group: Option<&str>) -> Vec<CronJob> {
        self.cron.get_or_init(|| breadth::CronIndex::build(&self.index.modules)).jobs(group)
    }

    /// Frontend/adminhtml routes (frontName → modules) in `area`.
    pub fn routes(&self, area: Area) -> Vec<Route> {
        self.routes.get_or_init(|| breadth::RouteIndex::build(&self.index.modules)).routes(area)
    }

    /// REST endpoints from `webapi.xml`, optionally filtered by a URL substring.
    pub fn webapi(&self, url_filter: Option<&str>) -> Vec<WebapiRoute> {
        self.webapi.get_or_init(|| breadth::WebapiIndex::build(&self.index.modules)).routes(url_filter)
    }

    fn schema_index(&self) -> &breadth::SchemaIndex {
        self.schema.get_or_init(|| breadth::SchemaIndex::build(&self.index.modules))
    }

    /// Database tables from declarative `db_schema.xml`, merged across modules in load order
    /// (a module can add columns/indexes/constraints to another's table; `disabled="true"`
    /// drops them). Static — no DB needed. Filtered by a table-name substring, sorted by name.
    pub fn schema(&self, name_filter: Option<&str>) -> Vec<DbTable> {
        self.schema_index().tables(name_filter)
    }

    /// One table by exact name, with its full column/index/constraint set and provenance.
    pub fn table(&self, name: &str) -> Option<DbTable> {
        self.schema_index().table(name)
    }

    /// Admin configuration fields from `adminhtml/system.xml` (where each `Stores →
    /// Configuration` setting lives: tab → section → group → field, its scopes and models),
    /// merged across modules. Static. Filtered by a config-path or label substring, sorted by
    /// path.
    pub fn system_config(&self, filter: Option<&str>) -> Vec<SystemField> {
        self.system_config
            .get_or_init(|| breadth::SystemConfigIndex::build(&self.index.modules))
            .fields(filter)
    }

    fn acl_index(&self) -> &breadth::AclIndex {
        self.acl.get_or_init(|| breadth::AclIndex::build(&self.index.modules))
    }

    /// Admin ACL resources from `acl.xml`, merged across modules in load order (a module can
    /// attach resources under another's). Static. No filter → the whole tree in pre-order; a
    /// filter → resources whose id or title contains it. These are the ids `webapi` and
    /// `system-config` cite as required `<resource>`s.
    pub fn acl(&self, filter: Option<&str>) -> Vec<AclResource> {
        self.acl_index().resources(filter)
    }

    /// One ACL resource by exact id, with its parent and direct-child ids.
    pub fn acl_resource(&self, id: &str) -> Option<AclResource> {
        self.acl_index().resource(id)
    }

    /// The breadcrumb for an ACL resource: ancestors from the root down to (excluding) `id`.
    pub fn acl_ancestors(&self, id: &str) -> Vec<AclResource> {
        self.acl_index().ancestors(id)
    }

    /// The direct children of an ACL resource — the sub-permissions it groups.
    pub fn acl_children(&self, id: &str) -> Vec<AclResource> {
        self.acl_index().children(id)
    }

    /// Reverse DI — everything the merged di.xml config wires *to* `class` (which may
    /// itself be a virtual type): the types whose preference resolves to it, the virtual
    /// types built on it, and every constructor argument (incl. nested array items) that
    /// injects it — as the class itself, its generated `\Proxy`, or its name as a string
    /// (factory/pool style). di.xml facts only: plain constructor type-hints resolved by
    /// autowiring have no di.xml declaration and aren't listed.
    ///
    /// With `area: None`, scans the global config plus each area's **own** declarations
    /// (facts inherited from global aren't repeated per area) — a merged all-areas view;
    /// each hit's `source.area` says where it was declared. With `Some(area)`, scans that
    /// area's fully merged config.
    pub fn uses(&self, class: &ClassName, area: Option<Area>) -> Result<Uses> {
        let mut uses = Uses {
            class: class.clone(),
            preferred_for: Vec::new(),
            virtual_types: Vec::new(),
            injections: Vec::new(),
        };
        match area {
            Some(a) => self.scan_uses(a, class, None, &mut uses),
            None => {
                self.scan_uses(Area::Global, class, None, &mut uses);
                for &a in Area::ALL.iter().filter(|&&a| a != Area::Global) {
                    self.scan_uses(a, class, Some(a), &mut uses);
                }
            }
        }
        uses.preferred_for.sort_by(|a, b| a.name.cmp(&b.name));
        uses.virtual_types.sort_by(|a, b| a.name.cmp(&b.name));
        uses.injections.sort_by(|a, b| {
            a.consumer
                .cmp(&b.consumer)
                .then_with(|| a.argument.cmp(&b.argument))
                .then_with(|| a.item_path.cmp(&b.item_path))
        });

        // No references at all: fine for a real class ("unused"), an error for a typo.
        if uses.preferred_for.is_empty()
            && uses.virtual_types.is_empty()
            && uses.injections.is_empty()
            && !self.class_known(class, area.unwrap_or(Area::Global))
        {
            return Err(Error::ClassNotFound(class.clone()));
        }
        Ok(uses)
    }

    /// Scan one area's merged config for references to `class`. `declared_in` restricts
    /// hits to declarations made in that area's own files (used by the merged view to
    /// avoid repeating global-inherited facts per area).
    fn scan_uses(&self, area: Area, class: &ClassName, declared_in: Option<Area>, out: &mut Uses) {
        let cfg = self.di_index().config(area);
        let keep = |s: &Source| declared_in.is_none_or(|a| s.area == a);
        let proxy = ClassName::new(format!("{}\\Proxy", class.as_str()));

        for (for_, located) in &cfg.preferences {
            if located.value == *class && keep(&located.source) {
                out.preferred_for.push(UseRef { name: for_.clone(), source: located.source.clone() });
            }
        }
        for (name, vt) in &cfg.virtual_types {
            if vt.value == *class && keep(&vt.source) {
                out.virtual_types.push(UseRef { name: name.clone(), source: vt.source.clone() });
            }
        }
        for (consumer, args) in &cfg.type_args {
            let consumer_is_virtual = cfg.virtual_types.contains_key(consumer);
            for (arg_name, la) in args {
                scan_arg_for_class(
                    &la.value,
                    &la.source,
                    &mut Vec::new(),
                    &UseScan { class, proxy: &proxy, consumer, consumer_is_virtual, argument: arg_name, keep: &keep },
                    &mut out.injections,
                );
            }
        }
    }

    /// Console commands modules register on `CommandListInterface`'s `commands` array
    /// argument in di.xml — what `bin/magento` picks up. Each command's actual CLI name and
    /// description are extracted from its class (never executed). Optionally filtered by a
    /// case-insensitive substring of the name, class, or di.xml item key; sorted by command
    /// name (unknown names last, by class).
    pub fn console_commands(&self, filter: Option<&str>) -> Vec<ConsoleCommand> {
        let iface = ClassName::new("Magento\\Framework\\Console\\CommandListInterface");
        // The preference (app/etc/di.xml) points at the concrete CommandList; `args_of`
        // then merges arguments declared on the concrete AND — via the ancestor walk — on
        // the interface, because modules register on either.
        let concrete = match self.preference(&iface, Area::Global) {
            Ok(p) => p.concrete,
            Err(_) => iface,
        };
        let args = self.args_of(&concrete, Area::Global, &mut std::collections::HashSet::new());
        let Some((ArgValue::Array(items), _)) = args.get("commands") else {
            return Vec::new();
        };

        let needle = filter.map(str::to_lowercase);
        let mut out: Vec<ConsoleCommand> = items
            .iter()
            .filter_map(|item| {
                let ArgValue::Object(class) = &item.value else { return None };
                let (name, description) = self.index.resolver.command_info(class);
                let cmd = ConsoleCommand {
                    name,
                    description,
                    item_key: item.key.clone(),
                    class: class.clone(),
                    source: item.source.clone(),
                };
                match &needle {
                    Some(n)
                        if !cmd.name.as_deref().unwrap_or("").to_lowercase().contains(n)
                            && !cmd.class.as_str().to_lowercase().contains(n)
                            && !cmd.item_key.to_lowercase().contains(n) =>
                    {
                        None
                    }
                    _ => Some(cmd),
                }
            })
            .collect();
        out.sort_by(|a, b| match (&a.name, &b.name) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.class.cmp(&b.class),
        });
        out
    }

    fn indexer_index(&self) -> &breadth::IndexerIndex {
        self.indexers.get_or_init(|| breadth::IndexerIndex::build(&self.index.modules))
    }

    /// Indexers from `indexer.xml`, each joined (on `view_id`) with its `mview.xml` view —
    /// definition, dependencies, and the tables whose changes feed it. Static. Filtered by
    /// an id/title substring, sorted by id.
    pub fn indexers(&self, filter: Option<&str>) -> Vec<Indexer> {
        self.indexer_index().indexers(filter)
    }

    /// One indexer by exact id, with its full subscription list.
    pub fn indexer(&self, id: &str) -> Option<Indexer> {
        self.indexer_index().indexer(id)
    }

    /// The everyday facts, on one screen: version/distribution, deploy mode, maintenance
    /// state, base URLs, admin URL, module counts. Always *tries* the database for the
    /// config values (base URLs usually live only in `core_config_data`) and degrades to
    /// the static sources when it's unreachable — `db_error` says so instead of failing.
    /// On a fresh checkout with no `env.php`, every env-derived field is `None`.
    pub fn info(&self) -> InstanceInfo {
        // Version: the product package, most specific name first.
        let version_pkg = ["/product-enterprise-edition", "/product-community-edition", "/magento2-base"]
            .iter()
            .find_map(|suffix| {
                self.index.packages.iter().find(|p| p.name.ends_with(suffix))
            });

        let env = deploy::read_env(&self.index.root).ok();
        let mode = env
            .as_ref()
            .and_then(|e| e.get("MAGE_MODE"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let admin_front_name = env
            .as_ref()
            .and_then(|e| e.get("backend"))
            .and_then(|b| b.get("frontName"))
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let installed_at = env
            .as_ref()
            .and_then(|e| e.get("install"))
            .and_then(|i| i.get("date"))
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let maintenance = self.index.root.join("var/.maintenance.flag").is_file();
        let maintenance_allowed_ips = std::fs::read_to_string(self.index.root.join("var/.maintenance.ip"))
            .map(|t| {
                t.split([',', '\n', ' '])
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        // Config values, DB included when reachable; else the static sources alone.
        let (set, db_error) = match self.config(true) {
            Ok(s) => (Some(s), None),
            Err(e) => (self.config(false).ok(), Some(e.to_string())),
        };
        let get = |path: &str| {
            set.as_ref().and_then(|s| s.get("default", path)).map(|v| v.value.clone())
        };
        let base_url_overrides = set
            .as_ref()
            .map(|s| {
                ["web/unsecure/base_url", "web/secure/base_url"]
                    .iter()
                    .flat_map(|p| s.scopes_for(p))
                    .filter(|v| v.scope != "default")
                    .count()
            })
            .unwrap_or(0);
        let base_url = get("web/unsecure/base_url");
        let base_url_secure = get("web/secure/base_url");

        // Admin URL, mirroring Magento: base = `admin/url/custom` when `use_custom` is on,
        // else the first *concrete* store base URL (secure preferred) — never a
        // `{{base_url}}` placeholder (= auto-detect at request time). Path = frontName,
        // or `admin/url/custom_path` when `use_custom_path` is on.
        let concrete = |u: &Option<String>| u.clone().filter(|b| !b.contains("{{"));
        let admin_base = if get("admin/url/use_custom").as_deref() == Some("1") {
            concrete(&get("admin/url/custom"))
        } else {
            None
        }
        .or_else(|| concrete(&base_url_secure))
        .or_else(|| concrete(&base_url));
        let admin_path = if get("admin/url/use_custom_path").as_deref() == Some("1") {
            get("admin/url/custom_path").or_else(|| admin_front_name.clone())
        } else {
            admin_front_name.clone()
        };
        let admin_url = match (&admin_base, &admin_path) {
            (Some(base), Some(path)) => {
                Some(format!("{}/{path}/", base.trim_end_matches('/')))
            }
            _ => None,
        };

        let has_pkg = |prefix: &str| self.index.packages.iter().any(|p| p.name.starts_with(prefix));
        let frontend_pkgs_installed =
            has_pkg("hyva-themes/") || has_pkg("swissup/breeze") || has_pkg("swissup/module-breeze");

        // The active frontend theme: config `design/theme/theme_id` (default scope) —
        // a numeric id (needs the `theme` table) or a path string — falling back to the
        // DI default (`Magento\Theme\Model\View\Design`'s `themes['frontend']` argument,
        // the same fallback Magento itself uses when nothing is configured). The DI
        // default is only trusted when the DB was consulted (or no installed frontend
        // package contradicts it): with the DB unreachable, the real theme row is
        // invisible and "Magento/luma" would be a confident wrong answer on a Hyvä shop.
        let theme_value = get("design/theme/theme_id").or_else(|| {
            if db_error.is_some() && frontend_pkgs_installed {
                return None;
            }
            let design = ClassName::new("Magento\\Theme\\Model\\View\\Design");
            let args = self.args_of(&design, Area::Global, &mut std::collections::HashSet::new());
            let (ArgValue::Array(items), _) = args.get("themes")? else { return None };
            items.iter().find(|i| i.key == "frontend").and_then(|i| match &i.value {
                ArgValue::Scalar { text, .. } => Some(text.clone()),
                _ => None,
            })
        });
        // Resolve to a path + ancestor chain via the theme table when reachable; a path
        // string works without it (single-element chain).
        let theme_rows = if db_error.is_none() { self.fetch_theme_rows().ok() } else { None };
        let mut theme_chain: Vec<String> = Vec::new();
        if let Some(v) = &theme_value {
            let start_id = match v.parse::<u32>() {
                Ok(id) => Some(id),
                Err(_) => {
                    // Config may store the full-path form (`frontend/Hyva/default`);
                    // normalize to the theme table's area-less `theme_path`.
                    let vn = v.strip_prefix("frontend/").unwrap_or(v);
                    theme_chain.push(vn.to_string());
                    theme_rows.as_ref().and_then(|rows| {
                        rows.iter()
                            .find(|(_, _, p, area)| area == "frontend" && p.as_deref() == Some(vn))
                            .map(|(_, parent, _, _)| *parent)
                            .flatten()
                    })
                }
            };
            if let (Some(rows), Some(mut id)) = (&theme_rows, start_id) {
                let mut seen = std::collections::HashSet::new();
                while seen.insert(id) {
                    let Some((_, parent, path, _)) = rows.iter().find(|(tid, ..)| *tid == id)
                    else {
                        break;
                    };
                    if let Some(p) = path {
                        if !theme_chain.contains(p) {
                            theme_chain.push(p.clone());
                        }
                    }
                    match parent {
                        Some(p) => id = *p,
                        None => break,
                    }
                }
            }
        }
        let theme = theme_chain.first().cloned();
        // Classify the stack by ancestry; when the active theme is unknown, fall back to
        // "which frontend packages are installed" (honest but weaker — the CLI says so).
        let pkg_version = |exact: &str, prefix: &str| {
            self.index
                .packages
                .iter()
                .find(|p| p.name == exact)
                .or_else(|| self.index.packages.iter().find(|p| p.name.starts_with(prefix)))
                .and_then(|p| p.version.clone())
        };
        let (frontend, frontend_version) = if theme_chain.iter().any(|p| p.starts_with("Hyva/")) {
            (Some("Hyvä".to_string()), pkg_version("hyva-themes/magento2-default-theme", "hyva-themes/"))
        } else if theme_chain.iter().any(|p| p.to_lowercase().contains("breeze")) {
            (Some("Breeze".to_string()), pkg_version("swissup/module-breeze", "swissup/breeze"))
        } else if theme_chain.iter().any(|p| p == "Magento/luma") {
            (Some("Luma".to_string()), None)
        } else if theme_chain.iter().any(|p| p == "Magento/blank") {
            (Some("Blank".to_string()), None)
        } else if theme.is_none() && has_pkg("hyva-themes/") {
            (Some("Hyvä".to_string()), pkg_version("hyva-themes/magento2-default-theme", "hyva-themes/"))
        } else if theme.is_none() && (has_pkg("swissup/breeze") || has_pkg("swissup/module-breeze")) {
            (Some("Breeze".to_string()), pkg_version("swissup/module-breeze", "swissup/breeze"))
        } else {
            (None, None)
        };

        // The checkout stack: a curated map of the known solutions' packages, then a
        // generic "any non-core package named *checkout*" fallback (verbatim, so an
        // unlisted solution still surfaces). `None` = stock (Luma) checkout.
        const CHECKOUT_PACKAGES: [(&str, &str); 7] = [
            ("hyva-themes/magento2-hyva-checkout", "Hyvä Checkout"),
            // The core package first so the version is Loki's own, not an add-on's
            // (`loki-checkout/magento2-mollie` etc. share the vendor prefix).
            ("loki-checkout/magento2-core", "Loki Checkout"),
            ("loki-checkout/", "Loki Checkout"),
            ("swissup/firecheckout", "Firecheckout"),
            ("mageplaza/module-one-step-checkout", "Mageplaza One Step Checkout"),
            ("onestepcheckout/", "OneStepCheckout"),
            ("bold-commerce/module-checkout", "Bold Checkout"),
        ];
        let core_vendor = |name: &str| {
            name.starts_with("magento/")
                || name.starts_with("mage-os/")
                || name.starts_with("modulargento/")
        };
        let (checkout, checkout_version) = CHECKOUT_PACKAGES
            .iter()
            .find_map(|(prefix, label)| {
                self.index
                    .packages
                    .iter()
                    .find(|p| p.name.starts_with(prefix))
                    .map(|p| (Some(label.to_string()), p.version.clone()))
            })
            .or_else(|| {
                self.index
                    .packages
                    .iter()
                    .find(|p| p.name.contains("checkout") && !core_vendor(&p.name))
                    .map(|p| (Some(p.name.clone()), p.version.clone()))
            })
            .unwrap_or((None, None));

        // Hyvä Checkout is installed alongside the checkout it replaces and exposes which
        // one is selected (`hyva_themes_checkout/general/checkout`; `default` = the
        // Magento/Luma original) — read it so "installed" isn't reported as "active".
        let checkout_selected = if checkout.as_deref() == Some("Hyvä Checkout") {
            get("hyva_themes_checkout/general/checkout")
        } else {
            None
        };

        // Search engine + its endpoint (the host path is engine-specific:
        // `catalog/search/<engine>_server_hostname`).
        let search_engine = get("catalog/search/engine");
        let search_host = search_engine.as_ref().and_then(|e| {
            let host = get(&format!("catalog/search/{e}_server_hostname"))?;
            Some(match get(&format!("catalog/search/{e}_server_port")) {
                Some(port) if !port.is_empty() => format!("{host}:{port}"),
                _ => host,
            })
        });

        // Full-page cache application: 1 = built-in, 2 = Varnish.
        let fpc = get("system/full_page_cache/caching_application").map(|v| match v.as_str() {
            "1" => "built-in".to_string(),
            "2" => "varnish".to_string(),
            other => other.to_string(),
        });

        let queue_endpoint = self.queue_config().ok().and_then(|q| {
            q.connections.first().map(|c| {
                format!("{} @ {}:{}", c.name, c.host, c.port.unwrap_or(5672))
            })
        });

        let cron_last_success_ago =
            if db_error.is_none() { self.fetch_cron_last_success().ok().flatten() } else { None };

        // Deployment one-liners, from the existing env.php extractors (credentials
        // deliberately left out of this casual, paste-into-a-ticket view).
        let db_conn = self.db_config().ok().and_then(|c| {
            c.connections
                .iter()
                .find(|x| x.name == "default")
                .or_else(|| c.connections.first())
                .map(|x| {
                    let endpoint = match &x.unix_socket {
                        Some(s) => s.clone(),
                        None => match x.port {
                            Some(p) => format!("{}:{p}", x.host),
                            None => x.host.clone(),
                        },
                    };
                    (x.dbname.clone(), endpoint, c.table_prefix.clone())
                })
        });
        let cache = self.cache_config().ok();

        // The store hierarchy (websites → groups → store views): live DB when reachable,
        // else config.php's `scopes` node (only present when the config is dumped). The
        // synthetic admin scopes are excluded either way — id 0 in the DB; the `admin`
        // website/store keys and the groups whose website_id is 0 in the scopes node.
        let db_counts = if db_error.is_none() { self.fetch_scope_counts().ok() } else { None };
        let (websites, store_groups, store_views) = match db_counts {
            Some((w, g, s)) => (Some(w), Some(g), Some(s)),
            None => {
                let config_php = deploy::read_config_php(&self.index.root).ok();
                let section = |name: &str| {
                    config_php.as_ref()?.get("scopes")?.get(name)?.as_array()
                };
                let by_key = |name: &str| {
                    section(name)
                        .map(|items| items.iter().filter(|(k, _)| k.as_str() != Some("admin")).count())
                };
                let groups = section("groups").map(|items| {
                    items
                        .iter()
                        .filter(|(_, v)| {
                            !matches!(v.get("website_id"), Some(phparray::PhpValue::Int(0)))
                                && v.get("website_id").and_then(|w| w.as_str()) != Some("0")
                        })
                        .count()
                });
                (by_key("websites"), groups, by_key("stores"))
            }
        };

        InstanceInfo {
            db_error,
            search_engine,
            search_host,
            fpc,
            queue_endpoint,
            locale: get("general/locale/code"),
            currency: get("currency/options/base"),
            timezone: get("general/locale/timezone"),
            cron_last_success_ago,
            theme,
            frontend,
            frontend_version,
            checkout,
            checkout_version,
            checkout_selected,
            db_name: db_conn.as_ref().map(|(n, _, _)| n.clone()),
            db_endpoint: db_conn.as_ref().map(|(_, e, _)| e.clone()),
            table_prefix: db_conn.and_then(|(_, _, p)| (!p.is_empty()).then_some(p)),
            session: self.session_config().ok(),
            cache_frontends: cache.as_ref().map(|c| c.frontends.clone()).unwrap_or_default(),
            cache_types_enabled: cache
                .as_ref()
                .map(|c| c.types.iter().filter(|t| t.enabled).count())
                .unwrap_or(0),
            cache_types_total: cache.as_ref().map(|c| c.types.len()).unwrap_or(0),
            websites,
            store_groups,
            store_views,
            installed_at,
            modules_vendor: self
                .index
                .modules
                .iter()
                .filter(|m| m.source == model::ModuleSource::Vendor)
                .count(),
            modules_app: self
                .index
                .modules
                .iter()
                .filter(|m| m.source == model::ModuleSource::App)
                .count(),
            packages_total: self.index.packages.len(),
            version: version_pkg.and_then(|p| p.version.clone()),
            version_package: version_pkg.map(|p| p.name.clone()),
            mode,
            maintenance,
            maintenance_allowed_ips,
            base_url,
            base_url_secure,
            base_url_overrides,
            admin_front_name,
            admin_url,
            modules_total: self.index.modules.len(),
            modules_enabled: self.index.modules.iter().filter(|m| m.enabled).count(),
        }
    }

    /// The dependency graph around one module, both directions, from the two static
    /// sources: `<sequence>` in module.xml (load-order dependencies) and the owning
    /// composer package's `require`, with each required package mapped back to the
    /// module(s) it provides. Composer edges have composer's granularity — requiring a
    /// package that bundles several modules yields an edge per module. Non-module requires
    /// (framework, libraries, `php`/`ext-*`) are returned in `other_requires`.
    pub fn deps(&self, module: &ModuleName) -> Result<ModuleDeps> {
        let me = self
            .index
            .modules
            .iter()
            .position(|x| &x.name == module)
            .ok_or_else(|| Error::ModuleNotFound(module.clone()))?;
        let m = &self.index.modules[me];

        // package root -> package, so a module finds its owner by walking its ancestors.
        let root_of: std::collections::HashMap<&Path, &index::PackageMeta> =
            self.index.packages.iter().map(|p| (p.root.as_path(), p)).collect();

        // Every module's composer identity: package name + requires + the declaring
        // composer.json. Vendor modules resolve through installed.json; app/code modules
        // read their own composer.json (when they have one).
        let infos: Vec<Option<DepPkgInfo>> = self
            .index
            .modules
            .iter()
            .map(|x| match x.path.ancestors().find_map(|a| root_of.get(a)) {
                Some(p) => Some(DepPkgInfo {
                    name: p.name.clone(),
                    require: p.require.clone(),
                    file: p.root.join("composer.json"),
                }),
                None => read_app_composer(&x.path),
            })
            .collect();

        // package name -> the modules it provides.
        let mut modules_of_pkg: std::collections::HashMap<&str, Vec<&ModuleName>> =
            std::collections::HashMap::new();
        for (i, info) in infos.iter().enumerate() {
            if let Some(info) = info {
                modules_of_pkg.entry(info.name.as_str()).or_default().push(&self.index.modules[i].name);
            }
        }
        let by_name: std::collections::HashMap<&ModuleName, &Module> =
            self.index.modules.iter().map(|x| (&x.name, x)).collect();

        let my_info = &infos[me];

        // Forward: sequence entries, then composer requires resolved to modules.
        let mut depends_on: Vec<DepEdge> = Vec::new();
        let seq_source = |x: &Module| Source {
            module: x.name.clone(),
            file: x.path.join("etc/module.xml"),
            line: 0,
            area: Area::Global,
        };
        for target in &m.sequence {
            add_dep_edge(&mut depends_on, target, &by_name, true, false, seq_source(m));
        }
        let mut other_requires = Vec::new();
        if let Some(info) = my_info {
            let src = Source {
                module: m.name.clone(),
                file: info.file.clone(),
                line: 0,
                area: Area::Global,
            };
            for req in &info.require {
                match modules_of_pkg.get(req.as_str()) {
                    Some(mods) => {
                        for t in mods {
                            if *t != &m.name {
                                add_dep_edge(&mut depends_on, t, &by_name, false, true, src.clone());
                            }
                        }
                    }
                    None => other_requires.push(req.clone()),
                }
            }
        }

        // Reverse: every module whose sequence names this one, or whose package requires
        // this one's package.
        let my_pkg = my_info.as_ref().map(|i| i.name.as_str());
        let mut depended_on_by: Vec<DepEdge> = Vec::new();
        for (i, other) in self.index.modules.iter().enumerate() {
            if other.name == m.name {
                continue;
            }
            if other.sequence.contains(&m.name) {
                add_dep_edge(&mut depended_on_by, &other.name, &by_name, true, false, seq_source(other));
            }
            if let (Some(info), Some(mine)) = (&infos[i], my_pkg) {
                // Siblings bundled in the same package don't require each other.
                if info.name != mine && info.require.iter().any(|r| r == mine) {
                    let src = Source {
                        module: other.name.clone(),
                        file: info.file.clone(),
                        line: 0,
                        area: Area::Global,
                    };
                    add_dep_edge(&mut depended_on_by, &other.name, &by_name, false, true, src);
                }
            }
        }

        depends_on.sort_by(|a, b| a.module.cmp(&b.module));
        depended_on_by.sort_by(|a, b| a.module.cmp(&b.module));
        Ok(ModuleDeps {
            module: m.name.clone(),
            package: my_pkg.map(str::to_string),
            depends_on,
            depended_on_by,
            other_requires,
        })
    }

    fn mq_index(&self) -> &breadth::MqIndex {
        self.mq.get_or_init(|| breadth::MqIndex::build(&self.index.modules))
    }

    fn gql_index(&self) -> &breadth::GqlIndex {
        self.gql.get_or_init(|| breadth::GqlIndex::build(&self.index.modules))
    }

    /// GraphQL schema types merged from every module's `schema.graphqls` (fields union by
    /// name across modules, matching Magento's schema stitching — `Query` is assembled
    /// from dozens of modules, each field tagged with its declaring module). Filtered by a
    /// case-insensitive name substring, sorted by name. Static.
    pub fn graphql_types(&self, filter: Option<&str>) -> Vec<GqlType> {
        self.gql_index().types(filter)
    }

    /// One GraphQL type by exact name, fully merged.
    pub fn graphql_type(&self, name: &str) -> Option<GqlType> {
        self.gql_index().type_(name)
    }

    /// Message-queue topics from `communication.xml` (with handlers), optionally filtered
    /// by a name substring, sorted by name. Static.
    pub fn queue_topics(&self, filter: Option<&str>) -> Vec<MqTopic> {
        self.mq_index().topics(filter)
    }

    /// The full journey of one topic (exact name): definition + handlers, its publisher,
    /// and every queue its messages reach — via the publisher's direct `queue=` and/or
    /// each enabled exchange binding whose AMQP pattern matches — with that queue's
    /// consumers. The "who processes a message published on this topic" answer, joined
    /// from `communication.xml` + `queue_publisher.xml` + `queue_topology.xml` +
    /// `queue_consumer.xml`. (Consumers are joined by queue name; a consumer's declared
    /// connection is reported, not matched.)
    pub fn queue_topic(&self, name: &str) -> Option<MqTopicRoute> {
        self.mq_index().topic_route(name)
    }

    /// The database configuration from `app/etc/env.php` (`db` section).
    pub fn db_config(&self) -> Result<DbConfig> {
        let env = deploy::read_env(&self.index.root)?;
        Ok(deploy::db_config(&env))
    }

    /// Resolve the system configuration into a queryable [`ConfigSet`]. Always includes the
    /// static sources (config.xml defaults, config.php/env.php `system`, `CONFIG__*` env
    /// vars). With `include_db`, also reads `core_config_data` (requires the `db` feature and
    /// a reachable database; the DB layer sits above config.xml and below the `system`
    /// overrides).
    pub fn config(&self, include_db: bool) -> Result<ConfigSet> {
        let env = deploy::read_env(&self.index.root).unwrap_or(phparray::PhpValue::Null);
        let config_php =
            deploy::read_config_php(&self.index.root).unwrap_or(phparray::PhpValue::Null);
        let db_values = if include_db { self.fetch_core_config_data()? } else { Vec::new() };
        let order = self.system_config_source_order();
        Ok(ConfigSet::build(&self.index.root, &self.index.modules, &env, &config_php, db_values, &order))
    }

    /// The recognized system-config sources in ascending `sortOrder`, as declared by the
    /// `systemConfigSourceAggregated` virtual type in di.xml. This is what makes config
    /// precedence architecture-faithful instead of hardcoded: a module that re-orders or
    /// adds a source via di.xml is honored. Falls back to Magento's default
    /// modular → dynamic → initial order if the declaration can't be read.
    fn system_config_source_order(&self) -> Vec<sysconfig::SysCfgSource> {
        let agg = ClassName::new("systemConfigSourceAggregated");
        let args = self.args_of(&agg, Area::Global, &mut std::collections::HashSet::new());
        let Some((ArgValue::Array(items), _)) = args.get("sources") else {
            return sysconfig::DEFAULT_SOURCE_ORDER.to_vec();
        };

        let mut ranked: Vec<(i64, sysconfig::SysCfgSource)> = Vec::new();
        for item in items {
            let ArgValue::Array(fields) = &item.value else { continue };
            let source = fields.iter().find(|f| f.key == "source").and_then(|f| match &f.value {
                ArgValue::Object(c) => Some(c),
                _ => None,
            });
            let sort_order = fields
                .iter()
                .find(|f| f.key == "sortOrder")
                .and_then(|f| match &f.value {
                    ArgValue::Scalar { text, .. } => text.trim().parse::<i64>().ok(),
                    _ => None,
                })
                .unwrap_or(0);
            if let Some(kind) = source.and_then(|c| self.classify_config_source(c)) {
                ranked.push((sort_order, kind));
            }
        }

        if ranked.is_empty() {
            return sysconfig::DEFAULT_SOURCE_ORDER.to_vec();
        }
        ranked.sort_by_key(|(s, _)| *s);
        ranked.into_iter().map(|(_, k)| k).collect()
    }

    /// Map a di.xml config-source object (following virtual-type indirection to a concrete
    /// class) to a recognized [`SysCfgSource`], or `None` for an unrecognized custom source.
    fn classify_config_source(&self, class: &ClassName) -> Option<sysconfig::SysCfgSource> {
        use sysconfig::SysCfgSource;
        let cfg = self.di_index().config(Area::Global);
        let mut cur = class.clone();
        let mut seen = std::collections::HashSet::new();
        while let Some(vt) = cfg.virtual_types.get(&cur) {
            if !seen.insert(cur.clone()) {
                break;
            }
            cur = vt.value.clone();
        }
        match cur.as_str() {
            "Magento\\Config\\App\\Config\\Source\\ModularConfigSource" => Some(SysCfgSource::Modular),
            "Magento\\Config\\App\\Config\\Source\\RuntimeConfigSource" => Some(SysCfgSource::Dynamic),
            "Magento\\Framework\\App\\Config\\InitialConfigSource" => Some(SysCfgSource::Initial),
            _ => None,
        }
    }

    #[cfg(feature = "db")]
    fn fetch_core_config_data(&self) -> Result<Vec<(String, String, String)>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        db::fetch_config(conn, &cfg.table_prefix).map_err(Error::Db)
    }

    /// Seconds since the last successful cron job finished (DB clock).
    #[cfg(feature = "db")]
    fn fetch_cron_last_success(&self) -> Result<Option<i64>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        db::fetch_cron_last_success(conn, &cfg.table_prefix).map_err(Error::Db)
    }

    #[cfg(not(feature = "db"))]
    fn fetch_cron_last_success(&self) -> Result<Option<i64>> {
        Err(Error::Db("the `db` feature is not enabled in this build".to_string()))
    }

    /// `(websites, store groups, store views)` counts, admin scopes excluded.
    #[cfg(feature = "db")]
    fn fetch_scope_counts(&self) -> Result<(usize, usize, usize)> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        db::fetch_scope_counts(conn, &cfg.table_prefix).map_err(Error::Db)
    }

    #[cfg(not(feature = "db"))]
    fn fetch_scope_counts(&self) -> Result<(usize, usize, usize)> {
        Err(Error::Db("the `db` feature is not enabled in this build".to_string()))
    }

    /// `(theme_id, parent_id, theme_path, area)` rows from the `theme` table.
    #[cfg(feature = "db")]
    fn fetch_theme_rows(&self) -> Result<Vec<(u32, Option<u32>, Option<String>, String)>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        db::fetch_themes(conn, &cfg.table_prefix).map_err(Error::Db)
    }

    #[cfg(not(feature = "db"))]
    fn fetch_theme_rows(&self) -> Result<Vec<(u32, Option<u32>, Option<String>, String)>> {
        Err(Error::Db("the `db` feature is not enabled in this build".to_string()))
    }

    #[cfg(not(feature = "db"))]
    fn fetch_core_config_data(&self) -> Result<Vec<(String, String, String)>> {
        Err(Error::Db("the `db` feature is not enabled in this build".to_string()))
    }

    /// URL rewrites from the `url_rewrite` table (live DB). These are runtime data with no
    /// static source, so this needs the `db` feature and a reachable database. Filters
    /// (request/target path substring, store code, redirects-only) and `limit` are pushed
    /// into SQL; the result flags whether more rows existed beyond `limit`.
    #[cfg(feature = "db")]
    pub fn url_rewrites(
        &self,
        path_filter: Option<&str>,
        store: Option<&str>,
        redirects_only: bool,
        limit: usize,
    ) -> Result<UrlRewrites> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (rewrites, truncated) =
            db::fetch_url_rewrites(conn, &cfg.table_prefix, path_filter, store, redirects_only, limit)
                .map_err(Error::Db)?;
        Ok(UrlRewrites { rewrites, truncated })
    }

    /// Stub when the `db` feature is disabled: URL rewrites are DB-only.
    #[cfg(not(feature = "db"))]
    pub fn url_rewrites(
        &self,
        _path_filter: Option<&str>,
        _store: Option<&str>,
        _redirects_only: bool,
        _limit: usize,
    ) -> Result<UrlRewrites> {
        Err(Error::Db("the `db` feature is not enabled in this build".to_string()))
    }

    /// A [`Decryptor`] loaded with the `crypt.key`(s) from `env.php`, to decrypt encrypted
    /// config values (ChaCha20-Poly1305).
    pub fn decryptor(&self) -> Result<Decryptor> {
        let env = deploy::read_env(&self.index.root)?;
        Ok(Decryptor::new(deploy::crypt_keys(&env)))
    }

    /// Redis/Valkey usages from `app/etc/env.php` (cache, page cache, session).
    pub fn redis_config(&self) -> Result<RedisConfig> {
        let env = deploy::read_env(&self.index.root)?;
        Ok(deploy::redis_config(&env))
    }

    /// Session storage configuration (`session` section of `env.php`): the save handler and,
    /// for Redis/file handlers, where sessions live.
    pub fn session_config(&self) -> Result<SessionConfig> {
        let env = deploy::read_env(&self.index.root)?;
        Ok(deploy::session_config(&env))
    }

    /// Cache configuration (`cache`/`cache_types` of `env.php`): the backend per frontend and
    /// the per-type enable flags.
    pub fn cache_config(&self) -> Result<CacheConfig> {
        let env = deploy::read_env(&self.index.root)?;
        Ok(deploy::cache_config(&env))
    }

    /// Locking backend (`lock` section of `env.php`): the provider and its settings.
    pub fn lock_config(&self) -> Result<LockConfig> {
        let env = deploy::read_env(&self.index.root)?;
        Ok(deploy::lock_config(&env))
    }

    /// Message-queue connections (`queue` section of `env.php`).
    pub fn queue_config(&self) -> Result<QueueConfig> {
        let env = deploy::read_env(&self.index.root)?;
        Ok(deploy::queue_config(&env))
    }

    /// Ping every configured Redis/Valkey instance (raw RESP — no client crate, works over
    /// TCP and unix sockets), returning one result per instance.
    pub fn ping_redis(&self) -> Result<Vec<RedisPing>> {
        let cfg = self.redis_config()?;
        Ok(cfg.instances.iter().map(redis::ping).collect())
    }

    /// Test a database connection (`None` = the `default` connection) by connecting with the
    /// `env.php` credentials and querying the server version. Requires the `db` feature.
    #[cfg(feature = "db")]
    pub fn ping_db(&self, connection: Option<&str>) -> Result<DbPing> {
        let cfg = self.db_config()?;
        let conn = match connection {
            Some(name) => cfg.connections.iter().find(|c| c.name == name),
            None => cfg
                .connections
                .iter()
                .find(|c| c.name == "default")
                .or_else(|| cfg.connections.first()),
        }
        .ok_or_else(|| Error::Parse {
            file: self.index.root.join("app/etc/env.php"),
            detail: match connection {
                Some(n) => format!("no db connection named `{n}`"),
                None => "no db connections configured".to_string(),
            },
        })?;
        Ok(db::ping(conn))
    }

    /// Controller actions (subroutes) for `area` (`Frontend` or `Adminhtml`): every concrete
    /// `Controller/.../Action.php` in a route's modules, mapped to its `frontName/.../action`
    /// URL. Optionally filtered by a URL substring (which also avoids parsing non-matching
    /// controllers). Only classes implementing a Magento action base are included.
    pub fn actions(&self, area: Area, url_filter: Option<&str>) -> Vec<ControllerAction> {
        let admin = area == Area::Adminhtml;
        let routes = self.routes(area);
        let paths: std::collections::HashMap<&ModuleName, &Path> =
            self.index.modules.iter().map(|m| (&m.name, m.path.as_path())).collect();

        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for route in &routes {
            for module in &route.modules {
                let Some(&mpath) = paths.get(module) else { continue };
                let ns = module.as_str().replace('_', "\\");
                let base = if admin {
                    mpath.join("Controller/Adminhtml")
                } else {
                    mpath.join("Controller")
                };
                if !base.is_dir() {
                    continue;
                }
                let ctx = ActionScan {
                    front_name: &route.front_name,
                    ns: &ns,
                    admin,
                    area,
                    module,
                    filter: url_filter,
                    resolver: &self.index.resolver,
                };
                scan_actions(&ctx, &base, &base, &mut seen, &mut out);
            }
        }
        out.sort_by(|a, b| a.url.cmp(&b.url));
        out
    }

    fn events_index(&self) -> &breadth::EventIndex {
        self.events.get_or_init(|| breadth::EventIndex::build(&self.index.modules))
    }

    /// The flagship: full DI resolution of `class` in a single `area` — the concrete type,
    /// preference chain, virtual-type indirection, merged constructor arguments, and the
    /// plugin chain, with provenance throughout.
    pub fn resolve(&self, class: &ClassName, area: Area) -> Result<Resolution> {
        let pref = self.preference(class, area)?;
        let concrete = pref.concrete.clone();
        let cfg = self.di_index().config(area);

        // If the concrete is a virtual type, follow the type= chain to the real class.
        let instantiates = {
            let mut cur = concrete.clone();
            let mut seen = std::collections::HashSet::new();
            let mut is_virtual = false;
            while let Some(vt) = cfg.virtual_types.get(&cur) {
                if !seen.insert(cur.clone()) {
                    break;
                }
                is_virtual = true;
                cur = vt.value.clone();
            }
            is_virtual.then_some(cur)
        };

        let arguments = self.resolve_arguments(&concrete, area);
        let plugins = self.plugins(class, area)?;

        // Ancestors/interfaces that actually contributed plugins or arguments.
        let contributing_ancestors = self
            .index
            .resolver
            .ancestors(&concrete)
            .into_iter()
            .filter(|a| cfg.plugins.contains_key(a) || cfg.type_args.contains_key(a))
            .collect();

        Ok(Resolution {
            requested: class.clone(),
            area,
            concrete,
            preference_chain: pref.chain,
            instantiates,
            plugins,
            arguments,
            contributing_ancestors,
        })
    }

    /// Merged constructor arguments for `name` in `area`: virtual-type base args overlaid by
    /// the virtual type's own; for a real type, parent-type args (along the PHP ancestor
    /// chain) overlaid by the type's own. Per-argument last-wins; sorted by name.
    fn resolve_arguments(&self, name: &ClassName, area: Area) -> Vec<Argument> {
        let map = self.args_of(name, area, &mut std::collections::HashSet::new());
        let mut v: Vec<Argument> = map
            .into_iter()
            .map(|(name, (value, source))| Argument { name, value, source })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    fn args_of(
        &self,
        name: &ClassName,
        area: Area,
        seen: &mut std::collections::HashSet<ClassName>,
    ) -> std::collections::HashMap<String, (ArgValue, Source)> {
        let mut merged = std::collections::HashMap::new();
        if !seen.insert(name.clone()) {
            return merged; // cycle guard
        }
        let cfg = self.di_index().config(area);

        if let Some(vt) = cfg.virtual_types.get(name) {
            // Virtual type: inherit the base type's args, then overlay our own.
            merged = self.args_of(&vt.value, area, seen);
        } else {
            // Real type: merge parent-type args (distant ancestor first), self overrides.
            let mut chain = self.index.resolver.ancestors(name);
            chain.reverse();
            for ancestor in &chain {
                merge_args_into(&mut merged, cfg.type_args.get(ancestor));
            }
        }

        merge_args_into(&mut merged, cfg.type_args.get(name));
        merged
    }

    /// [`resolve`](Magento::resolve) across every [`Area`]. The caller renders it directly
    /// (`--all-areas`) or via [`ByArea::deltas`] (the default collapsed-diff view).
    pub fn resolve_all(&self, class: &ClassName) -> Result<ByArea<Resolution>> {
        let _ = class;
        todo!()
    }
}

/// The connection used for live introspection: `default`, else the first configured one.
#[cfg(feature = "db")]
fn default_connection(cfg: &DbConfig) -> Result<&DbConnection> {
    cfg.connections
        .iter()
        .find(|c| c.name == "default")
        .or_else(|| cfg.connections.first())
        .ok_or_else(|| Error::Db("no db connection configured in env.php".to_string()))
}

/// Build the execution chain (onion) per intercepted method from a run-ordered plugin list:
/// before (ascending) → around (nested ascending) → target → around (unwind) → after
/// (descending). `plugins` must already be in canonical run order; we only filter, so the
/// order is preserved. Disabled plugins are excluded.
struct ActionScan<'a> {
    front_name: &'a str,
    ns: &'a str,
    admin: bool,
    area: Area,
    module: &'a ModuleName,
    filter: Option<&'a str>,
    resolver: &'a resolver::ClassResolver,
}

/// Recursively walk a `Controller/` tree, emitting concrete action classes mapped to URLs.
fn scan_actions(
    ctx: &ActionScan,
    base: &Path,
    dir: &Path,
    seen: &mut std::collections::HashSet<String>,
    out: &mut Vec<ControllerAction>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            // For frontend, the Adminhtml subtree holds admin actions — skip it.
            if !ctx.admin && path.file_name().and_then(|n| n.to_str()) == Some("Adminhtml") {
                continue;
            }
            scan_actions(ctx, base, &path, seen, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("php") {
            continue;
        }
        // Path under the Controller root, e.g. ["Product", "View"].
        let Ok(rel) = path.strip_prefix(base) else { continue };
        let stem = rel.with_extension("");
        let parts: Vec<String> = stem
            .components()
            .filter_map(|c| c.as_os_str().to_str().map(String::from))
            .collect();
        if parts.is_empty() {
            continue;
        }

        let url = std::iter::once(ctx.front_name.to_lowercase())
            .chain(parts.iter().map(|p| p.to_lowercase()))
            .collect::<Vec<_>>()
            .join("/");
        if ctx.filter.is_some_and(|f| !url.contains(f)) {
            continue; // skip parsing controllers we won't show
        }

        let mut class = format!("{}\\Controller", ctx.ns);
        if ctx.admin {
            class.push_str("\\Adminhtml");
        }
        for p in &parts {
            class.push('\\');
            class.push_str(p);
        }
        if !seen.insert(class.clone()) {
            continue;
        }
        let class = ClassName::new(class);
        if ctx.resolver.is_action(&class) {
            out.push(ControllerAction {
                url,
                class,
                area: ctx.area,
                module: ctx.module.clone(),
                source: Source { module: ctx.module.clone(), file: path, line: 0, area: ctx.area },
            });
        }
    }
}

/// A module's composer identity for the `deps` graph.
struct DepPkgInfo {
    name: String,
    require: Vec<String>,
    file: std::path::PathBuf,
}

/// Read an app/code module's own `composer.json` (they're not in installed.json).
fn read_app_composer(dir: &Path) -> Option<DepPkgInfo> {
    #[derive(serde::Deserialize)]
    struct Cj {
        name: Option<String>,
        #[serde(default)]
        require: std::collections::HashMap<String, serde::de::IgnoredAny>,
    }
    let file = dir.join("composer.json");
    let cj: Cj = serde_json::from_str(&std::fs::read_to_string(&file).ok()?).ok()?;
    let mut require: Vec<String> = cj.require.into_keys().collect();
    require.sort();
    Some(DepPkgInfo { name: cj.name.unwrap_or_default(), require, file })
}

/// Append (or merge into) an edge to `module`, OR-ing the `via_*` flags. The first source
/// wins (sequence edges are added first, so a both-ways dependency cites module.xml).
fn add_dep_edge(
    edges: &mut Vec<DepEdge>,
    module: &ModuleName,
    by_name: &std::collections::HashMap<&ModuleName, &Module>,
    via_sequence: bool,
    via_composer: bool,
    source: Source,
) {
    match edges.iter_mut().find(|e| &e.module == module) {
        Some(e) => {
            e.via_sequence |= via_sequence;
            e.via_composer |= via_composer;
        }
        None => {
            let target = by_name.get(module);
            edges.push(DepEdge {
                module: module.clone(),
                via_sequence,
                via_composer,
                installed: target.is_some(),
                enabled: target.is_some_and(|t| t.enabled),
                source,
            });
        }
    }
}

/// Context for one argument-tree scan in [`Magento::uses`]: what to match and which
/// consumer/argument the hits belong to.
struct UseScan<'a> {
    class: &'a ClassName,
    /// `class\Proxy` — the generated lazy wrapper counts as an injection of the class.
    proxy: &'a ClassName,
    consumer: &'a ClassName,
    consumer_is_virtual: bool,
    argument: &'a str,
    keep: &'a dyn Fn(&Source) -> bool,
}

/// Walk an argument value looking for references to the scanned class: `object` values
/// (the class or its `\Proxy`) and `string` values spelling its name, recursing into array
/// items (each with its own provenance and key path).
fn scan_arg_for_class(
    value: &ArgValue,
    source: &Source,
    path: &mut Vec<String>,
    scan: &UseScan<'_>,
    out: &mut Vec<model::InjectionSite>,
) {
    let mut hit = |declared: ClassName, as_string: bool| {
        if (scan.keep)(source) {
            out.push(model::InjectionSite {
                consumer: scan.consumer.clone(),
                consumer_is_virtual: scan.consumer_is_virtual,
                argument: scan.argument.to_string(),
                item_path: path.clone(),
                declared,
                as_string,
                source: source.clone(),
            });
        }
    };
    match value {
        ArgValue::Object(c) => {
            if c == scan.class || c == scan.proxy {
                hit(c.clone(), false);
            }
        }
        ArgValue::Scalar { xsi_type, text } => {
            if xsi_type == "string" && text.trim().trim_start_matches('\\') == scan.class.as_str() {
                hit(scan.class.clone(), true);
            }
        }
        ArgValue::Array(items) => {
            for item in items {
                path.push(item.key.clone());
                scan_arg_for_class(&item.value, &item.source, path, scan, out);
                path.pop();
            }
        }
        ArgValue::Null => {}
    }
}

/// Deep-merge one type's declared arguments into the accumulator (array args merge
/// item-by-item; scalars/objects replace).
fn merge_args_into(
    merged: &mut std::collections::HashMap<String, (ArgValue, Source)>,
    args: Option<&std::collections::HashMap<String, di::LocatedArg>>,
) {
    let Some(args) = args else { return };
    for (k, la) in args {
        let value = match merged.get(k) {
            Some((existing, _)) => existing.merged_with(&la.value),
            None => la.value.clone(),
        };
        merged.insert(k.clone(), (value, la.source.clone()));
    }
}

fn chains_from(plugins: &[Plugin], only: Option<&str>) -> Vec<MethodChain> {
    use std::collections::BTreeMap;

    let mut by_method: BTreeMap<String, Vec<(InterceptKind, ChainPluginRef)>> = BTreeMap::new();
    for p in plugins {
        if p.disabled {
            continue;
        }
        for m in &p.methods {
            if only.is_some_and(|f| f != m.target) {
                continue;
            }
            let r = ChainPluginRef {
                name: p.name.clone(),
                class: p.class.clone(),
                plugin_method: m.plugin_method.clone(),
                sort_order: p.sort_order,
                declared_on: p.declared_on.clone(),
                source: p.source.clone(),
                areas: p.areas.clone(),
            };
            by_method.entry(m.target.clone()).or_default().push((m.kind, r));
        }
    }

    let mut chains = Vec::new();
    for (method, parts) in by_method {
        let pick = |kind: InterceptKind| -> Vec<ChainPluginRef> {
            parts.iter().filter(|(k, _)| *k == kind).map(|(_, r)| r.clone()).collect()
        };
        let befores = pick(InterceptKind::Before);
        let arounds = pick(InterceptKind::Around);
        let afters = pick(InterceptKind::After);

        let mut steps = Vec::new();
        for r in &befores {
            steps.push(ChainStep::Before(r.clone()));
        }
        for r in &arounds {
            steps.push(ChainStep::AroundEnter(r.clone()));
        }
        steps.push(ChainStep::Target);
        for r in arounds.iter().rev() {
            steps.push(ChainStep::AroundExit(r.clone()));
        }
        for r in afters.iter().rev() {
            steps.push(ChainStep::After(r.clone()));
        }
        chains.push(MethodChain { method, steps });
    }
    chains
}
