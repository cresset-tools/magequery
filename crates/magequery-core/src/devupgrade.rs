//! A fast `setup:upgrade` for development: decide natively whether the real command
//! has anything to do, and do the cheap parts of it in Rust when it hasn't.
//!
//! `bin/magento setup:upgrade` with nothing to upgrade still boots Magento and runs
//! every module's `Setup\Recurring` / `Setup\RecurringData` script. This module
//! splits that into:
//!
//! * [`Magento::upgraded_config_php`] — `Installer::createModulesConfig`: every
//!   module on disk in `<sequence>` order, a disabled one kept at 0. Only written
//!   when the current file is one this crate can reproduce byte for byte.
//! * [`Magento::dev_upgrade_check`] — everything that needs PHP: the
//!   `setup:db:status` validators, `app:config:status`, theme registration, and a
//!   check per core recurring script of whether it would change anything. A
//!   recurring script this module does not know (any non-core module's) always
//!   needs PHP.
//! * [`Magento::dev_upgrade_apply`] — the recurring work that is a plain write:
//!   Cron's "terminated by upgrade" update, the consumers' poison pill, and
//!   MysqlMq's missing queues.
//! * [`Magento::clean_caches`] — `Cache\Manager::clean` for file and Redis
//!   backends.
//!
//! A caller runs the PHP command whenever the check lists a reason, so a check that
//! errs towards "needs PHP" costs time, never correctness.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::db::query::Db;
use crate::ids::{ClassName, ModuleName};
use crate::phparray::PhpValue;
use crate::{deploy, Error, Magento, Result};

/// What `setup:upgrade` would do to `app/etc/config.php`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigPhpUpdate {
    /// The module list is already what Magento would write.
    Unchanged,
    /// The new file, and the modules that changed.
    Rewrite { contents: String, added: Vec<String>, removed: Vec<String>, reordered: bool },
    /// The current file is not in the form Magento writes (hand-edited, a constant,
    /// comments), so rewriting it here could differ from what Magento would write.
    CannotReproduce(String),
}

/// What `DbInitStatementsCleanup` would do to `app/etc/env.php`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvPhpUpdate {
    Unchanged,
    /// The new file.
    Rewrite(String),
    /// The cleanup applies, but env.php is not laid out the way Magento writes it.
    CannotReproduce(String),
}

/// The outcome of [`Magento::dev_upgrade_check`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DevUpgradeCheck {
    /// Why the real `setup:upgrade` has to run. Empty means the fast path applies.
    pub needs_php: Vec<String>,
    /// MysqlMq queues to insert (the one recurring write with content).
    pub missing_queues: Vec<String>,
    /// Recurring scripts proven to do nothing this time, or deliberately skipped.
    pub recurring_skipped: Vec<String>,
}

/// The core recurring scripts this module knows, by module. Any other module with
/// a `Setup\Recurring` or `Setup\RecurringData` class needs PHP.
const KNOWN_RECURRING: &[&str] = &[
    "Magento_Amqp",
    "Magento_Bundle",
    "Magento_Catalog",
    "Magento_CatalogInventory",
    "Magento_CatalogUrlRewrite",
    "Magento_ConfigurableProduct",
    "Magento_Cron",
    "Magento_Customer",
    "Magento_Indexer",
    "Magento_Integration",
    "Magento_MessageQueue",
    "Magento_MysqlMq",
    "Magento_ProductAlert",
    "Magento_Reports",
    "Magento_SalesSequence",
    "Magento_Stomp",
    "Magento_Theme",
    "Magento_Weee",
    "Magento_Wishlist",
];

const PRODUCT: (&str, &str) = ("catalog_product_entity", "entity_id");
const CATEGORY: (&str, &str) = ("catalog_category_entity", "entity_id");

