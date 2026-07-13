//! Static-index, frontend, and entry-point [`Magento`] queries.

use super::super::*;
#[cfg(feature = "db")]
use crate::db;

impl Magento {
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
            self.cron.get_or_init(|| breadth::CronIndex::build(&self.index.modules, &self.index.vfs)).jobs(group);
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
        self.routes.get_or_init(|| breadth::RouteIndex::build(&self.index.modules, &self.index.vfs)).routes(area)
    }

    /// REST endpoints from `webapi.xml`, optionally filtered by a URL substring.
    pub fn webapi(&self, url_filter: Option<&str>) -> Vec<WebapiRoute> {
        self.webapi.get_or_init(|| breadth::WebapiIndex::build(&self.index.modules, &self.index.vfs)).routes(url_filter)
    }

    fn schema_index(&self) -> &breadth::SchemaIndex {
        self.schema.get_or_init(|| breadth::SchemaIndex::build(&self.index.modules, &self.index.vfs))
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
            let Ok(text) = self.index.vfs.read_to_string(&m.path.join("etc/db_schema_whitelist.json"))
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
            .get_or_init(|| breadth::SystemConfigIndex::build(&self.index.modules, &self.index.vfs))
            .fields(filter)
    }

    fn acl_index(&self) -> &breadth::AclIndex {
        self.acl.get_or_init(|| breadth::AclIndex::build(&self.index.modules, &self.index.vfs))
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

    fn indexer_index(&self) -> &breadth::IndexerIndex {
        self.indexers.get_or_init(|| breadth::IndexerIndex::build(&self.index.modules, &self.index.vfs))
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

    fn mq_index(&self) -> &breadth::MqIndex {
        self.mq.get_or_init(|| breadth::MqIndex::build(&self.index.modules, &self.index.vfs))
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
                self.index.vfs.read_to_string(path)
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
            let Ok(text) = self.index.vfs.read_to_string(&dir.join("language.xml")) else { return };
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
        self.catalog_attrs.get_or_init(|| breadth::CatalogAttrIndex::build(&self.index.modules, &self.index.vfs))
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
            breadth::EmailTemplateIndex::build(&self.index.modules, &self.index.vfs, &self.discover_themes())
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
        self.widgets.get_or_init(|| breadth::WidgetIndex::build(&self.index.modules, &self.index.vfs))
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
            breadth::LayoutIndex::build(&self.index.modules, &self.index.vfs, &self.discover_themes())
        })
    }

    /// Themes on disk as `(id, dir)` — the public face of theme discovery, for frontends
    /// that resolve template overrides themselves (which override *applies* is active-
    /// theme runtime state; the set of candidates is static).
    pub fn themes(&self) -> Vec<(String, PathBuf)> {
        self.discover_themes()
    }

    /// Themes on disk as `(id, dir)`: composer packages whose root holds a `theme.xml`
    /// (id read from `registration.php`) plus `app/design/<area>/<Vendor>/<theme>`.
    fn discover_themes(&self) -> Vec<(String, std::path::PathBuf)> {
        let mut out = Vec::new();
        for p in &self.index.packages {
            if !p.root.join("theme.xml").is_file() {
                continue;
            }
            let Ok(reg) = self.index.vfs.read_to_string(&p.root.join("registration.php")) else {
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

    /// Templates in an area, including unreferenced `.phtml` files and unresolved layout
    /// references. Filter is a case-insensitive reference substring.
    pub fn templates(&self, area: Area, filter: Option<&str>) -> Vec<Template> {
        self.layout_index().templates(area, filter)
    }

    /// One exact `Vendor_Module::path.phtml` template joined to every physical candidate
    /// and layout usage in the area.
    pub fn template(&self, reference: &str, area: Area) -> Option<Template> {
        self.layout_index().template(reference, area)
    }

    fn ui_component_index(&self) -> &breadth::UiComponentIndex {
        self.ui_components.get_or_init(|| {
            breadth::UiComponentIndex::build(&self.index.modules, &self.index.vfs, &self.discover_themes())
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
        self.ext_attrs.get_or_init(|| breadth::ExtAttrIndex::build(&self.index.modules, &self.index.vfs))
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
        self.menu.get_or_init(|| breadth::MenuIndex::build(&self.index.modules, &self.index.vfs))
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
        self.gql.get_or_init(|| breadth::GqlIndex::build(&self.index.modules, &self.index.vfs))
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

    /// Every enabled module's source file (`.php`/`.xml`/`.phtml`/`.graphqls`) whose text
    /// contains `needle` as a substring — the candidate set for a rename sweep, and the one
    /// primitive that reaches occurrences no index holds (a PHP `dispatch()`/`isAllowed()`
    /// string literal can sit in any file). A **coarse** pre-filter: substring, not
    /// whole-token — the caller confirms each hit is a real reference. Reads go through the
    /// VFS so unsaved buffers are seen; the module directory trees are walked in parallel.
    /// Test-fixture and runtime/output dirs are skipped (the class walk's exclusions), so a
    /// rename never rewrites generated code or fixtures. Sorted, deduped.
    pub fn files_containing(&self, needle: &str) -> Vec<PathBuf> {
        use rayon::prelude::*;
        if needle.is_empty() {
            return Vec::new();
        }
        let lists: Vec<Vec<PathBuf>> = self
            .index
            .modules
            .par_iter()
            .map(|m| {
                let mut out = Vec::new();
                walk_source(&m.path, 0, &mut |path| {
                    if self.index.vfs.read_to_string(path).is_ok_and(|t| t.contains(needle)) {
                        out.push(path.to_path_buf());
                    }
                });
                out
            })
            .collect();
        let mut all: Vec<PathBuf> = lists.into_iter().flatten().collect();
        all.sort();
        all.dedup();
        all
    }

    fn events_index(&self) -> &breadth::EventIndex {
        self.events.get_or_init(|| breadth::EventIndex::build(&self.index.modules, &self.index.vfs))
    }

}

/// Recursively walk a module tree, calling `f` on each `.php`/`.xml`/`.phtml`/`.graphqls`
/// file. Skips test fixtures and runtime/output dirs (mirrors the class walk's exclusions)
/// so a rename sweep never touches generated code or fixtures.
fn walk_source(dir: &Path, depth: usize, f: &mut impl FnMut(&Path)) {
    if depth > 12 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(
                name.as_ref(),
                "Test" | "Tests" | "_files" | "node_modules" | "generated" | "var" | "pub" | "dev"
            ) || name.starts_with('.')
            {
                continue;
            }
            walk_source(&path, depth + 1, f);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("php" | "xml" | "phtml" | "graphqls")
        ) {
            f(&path);
        }
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
    resolver: &'a engine::resolver::ClassResolver,
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
