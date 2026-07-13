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
mod engine;
#[cfg(feature = "db")]
mod db;
mod decrypt;
mod deploy;
mod doctor;
mod eav;
mod graphql;
mod parse;
mod php;
mod phparray;
mod redis;
mod queries;
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
    Indexer, IndexerLive, InstanceInfo, Integration, InterceptKind,
    LayoutContribution, LayoutLayer, LayoutOp, LayoutOpKind, LayoutView,
    MenuItem, MethodChain, Module, ModuleCheck, ModuleDeps, Patch, PatchKind, Patches,
    Template, TemplateFile, TemplateUsage,
    MviewSubscription, Observer, PluginTarget,
    BundleOption, BundleSelection, Category, CategoryHit, CategoryIndexCount,
    CategoryIndexedProduct, CategoryProduct,
    CategoryTreeNode, CategoryVisibilityIssue,
    CatalogRule, CatalogRuleHit,
    ChildPrice, CmsEntry, CmsHit, CmsKind, Customer, CustomerAddress, CustomerGroup, CustomerHit,
    CustomerNewsletter, CustomerOrders,
    IndexedPrice, Order, OrderAddress, OrderComment, OrderDocument, OrderHit,
    OrderItem, OrderPayment, OrderShipment, OrderStatus, OrderStatusState, OrderTotal,
    OrderTransaction, SalesSequence,
    Preference, PreferenceStep, Plugin,
    PluginMethod, Product,
    ProductCategory, ProductChild, Quote, QuoteAddress, QuoteHit, QuoteItem, RuleCoupon,
    SalesDocKind, SalesDocument, SalesDocumentHit, SalesDocumentItem, SalesRule, SalesRuleHit,
    StoreGroupNode, StoreTree, StoreViewNode, TaxClassInfo, TaxInfo, TaxRate, TaxRule,
    WebsiteNode,
    ProductHit, ProductLegacyStock, ProductLinks, ProductLinkTarget, ProductMedia, ProductPrices,
    ProductRewrite, ProductScopeValue,
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

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// An opened Magento installation. Holds the parsed module index and merged per-area DI
/// config; every query method is a projection over it. Cheap to query, built once on
/// [`open`](Magento::open).
pub struct Magento {
    // Filled in by the indexer (steps 1–3). Kept private so the internal representation
    // can evolve without breaking the public, struct-returning API.
    index: engine::index::Index,
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
    index: engine::di::DiIndex,
    diagnostics: Vec<Diagnostic>,
}

/// The four OAuth 1.0a credentials of an integration, returned only by
/// [`Magento::integration_credentials`]. Deliberately **not** `Serialize` (unlike every
/// other public type here): these are live secrets and must never reach a `--json` path.
/// The access pair is `None` until the integration is activated.
#[cfg(feature = "db")]
#[non_exhaustive]
pub struct IntegrationCredentials {
    pub consumer_key: String,
    pub consumer_secret: String,
    pub access_token: Option<String>,
    pub access_secret: Option<String>,
    pub revoked: bool,
}