/// `ExternalFKSetup::install` calls, per module: (external table, column, entity).
const EXTERNAL_FKS: &[(&str, &str, &str, (&str, &str))] = &[
    ("Magento_Bundle", "catalog_product_bundle_price_index", "entity_id", PRODUCT),
    ("Magento_Bundle", "catalog_product_bundle_selection", "product_id", PRODUCT),
    ("Magento_Catalog", "catalog_category_product", "category_id", CATEGORY),
    ("Magento_CatalogInventory", "cataloginventory_stock_item", "product_id", PRODUCT),
    ("Magento_CatalogUrlRewrite", "catalog_url_rewrite_product_category", "category_id", CATEGORY),
    ("Magento_CatalogUrlRewrite", "catalog_url_rewrite_product_category", "product_id", PRODUCT),
    ("Magento_ConfigurableProduct", "catalog_product_super_link", "product_id", PRODUCT),
    ("Magento_ProductAlert", "product_alert_price", "product_id", PRODUCT),
    ("Magento_ProductAlert", "product_alert_stock", "product_id", PRODUCT),
    ("Magento_Reports", "report_viewed_product_aggregated_daily", "product_id", PRODUCT),
    ("Magento_Reports", "report_viewed_product_aggregated_monthly", "product_id", PRODUCT),
    ("Magento_Reports", "report_viewed_product_aggregated_yearly", "product_id", PRODUCT),
    ("Magento_Reports", "report_compared_product_index", "product_id", PRODUCT),
    ("Magento_Reports", "report_viewed_product_index", "product_id", PRODUCT),
    ("Magento_Weee", "weee_tax", "entity_id", PRODUCT),
    ("Magento_Wishlist", "wishlist_item", "product_id", PRODUCT),
];

/// `Magento\Sales`'s `EntityPool` entities, and the stores `SequenceCreator` covers.
const SEQUENCE_ENTITIES: &[&str] = &["order", "invoice", "creditmemo", "shipment"];
const SEQUENCE_STORES: &[u32] = &[0, 1];

impl Magento {
    /// `DbInitStatementsCleanup::execute`, the first thing `setup:upgrade` does: drop
    /// the deprecated `SET NAMES utf8` from every connection's `initStatements`
    /// (and the key itself when nothing is left).
    pub fn cleaned_env_php(&self) -> Result<EnvPhpUpdate> {
        let path = self.root().join("app/etc/env.php");
        let text = std::fs::read_to_string(&path).map_err(|source| Error::Io { file: path.clone(), source })?;
        let mut tree = crate::phparray::parse(&text).map_err(|detail| Error::Parse { file: path.clone(), detail })?;
        let mut modified = false;
        if let PhpValue::Array(top) = &mut tree
            && let Some((_, PhpValue::Array(db))) = top.iter_mut().find(|(k, _)| k.as_str() == Some("db"))
        {
            for (k, group) in db.iter_mut() {
                if !matches!(k.as_str(), Some("connection" | "slave_connection")) {
                    continue;
                }
                let PhpValue::Array(conns) = group else { continue };
                for (_, conn) in conns.iter_mut() {
                    let PhpValue::Array(items) = conn else { continue };
                    let Some(i) = items.iter().position(|(k, _)| k.as_str() == Some("initStatements")) else {
                        continue;
                    };
                    let PhpValue::Str(init) = &items[i].1 else { continue };
                    match remove_set_names_utf8(init) {
                        None => {
                            items.remove(i);
                            modified = true;
                        }
                        Some(cleaned) if cleaned != *init => {
                            items[i].1 = PhpValue::Str(cleaned);
                            modified = true;
                        }
                        Some(_) => {}
                    }
                }
            }
        }
        if !modified {
            return Ok(EnvPhpUpdate::Unchanged);
        }
        let original = crate::phparray::parse(&text).map_err(|detail| Error::Parse { file: path.clone(), detail })?;
        if original.to_magento_php_file().as_deref() != Some(text.as_str()) {
            return Ok(EnvPhpUpdate::CannotReproduce(
                "app/etc/env.php needs its SET NAMES utf8 removed and is not laid out the way Magento writes it".into(),
            ));
        }
        match tree.to_magento_php_file() {
            Some(contents) => Ok(EnvPhpUpdate::Rewrite(contents)),
            None => Ok(EnvPhpUpdate::CannotReproduce("app/etc/env.php holds a value Magento would rewrite".into())),
        }
    }

