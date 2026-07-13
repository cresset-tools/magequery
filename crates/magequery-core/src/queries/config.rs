//! Deployment and system-configuration [`Magento`] queries.

use super::super::*;
#[cfg(feature = "db")]
use crate::db;

impl Magento {
    /// The database configuration from `app/etc/env.php` (`db` section).
    pub fn db_config(&self) -> Result<DbConfig> {
        let env = deploy::read_env(&self.index.root, &self.index.vfs)?;
        Ok(deploy::db_config(&env))
    }

    /// Resolve the system configuration into a queryable [`ConfigSet`]. Always includes the
    /// static sources (config.xml defaults, config.php/env.php `system`, `CONFIG__*` env
    /// vars). With `include_db`, also reads `core_config_data` (requires the `db` feature and
    /// a reachable database; the DB layer sits above config.xml and below the `system`
    /// overrides).
    pub fn config(&self, include_db: bool) -> Result<ConfigSet> {
        let env = deploy::read_env(&self.index.root, &self.index.vfs).unwrap_or(phparray::PhpValue::Null);
        let config_php =
            deploy::read_config_php(&self.index.root, &self.index.vfs).unwrap_or(phparray::PhpValue::Null);
        let db_values = if include_db { self.fetch_core_config_data()? } else { Vec::new() };
        let order = self.system_config_source_order();
        Ok(ConfigSet::build(&self.index.root, &self.index.modules, &self.index.vfs, &env, &config_php, db_values, &order))
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
    pub(crate) fn fetch_cron_last_success(&self) -> Result<Option<i64>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        db::fetch_cron_last_success(conn, &cfg.table_prefix).map_err(Error::Db)
    }

    #[cfg(not(feature = "db"))]
    pub(crate) fn fetch_cron_last_success(&self) -> Result<Option<i64>> {
        Err(Error::Db("the `db` feature is not enabled in this build".to_string()))
    }

    /// `(websites, store groups, store views)` counts, admin scopes excluded.
    #[cfg(feature = "db")]
    pub(crate) fn fetch_scope_counts(&self) -> Result<(usize, usize, usize)> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        db::fetch_scope_counts(conn, &cfg.table_prefix).map_err(Error::Db)
    }

    #[cfg(not(feature = "db"))]
    pub(crate) fn fetch_scope_counts(&self) -> Result<(usize, usize, usize)> {
        Err(Error::Db("the `db` feature is not enabled in this build".to_string()))
    }

    /// `(theme_id, parent_id, theme_path, area)` rows from the `theme` table.
    #[cfg(feature = "db")]
    pub(crate) fn fetch_theme_rows(&self) -> Result<Vec<(u32, Option<u32>, Option<String>, String)>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        db::fetch_themes(conn, &cfg.table_prefix).map_err(Error::Db)
    }

    #[cfg(not(feature = "db"))]
    pub(crate) fn fetch_theme_rows(&self) -> Result<Vec<(u32, Option<u32>, Option<String>, String)>> {
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
        let env = deploy::read_env(&self.index.root, &self.index.vfs)?;
        Ok(Decryptor::new(deploy::crypt_keys(&env)))
    }

    /// Redis/Valkey usages from `app/etc/env.php` (cache, page cache, session).
    pub fn redis_config(&self) -> Result<RedisConfig> {
        let env = deploy::read_env(&self.index.root, &self.index.vfs)?;
        Ok(deploy::redis_config(&env))
    }

    /// Session storage configuration (`session` section of `env.php`): the save handler and,
    /// for Redis/file handlers, where sessions live.
    pub fn session_config(&self) -> Result<SessionConfig> {
        let env = deploy::read_env(&self.index.root, &self.index.vfs)?;
        Ok(deploy::session_config(&env))
    }

    /// Cache configuration (`cache`/`cache_types` of `env.php`): the backend per frontend and
    /// the per-type enable flags.
    pub fn cache_config(&self) -> Result<CacheConfig> {
        let env = deploy::read_env(&self.index.root, &self.index.vfs)?;
        Ok(deploy::cache_config(&env))
    }

    /// Locking backend (`lock` section of `env.php`): the provider and its settings.
    pub fn lock_config(&self) -> Result<LockConfig> {
        let env = deploy::read_env(&self.index.root, &self.index.vfs)?;
        Ok(deploy::lock_config(&env))
    }

    /// Message-queue connections (`queue` section of `env.php`).
    pub fn queue_config(&self) -> Result<QueueConfig> {
        let env = deploy::read_env(&self.index.root, &self.index.vfs)?;
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

}
