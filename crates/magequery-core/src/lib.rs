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
mod doctor;
mod eav;
mod graphql;
mod index;
mod parse;
mod php;
mod phparray;
mod redis;
mod resolver;
mod sysconfig;
mod whatis;

pub use error::{Diagnostic, Error, Result, Severity};
pub use ids::{Area, ClassName, ConfigPath, EventName, ModuleName};
pub use model::{
    AclResource, AdminRole, AdminRule, AdminUser, ArgItem, ArgValue, Argument, ByArea,
    ChainPluginRef, ChainStep, ConfigSourceKind, ConfigValue,
    ConsoleCommand, ControllerAction, CronJob, CronJobLive, CronJobs, CronRun,
    DbColumn, DbConfig, DbConnection, DbConstraint, DbIndex, DbPing,
    DbTable, DepEdge, DoctorFinding, DoctorLint, DoctorReport, EavAttribute, EavAttributeCard,
    EavCatalogFlags, EavEntityType, EavScope, EavSetMembership, EavSetupKind, EavSetupProp,
    EavSetupRef, EavValueKind, EmailTemplate,
    EmailTemplateOverride, ExtendedType, ExtensionAttribute,
    ExtensionJoin, GqlArg, GqlField, GqlKind, GqlType,
    Indexer, IndexerLive, InstanceInfo, InterceptKind,
    LayoutContribution, LayoutLayer, LayoutOp, LayoutOpKind, LayoutView,
    MenuItem, MethodChain, Module, ModuleCheck, ModuleDeps, Patch, PatchKind, Patches,
    MviewSubscription, Observer,
    BundleOption, BundleSelection, Category, CategoryHit, CategoryIndexCount,
    CategoryIndexedProduct, CategoryProduct,
    CategoryTreeNode, CategoryVisibilityIssue,
    ChildPrice, Customer, CustomerAddress, CustomerHit, CustomerNewsletter, CustomerOrders,
    IndexedPrice, Order, OrderAddress, OrderComment, OrderDocument, OrderHit,
    OrderItem, OrderPayment, OrderShipment, OrderTotal, OrderTransaction,
    Preference, PreferenceStep, Plugin,
    PluginMethod, Product,
    ProductCategory, ProductChild,
    ProductHit, ProductLegacyStock, ProductPrices, ProductRewrite, ProductScopeValue,
    ProductSourceStock, ProductValue,
    RedisConfig, RedisInstance, RedisPing, RulePrice, TierPrice,
    Resolution, Route, SchemaDrift, TableColumn, TranslationEntry, TranslationLayer,
    TranslationMatch, Translations, UiComponentContribution, UiComponentOp, UiComponentView,
    UnregisteredModule, WebapiRoute, Widget,
    WidgetParam,
};
pub use model::{
    CacheConfig, CacheFrontend, CacheType, InjectionSite, LockConfig, MqConsumer, MqHandler,
    MqPublisher, MqRoute, MqTopic, MqTopicRoute, MqVia, QueueBacklog, QueueConfig, QueueConnection,
    SessionConfig, SystemField, UrlRewrite, UrlRewrites, UseRef, Uses, Whatis,
};
pub use model::{CatalogAttribute, CatalogAttributeGroup, ClassRef};
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
    menu: OnceLock<breadth::MenuIndex>,
    ext_attrs: OnceLock<breadth::ExtAttrIndex>,
    layout: OnceLock<breadth::LayoutIndex>,
    ui_components: OnceLock<breadth::UiComponentIndex>,
    eav_setup: OnceLock<eav::EavSetupIndex>,
    widgets: OnceLock<breadth::WidgetIndex>,
    email_templates: OnceLock<breadth::EmailTemplateIndex>,
    catalog_attrs: OnceLock<breadth::CatalogAttrIndex>,
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
            menu: OnceLock::new(),
            ext_attrs: OnceLock::new(),
            layout: OnceLock::new(),
            ui_components: OnceLock::new(),
            eav_setup: OnceLock::new(),
            widgets: OnceLock::new(),
            email_templates: OnceLock::new(),
            catalog_attrs: OnceLock::new(),
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
    pub fn cron_jobs(&self, group: Option<&str>, include_db: bool) -> Result<CronJobs> {
        let mut jobs =
            self.cron.get_or_init(|| breadth::CronIndex::build(&self.index.modules)).jobs(group);
        let mut orphaned_codes = Vec::new();
        if include_db {
            orphaned_codes = self.attach_cron_live(&mut jobs)?;
            if group.is_some() {
                // Orphans can't be attributed to a group — only meaningful unfiltered.
                orphaned_codes.clear();
            }
        }
        Ok(CronJobs { jobs, orphaned_codes })
    }

    /// A job's recent `cron_schedule` rows (runs, errors, misses — not future pendings),
    /// newest first.
    #[cfg(feature = "db")]
    pub fn cron_history(&self, job_code: &str, limit: usize) -> Result<Vec<CronRun>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let rows = db::fetch_cron_history(conn, &cfg.table_prefix, job_code, limit)
            .map_err(Error::Db)?;
        Ok(rows
            .into_iter()
            .map(|(status, scheduled_at, executed_at, finished_at, duration_secs, messages)| {
                CronRun {
                    status,
                    scheduled_at,
                    executed_at,
                    finished_at,
                    duration_secs,
                    messages: messages.filter(|m| !m.is_empty()),
                }
            })
            .collect())
    }

    /// Overlay `cron_schedule` stats onto the job list; returns the job codes present in
    /// the table that no crontab.xml defines (removed modules' leftover schedules).
    #[cfg(feature = "db")]
    fn attach_cron_live(&self, jobs: &mut [CronJob]) -> Result<Vec<String>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let stats = db::fetch_cron_stats(conn, &cfg.table_prefix).map_err(Error::Db)?;
        for job in jobs.iter_mut() {
            let s = stats.iter().find(|s| s.job_code == job.name);
            job.live = Some(match s {
                Some(s) => CronJobLive {
                    last_status: s.last_status.clone(),
                    last_run: s.last_run.clone(),
                    last_run_secs: s.last_run_secs,
                    last_duration_secs: s.last_duration_secs,
                    last_error: s.last_error.clone(),
                    next_scheduled: s.next_scheduled.clone(),
                    pending: s.pending,
                    running: s.running,
                    success: s.success,
                    error: s.error,
                    missed: s.missed,
                },
                None => CronJobLive {
                    last_status: None,
                    last_run: None,
                    last_run_secs: None,
                    last_duration_secs: None,
                    last_error: None,
                    next_scheduled: None,
                    pending: 0,
                    running: 0,
                    success: 0,
                    error: 0,
                    missed: 0,
                },
            });
        }
        let known: std::collections::HashSet<&str> =
            jobs.iter().map(|j| j.name.as_str()).collect();
        let mut orphans: Vec<String> = stats
            .iter()
            .filter(|s| !known.contains(s.job_code.as_str()))
            .map(|s| s.job_code.clone())
            .collect();
        orphans.sort();
        Ok(orphans)
    }

    #[cfg(not(feature = "db"))]
    fn attach_cron_live(&self, _jobs: &mut [CronJob]) -> Result<Vec<String>> {
        Err(Error::Db("the `db` feature is not enabled in this build".to_string()))
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

    /// Presence-level drift between the declared schema (`db_schema.xml`) and the live
    /// database: tables/columns declared but missing live (what `setup:upgrade` would
    /// create) and live-but-undeclared ones (legacy install scripts, non-declarative
    /// modules). Runtime-managed tables — mview `*_cl` changelogs, `sequence_*`, and the
    /// setup framework's own bookkeeping — are excluded from the undeclared side and
    /// counted instead. Requires the `db` feature and a reachable database.
    pub fn schema_drift(&self) -> Result<SchemaDrift> {
        let live = self.fetch_live_schema()?;
        let declared = self.schema_index().tables(None);
        let whitelist = self.schema_whitelist();

        let mut drift = SchemaDrift {
            missing_tables: Vec::new(),
            missing_columns: Vec::new(),
            would_drop_tables: Vec::new(),
            would_drop_columns: Vec::new(),
            not_whitelisted_tables: Vec::new(),
            not_whitelisted_columns: Vec::new(),
            undeclared_tables: Vec::new(),
            undeclared_columns: Vec::new(),
            runtime_tables_skipped: 0,
        };

        let declared_names: std::collections::HashSet<&str> =
            declared.iter().map(|t| t.name.as_str()).collect();
        let wl_cols = |table: &str| whitelist.get(table);

        for t in &declared {
            // Declared elements must be whitelisted, or their future removal is inert.
            match wl_cols(&t.name) {
                None => drift.not_whitelisted_tables.push(t.name.clone()),
                Some(wl) => {
                    for c in &t.columns {
                        if !wl.contains(c.name.as_str()) {
                            drift.not_whitelisted_columns.push(TableColumn {
                                table: t.name.clone(),
                                column: c.name.clone(),
                            });
                        }
                    }
                }
            }
            match live.get(&t.name) {
                None => drift.missing_tables.push(t.name.clone()),
                Some(live_cols) => {
                    let live_set: std::collections::HashSet<&str> =
                        live_cols.iter().map(String::as_str).collect();
                    for c in &t.columns {
                        if !live_set.contains(c.name.as_str()) {
                            drift.missing_columns.push(TableColumn {
                                table: t.name.clone(),
                                column: c.name.clone(),
                            });
                        }
                    }
                    let declared_cols: std::collections::HashSet<&str> =
                        t.columns.iter().map(|c| c.name.as_str()).collect();
                    for c in live_cols {
                        if declared_cols.contains(c.as_str()) {
                            continue;
                        }
                        // Whitelisted = the declarative system owns it; no longer
                        // declared + still live means setup:upgrade would drop it.
                        if wl_cols(&t.name).is_some_and(|wl| wl.contains(c.as_str())) {
                            drift.would_drop_columns.push(TableColumn {
                                table: t.name.clone(),
                                column: c.clone(),
                            });
                        } else {
                            drift.undeclared_columns.push(TableColumn {
                                table: t.name.clone(),
                                column: c.clone(),
                            });
                        }
                    }
                }
            }
        }

        for name in live.keys() {
            if declared_names.contains(name.as_str()) {
                continue;
            }
            // Runtime/bookkeeping tables first: Magento's declarative diff ignores these
            // even when a whitelist names them (MSI's whitelists infamously include
            // patch_list — it still never gets dropped).
            if is_runtime_table(name) {
                drift.runtime_tables_skipped += 1;
            } else if whitelist.contains_key(name.as_str()) {
                drift.would_drop_tables.push(name.clone());
            } else {
                drift.undeclared_tables.push(name.clone());
            }
        }

        drift.missing_tables.sort();
        drift.would_drop_tables.sort();
        drift.not_whitelisted_tables.sort();
        drift.undeclared_tables.sort();
        for v in [
            &mut drift.missing_columns,
            &mut drift.would_drop_columns,
            &mut drift.not_whitelisted_columns,
            &mut drift.undeclared_columns,
        ] {
            v.sort_by(|a, b| (&a.table, &a.column).cmp(&(&b.table, &b.column)));
        }
        Ok(drift)
    }

    /// The union of every enabled module's `etc/db_schema_whitelist.json`: table → the
    /// column names the declarative system is allowed to manage (drop/alter). Generated
    /// by `setup:db-declaration:generate-whitelist`, one file per module.
    fn schema_whitelist(
        &self,
    ) -> std::collections::HashMap<String, std::collections::HashSet<String>> {
        let mut out: std::collections::HashMap<String, std::collections::HashSet<String>> =
            std::collections::HashMap::new();
        for m in self.index.modules.iter().filter(|m| m.enabled) {
            let Ok(text) = std::fs::read_to_string(m.path.join("etc/db_schema_whitelist.json"))
            else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
            let Some(tables) = value.as_object() else { continue };
            for (table, entry) in tables {
                let cols = out.entry(table.clone()).or_default();
                if let Some(columns) = entry.get("column").and_then(|c| c.as_object()) {
                    cols.extend(columns.keys().cloned());
                }
            }
        }
        out
    }

    #[cfg(feature = "db")]
    fn fetch_live_schema(&self) -> Result<std::collections::HashMap<String, Vec<String>>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        db::fetch_live_schema(conn, &cfg.table_prefix).map_err(Error::Db)
    }

    #[cfg(not(feature = "db"))]
    fn fetch_live_schema(&self) -> Result<std::collections::HashMap<String, Vec<String>>> {
        Err(Error::Db("the `db` feature is not enabled in this build".to_string()))
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
    /// definition, dependencies, and the tables whose changes feed it. Static by default;
    /// with `include_db` each gets its live [`IndexerLive`] state (`indexer_state` +
    /// `mview_state` + changelog backlog; clean [`Error::Db`] when unreachable). Filtered
    /// by an id/title substring, sorted by id.
    pub fn indexers(&self, filter: Option<&str>, include_db: bool) -> Result<Vec<Indexer>> {
        let mut list = self.indexer_index().indexers(filter);
        if include_db {
            self.attach_indexer_live(&mut list)?;
        }
        Ok(list)
    }

    /// One indexer by exact id, with its full subscription list (and live state with
    /// `include_db`).
    pub fn indexer(&self, id: &str, include_db: bool) -> Result<Option<Indexer>> {
        let Some(ix) = self.indexer_index().indexer(id) else { return Ok(None) };
        let mut list = vec![ix];
        if include_db {
            self.attach_indexer_live(&mut list)?;
        }
        Ok(list.pop())
    }

    #[cfg(feature = "db")]
    fn attach_indexer_live(&self, list: &mut [Indexer]) -> Result<()> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (states, views) =
            db::fetch_indexer_states(conn, &cfg.table_prefix).map_err(Error::Db)?;
        for ix in list {
            let state = states.iter().find(|s| s.indexer_id == ix.id);
            let view = ix
                .view_id
                .as_deref()
                .and_then(|v| views.iter().find(|m| m.view_id == v));
            ix.live = Some(IndexerLive {
                status: state.map(|s| s.status.clone()),
                updated: state.and_then(|s| s.updated.clone()),
                by_schedule: view.map(|v| v.mode == "enabled"),
                view_status: view.map(|v| v.status.clone()),
                backlog: view.and_then(|v| v.backlog),
            });
        }
        Ok(())
    }

    #[cfg(not(feature = "db"))]
    fn attach_indexer_live(&self, _list: &mut [Indexer]) -> Result<()> {
        Err(Error::Db("the `db` feature is not enabled in this build".to_string()))
    }

    /// Setup patches: every `Setup/Patch/Data|Schema` class of the enabled modules (the
    /// classes `setup:upgrade` runs), filtered by a class/module substring. With
    /// `include_db`, each is marked applied/pending per the `patch_list` table (clean
    /// [`Error::Db`] when unreachable), and applied entries with no class on disk —
    /// patches of removed modules — are returned in `orphaned_applied`.
    pub fn patches(&self, filter: Option<&str>, include_db: bool) -> Result<Patches> {
        let applied: Option<std::collections::HashSet<String>> = if include_db {
            Some(self.fetch_patch_list()?.into_iter().collect())
        } else {
            None
        };

        let needle = filter.map(str::to_lowercase);
        let mut patches = Vec::new();
        for m in self.index.modules.iter().filter(|m| m.enabled) {
            for (kind, sub) in
                [(PatchKind::Data, "Setup/Patch/Data"), (PatchKind::Schema, "Setup/Patch/Schema")]
            {
                let base = m.path.join(sub);
                if !base.is_dir() {
                    continue;
                }
                let ns = m.name.as_str().replace('_', "\\");
                doctor::walk_php(&base, 0, &mut |path| {
                    let Ok(rel) = path.strip_prefix(&m.path) else { return };
                    let stem = rel.with_extension("");
                    let mut class = ns.clone();
                    for part in stem.components() {
                        class.push('\\');
                        class.push_str(&part.as_os_str().to_string_lossy());
                    }
                    let class = ClassName::new(class);
                    // Convention verified through PSR-4, like the doctor scans.
                    match self.index.resolver.file_for(&class) {
                        Some(resolved) if doctor::same_file(&resolved, path) => {}
                        _ => return,
                    }
                    let Some(h) = self.index.resolver.header_of(&class) else { return };
                    if h.is_interface || h.is_abstract {
                        return;
                    }
                    // Only actual patches — Setup/Patch dirs also hold helper classes.
                    let is_patch = self.index.resolver.ancestors(&class).iter().any(|a| {
                        matches!(
                            a.as_str(),
                            "Magento\\Framework\\Setup\\Patch\\DataPatchInterface"
                                | "Magento\\Framework\\Setup\\Patch\\SchemaPatchInterface"
                        )
                    });
                    if !is_patch {
                        return;
                    }
                    if let Some(n) = &needle {
                        if !class.as_str().to_lowercase().contains(n)
                            && !m.name.as_str().to_lowercase().contains(n)
                        {
                            return;
                        }
                    }
                    patches.push(Patch {
                        applied: applied.as_ref().map(|set| set.contains(class.as_str())),
                        class,
                        kind,
                        module: m.name.clone(),
                        source: Source {
                            module: m.name.clone(),
                            file: path.to_path_buf(),
                            line: 0,
                            area: Area::Global,
                        },
                    });
                });
            }
        }
        patches.sort_by(|a, b| {
            a.module.cmp(&b.module).then_with(|| a.class.cmp(&b.class))
        });

        // Applied entries no on-disk class explains (patches of removed modules) — only
        // meaningful unfiltered, and never silently dropped.
        let mut orphaned_applied = Vec::new();
        if let Some(applied) = &applied {
            if filter.is_none() {
                let on_disk: std::collections::HashSet<&str> =
                    patches.iter().map(|p| p.class.as_str()).collect();
                orphaned_applied =
                    applied.iter().filter(|a| !on_disk.contains(a.as_str())).cloned().collect();
                orphaned_applied.sort();
            }
        }

        Ok(Patches { patches, orphaned_applied })
    }

    /// One product by exact SKU, as the database stores it. Live DB.
    #[cfg(feature = "db")]
    pub fn product_by_sku(&self, sku: &str) -> Result<Option<Product>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_product(conn, &cfg.table_prefix, db::ProductIdent::Sku(sku))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_product(r, false)))
    }

    /// One product by entity_id (`matched_by_id` is set on the result).
    #[cfg(feature = "db")]
    pub fn product_by_id(&self, id: u32) -> Result<Option<Product>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_product(conn, &cfg.table_prefix, db::ProductIdent::Id(id))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_product(r, true)))
    }

    /// The category tree, pre-order flattened (`level` 1 = a root tree), each root
    /// tagged with the store groups using it. Live DB.
    #[cfg(feature = "db")]
    pub fn category_tree(&self) -> Result<Vec<CategoryTreeNode>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (nodes, roots) =
            db::fetch_category_nodes(conn, &cfg.table_prefix).map_err(Error::Db)?;

        let mut by_parent: std::collections::BTreeMap<u32, Vec<&db::DbCategoryNode>> =
            std::collections::BTreeMap::new();
        for n in &nodes {
            by_parent.entry(n.parent_id).or_default().push(n);
        }
        for children in by_parent.values_mut() {
            children.sort_by_key(|n| (n.position, n.id));
        }
        let to_node = |n: &db::DbCategoryNode| CategoryTreeNode {
            id: n.id,
            name: n.name.clone().unwrap_or_else(|| format!("(category {})", n.id)),
            level: n.level,
            direct_products: n.direct_products,
            active: n.active,
            in_menu: n.in_menu,
            anchor: n.anchor,
            root_of: roots
                .iter()
                .filter(|(root, _)| *root == n.id)
                .map(|(_, g)| g.clone())
                .collect(),
        };
        // Pre-order DFS from the roots (children of the global root, id 1),
        // cycle-guarded by a visited set.
        let mut out = Vec::with_capacity(nodes.len());
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<&db::DbCategoryNode> = by_parent
            .get(&1)
            .map(|roots| roots.iter().rev().copied().collect())
            .unwrap_or_default();
        while let Some(n) = stack.pop() {
            if !seen.insert(n.id) {
                continue;
            }
            out.push(to_node(n));
            if let Some(children) = by_parent.get(&n.id) {
                stack.extend(children.iter().rev());
            }
        }
        Ok(out)
    }

    /// Categories whose name or url_key contains `needle` (case-insensitive). Live DB.
    #[cfg(feature = "db")]
    pub fn categories_like(&self, needle: &str) -> Result<Vec<CategoryHit>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (nodes, _) = db::fetch_category_nodes(conn, &cfg.table_prefix).map_err(Error::Db)?;
        let n = needle.to_lowercase();
        let mut hits: Vec<CategoryHit> = nodes
            .into_iter()
            .filter(|c| {
                c.name.as_deref().is_some_and(|x| x.to_lowercase().contains(&n))
                    || c.url_key.as_deref().is_some_and(|x| x.to_lowercase().contains(&n))
            })
            .map(|c| CategoryHit {
                id: c.id,
                name: c.name.unwrap_or_else(|| format!("(category {})", c.id)),
                url_key: c.url_key,
                level: c.level,
                active: c.active,
            })
            .collect();
        hits.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        Ok(hits)
    }

    /// One category by id: per-scope values, the visibility diagnosis (own scopes + the
    /// ancestor walk), direct vs indexed product counts, rewrites. `include_products`
    /// lists the direct assignments; `indexed_store` (`Some(None)` = the first store
    /// view, `Some(Some(code))` = that store) lists the store's *index* — what the
    /// storefront shows, anchor-inherited included. Live DB.
    #[cfg(feature = "db")]
    pub fn category(
        &self,
        id: u32,
        include_products: bool,
        indexed_store: Option<Option<&str>>,
    ) -> Result<Option<Category>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw =
            db::fetch_category_card(conn, &cfg.table_prefix, id, include_products, indexed_store)
                .map_err(Error::Db)?;
        Ok(raw.map(to_category))
    }

    /// One customer by exact email. Live DB.
    #[cfg(feature = "db")]
    pub fn customer_by_email(&self, email: &str) -> Result<Option<Customer>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_customer(conn, &cfg.table_prefix, db::CustomerIdent::Email(email))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_customer(r, false)))
    }

    /// One customer by entity_id.
    #[cfg(feature = "db")]
    pub fn customer_by_id(&self, id: u32) -> Result<Option<Customer>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_customer(conn, &cfg.table_prefix, db::CustomerIdent::Id(id))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_customer(r, true)))
    }

    /// Customer search: email or name substring, newest first.
    #[cfg(feature = "db")]
    pub fn customers_like(&self, needle: &str, limit: usize) -> Result<(Vec<CustomerHit>, bool)> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (rows, truncated) =
            db::fetch_customers_like(conn, &cfg.table_prefix, needle, limit).map_err(Error::Db)?;
        Ok((
            rows.into_iter()
                .map(|(entity_id, email, name, group, created_at, _)| CustomerHit {
                    entity_id,
                    email,
                    name: name.unwrap_or_default(),
                    group,
                    created_at,
                })
                .collect(),
            truncated,
        ))
    }

    /// One order by exact increment_id. Live DB.
    #[cfg(feature = "db")]
    pub fn order_by_increment(&self, increment: &str) -> Result<Option<Order>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_order(conn, &cfg.table_prefix, db::OrderIdent::Increment(increment))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_order(r, false)))
    }

    /// One order by entity_id (`matched_by_id` set on the result).
    #[cfg(feature = "db")]
    pub fn order_by_id(&self, id: u32) -> Result<Option<Order>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_order(conn, &cfg.table_prefix, db::OrderIdent::Id(id))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_order(r, true)))
    }

    /// Order search: increment_id or customer email substring, newest first.
    #[cfg(feature = "db")]
    pub fn orders_like(&self, needle: &str, limit: usize) -> Result<(Vec<OrderHit>, bool)> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (rows, truncated) =
            db::fetch_orders_like(conn, &cfg.table_prefix, needle, limit).map_err(Error::Db)?;
        Ok((
            rows.into_iter()
                .map(
                    |(entity_id, increment_id, status, grand_total, currency, email, created)| {
                        OrderHit {
                            entity_id,
                            increment_id,
                            status,
                            grand_total,
                            currency,
                            customer_email: email,
                            created_at: created,
                        }
                    },
                )
                .collect(),
            truncated,
        ))
    }

    /// Light lookup: the SKU of an entity_id (for shadow-note checks). Live DB.
    #[cfg(feature = "db")]
    pub fn product_sku_of_id(&self, id: u32) -> Result<Option<String>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        Ok(db::fetch_product_identity(conn, &cfg.table_prefix, &db::ProductIdent::Id(id))
            .map_err(Error::Db)?
            .map(|(_, sku, _)| sku))
    }

    /// Every price the database stores for a product, by exact SKU. Live DB.
    #[cfg(feature = "db")]
    pub fn product_prices_by_sku(&self, sku: &str) -> Result<Option<ProductPrices>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_product_prices(conn, &cfg.table_prefix, db::ProductIdent::Sku(sku))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_product_prices(r, false)))
    }

    /// Every price for a product, by entity_id.
    #[cfg(feature = "db")]
    pub fn product_prices_by_id(&self, id: u32) -> Result<Option<ProductPrices>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_product_prices(conn, &cfg.table_prefix, db::ProductIdent::Id(id))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_product_prices(r, true)))
    }

    /// SKU-substring search, `limit + 1` fetched to flag truncation.
    #[cfg(feature = "db")]
    pub fn products_like(&self, needle: &str, limit: usize) -> Result<(Vec<ProductHit>, bool)> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (rows, truncated) =
            db::fetch_products_like(conn, &cfg.table_prefix, needle, limit).map_err(Error::Db)?;
        Ok((
            rows.into_iter()
                .map(|(entity_id, sku, type_id, name, status)| ProductHit {
                    entity_id,
                    sku,
                    type_id,
                    name,
                    enabled: status.map(|s| s == 1),
                })
                .collect(),
            truncated,
        ))
    }

    /// Admin users from the live `admin_user` table, each joined with its role name;
    /// lock state and login age computed on the DB server's clock. Sorted by username.
    #[cfg(feature = "db")]
    pub fn admin_users(&self) -> Result<Vec<AdminUser>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let rows = db::fetch_admin_users(conn, &cfg.table_prefix).map_err(Error::Db)?;
        Ok(rows
            .into_iter()
            .map(|u| AdminUser {
                id: u.id,
                username: u.username,
                firstname: u.firstname,
                lastname: u.lastname,
                email: u.email,
                active: u.active,
                role: u.role,
                created: u.created,
                last_login: u.last_login,
                last_login_secs: u.last_login_secs,
                logins: u.logins,
                failures: u.failures,
                locked: u.locked,
                lock_expires: u.lock_expires,
                locale: u.locale,
            })
            .collect())
    }

    /// Admin roles from the live `authorization_role`/`authorization_rule` tables: each
    /// with its member usernames and permission rules, every rule's resource id joined
    /// with its title from the static acl.xml index (`None` title = no module declares
    /// the resource — a stale rule of an uninstalled module). Sorted by role name.
    #[cfg(feature = "db")]
    pub fn admin_roles(&self) -> Result<Vec<AdminRole>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (roles, members, rules) =
            db::fetch_admin_roles(conn, &cfg.table_prefix).map_err(Error::Db)?;
        Ok(roles
            .into_iter()
            .map(|(id, name)| {
                let users: Vec<String> = members
                    .iter()
                    .filter(|(rid, _)| *rid == id)
                    .map(|(_, u)| u.clone())
                    .collect();
                let rules: Vec<AdminRule> = rules
                    .iter()
                    .filter(|(rid, _, _)| *rid == id)
                    .map(|(_, resource, allow)| AdminRule {
                        title: self
                            .acl_resource(resource)
                            .map(|r| r.title)
                            .filter(|t| !t.is_empty()),
                        resource: resource.clone(),
                        allow: *allow,
                    })
                    .collect();
                let all_resources =
                    rules.iter().any(|r| r.resource == "Magento_Backend::all" && r.allow);
                AdminRole { id, name, users, all_resources, rules }
            })
            .collect())
    }

    fn eav_setup_index(&self) -> &eav::EavSetupIndex {
        self.eav_setup.get_or_init(|| eav::EavSetupIndex::build(&self.index.modules))
    }

    /// Setup-script attribute calls (`addAttribute`/`updateAttribute`/`removeAttribute`
    /// with literal arguments) across the enabled modules — the static "who created this
    /// attribute" half of `eav`. Optionally filtered by exact attribute code. Core
    /// catalog attributes won't appear (Magento installs them from data arrays, not
    /// `addAttribute`); the value is third-party and project attributes.
    pub fn eav_setup_refs(&self, code: Option<&str>) -> Vec<EavSetupRef> {
        let refs = &self.eav_setup_index().refs;
        match code {
            Some(c) => refs.iter().filter(|r| r.code == c).cloned().collect(),
            None => refs.clone(),
        }
    }

    /// The `eav_entity_type` rows with attribute counts. Live DB (clean [`Error::Db`]
    /// when unreachable).
    #[cfg(feature = "db")]
    pub fn eav_entity_types(&self) -> Result<Vec<EavEntityType>> {
        Ok(self
            .eav_fetch_entities()?
            .into_iter()
            .map(|e| EavEntityType {
                code: e.code,
                entity_table: e.entity_table,
                attributes: e.attributes,
            })
            .collect())
    }

    /// Live attributes, optionally restricted to one entity type (aliases accepted:
    /// `product` → `catalog_product`). Sorted by (entity, code).
    #[cfg(feature = "db")]
    pub fn eav_attributes(&self, entity: Option<&str>) -> Result<Vec<EavAttribute>> {
        let entities = self.eav_fetch_entities()?;
        let wanted = entity.map(|e| eav::resolve_entity_alias(e));
        let rows = self.eav_fetch_attributes()?;
        Ok(rows
            .into_iter()
            .filter(|r| wanted.as_deref().map_or(true, |w| r.entity_code == w))
            .map(|r| to_eav_attribute(r, &entities))
            .collect())
    }

    /// The full card(s) for an exact attribute code — one per entity type declaring it
    /// (`name` exists on both products and categories): the live row plus set
    /// memberships, options, and the static setup-script join.
    #[cfg(feature = "db")]
    pub fn eav_attribute_cards(&self, code: &str) -> Result<Vec<EavAttributeCard>> {
        let entities = self.eav_fetch_entities()?;
        let rows: Vec<db::DbEavAttribute> = self
            .eav_fetch_attributes()?
            .into_iter()
            .filter(|r| r.code == code)
            .collect();

        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let refs = self.eav_setup_refs(Some(code));
        let mut cards = Vec::new();
        for row in rows {
            let (sets, total_sets) =
                db::fetch_eav_sets(conn, &cfg.table_prefix, row.attribute_id, &row.entity_code)
                    .map_err(Error::Db)?;
            let options = db::fetch_eav_options(conn, &cfg.table_prefix, row.attribute_id)
                .map_err(Error::Db)?;
            // Setup calls naming this entity, plus ones whose entity we couldn't resolve.
            let setup_refs: Vec<EavSetupRef> = refs
                .iter()
                .filter(|r| !r.entity_known || r.entity == row.entity_code)
                .cloned()
                .collect();
            let entity_table =
                entities.iter().find(|e| e.code == row.entity_code).and_then(|e| e.entity_table.clone());
            cards.push(EavAttributeCard {
                attribute: to_eav_attribute(row, &entities),
                entity_table,
                sets: sets
                    .into_iter()
                    .map(|(set, group)| EavSetMembership { set, group })
                    .collect(),
                total_sets,
                options,
                setup_refs,
            });
        }
        Ok(cards)
    }

    #[cfg(feature = "db")]
    fn eav_fetch_entities(&self) -> Result<Vec<db::DbEavEntity>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        db::fetch_eav_entities(conn, &cfg.table_prefix).map_err(Error::Db)
    }

    #[cfg(feature = "db")]
    fn eav_fetch_attributes(&self) -> Result<Vec<db::DbEavAttribute>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        db::fetch_eav_attributes(conn, &cfg.table_prefix).map_err(Error::Db)
    }

    #[cfg(feature = "db")]
    fn fetch_patch_list(&self) -> Result<Vec<String>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        db::fetch_patch_list(conn, &cfg.table_prefix).map_err(Error::Db)
    }

    #[cfg(not(feature = "db"))]
    fn fetch_patch_list(&self) -> Result<Vec<String>> {
        Err(Error::Db("the `db` feature is not enabled in this build".to_string()))
    }

    /// Everything known about one class (or virtual type) on one screen: identity (file,
    /// module, package, hierarchy), a compressed DI summary (forward + reverse), and every
    /// configuration reference — events it observes, cron jobs, webapi routes, console
    /// command, GraphQL resolvers, queue handlers, controller URLs. The aggregate view;
    /// `di` and `uses` stay the focused drill-downs. A file with no references at all is
    /// the interesting negative: candidate dead code.
    pub fn whatis(&self, class: &ClassName) -> Result<Whatis> {
        whatis::run(self, class)
    }

    /// Modules of one source across everything seen on disk: the config.php list plus the
    /// unregistered ones (on disk, not in config.php) — so vendor + app = enabled + disabled.
    fn count_modules(&self, source: model::ModuleSource) -> usize {
        self.index.modules.iter().filter(|m| m.source == source).count()
            + self.index.check.on_disk_not_in_config.iter().filter(|m| m.source == source).count()
    }

    /// Cross-index lints: everything the merged configuration references that doesn't
    /// exist (classes, ACL resources), structural problems (preference/virtual-type/
    /// `<sequence>` cycles, module-set drift), and probably-forgotten wiring (command/
    /// observer/plugin classes registered nowhere, queues nothing consumes). Errors break
    /// at runtime; warnings may be intentional. `source` restricts the *unregistered-code*
    /// scans to app or vendor modules (the reference checks always cover everything).
    pub fn doctor(&self, source: Option<model::ModuleSource>) -> DoctorReport {
        doctor::run(self, source)
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
            modules_unregistered: self.index.check.on_disk_not_in_config.len(),
            modules_vendor: self.count_modules(model::ModuleSource::Vendor),
            modules_app: self.count_modules(model::ModuleSource::App),
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

    /// Every dictionary row whose phrase key contains `needle` (case-insensitive), in
    /// Magento's verified precedence order — module i18n CSVs (load order; at runtime the
    /// *current request's controller module* additionally wins within this layer),
    /// language packs (by `sort_order`), theme i18n (which theme applies depends on the
    /// active theme), and with `include_db` the `translation` table. An identity row
    /// (`key == value`) is flagged `reset`: Magento's loader deletes earlier translations
    /// for it. `locale` defaults to the store's configured `general/locale/code`.
    pub fn translations(
        &self,
        needle: &str,
        locale: Option<&str>,
        include_db: bool,
    ) -> Result<Translations> {
        let locale = match locale {
            Some(l) => l.to_string(),
            None => self
                .config(false)
                .ok()
                .and_then(|set| set.get("default", "general/locale/code").map(|v| v.value.clone()))
                .unwrap_or_else(|| "en_US".to_string()),
        };
        let n = needle.to_lowercase();

        // (layer, csv path, synthetic module tag) per source, in precedence order.
        let mut jobs: Vec<(TranslationLayer, std::path::PathBuf, ModuleName)> = Vec::new();
        for m in self.index.modules.iter().filter(|m| m.enabled) {
            jobs.push((
                TranslationLayer::Module(m.name.clone()),
                m.path.join("i18n").join(format!("{locale}.csv")),
                m.name.clone(),
            ));
        }
        let mut packs = self.discover_language_packs(&locale);
        packs.sort_by_key(|(_, sort, _)| *sort);
        for (name, _, dir) in packs {
            jobs.push((
                TranslationLayer::Pack(name.clone()),
                dir.join(format!("{locale}.csv")),
                ModuleName::new(name),
            ));
        }
        for (id, dir) in self.discover_themes() {
            jobs.push((
                TranslationLayer::Theme(id.clone()),
                dir.join("i18n").join(format!("{locale}.csv")),
                ModuleName::new(id),
            ));
        }

        use rayon::prelude::*;
        let parsed: Vec<Vec<(String, String, u32)>> = jobs
            .par_iter()
            .map(|(_, path, _)| {
                std::fs::read_to_string(path)
                    .map(|t| {
                        parse::i18n_csv(&t)
                            .into_iter()
                            .filter(|(k, _, _)| k.to_lowercase().contains(&n))
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect();

        let mut by_key: std::collections::HashMap<String, Vec<TranslationEntry>> =
            std::collections::HashMap::new();
        for ((layer, path, module), rows) in jobs.iter().zip(parsed) {
            for (key, value, line) in rows {
                by_key.entry(key.clone()).or_default().push(TranslationEntry {
                    layer: layer.clone(),
                    reset: key == value,
                    value,
                    store_id: None,
                    source: Source {
                        module: module.clone(),
                        file: path.clone(),
                        line,
                        area: Area::Global,
                    },
                });
            }
        }

        if include_db {
            for (key, value, store_id) in self.fetch_translations(&locale, needle)? {
                by_key.entry(key.clone()).or_default().push(TranslationEntry {
                    layer: TranslationLayer::Db,
                    reset: key == value,
                    value,
                    store_id: Some(store_id),
                    source: Source {
                        module: ModuleName::new("(db)"),
                        file: std::path::PathBuf::from("translation"),
                        line: 0,
                        area: Area::Global,
                    },
                });
            }
        }

        let mut matches: Vec<TranslationMatch> = by_key
            .into_iter()
            .map(|(key, entries)| TranslationMatch { key, entries })
            .collect();
        matches.sort_by(|a, b| a.key.cmp(&b.key));

        // Context for an honest empty result: what was scanned, and which dictionaries
        // exist but can never load (disabled / not-in-config.php modules).
        let dictionaries_scanned = jobs.iter().filter(|(_, p, _)| p.is_file()).count();
        let dict = format!("{locale}.csv");
        let mut inactive_dictionaries: Vec<ModuleName> = self
            .index
            .modules
            .iter()
            .filter(|m| !m.enabled)
            .filter(|m| m.path.join("i18n").join(&dict).is_file())
            .map(|m| m.name.clone())
            .chain(
                self.index
                    .check
                    .on_disk_not_in_config
                    .iter()
                    .filter(|m| m.path.join("i18n").join(&dict).is_file())
                    .map(|m| m.name.clone()),
            )
            .collect();
        inactive_dictionaries.sort();

        Ok(Translations { locale, matches, dictionaries_scanned, inactive_dictionaries })
    }

    /// Language packs on disk for `locale`: `(name, sort_order, dir)` — composer packages
    /// with a root `language.xml` plus `app/i18n/<vendor>/<pack>`.
    fn discover_language_packs(&self, locale: &str) -> Vec<(String, i32, std::path::PathBuf)> {
        let mut out = Vec::new();
        let mut probe = |name: String, dir: &std::path::Path| {
            let Ok(text) = std::fs::read_to_string(dir.join("language.xml")) else { return };
            let (code, sort) = parse::language_xml(&text);
            if code.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(locale)) {
                out.push((name, sort.unwrap_or(0), dir.to_path_buf()));
            }
        };
        for p in &self.index.packages {
            probe(p.name.clone(), &p.root);
        }
        let base = self.index.root.join("app/i18n");
        if let Ok(vendors) = std::fs::read_dir(&base) {
            for vendor in vendors.flatten() {
                if let Ok(packs) = std::fs::read_dir(vendor.path()) {
                    for pack in packs.flatten() {
                        let name = format!(
                            "{}/{}",
                            vendor.file_name().to_string_lossy(),
                            pack.file_name().to_string_lossy()
                        );
                        probe(name, &pack.path());
                    }
                }
            }
        }
        out
    }

    #[cfg(feature = "db")]
    fn fetch_translations(&self, locale: &str, needle: &str) -> Result<Vec<(String, String, u32)>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        db::fetch_translations(conn, &cfg.table_prefix, locale, needle).map_err(Error::Db)
    }

    #[cfg(not(feature = "db"))]
    fn fetch_translations(&self, _locale: &str, _needle: &str) -> Result<Vec<(String, String, u32)>> {
        Err(Error::Db("the `db` feature is not enabled in this build".to_string()))
    }

    fn catalog_attr_index(&self) -> &breadth::CatalogAttrIndex {
        self.catalog_attrs.get_or_init(|| breadth::CatalogAttrIndex::build(&self.index.modules))
    }

    /// The `catalog_attributes.xml` groups — which attributes load in each context
    /// (`quote_item`, `wishlist_item`, …), each attribute with its adding module.
    pub fn catalog_attribute_groups(&self) -> Vec<CatalogAttributeGroup> {
        self.catalog_attr_index().groups()
    }

    /// One group by exact name.
    pub fn catalog_attribute_group(&self, name: &str) -> Option<CatalogAttributeGroup> {
        self.catalog_attr_index().group(name)
    }

    fn email_template_index(&self) -> &breadth::EmailTemplateIndex {
        self.email_templates.get_or_init(|| {
            breadth::EmailTemplateIndex::build(&self.index.modules, &self.discover_themes())
        })
    }

    /// Transactional email templates from `etc/email_templates.xml`, each with its
    /// resolved module file (`None` = declared but missing) and any theme overrides.
    /// Filtered by an id/label substring, sorted by id.
    pub fn email_templates(&self, filter: Option<&str>) -> Vec<EmailTemplate> {
        self.email_template_index().templates(filter)
    }

    /// One email template by exact id.
    pub fn email_template(&self, id: &str) -> Option<EmailTemplate> {
        self.email_template_index().template(id)
    }

    fn widget_index(&self) -> &breadth::WidgetIndex {
        self.widgets.get_or_init(|| breadth::WidgetIndex::build(&self.index.modules))
    }

    /// Widget types declared in `etc/widget.xml` (what the admin's "Insert Widget"
    /// offers), merged across modules. Filtered by an id/label substring, sorted by id.
    pub fn widgets(&self, filter: Option<&str>) -> Vec<Widget> {
        self.widget_index().widgets(filter)
    }

    /// One widget by exact id, with its full parameter set.
    pub fn widget(&self, id: &str) -> Option<Widget> {
        self.widget_index().widget(id)
    }

    fn layout_index(&self) -> &breadth::LayoutIndex {
        self.layout.get_or_init(|| {
            breadth::LayoutIndex::build(&self.index.modules, &self.discover_themes())
        })
    }

    /// Themes on disk as `(id, dir)`: composer packages whose root holds a `theme.xml`
    /// (id read from `registration.php`) plus `app/design/<area>/<Vendor>/<theme>`.
    fn discover_themes(&self) -> Vec<(String, std::path::PathBuf)> {
        let mut out = Vec::new();
        for p in &self.index.packages {
            if !p.root.join("theme.xml").is_file() {
                continue;
            }
            let Ok(reg) = std::fs::read_to_string(p.root.join("registration.php")) else {
                continue;
            };
            // ComponentRegistrar::register(THEME, 'frontend/Vendor/name', __DIR__)
            if let Some(id) = reg
                .split('\'')
                .chain(reg.split('"'))
                .find(|s| s.starts_with("frontend/") || s.starts_with("adminhtml/"))
            {
                out.push((id.to_string(), p.root.clone()));
            }
        }
        for area in ["frontend", "adminhtml"] {
            let base = self.index.root.join("app/design").join(area);
            let Ok(vendors) = std::fs::read_dir(&base) else { continue };
            for vendor in vendors.flatten() {
                let Ok(themes) = std::fs::read_dir(vendor.path()) else { continue };
                for theme in themes.flatten() {
                    if theme.path().join("theme.xml").is_file() {
                        let id = format!(
                            "{area}/{}/{}",
                            vendor.file_name().to_string_lossy(),
                            theme.file_name().to_string_lossy()
                        );
                        out.push((id, theme.path()));
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// Layout handles in `area` with their contributing-file counts (modules + themes).
    pub fn layout_handles(&self, area: Area) -> Vec<(String, usize)> {
        self.layout_index().handles(area)
    }

    /// Everything contributing to one layout handle in `area`: each file's operations
    /// (module files in load order, then theme files — theme *application* order depends
    /// on the active theme's ancestry, which is runtime state), plus the handle-inclusion
    /// graph around it.
    pub fn layout(&self, handle: &str, area: Area) -> Option<LayoutView> {
        self.layout_index().view(handle, area)
    }

    fn ui_component_index(&self) -> &breadth::UiComponentIndex {
        self.ui_components.get_or_init(|| {
            breadth::UiComponentIndex::build(&self.index.modules, &self.discover_themes())
        })
    }

    /// UI components (admin grids, forms, …) in `area` as `(name, kind, contributing
    /// files)`, sorted by name. Kind = the first declaring file's root element.
    pub fn ui_components(&self, area: Area) -> Vec<(String, String, usize)> {
        self.ui_component_index().list(area)
    }

    /// Everything contributing to one ui component in `area`: each file's component
    /// nodes (module files in load order, then theme files — theme *application* order
    /// depends on the active theme's ancestry, which is runtime state).
    pub fn ui_component(&self, name: &str, area: Area) -> Option<UiComponentView> {
        self.ui_component_index().view(name, area)
    }

    fn ext_attr_index(&self) -> &breadth::ExtAttrIndex {
        self.ext_attrs.get_or_init(|| breadth::ExtAttrIndex::build(&self.index.modules))
    }

    /// API data interfaces extended via `extension_attributes.xml`, each with every
    /// attribute modules bolt on (keyed by code, last wins, per-attribute provenance).
    /// Filtered by a type-name substring, sorted by type. Static.
    pub fn extension_attributes(&self, filter: Option<&str>) -> Vec<ExtendedType> {
        self.ext_attr_index().types(filter)
    }

    /// One extended type by exact name.
    pub fn extended_type(&self, name: &ClassName) -> Option<ExtendedType> {
        self.ext_attr_index().extended_type(name)
    }

    fn menu_index(&self) -> &breadth::MenuIndex {
        self.menu.get_or_init(|| breadth::MenuIndex::build(&self.index.modules))
    }

    /// Admin menu items from `adminhtml/menu.xml`, merged across modules in load order
    /// (`<add>`/`<update>` upsert attribute-level, `<remove>` deletes). No filter → the
    /// whole tree in pre-order; a filter → items whose id or title contains it. Static.
    pub fn menu(&self, filter: Option<&str>) -> Vec<MenuItem> {
        self.menu_index().items(filter)
    }

    /// One menu item by exact id.
    pub fn menu_item(&self, id: &str) -> Option<MenuItem> {
        self.menu_index().item(id)
    }

    /// The breadcrumb for a menu item: ancestors from the root down to (excluding) `id`.
    pub fn menu_ancestors(&self, id: &str) -> Vec<MenuItem> {
        self.menu_index().ancestors(id)
    }

    /// The direct children of a menu item.
    pub fn menu_children(&self, id: &str) -> Vec<MenuItem> {
        self.menu_index().children(id)
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

    /// Live message backlog per queue: every queue the static config knows (with its
    /// consumers) joined with the MysqlMq driver's `queue`/`queue_message_status` counts.
    ///
    /// **MySQL (db) queue driver only.** AMQP/RabbitMQ state is never read: a static
    /// queue absent from the `queue` table is reported with `in_db: false` (amqp-only or
    /// setup:upgrade pending), and — the subtler case — on a store whose env.php
    /// configures amqp, a queue may have rows here while its real traffic flows through
    /// the broker, so zero counts are not proof of an empty queue. Check
    /// [`Self::queue_config`] for configured amqp connections before treating these
    /// numbers as the whole story. A DB queue no static config references is `orphaned`
    /// (removed module's leftover). Sorted by queue name. Clean [`Error::Db`] when the
    /// database is unreachable.
    #[cfg(feature = "db")]
    pub fn queue_backlog(&self) -> Result<Vec<QueueBacklog>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let counts = db::fetch_queue_backlog(conn, &cfg.table_prefix).map_err(Error::Db)?;

        let mut out: Vec<QueueBacklog> = Vec::new();
        for (queue, consumers) in self.mq_index().queues() {
            let c = counts.iter().find(|c| c.queue == queue);
            out.push(QueueBacklog {
                queue,
                consumers,
                in_db: c.is_some(),
                orphaned: false,
                new: c.map_or(0, |c| c.new),
                in_progress: c.map_or(0, |c| c.in_progress),
                retry: c.map_or(0, |c| c.retry),
                error: c.map_or(0, |c| c.error),
                done: c.map_or(0, |c| c.done),
                oldest_waiting_secs: c.and_then(|c| c.oldest_waiting_secs),
            });
        }
        for c in &counts {
            if !out.iter().any(|q| q.queue == c.queue) {
                out.push(QueueBacklog {
                    queue: c.queue.clone(),
                    consumers: Vec::new(),
                    in_db: true,
                    orphaned: true,
                    new: c.new,
                    in_progress: c.in_progress,
                    retry: c.retry,
                    error: c.error,
                    done: c.done,
                    oldest_waiting_secs: c.oldest_waiting_secs,
                });
            }
        }
        out.sort_by(|a, b| a.queue.cmp(&b.queue));
        Ok(out)
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

/// Tables the runtime creates and manages outside `db_schema.xml`: mview changelogs,
/// per-store sequence and dimension-index tables, indexer flats/replicas, and the
/// framework's own bootstrap/bookkeeping tables.
fn is_runtime_table(name: &str) -> bool {
    name.ends_with("_cl")
        || name.ends_with("_replica")
        || name.ends_with("_flat")
        || name.contains("_index_store")
        || name.starts_with("sequence_")
        || name.starts_with("catalog_product_flat_")
        || name.starts_with("catalog_category_flat_")
        || matches!(
            name,
            "setup_module" | "patch_list" | "cache" | "cache_tag" | "flag" | "session"
        )
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

/// Assemble [`Customer`]: decode the newsletter status, name the addresses, and pass
/// custom EAV values through the shared scope machinery (single `default` scope —
/// customer attributes aren't store-scoped).
#[cfg(feature = "db")]
fn to_customer(raw: db::DbCustomer, matched_by_id: bool) -> Customer {
    let newsletter_status = |s: i64| match s {
        1 => "subscribed".to_string(),
        2 => "not active".to_string(),
        3 => "unsubscribed".to_string(),
        4 => "unconfirmed".to_string(),
        other => format!("status {other}"),
    };
    let name = [raw.firstname.clone(), raw.lastname.clone()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");

    let mut values: Vec<ProductValue> = Vec::new();
    for v in &raw.values {
        let scope = ProductScopeValue {
            store: "default".to_string(),
            label: None,
            value: v.value.clone().unwrap_or_else(|| "NULL".to_string()),
        };
        match values.iter_mut().find(|e| e.attribute == v.attribute) {
            Some(e) => e.scopes.push(scope),
            None => values.push(ProductValue {
                attribute: v.attribute.clone(),
                backend_type: v.backend_type.clone(),
                input: v.input.clone(),
                scopes: vec![scope],
            }),
        }
    }
    values.sort_by(|a, b| a.attribute.cmp(&b.attribute));

    Customer {
        entity_id: raw.entity_id,
        email: raw.email,
        name,
        group: raw.group,
        website: raw.website,
        created_in: raw.created_in,
        created_at: raw.created_at,
        active: raw.active,
        confirmed: raw.confirmed,
        locked: raw.locked,
        lock_expires: raw.lock_expires,
        failures: raw.failures,
        last_login: raw.last_login,
        last_logout: raw.last_logout,
        dob: raw.dob,
        taxvat: raw.taxvat,
        addresses: raw
            .addresses
            .into_iter()
            .map(|(id, f, l, company, street, postcode, city, region, country, telephone, db, ds)| {
                CustomerAddress {
                    id,
                    name: [f, l].into_iter().flatten().collect::<Vec<_>>().join(" "),
                    company,
                    street: street.map(|s| s.replace('\n', ", ")),
                    postcode,
                    city,
                    region,
                    country,
                    telephone,
                    default_billing: db,
                    default_shipping: ds,
                }
            })
            .collect(),
        newsletter: raw
            .newsletter
            .into_iter()
            .map(|(store, status)| CustomerNewsletter { store, status: newsletter_status(status) })
            .collect(),
        values,
        orders: CustomerOrders {
            count: raw.order_stats.0,
            lifetime: raw.order_stats.1,
            first_at: raw.order_stats.2,
            last_at: raw.order_stats.3,
            last_increment: raw.last_order.as_ref().map(|(i, _)| i.clone()),
            last_status: raw.last_order.and_then(|(_, s)| s),
        },
        guest_orders: raw.guest_orders,
        matched_by_id,
    }
}

/// Assemble [`Order`]: decode document states, join tracks onto their shipments, and
/// flatten the payment's `additional_information` JSON (top-level keys; nested values
/// re-serialized compactly).
#[cfg(feature = "db")]
fn to_order(raw: db::DbOrder, matched_by_id: bool) -> Order {
    let invoice_state = |s: Option<i64>| {
        s.map(|s| match s {
            1 => "open".to_string(),
            2 => "paid".to_string(),
            3 => "canceled".to_string(),
            other => format!("state {other}"),
        })
    };
    let memo_state = |s: Option<i64>| {
        s.map(|s| match s {
            1 => "open".to_string(),
            2 => "refunded".to_string(),
            3 => "canceled".to_string(),
            other => format!("state {other}"),
        })
    };

    let payment = raw.payment.map(|(method, last_trans_id, blob)| {
        let additional: Vec<(String, String)> = blob
            .as_deref()
            .and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok())
            .and_then(|v| match v {
                serde_json::Value::Object(map) => Some(
                    map.into_iter()
                        .map(|(k, v)| {
                            let val = match v {
                                serde_json::Value::String(s) => s,
                                other => other.to_string(),
                            };
                            (k, val)
                        })
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default();
        OrderPayment { method, last_trans_id, additional }
    });

    let customer_name = match (&raw.customer_firstname, &raw.customer_lastname) {
        (Some(f), Some(l)) => Some(format!("{f} {l}")),
        (Some(f), None) => Some(f.clone()),
        (None, Some(l)) => Some(l.clone()),
        _ => None,
    };

    Order {
        entity_id: raw.entity_id,
        increment_id: raw.increment_id,
        state: raw.state,
        status: raw.status,
        status_label: raw.status_label,
        store: raw.store,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
        customer_id: raw.customer_id,
        customer_email: raw.customer_email,
        customer_name,
        guest: raw.guest,
        order_currency: raw.order_currency,
        base_currency: raw.base_currency,
        total_qty: raw.total_qty,
        coupon: raw.coupon,
        applied_rule_ids: raw.applied_rule_ids,
        shipping_method: raw.shipping_method,
        shipping_description: raw.shipping_description,
        totals: raw
            .totals
            .into_iter()
            .map(|(key, amount, base_amount)| OrderTotal { key, amount, base_amount })
            .collect(),
        items: raw
            .items
            .into_iter()
            .map(
                |(sku, name, product_type, is_child, ordered, invoiced, shipped, refunded, canceled, price, row_total)| {
                    OrderItem {
                        sku,
                        name,
                        product_type,
                        is_child,
                        qty_ordered: ordered,
                        qty_invoiced: invoiced,
                        qty_shipped: shipped,
                        qty_refunded: refunded,
                        qty_canceled: canceled,
                        price,
                        row_total,
                    }
                },
            )
            .collect(),
        addresses: raw
            .addresses
            .into_iter()
            .map(
                |(kind, first, last, company, street, postcode, city, country, telephone)| {
                    let name = [first, last].into_iter().flatten().collect::<Vec<_>>().join(" ");
                    OrderAddress {
                        kind,
                        name,
                        company,
                        street: street.map(|s| s.replace('\n', ", ")),
                        postcode,
                        city,
                        country,
                        telephone,
                    }
                },
            )
            .collect(),
        payment,
        transactions: raw
            .transactions
            .into_iter()
            .map(|(txn_id, kind, closed, created_at)| OrderTransaction {
                txn_id,
                kind,
                closed,
                created_at,
            })
            .collect(),
        invoices: raw
            .invoices
            .into_iter()
            .map(|(increment_id, state, total, created_at)| OrderDocument {
                increment_id,
                state: invoice_state(state),
                total,
                created_at,
            })
            .collect(),
        shipments: raw
            .shipments
            .into_iter()
            .map(|(sid, increment_id, qty, created_at)| OrderShipment {
                increment_id,
                qty,
                created_at,
                tracks: raw
                    .tracks
                    .iter()
                    .filter(|(parent, ..)| *parent == sid)
                    .map(|(_, carrier, title, number)| {
                        (
                            carrier.clone().unwrap_or_default(),
                            title.clone().unwrap_or_default(),
                            number.clone().unwrap_or_default(),
                        )
                    })
                    .collect(),
            })
            .collect(),
        creditmemos: raw
            .creditmemos
            .into_iter()
            .map(|(increment_id, state, total, created_at)| OrderDocument {
                increment_id,
                state: memo_state(state),
                total,
                created_at,
            })
            .collect(),
        history: raw
            .history
            .into_iter()
            .map(|(status, comment, created_at, notified)| OrderComment {
                status,
                comment,
                created_at,
                notified,
            })
            .collect(),
        in_grid: raw.in_grid,
        quote_id: raw.quote_id,
        matched_by_id,
    }
}

/// Assemble [`Category`]: per-scope values with Yes/No labels for the boolean flags,
/// the admin-style breadcrumb, and the visibility walk — the category's own effectively
/// inactive scopes plus every ancestor whose inactivity hides the subtree.
#[cfg(feature = "db")]
fn to_category(raw: db::DbCategoryCard) -> Category {
    let scope_name = |store_id: u32| -> String {
        if store_id == 0 {
            "default".to_string()
        } else {
            let code = raw
                .stores
                .get(&store_id)
                .cloned()
                .unwrap_or_else(|| format!("{store_id}"));
            format!("stores/{code}")
        }
    };

    let mut values: Vec<ProductValue> = Vec::new();
    for v in &raw.values {
        let label = match (v.attribute.as_str(), v.value.as_deref()) {
            ("is_active" | "include_in_menu" | "is_anchor", Some("1")) => {
                Some("Yes".to_string())
            }
            ("is_active" | "include_in_menu" | "is_anchor", Some("0")) => Some("No".to_string()),
            _ => None,
        };
        let scope = ProductScopeValue {
            store: scope_name(v.store_id),
            label,
            value: v.value.clone().unwrap_or_else(|| "NULL".to_string()),
        };
        match values.iter_mut().find(|e| e.attribute == v.attribute) {
            Some(e) => e.scopes.push(scope),
            None => values.push(ProductValue {
                attribute: v.attribute.clone(),
                backend_type: v.backend_type.clone(),
                input: v.input.clone(),
                scopes: vec![scope],
            }),
        }
    }
    for v in &mut values {
        v.scopes.sort_by(|a, b| {
            (a.store != "default").cmp(&(b.store != "default")).then_with(|| a.store.cmp(&b.store))
        });
    }
    const ORDER: [&str; 10] = [
        "name",
        "is_active",
        "include_in_menu",
        "is_anchor",
        "url_key",
        "url_path",
        "display_mode",
        "available_sort_by",
        "default_sort_by",
        "landing_page",
    ];
    let rank = |a: &str| ORDER.iter().position(|f| *f == a).unwrap_or(ORDER.len());
    values.sort_by(|a, b| {
        rank(&a.attribute).cmp(&rank(&b.attribute)).then_with(|| a.attribute.cmp(&b.attribute))
    });

    // Effective inactivity per entity: the default row unless a store row overrides it.
    // No is_active row at all = active (the attribute default).
    let store_ids: Vec<u32> = {
        let mut v: Vec<u32> = raw.stores.keys().copied().filter(|&s| s > 0).collect();
        v.sort();
        v
    };
    let inactive_scopes = |entity: u32| -> Vec<String> {
        let row = |store: u32| {
            raw.active_rows
                .iter()
                .find(|(e, s, _)| *e == entity && *s == store)
                .and_then(|(_, _, v)| *v)
        };
        match row(0) {
            Some(0) => {
                let enabling: Vec<u32> =
                    store_ids.iter().copied().filter(|&s| row(s) == Some(1)).collect();
                if enabling.is_empty() {
                    vec!["all scopes".to_string()]
                } else {
                    let mut out = vec!["default".to_string()];
                    out.extend(
                        store_ids
                            .iter()
                            .copied()
                            .filter(|&s| row(s) != Some(1))
                            .map(scope_name),
                    );
                    out
                }
            }
            _ => store_ids.iter().copied().filter(|&s| row(s) == Some(0)).map(scope_name).collect(),
        }
    };
    let mut visibility: Vec<CategoryVisibilityIssue> = Vec::new();
    let own = inactive_scopes(raw.id);
    if !own.is_empty() {
        visibility.push(CategoryVisibilityIssue {
            ancestor_id: None,
            ancestor_name: None,
            scopes: own,
        });
    }
    for (aid, aname) in &raw.ancestors {
        let scopes = inactive_scopes(*aid);
        if !scopes.is_empty() {
            visibility.push(CategoryVisibilityIssue {
                ancestor_id: Some(*aid),
                ancestor_name: Some(aname.clone()),
                scopes,
            });
        }
    }

    // Admin-style breadcrumb: ancestors past the tree root.
    let breadcrumb = raw
        .ancestors
        .iter()
        .skip(1)
        .map(|(_, n)| n.as_str())
        .collect::<Vec<_>>()
        .join(" > ");
    let parent_name = raw.ancestors.last().map(|(_, n)| n.clone());

    Category {
        id: raw.id,
        path: raw.path,
        level: raw.level,
        position: raw.position,
        parent_id: (raw.parent_id > 0).then_some(raw.parent_id),
        parent_name,
        children: raw.children,
        breadcrumb,
        values,
        visibility,
        direct_products: raw.direct_products,
        indexed: raw
            .indexed
            .into_iter()
            .map(|(store, products)| CategoryIndexCount { store, products })
            .collect(),
        rewrites: raw
            .rewrites
            .into_iter()
            .map(|(request_path, store, redirect)| ProductRewrite { request_path, store, redirect })
            .collect(),
        root_of: raw.root_of,
        products: raw
            .products
            .into_iter()
            .map(|(entity_id, sku, name, position)| CategoryProduct {
                entity_id,
                sku,
                name,
                position,
            })
            .collect(),
        indexed_store: raw.indexed_store,
        indexed_products: raw.indexed_products.map(|rows| {
            rows.into_iter()
                .map(|(entity_id, sku, name, position, is_parent, visibility)| {
                    CategoryIndexedProduct {
                        entity_id,
                        sku,
                        name,
                        position,
                        via_anchor: !is_parent,
                        visibility,
                    }
                })
                .collect()
        }),
    }
}

/// Assemble [`ProductPrices`]: the EAV price attributes reuse the product scope
/// grouping; tier/rule/index rows resolve website codes and customer-group names.
#[cfg(feature = "db")]
fn to_product_prices(raw: db::DbProductPrices, matched_by_id: bool) -> ProductPrices {
    let website = |id: u32| -> String {
        if id == 0 {
            "(all)".to_string()
        } else {
            raw.websites.get(&id).cloned().unwrap_or_else(|| format!("website/{id}"))
        }
    };
    let group = |id: u32| -> String {
        raw.customer_groups.get(&id).cloned().unwrap_or_else(|| format!("group/{id}"))
    };

    let mut attributes: Vec<ProductValue> = Vec::new();
    for v in &raw.values {
        let store = if v.store_id == 0 {
            "default".to_string()
        } else {
            let code = raw
                .stores
                .get(&v.store_id)
                .cloned()
                .unwrap_or_else(|| format!("{}", v.store_id));
            format!("stores/{code}")
        };
        let scope = ProductScopeValue {
            store,
            label: None,
            value: v.value.clone().unwrap_or_else(|| "NULL".to_string()),
        };
        match attributes.iter_mut().find(|e| e.attribute == v.attribute) {
            Some(e) => e.scopes.push(scope),
            None => attributes.push(ProductValue {
                attribute: v.attribute.clone(),
                backend_type: v.backend_type.clone(),
                input: v.input.clone(),
                scopes: vec![scope],
            }),
        }
    }
    for v in &mut attributes {
        v.scopes.sort_by(|a, b| {
            (a.store != "default").cmp(&(b.store != "default")).then_with(|| a.store.cmp(&b.store))
        });
    }
    const ORDER: [&str; 7] = [
        "price",
        "special_price",
        "special_from_date",
        "special_to_date",
        "cost",
        "msrp",
        "minimal_price",
    ];
    let rank = |a: &str| ORDER.iter().position(|f| *f == a).unwrap_or(ORDER.len());
    attributes.sort_by(|a, b| {
        rank(&a.attribute).cmp(&rank(&b.attribute)).then_with(|| a.attribute.cmp(&b.attribute))
    });

    ProductPrices {
        entity_id: raw.entity_id,
        sku: raw.sku,
        type_id: raw.type_id,
        price_scope_website: raw.price_scope_website,
        attributes,
        tier_prices: raw
            .tiers
            .into_iter()
            .map(|(w, all, g, qty, value, percentage)| TierPrice {
                website: website(w),
                customer_group: if all { "ALL GROUPS".to_string() } else { group(g) },
                qty,
                value,
                percentage,
            })
            .collect(),
        rule_prices: raw
            .rules
            .into_iter()
            .map(|(date, g, w, rule_price)| RulePrice {
                date,
                website: website(w),
                customer_group: group(g),
                rule_price,
            })
            .collect(),
        index: raw
            .index
            .into_iter()
            .map(|(g, w, price, final_price, min_price, max_price, tier_price)| IndexedPrice {
                website: website(w),
                customer_group: group(g),
                price,
                final_price,
                min_price,
                max_price,
                tier_price,
            })
            .collect(),
        children: raw
            .children
            .into_iter()
            .map(|c| ChildPrice {
                sku: c.sku,
                entity_id: c.entity_id,
                enabled: c.enabled,
                price: c.price,
                special_price: c.special,
                final_min: c.final_min,
                final_max: c.final_max,
                selection_price: c.selection_price,
                selection_percent: c.selection_percent,
            })
            .collect(),
        bundle_price_type: raw.bundle_price_type,
        matched_by_id,
    }
}

/// Assemble the public [`Product`] from the raw rows: group values per attribute with
/// the default scope first, and resolve human labels where the data allows — Yes/No for
/// booleans, the `Status`/`Visibility` source-model constants (hardcoded faithfully to
/// core), tax classes from `tax_class`, and admin option labels for table-source
/// select/multiselect values.
#[cfg(feature = "db")]
fn to_product(raw: db::DbProduct, matched_by_id: bool) -> Product {
    let label_of = |v: &db::DbProductValue, value: &str| -> Option<String> {
        match (v.attribute.as_str(), v.input.as_deref()) {
            ("status", _) => match value {
                "1" => Some("Enabled".to_string()),
                "2" => Some("Disabled".to_string()),
                _ => None,
            },
            ("visibility", _) => match value {
                "1" => Some("Not Visible Individually".to_string()),
                "2" => Some("Catalog".to_string()),
                "3" => Some("Search".to_string()),
                "4" => Some("Catalog, Search".to_string()),
                _ => None,
            },
            ("tax_class_id", _) => {
                value.parse::<u32>().ok().and_then(|id| raw.tax_classes.get(&id).cloned())
            }
            (_, Some("boolean")) => match value {
                "1" => Some("Yes".to_string()),
                "0" => Some("No".to_string()),
                _ => None,
            },
            (_, Some("select")) => value
                .parse::<u32>()
                .ok()
                .and_then(|o| raw.option_labels.get(&(v.attribute_id, o)).cloned()),
            (_, Some("multiselect")) => {
                let labels: Vec<String> = value
                    .split(',')
                    .filter_map(|part| {
                        part.trim()
                            .parse::<u32>()
                            .ok()
                            .and_then(|o| raw.option_labels.get(&(v.attribute_id, o)).cloned())
                    })
                    .collect();
                (!labels.is_empty()).then(|| labels.join(", "))
            }
            _ => None,
        }
    };

    let mut values: Vec<ProductValue> = Vec::new();
    for v in &raw.values {
        // The `config` scope convention: `default` = store_id 0, else `stores/<code>` —
        // a store view *coded* "default" (nearly every install has one) must not collide
        // with the default scope.
        let store = if v.store_id == 0 {
            "default".to_string()
        } else {
            let code = raw
                .stores
                .get(&v.store_id)
                .cloned()
                .unwrap_or_else(|| format!("{}", v.store_id));
            format!("stores/{code}")
        };
        let value = v.value.clone().unwrap_or_else(|| "NULL".to_string());
        let scope = ProductScopeValue {
            store,
            label: v.value.as_deref().and_then(|val| label_of(v, val)),
            value,
        };
        match values.iter_mut().find(|e| e.attribute == v.attribute) {
            Some(e) => e.scopes.push(scope),
            None => values.push(ProductValue {
                attribute: v.attribute.clone(),
                backend_type: v.backend_type.clone(),
                input: v.input.clone(),
                scopes: vec![scope],
            }),
        }
    }
    for v in &mut values {
        v.scopes.sort_by(|a, b| {
            (a.store != "default").cmp(&(b.store != "default")).then_with(|| a.store.cmp(&b.store))
        });
    }
    // The everyday attributes first, the rest alphabetical.
    const FIRST: [&str; 6] = ["name", "status", "visibility", "price", "special_price", "url_key"];
    let rank = |a: &str| FIRST.iter().position(|f| *f == a).unwrap_or(FIRST.len());
    values.sort_by(|a, b| {
        rank(&a.attribute).cmp(&rank(&b.attribute)).then_with(|| a.attribute.cmp(&b.attribute))
    });

    Product {
        entity_id: raw.entity_id,
        sku: raw.sku,
        type_id: raw.type_id,
        attribute_set: raw.attribute_set,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
        websites: raw.websites,
        values,
        stock: raw
            .stock
            .into_iter()
            .map(|(source, quantity, in_stock)| ProductSourceStock { source, quantity, in_stock })
            .collect(),
        legacy_stock: raw
            .legacy_stock
            .map(|(qty, in_stock, manage_stock)| ProductLegacyStock { qty, in_stock, manage_stock }),
        categories: raw
            .categories
            .into_iter()
            .map(|(id, breadcrumb)| ProductCategory { id, breadcrumb })
            .collect(),
        rewrites: raw
            .rewrites
            .into_iter()
            .map(|(request_path, store, redirect)| ProductRewrite { request_path, store, redirect })
            .collect(),
        parents: raw.parents,
        super_attributes: raw.super_attributes,
        children: raw
            .children
            .into_iter()
            .map(|(entity_id, sku, enabled, options, qty, in_stock, default_qty)| ProductChild {
                sku,
                entity_id,
                enabled,
                options,
                qty,
                in_stock,
                default_qty,
            })
            .collect(),
        bundle_options: raw
            .bundle_options
            .into_iter()
            .map(|o| BundleOption {
                title: o.title,
                required: o.required,
                input_type: o.input_type,
                selections: o
                    .selections
                    .into_iter()
                    .map(
                        |(entity_id, sku, enabled, qty, is_default, price, price_percent, in_stock)| {
                            BundleSelection {
                                sku,
                                entity_id,
                                enabled,
                                qty,
                                is_default,
                                price,
                                price_percent,
                                in_stock,
                            }
                        },
                    )
                    .collect(),
            })
            .collect(),
        matched_by_id,
    }
}

/// Map a raw DB attribute row to the public type: decode `is_global`, split `apply_to`,
/// and derive the value table (`<entity_table>_<backend_type>`, honoring the rare
/// `value_table_prefix`; `static` attributes live on the entity table itself).
#[cfg(feature = "db")]
fn to_eav_attribute(r: db::DbEavAttribute, entities: &[db::DbEavEntity]) -> EavAttribute {
    let entity = entities.iter().find(|e| e.code == r.entity_code);
    let value_table = if r.backend_type == "static" {
        None
    } else {
        entity.and_then(|e| {
            let base = e.value_table_prefix.clone().or_else(|| e.entity_table.clone())?;
            Some(format!("{base}_{}", r.backend_type))
        })
    };
    EavAttribute {
        code: r.code,
        entity_type: r.entity_code,
        attribute_id: r.attribute_id,
        label: r.label,
        backend_type: r.backend_type,
        frontend_input: r.frontend_input,
        required: r.required,
        unique: r.unique,
        user_defined: r.user_defined,
        default_value: r.default_value,
        source_model: r.source_model.map(ClassName::new),
        backend_model: r.backend_model.map(ClassName::new),
        frontend_model: r.frontend_model.map(ClassName::new),
        catalog: r.catalog.map(|c| EavCatalogFlags {
            scope: match c.is_global {
                1 => EavScope::Global,
                2 => EavScope::Website,
                _ => EavScope::Store,
            },
            searchable: c.searchable,
            filterable: c.filterable,
            filterable_in_search: c.filterable_in_search,
            comparable: c.comparable,
            used_in_listing: c.used_in_listing,
            used_for_sort_by: c.used_for_sort_by,
            visible_on_front: c.visible_on_front,
            apply_to: c
                .apply_to
                .map(|a| {
                    a.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
        }),
        value_table,
    }
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