    /// `Installer::createModulesConfig` without `--enable-modules`/`--disable-modules`:
    /// every module on disk, in the order `ModuleList\Loader` sorts them, a module
    /// that `config.php` disables staying at 0, and modules no longer on disk
    /// dropped. The other sections keep their place and content.
    pub fn upgraded_config_php(&self) -> Result<ConfigPhpUpdate> {
        let path = self.root().join("app/etc/config.php");
        let text = std::fs::read_to_string(&path).map_err(|source| Error::Io { file: path.clone(), source })?;
        let tree = crate::phparray::parse(&text).map_err(|detail| Error::Parse { file: path.clone(), detail })?;
        match tree.to_magento_php_file() {
            Some(same) if same == text => {}
            _ => {
                return Ok(ConfigPhpUpdate::CannotReproduce(
                    "app/etc/config.php is not laid out the way Magento writes it".into(),
                ))
            }
        }
        let PhpValue::Array(sections) = &tree else {
            return Ok(ConfigPhpUpdate::CannotReproduce("app/etc/config.php does not return an array".into()));
        };

        let current: Vec<(String, bool)> = match tree.get("modules").and_then(PhpValue::as_array) {
            Some(items) => items
                .iter()
                .filter_map(|(k, v)| {
                    // `!$currentModules[$module]`: 0, false and "0" are off.
                    let enabled = !matches!(v.scalar_string().as_deref(), Some("0") | Some("false") | Some(""));
                    Some((k.as_str()?.to_string(), enabled))
                })
                .collect(),
            None => Vec::new(),
        };
        let disabled: BTreeSet<&str> = current.iter().filter(|(_, on)| !on).map(|(n, _)| n.as_str()).collect();

        // Every module on disk: the ones config.php lists and the ones it doesn't.
        let mut on_disk: Vec<(ModuleName, Vec<ModuleName>)> =
            self.index.modules.iter().map(|m| (m.name.clone(), m.sequence.clone())).collect();
        on_disk.extend(self.index.check.on_disk_not_in_config.iter().map(|m| (m.name.clone(), m.sequence.clone())));
        let order = crate::engine::index::sort_by_sequence(on_disk.iter().map(|(n, s)| (n, s.as_slice())))
            .map_err(|detail| Error::Parse { file: PathBuf::from("etc/module.xml"), detail })?;

        let wanted: Vec<(String, bool)> =
            order.iter().map(|n| (n.as_str().to_string(), !disabled.contains(n.as_str()))).collect();
        if wanted == current {
            return Ok(ConfigPhpUpdate::Unchanged);
        }

        let before: BTreeSet<&str> = current.iter().map(|(n, _)| n.as_str()).collect();
        let after: BTreeSet<&str> = wanted.iter().map(|(n, _)| n.as_str()).collect();
        let added: Vec<String> = after.difference(&before).map(|s| s.to_string()).collect();
        let removed: Vec<String> = before.difference(&after).map(|s| s.to_string()).collect();
        let reordered = added.is_empty() && removed.is_empty();

        let modules = PhpValue::Array(
            wanted
                .iter()
                .map(|(n, on)| (PhpValue::Str(n.clone()), PhpValue::Int(i64::from(*on))))
                .collect(),
        );
        // `array_merge($current, ['modules' => …])`: the key keeps its position,
        // or goes last when the file had none.
        let mut items = sections.clone();
        match items.iter_mut().find(|(k, _)| k.as_str() == Some("modules")) {
            Some((_, v)) => *v = modules,
            None => items.push((PhpValue::Str("modules".into()), modules)),
        }
        let Some(contents) = PhpValue::Array(items).to_magento_php_file() else {
            return Ok(ConfigPhpUpdate::CannotReproduce("app/etc/config.php holds a value Magento would rewrite".into()));
        };
        Ok(ConfigPhpUpdate::Rewrite { contents, added, removed, reordered })
    }

    /// Whether a development `setup:upgrade` needs PHP, and if not, which of its
    /// writes are left to do natively. Errors only when the store cannot be read at
    /// all; a check that cannot decide adds a reason instead.
    pub fn dev_upgrade_check(&self) -> Result<DevUpgradeCheck> {
        let mut out = DevUpgradeCheck::default();
        let env = deploy::read_env(self.root(), &self.index.vfs)?;
        if env.get("MAGE_MODE").and_then(PhpValue::as_str) == Some("production") {
            return Err(Error::Db(
                "MAGE_MODE is production: the fast setup:upgrade is for development only".into(),
            ));
        }

        // The validators of setup:db:status.
        match self.db_status() {
            Ok(st) if st.is_up_to_date() => {}
            Ok(st) => {
                let mut what = Vec::new();
                if !st.schema_changes.is_empty() {
                    what.push(format!("{} schema change(s)", st.schema_changes.len()));
                }
                if !st.schema_patches.is_empty() {
                    what.push(format!("{} schema patch(es)", st.schema_patches.len()));
                }
                if !st.data_patches.is_empty() {
                    what.push(format!("{} data patch(es)", st.data_patches.len()));
                }
                if !st.module_versions.is_empty() {
                    what.push(format!("{} module version(s)", st.module_versions.len()));
                }
                what.extend(st.errors.iter().map(|e| format!("check failed: {e}")));
                out.needs_php.push(format!("database: {}", what.join(", ")));
            }
            Err(e) => out.needs_php.push(format!("database: {e}")),
        }
        match self.config_status() {
            Ok(st) if !st.import_required() => {}
            Ok(st) => {
                let changed: Vec<&str> = st.sections.iter().filter(|s| s.changed).map(|s| s.name.as_str()).collect();
                out.needs_php.push(format!("app:config:import: {} changed", changed.join(", ")));
            }
            Err(e) => out.needs_php.push(format!("app:config:import: {e}")),
        }

        self.check_recurring(&env, &mut out);
        Ok(out)
    }