impl Magento {
    /// Build the index for the installation rooted at `root`.
    ///
    /// Fails only on problems that prevent any meaningful answer (no Magento root,
    /// unreadable `config.php`). Per-module problems are recorded as [`Diagnostic`]s and
    /// retrieved via [`diagnostics`](Magento::diagnostics) — a single broken file does not
    /// fail the build.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_overlay(root, std::collections::HashMap::new())
    }

    /// [`open`](Magento::open) with unsaved-buffer contents overlaid on the checkout:
    /// every content read of a source file prefers `overlay` (keyed by absolute path)
    /// over disk, so an editor frontend can analyze what's in its buffers. The overlay
    /// affects file *content* only — discovery and existence checks stay on the real
    /// filesystem, so a never-saved new file is invisible until saved. Like `open`, the
    /// handle is immutable: on buffer change, rebuild.
    pub fn open_with_overlay(
        root: impl AsRef<Path>,
        overlay: std::collections::HashMap<PathBuf, String>,
    ) -> Result<Self> {
        let index = engine::index::Index::build(
            root.as_ref(),
            std::sync::Arc::new(engine::vfs::Vfs::new(overlay)),
        )?;
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

    /// Locate the Magento root for an arbitrary path inside (or beside) an installation:
    /// the directory itself, then each ancestor, then — for monorepos that keep the shop
    /// in a subdirectory — each direct child, in name order so the answer is
    /// deterministic. A directory is a root iff `app/etc/config.php` exists, the same
    /// probe [`open`](Magento::open) requires. Editor frontends get handed workspace
    /// folders, not roots; the CLI's `--root` stays exact.
    pub fn find_root(start: impl AsRef<Path>) -> Option<PathBuf> {
        let start = start.as_ref();
        let is_root = |dir: &Path| dir.join("app/etc/config.php").is_file();
        for dir in start.ancestors() {
            if is_root(dir) {
                return Some(dir.to_path_buf());
            }
        }
        let mut children: Vec<PathBuf> = std::fs::read_dir(start)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        children.sort();
        children.into_iter().find(|child| is_root(child))
    }

    /// [`find_root`](Magento::find_root) + [`open`](Magento::open): open the installation
    /// that contains `start`. `Error::NotMagentoRoot` when neither `start`, an ancestor,
    /// nor a direct child is a Magento root.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self> {
        let start = start.as_ref();
        let root = Self::find_root(start).ok_or_else(|| Error::NotMagentoRoot {
            path: start.to_path_buf(),
        })?;
        Self::open(root)
    }

    /// The merged DI config, built (and its diagnostics collected) on first DI query.
    fn di_index(&self) -> &engine::di::DiIndex {
        &self
            .di
            .get_or_init(|| {
                let mut diagnostics = Vec::new();
                let index = engine::di::build(
                    &self.index.root,
                    &self.index.modules,
                    &self.index.vfs,
                    &mut diagnostics,
                );
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

    /// The installation root this handle was opened on.
    pub fn root(&self) -> &Path {
        &self.index.root
    }

    /// A source file's content as this handle sees it: the overlay's version for paths
    /// opened via [`open_with_overlay`](Magento::open_with_overlay), disk otherwise.
    /// Frontends read files through this so their answers match the index.
    pub fn read_source(&self, path: &Path) -> std::io::Result<String> {
        self.index.vfs.read_to_string(path)
    }

    /// The on-disk PHP file `class` resolves to via the composer PSR-4/PSR-0 maps (plus
    /// the app/code naming convention), if it exists. The jump-to-source primitive for
    /// editor frontends. `None` for PHP built-ins, generated code that hasn't been
    /// generated, and virtual types.
    pub fn class_file(&self, class: &ClassName) -> Option<PathBuf> {
        self.index.resolver.file_for(class)
    }

    /// Glob patterns (relative to [`root`](Magento::root), LSP watcher semantics: `**`
    /// matches zero or more path segments) of the files the index is computed from. A
    /// change to a matching file invalidates an open handle; there is no in-place refresh
    /// — a full [`open`](Magento::open) is the rebuild (~tens of ms warm, by design). One
    /// canonical list so every long-lived frontend registers the same watches.
    pub fn watch_globs() -> &'static [&'static str] {
        &[
            // Class headers, patches, EAV setup calls, console command names, and
            // config.php/env.php/registration.php — PHP is parsed on demand, so any .php
            // edit can change an answer (extends, plugin methods, addAttribute, …).
            "**/*.php",
            // Every module + primary config file (di, events, routes, webapi, acl, menu,
            // system, db_schema, crontab, widget, indexer, mview, queue_*, config.xml).
            "**/etc/**/*.xml",
            "**/etc/*.graphqls",
            // Package metadata: module discovery + the PSR-4 maps.
            "vendor/composer/installed.json",
            // Frontend indexes: layout, ui components, module email templates.
            "**/view/**/*.xml",
            "**/view/**/email/**",
            // Themes (layout/email/i18n overrides live outside view/).
            "app/design/**",
            // Translations.
            "**/i18n/*.csv",
            // Templates: create/delete changes the template catalog and override chains.
            "**/*.phtml",
        ]
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
        let referenced = |cfg: &engine::di::AreaConfig| {
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
        let targets = self.plugin_lookup_chain(&concrete);
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
        let targets = self.plugin_lookup_chain(&concrete);

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
    fn plugin_lookup_chain(&self, concrete: &ClassName) -> Vec<ClassName> {
        let mut targets = vec![concrete.clone()];
        targets.extend(self.index.resolver.ancestors(concrete));
        targets
    }

    /// Every `<plugin>` declaration whose *class* is `class` — the reverse of
    /// [`plugins`](Magento::plugins): given a plugin class, the types it intercepts.
    /// Merged across all areas, deduped by (type, plugin name), sorted. `declared_on` is
    /// the type as written in di.xml (often an interface/parent); resolve it with
    /// [`preference`](Magento::preference) for the concrete class that actually runs.
    pub fn plugin_targets(&self, class: &ClassName) -> Vec<PluginTarget> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for area in Area::ALL {
            let cfg = self.di_index().config(area);
            for (target, plugins) in &cfg.plugins {
                for (name, plugin) in plugins {
                    if plugin.class.as_ref() != Some(class) {
                        continue;
                    }
                    if seen.insert((target.clone(), name.clone())) {
                        out.push(PluginTarget {
                            declared_on: target.clone(),
                            plugin_name: name.clone(),
                            disabled: plugin.disabled,
                            source: plugin.source.clone(),
                        });
                    }
                }
            }
        }
        out.sort_by(|a, b| {
            (a.declared_on.as_str(), &a.plugin_name).cmp(&(b.declared_on.as_str(), &b.plugin_name))
        });
        out
    }

    /// The full ancestor set of a class (parents + interfaces, transitively,
    /// nearest-first) — the hierarchy plugin resolution walks, exposed for frontends
    /// that need it themselves (the LSP's jump from a plugin method to the intercepted
    /// implementation searches it for the method's defining file).
    pub fn ancestors(&self, class: &ClassName) -> Vec<ClassName> {
        self.index.resolver.ancestors(class)
    }

    /// Every class name derivable from the composer autoload maps and the app/code
    /// convention — the completion candidate set, sorted and deduped. A parallel
    /// directory walk (~100ms on a real install), so long-lived frontends cache the
    /// result outside the handle: it only changes when PHP files are created or
    /// deleted, never on content edits or handle rebuilds.
    pub fn class_names(&self) -> Vec<ClassName> {
        self.index.resolver.class_names()
    }

    /// Every virtual type name declared in any area's merged DI config — completion
    /// candidates alongside [`class_names`](Magento::class_names) (virtual types are
    /// valid wherever di.xml expects a type).
    pub fn virtual_type_names(&self) -> Vec<ClassName> {
        let mut names = std::collections::BTreeSet::new();
        for area in Area::ALL {
            for name in self.di_index().config(area).virtual_types.keys() {
                names.insert(name.clone());
            }
        }
        names.into_iter().collect()
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

        let env = deploy::read_env(&self.index.root, &self.index.vfs).ok();
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
                let config_php = deploy::read_config_php(&self.index.root, &self.index.vfs).ok();
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
        let root_of: std::collections::HashMap<&Path, &engine::index::PackageMeta> =
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
                None => read_app_composer(&x.path, &self.index.vfs),
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

/// A module's composer identity for the `deps` graph.
struct DepPkgInfo {
    name: String,
    require: Vec<String>,
    file: std::path::PathBuf,
}

/// Read an app/code module's own `composer.json` (they're not in installed.json).
fn read_app_composer(dir: &Path, vfs: &engine::vfs::Vfs) -> Option<DepPkgInfo> {
    #[derive(serde::Deserialize)]
    struct Cj {
        name: Option<String>,
        #[serde(default)]
        require: std::collections::HashMap<String, serde::de::IgnoredAny>,
    }
    let file = dir.join("composer.json");
    let cj: Cj = serde_json::from_str(&vfs.read_to_string(&file).ok()?).ok()?;
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
    args: Option<&std::collections::HashMap<String, engine::di::LocatedArg>>,
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

#[cfg(test)]
mod handle_tests {
    use std::path::PathBuf;

    /// Long-lived frontends (the LSP server) share one handle across threads and swap it
    /// on rebuild; a field that isn't `Send + Sync` (an `Rc`, a `RefCell`) must fail here
    /// at compile time, not at the editor integration.
    #[test]
    fn magento_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<super::Magento>();
    }

    /// A unique throwaway directory tree, removed on drop. std-only on purpose — the
    /// crate has no dev-dependencies and two tests don't justify one.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "magequery-test-{}-{}",
                std::process::id(),
                name
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn touch(&self, rel: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn find_root_walks_up_from_a_nested_folder() {
        let tree = TempTree::new("find-root-up");
        tree.touch("app/etc/config.php");
        tree.touch("app/code/Acme/Widget/etc/module.xml");

        let found = super::Magento::find_root(tree.0.join("app/code/Acme/Widget"));
        assert_eq!(found.as_deref(), Some(tree.0.as_path()));
    }

    #[test]
    fn find_root_probes_direct_children_of_a_monorepo_folder() {
        let tree = TempTree::new("find-root-down");
        tree.touch("docs/readme.md");
        tree.touch("shop/app/etc/config.php");

        let found = super::Magento::find_root(&tree.0);
        assert_eq!(found.as_deref(), Some(tree.0.join("shop").as_path()));
    }

    #[test]
    fn find_root_returns_none_outside_an_installation() {
        let tree = TempTree::new("find-root-none");
        tree.touch("src/main.rs");

        assert_eq!(super::Magento::find_root(tree.0.join("src")), None);
    }

    #[test]
    fn class_names_enumerates_the_convention_tree() {
        let tree = TempTree::new("class-names");
        let write = |rel: &str| {
            let path = tree.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "<?php\n").unwrap();
        };
        std::fs::create_dir_all(tree.0.join("app/code/Acme/Widget/etc")).unwrap();
        std::fs::create_dir_all(tree.0.join("app/etc")).unwrap();
        std::fs::write(
            tree.0.join("app/etc/config.php"),
            "<?php\nreturn ['modules' => ['Acme_Widget' => 1]];\n",
        )
        .unwrap();
        std::fs::write(
            tree.0.join("app/code/Acme/Widget/etc/module.xml"),
            r#"<config><module name="Acme_Widget"/></config>"#,
        )
        .unwrap();
        write("app/code/Acme/Widget/Model/Thing.php");
        write("app/code/Acme/Widget/Api/ThingInterface.php");
        write("app/code/Acme/Widget/registration.php"); // lowercase stem: not a class
        write("app/code/Acme/Widget/Test/Unit/ThingTest.php"); // test tree: skipped
        // Runtime-written code below an autoload-covered dir: never a candidate.
        write("app/code/Acme/Widget/generated/Thing/Interceptor.php");

        let magento = super::Magento::open(&tree.0).unwrap();
        let names: Vec<String> =
            magento.class_names().iter().map(|c| c.as_str().to_string()).collect();
        assert!(names.contains(&"Acme\\Widget\\Model\\Thing".to_string()), "{names:?}");
        assert!(names.contains(&"Acme\\Widget\\Api\\ThingInterface".to_string()));
        assert!(!names.iter().any(|n| n.contains("registration")));
        assert!(!names.iter().any(|n| n.contains("ThingTest")));
        assert!(!names.iter().any(|n| n.contains("Interceptor")), "{names:?}");
    }

    /// The two per-file lints fire: a layout template no file provides, and a plugin
    /// name declared twice for one type in one file.
    #[test]
    fn template_and_duplicate_plugin_lints_fire() {
        let tree = TempTree::new("nit-lints");
        let write = |rel: &str, content: &str| {
            let path = tree.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write("app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Widget' => 1]];\n");
        write(
            "app/code/Acme/Widget/etc/module.xml",
            r#"<config><module name="Acme_Widget"/></config>"#,
        );
        write(
            "app/code/Acme/Widget/etc/di.xml",
            r#"<config>
    <type name="Acme\Widget\Model\Thing">
        <plugin name="tweak" type="Acme\Widget\Plugin\A"/>
        <plugin name="tweak" type="Acme\Widget\Plugin\B"/>
    </type>
</config>"#,
        );
        write(
            "app/code/Acme/Widget/view/frontend/layout/default.xml",
            r#"<page><body>
    <block class="Acme\Widget\Block\Chip" name="acme.chip" template="Acme_Widget::missing.phtml"/>
</body></page>"#,
        );

        let magento = super::Magento::open(&tree.0).unwrap();
        let report = magento.doctor(None);
        let template_lint = report
            .findings
            .iter()
            .find(|f| f.lint == crate::model::DoctorLint::TemplateFileMissing)
            .expect("template lint fires");
        assert_eq!(template_lint.subject.as_deref(), Some("Acme_Widget::missing.phtml"));
        assert!(template_lint.source.is_some());

        let diags = magento.diagnostics();
        assert!(
            diags.iter().any(|d| d.message.contains("duplicate <plugin name=\"tweak\"")),
            "{:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    /// The buffer overlay wins over disk: the same root answers differently when a
    /// di.xml is overlaid — the editor's unsaved state, analyzed without saving.
    #[test]
    fn overlay_content_overrides_disk() {
        let tree = TempTree::new("overlay");
        let write = |rel: &str, content: &str| {
            let path = tree.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write("app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Widget' => 1]];\n");
        write(
            "app/code/Acme/Widget/etc/module.xml",
            r#"<config><module name="Acme_Widget"/></config>"#,
        );
        let di_path = tree.0.join("app/code/Acme/Widget/etc/di.xml");
        write(
            "app/code/Acme/Widget/etc/di.xml",
            r#"<config><preference for="Acme\Widget\Api\ThingInterface" type="Acme\Widget\Model\Disk"/></config>"#,
        );

        let iface = crate::ClassName::new("Acme\\Widget\\Api\\ThingInterface");
        let on_disk = super::Magento::open(&tree.0).unwrap();
        assert_eq!(
            on_disk.preference(&iface, crate::Area::Global).unwrap().concrete.as_str(),
            "Acme\\Widget\\Model\\Disk"
        );

        let overlay = std::collections::HashMap::from([(
            di_path,
            r#"<config><preference for="Acme\Widget\Api\ThingInterface" type="Acme\Widget\Model\Buffer"/></config>"#
                .to_string(),
        )]);
        let overlaid = super::Magento::open_with_overlay(&tree.0, overlay).unwrap();
        assert_eq!(
            overlaid.preference(&iface, crate::Area::Global).unwrap().concrete.as_str(),
            "Acme\\Widget\\Model\\Buffer"
        );
    }
}