    fn check_recurring(&self, env: &PhpValue, out: &mut DevUpgradeCheck) {
        let enabled: HashMap<&str, &Path> =
            self.index.modules.iter().filter(|m| m.enabled).map(|m| (m.name.as_str(), m.path.as_path())).collect();

        // `Installer::getSchemaDataHandler`: `Vendor\Module\Setup\Recurring{,Data}`.
        let mut unknown = Vec::new();
        for m in self.index.modules.iter().filter(|m| m.enabled) {
            if KNOWN_RECURRING.contains(&m.name.as_str()) {
                continue;
            }
            let ns = m.name.as_str().replace('_', "\\");
            for class in ["Recurring", "RecurringData"] {
                let class = ClassName::new(format!("{ns}\\Setup\\{class}"));
                if self.index.resolver.file_for(&class).is_some() {
                    unknown.push(class.as_str().to_string());
                }
            }
        }
        if !unknown.is_empty() {
            out.needs_php.push(format!("recurring setup scripts outside core: {}", unknown.join(", ")));
        }

        let on = |m: &str| enabled.contains_key(m);

        // Broker topology (Amqp, Stomp).
        for (module, key) in [("Magento_Amqp", "amqp"), ("Magento_Stomp", "stomp")] {
            if !on(module) {
                continue;
            }
            let configured = env
                .get("queue")
                .and_then(|q| q.get(key))
                .and_then(|a| a.get("host"))
                .and_then(PhpValue::scalar_string)
                .is_some_and(|h| !h.is_empty());
            if configured {
                out.needs_php.push(format!("{module}: the {key} broker topology is declared by PHP"));
            } else {
                out.recurring_skipped.push(format!("{module} (no {key} connection)"));
            }
        }

        // Config-based integrations.
        if on("Magento_Integration") {
            let declaring: Vec<&str> = self
                .index
                .modules
                .iter()
                .filter(|m| m.enabled && m.path.join("etc/integration/config.xml").is_file())
                .map(|m| m.name.as_str())
                .collect();
            if declaring.is_empty() {
                out.recurring_skipped.push("Magento_Integration (no integration/config.xml)".into());
            } else {
                out.needs_php.push(format!("Magento_Integration: integrations declared by {}", declaring.join(", ")));
            }
        }

        // Theme registration (the design-config grid rebuild is skipped).
        if on("Magento_Theme") {
            match self.unregistered_themes() {
                Ok(missing) if missing.is_empty() => {
                    out.recurring_skipped.push("Magento_Theme (all themes registered)".into())
                }
                Ok(missing) => out.needs_php.push(format!("themes to register: {}", missing.join(", "))),
                Err(e) => out.needs_php.push(format!("theme registration: {e}")),
            }
        }

        // MysqlMq: the missing queues are inserted natively.
        if on("Magento_MysqlMq") {
            match self.missing_db_queues() {
                Ok(q) => out.missing_queues = q,
                Err(e) => out.needs_php.push(format!("Magento_MysqlMq: {e}")),
            }
        }

        // The rest read the database.
        let mut db = match self.upgrade_db() {
            Ok(db) => db,
            Err(e) => {
                out.needs_php.push(format!("database: {e}"));
                return;
            }
        };
        let prefix = self.db_config().map(|c| c.table_prefix).unwrap_or_default();
        let (db, prefix) = (&mut db, prefix.as_str());

        self.check_external_fks(db, prefix, &on, out);
        if on("Magento_SalesSequence") {
            self.check_sales_sequences(db, prefix, out);
        }
        if on("Magento_Customer") {
            check_customer_grid(db, prefix, out);
        }
        if on("Magento_Indexer") {
            self.check_indexers(db, prefix, out);
        }
    }

    fn upgrade_db(&self) -> Result<Db> {
        let cfg = self.db_config()?;
        let conn = crate::default_connection(&cfg)?.clone();
        crate::db::status::open(&conn).map_err(Error::Db)
    }

    /// `ExternalFKSetup::install` adds a foreign key only when the external column
    /// has none to the entity table, and only when the entity column is the primary
    /// key; otherwise (a staging `row_id` schema) it rewrites keys, left to PHP.
    fn check_external_fks(&self, db: &mut Db, prefix: &str, on: &dyn Fn(&str) -> bool, out: &mut DevUpgradeCheck) {
        let mut missing = Vec::new();
        let mut checked: BTreeSet<&str> = BTreeSet::new();
        for &(module, table, column, (entity, entity_column)) in EXTERNAL_FKS {
            if !on(module) {
                continue;
            }
            checked.insert(module);
            let primary: std::result::Result<Option<String>, String> = db.exec_first(
                "SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
                 WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? AND CONSTRAINT_NAME = 'PRIMARY' \
                 AND COLUMN_NAME = ?",
                (format!("{prefix}{entity}"), entity_column),
            );
            match primary {
                Ok(Some(_)) => {}
                Ok(None) => {
                    missing.push(format!("{module}: {entity}.{entity_column} is not the primary key"));
                    continue;
                }
                Err(e) => {
                    missing.push(format!("{module}: {e}"));
                    continue;
                }
            }
            let fk: std::result::Result<Option<String>, String> = db.exec_first(
                "SELECT CONSTRAINT_NAME FROM information_schema.KEY_COLUMN_USAGE \
                 WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? AND COLUMN_NAME = ? \
                 AND REFERENCED_TABLE_NAME = ?",
                (format!("{prefix}{table}"), column, format!("{prefix}{entity}")),
            );
            match fk {
                Ok(Some(_)) => {}
                Ok(None) => missing.push(format!("{module}: foreign key {table}.{column} → {entity}")),
                Err(e) => missing.push(format!("{module}: {e}")),
            }
        }
        if missing.is_empty() {
            out.recurring_skipped.extend(checked.into_iter().map(|m| format!("{m} (foreign keys present)")));
        } else {
            out.needs_php.extend(missing);
        }
    }

    /// `SequenceCreator::create`: a `sequence_<entity>_<store>` table recorded in
    /// `sales_sequence_meta` for stores 0 and 1 and every `EntityPool` entity.
    fn check_sales_sequences(&self, db: &mut Db, prefix: &str, out: &mut DevUpgradeCheck) {
        // The pool is a di.xml argument; a module other than Magento_Sales adding to
        // it would add an entity this list does not know.
        let extended: Vec<&str> = self
            .index
            .modules
            .iter()
            .filter(|m| m.enabled && m.name.as_str() != "Magento_Sales")
            .filter(|m| {
                std::fs::read_to_string(m.path.join("etc/di.xml"))
                    .is_ok_and(|s| s.contains("Magento\\SalesSequence\\Model\\EntityPool"))
            })
            .map(|m| m.name.as_str())
            .collect();
        if !extended.is_empty() {
            out.needs_php.push(format!("Magento_SalesSequence: EntityPool extended by {}", extended.join(", ")));
            return;
        }
        let rows: std::result::Result<Vec<(String, u32, String)>, String> = db.query(format!(
            "SELECT entity_type, store_id, sequence_table FROM `{prefix}sales_sequence_meta`"
        ));
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                out.needs_php.push(format!("Magento_SalesSequence: {e}"));
                return;
            }
        };
        let have: BTreeSet<(String, u32, String)> = rows.into_iter().collect();
        let mut missing = Vec::new();
        for entity in SEQUENCE_ENTITIES {
            for &store in SEQUENCE_STORES {
                let table = format!("{prefix}sequence_{entity}_{store}");
                if !have.contains(&(entity.to_string(), store, table.clone())) {
                    missing.push(table);
                }
            }
        }
        if missing.is_empty() {
            out.recurring_skipped.push("Magento_SalesSequence (sequences present)".into());
        } else {
            out.needs_php.push(format!("Magento_SalesSequence: missing {}", missing.join(", ")));
        }
    }

    /// Indexer Recurring: every indexer has a state row whose `hash_config` matches
    /// its configuration, and every scheduled indexer has its mview triggers.
    fn check_indexers(&self, db: &mut Db, prefix: &str, out: &mut DevUpgradeCheck) {
        match self.indexer_config_hashes() {
            Ok(expected) => {
                let rows: std::result::Result<Vec<(String, Option<String>)>, String> =
                    db.query(format!("SELECT indexer_id, hash_config FROM `{prefix}indexer_state`"));
                match rows {
                    Ok(rows) => {
                        let stored: HashMap<String, Option<String>> = rows.into_iter().collect();
                        let mut stale = Vec::new();
                        for (id, hash) in &expected {
                            match stored.get(id) {
                                Some(Some(h)) if h == hash => {}
                                Some(_) => stale.push(format!("{id} (config changed)")),
                                None => stale.push(format!("{id} (new)")),
                            }
                        }
                        if !stale.is_empty() {
                            out.needs_php.push(format!("Magento_Indexer: {}", stale.join(", ")));
                            return;
                        }
                    }
                    Err(e) => {
                        out.needs_php.push(format!("Magento_Indexer: {e}"));
                        return;
                    }
                }
            }
            Err(e) => {
                out.needs_php.push(format!("Magento_Indexer: {e}"));
                return;
            }
        }
        match self.missing_mview_triggers(db, prefix) {
            Ok(missing) if missing.is_empty() => {
                out.recurring_skipped.push("Magento_Indexer (config hashes and triggers current)".into())
            }
            Ok(missing) => out.needs_php.push(format!("Magento_Indexer: missing triggers {}", missing.join(", "))),
            Err(e) => out.needs_php.push(format!("Magento_Indexer: {e}")),
        }
    }

    /// The writes left on the fast path: Cron marks running jobs as terminated,
    /// MessageQueue puts a new poison pill so consumers restart on the new code,
    /// MysqlMq inserts missing queues. Returns a line per write.
    pub fn dev_upgrade_apply(&self, check: &DevUpgradeCheck) -> Result<Vec<String>> {
        let on = |m: &str| self.index.modules.iter().any(|x| x.enabled && x.name.as_str() == m);
        let mut db = self.upgrade_db()?;
        let prefix = self.db_config()?.table_prefix;
        let mut done = Vec::new();

        // `Installer::cleanMemoryTables`: every MEMORY-engine table is emptied.
        let memory: Vec<String> = db
            .query(
                "SELECT TABLE_NAME FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = DATABASE() AND ENGINE = 'MEMORY'",
            )
            .map_err(Error::Db)?;
        for t in &memory {
            db.query_drop(format!("TRUNCATE TABLE `{}`", t.replace('`', "``"))).map_err(Error::Db)?;
        }
        if !memory.is_empty() {
            done.push(format!("memory tables emptied: {}", memory.join(", ")));
        }
        if on("Magento_Cron") {
            db.exec::<u8>(
                format!(
                    "UPDATE `{prefix}cron_schedule` SET status = 'error', \
                     messages = 'The job is terminated due to system upgrade' WHERE status = 'running'"
                ),
                (),
            )
            .map_err(Error::Db)?;
            done.push("cron: running jobs marked as terminated".into());
        }
        if on("Magento_MysqlMq") && !check.missing_queues.is_empty() {
            for q in &check.missing_queues {
                db.exec::<u8>(format!("INSERT INTO `{prefix}queue` (name) VALUES (?)"), (q,)).map_err(Error::Db)?;
            }
            done.push(format!("queues added: {}", check.missing_queues.join(", ")));
        }
        if on("Magento_MessageQueue") {
            let version = php_uniqid("version-");
            let table = format!("`{prefix}queue_poison_pill`");
            let existing: Option<String> =
                db.query_first(format!("SELECT version FROM {table} LIMIT 1")).map_err(Error::Db)?;
            if existing.is_some_and(|v| !v.is_empty()) {
                db.exec::<u8>(format!("UPDATE {table} SET version = ?"), (&version,)).map_err(Error::Db)?;
            } else {
                db.exec::<u8>(format!("INSERT INTO {table} (version) VALUES (?)"), (&version,)).map_err(Error::Db)?;
            }
            done.push(format!("consumers told to restart (poison pill {version})"));
        }
        Ok(done)
    }

    /// `Cache\Manager::clean` over every cache type, as a flush of each configured
    /// frontend: a file backend's directory is emptied, a Redis backend's database
    /// flushed. Other backends are an error, for the caller to hand to PHP.
    pub fn clean_caches(&self) -> std::result::Result<Vec<String>, String> {
        let env = deploy::read_env(self.root(), &self.index.vfs).map_err(|e| e.to_string())?;
        let cache = deploy::cache_config(&env);
        let redis = deploy::redis_config(&env);
        let mut done = Vec::new();
        for (id, dir) in [("default", "var/cache"), ("page_cache", "var/page_cache")] {
            let fe = cache.frontends.iter().find(|f| f.id == id);
            let backend = fe.map(|f| f.backend.trim_start_matches('\\').to_string()).unwrap_or_default();
            match backend.as_str() {
                "" | "Cm_Cache_Backend_File" | "Magento\\Framework\\Cache\\Backend\\File" | "file" => {
                    let path = self.root().join(dir);
                    let n = empty_dir(&path).map_err(|e| format!("{id} cache: emptying {dir}: {e}"))?;
                    done.push(format!("{id} cache: emptied {dir} ({n} entries)"));
                }
                "Cm_Cache_Backend_Redis" | "Magento\\Framework\\Cache\\Backend\\Redis" | "redis" => {
                    let inst = redis
                        .instances
                        .iter()
                        .find(|i| i.purpose == id)
                        .ok_or_else(|| format!("{id} cache: no Redis settings in env.php"))?;
                    crate::redis::flushdb(inst).map_err(|e| format!("{id} cache: {e}"))?;
                    done.push(format!(
                        "{id} cache: flushed Redis {}{}",
                        inst.host,
                        inst.database.as_deref().map(|d| format!(" db {d}")).unwrap_or_default()
                    ));
                }
                other => return Err(format!("{id} cache: backend {other} is cleaned by PHP")),
            }
        }
        Ok(done)
    }
}

impl Magento {
    /// Tables a scheduled indexer subscribes to whose `AFTER INSERT/UPDATE/DELETE`
    /// trigger is missing. Indexer Recurring drops and re-creates these triggers on
    /// every run; when they all exist the re-creation is taken as a no-op (their
    /// bodies are not compared).
    fn missing_mview_triggers(&self, db: &mut Db, prefix: &str) -> std::result::Result<Vec<String>, String> {
        let enabled: Vec<String> = db.query(format!("SELECT view_id FROM `{prefix}mview_state` WHERE mode = 'enabled'"))?;
        if enabled.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(String, String)> = db.query(
            "SELECT EVENT_OBJECT_TABLE, EVENT_MANIPULATION FROM information_schema.TRIGGERS \
             WHERE TRIGGER_SCHEMA = DATABASE() AND ACTION_TIMING = 'AFTER'",
        )?;
        let have: BTreeSet<(String, String)> =
            rows.into_iter().map(|(t, e)| (t, e.to_ascii_uppercase())).collect();
        let indexers = self.indexers(None, false).map_err(|e| e.to_string())?;
        let mut missing = BTreeSet::new();
        for ix in indexers.iter().filter(|ix| ix.view_id.as_ref().is_some_and(|v| enabled.contains(v))) {
            for sub in &ix.subscriptions {
                let table = format!("{prefix}{}", sub.table);
                for event in ["INSERT", "UPDATE", "DELETE"] {
                    if !have.contains(&(table.clone(), event.to_string())) {
                        missing.insert(format!("{table} {}", event.to_ascii_lowercase()));
                    }
                }
            }
        }
        Ok(missing.into_iter().collect())
    }
}

/// Customer RecurringData reindexes the customer grid when its table is missing or
/// its indexer is invalid.
fn check_customer_grid(db: &mut Db, prefix: &str, out: &mut DevUpgradeCheck) {
    let table: std::result::Result<Option<String>, String> = db.exec_first(
        "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?",
        (format!("{prefix}customer_grid_flat"),),
    );
    let status: std::result::Result<Option<String>, String> = db.query_first(format!(
        "SELECT status FROM `{prefix}indexer_state` WHERE indexer_id = 'customer_grid'"
    ));
    match (table, status) {
        (Ok(Some(_)), Ok(Some(s))) if s != "invalid" => {
            out.recurring_skipped.push("Magento_Customer (customer grid valid)".into())
        }
        (Ok(None), _) => out.needs_php.push("Magento_Customer: customer_grid_flat is missing".into()),
        (Err(e), _) | (_, Err(e)) => out.needs_php.push(format!("Magento_Customer: {e}")),
        _ => out.needs_php.push("Magento_Customer: the customer grid needs a reindex".into()),
    }
}

/// Empty a directory the way `CleanupFiles::emptyDir` does: every entry
/// `Directory::search('*')` returns, which leaves dotfiles (`.htaccess`) alone. A
/// missing directory is nothing to do.
pub fn empty_dir(dir: &Path) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut n = 0;
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
        n += 1;
    }
    Ok(n)
}

/// PHP's `uniqid($prefix)`: the prefix, then the time as 8 hex digits of seconds
/// and 5 of microseconds.
fn php_uniqid(prefix: &str) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("{prefix}{:08x}{:05x}", now.as_secs(), now.subsec_micros())
}

/// `InitStatementsCleanup::removeSetNamesUtf8`: `None` means "remove the key".
fn remove_set_names_utf8(init: &str) -> Option<String> {
    // PHP's empty(): "" and "0".
    let php_empty = |s: &str| s.is_empty() || s == "0";
    if php_empty(php_trim(init, PHP_TRIM)) {
        return None;
    }
    if !init.to_ascii_lowercase().contains("set names utf8") {
        return Some(init.to_string());
    }
    let cleaned = replace_set_names(init);
    let cleaned = php_trim(&cleaned, PHP_TRIM).to_string();
    let cleaned = collapse_semicolons(&cleaned);
    let cleaned = php_trim(&cleaned, ";");
    let cleaned = collapse_ws(cleaned);
    if php_empty(&cleaned) { None } else { Some(cleaned + ";") }
}

/// PHP trim()'s default characters.
const PHP_TRIM: &str = " \t\n\r\0\x0B";

fn php_trim<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_matches(|c| chars.contains(c))
}

/// PCRE `\s`: space, \t, \n, \v, \f, \r.
fn is_pcre_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `preg_replace('/\s*SET\s+NAMES\s+utf8(?!mb4)(?:\s*;|\s+COLLATE\s+[\w_]+\s*;?)\s*/i', '', $s)`.
fn replace_set_names(s: &str) -> String {
    let b = s.as_bytes();
    let ws = |mut i: usize| {
        while i < b.len() && is_pcre_space(b[i]) {
            i += 1;
        }
        i
    };
    let lit = |i: usize, w: &str| -> Option<usize> {
        let end = i + w.len();
        (end <= b.len() && b[i..end].eq_ignore_ascii_case(w.as_bytes())).then_some(end)
    };
    let ws1 = |i: usize| -> Option<usize> {
        let j = ws(i);
        (j > i).then_some(j)
    };
    let attempt = |start: usize| -> Option<usize> {
        let i = lit(ws(start), "SET")?;
        let i = lit(ws1(i)?, "NAMES")?;
        let i = lit(ws1(i)?, "utf8")?;
        if lit(i, "mb4").is_some() {
            return None;
        }
        // `\s*;`
        let j = ws(i);
        let end = if j < b.len() && b[j] == b';' {
            j + 1
        } else {
            // `\s+COLLATE\s+[\w_]+\s*;?`
            let k = lit(ws1(i)?, "COLLATE")?;
            let mut k = ws1(k)?;
            let w0 = k;
            while k < b.len() && (b[k].is_ascii_alphanumeric() || b[k] == b'_') {
                k += 1;
            }
            if k == w0 {
                return None;
            }
            let k = ws(k);
            if k < b.len() && b[k] == b';' { k + 1 } else { k }
        };
        Some(ws(end))
    };
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match attempt(i) {
            Some(end) => i = end,
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `preg_replace('/;\s*;+/', ';', $s)`.
fn collapse_semicolons(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b';' {
            let mut j = i + 1;
            while j < b.len() && is_pcre_space(b[j]) {
                j += 1;
            }
            if j < b.len() && b[j] == b';' {
                while j < b.len() && b[j] == b';' {
                    j += 1;
                }
                out.push(b';');
                i = j;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `preg_replace('/\s+/', ' ', $s)`.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_ascii() && is_pcre_space(c as u8) {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

#[cfg(test)]
mod init_statement_tests {
    use super::remove_set_names_utf8 as clean;

    /// Expected values from PHP running Magento's own `removeSetNamesUtf8`.
    #[test]
    fn set_names_utf8_is_removed_like_magento() {
        let cases: &[(&str, Option<&str>)] = &[
            ("SET NAMES utf8;", None),
            ("  ", None),
            ("SET NAMES utf8mb4;", Some("SET NAMES utf8mb4;")),
            ("SET NAMES utf8; SET time_zone = '+00:00';", Some("SET time_zone = '+00:00';")),
            ("SET sql_mode = '';\nset names UTF8 collate utf8_general_ci;  SET  a=1;;", Some("SET sql_mode = '';SET a=1;")),
            ("SET time_zone = '+00:00';", Some("SET time_zone = '+00:00';")),
            ("0", None),
            ("SET NAMES utf8 ;;; ;0", Some(" ;0;")),
            ("SET NAMES utf8mb3;", Some("SET NAMES utf8mb3;")),
            ("SET NAMES utf8 COLLATE utf8_unicode_ci", None),
            ("X;SET NAMES utf8;Y", Some("X;Y;")),
        ];
        for (input, want) in cases {
            assert_eq!(clean(input).as_deref(), *want, "{input:?}");
        }
    }
}
