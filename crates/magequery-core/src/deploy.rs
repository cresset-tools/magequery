//! Deployment configuration from `app/etc/env.php` — parsed via the PHP array parser.
//! Currently extracts the `db` section; the same parsed tree will back config resolution,
//! session/cache/redis info, and the crypt key later.

use std::path::Path;

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::model::{
    CacheConfig, CacheFrontend, CacheType, DbConfig, DbConnection, LockConfig, QueueConfig,
    QueueConnection, RedisConfig, RedisInstance, SessionConfig,
};
use crate::engine::vfs::Vfs;
use crate::phparray::{self, PhpValue};

/// Parse `app/etc/env.php` into its `PhpValue` tree.
pub(crate) fn read_env(root: &Path, vfs: &Vfs) -> Result<PhpValue> {
    read_php(&root.join("app/etc/env.php"), vfs)
}

/// Parse `app/etc/config.php` into its `PhpValue` tree (whole file, for the `system` node).
pub(crate) fn read_config_php(root: &Path, vfs: &Vfs) -> Result<PhpValue> {
    read_php(&root.join("app/etc/config.php"), vfs)
}

fn read_php(path: &Path, vfs: &Vfs) -> Result<PhpValue> {
    let text = vfs.read_to_string(&path)
        .map_err(|source| Error::Io { file: path.to_path_buf(), source })?;
    phparray::parse(&text).map_err(|detail| Error::Parse { file: path.to_path_buf(), detail })
}

/// The encryption key(s) from `env.php` `crypt/key`. Magento loads them with
/// `preg_split('/\s+/', trim($key))`, so they're split on **any** whitespace (the
/// newline-separated keys from key rotation, but also spaces/tabs). The encrypted value's
/// `keyVersion` indexes into this list.
pub(crate) fn crypt_keys(env: &PhpValue) -> Vec<String> {
    env.get("crypt")
        .and_then(|c| c.get("key"))
        .and_then(PhpValue::scalar_string)
        .map(|k| k.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Extract the `db` config (table prefix + connections) from a parsed `env.php`.
pub(crate) fn db_config(env: &PhpValue) -> DbConfig {
    let db = env.get("db");
    let table_prefix =
        db.and_then(|d| d.get("table_prefix")).and_then(PhpValue::as_str).unwrap_or("").to_string();

    let mut connections = Vec::new();
    if let Some(conns) = db.and_then(|d| d.get("connection")).and_then(PhpValue::as_array) {
        for (key, val) in conns {
            let name = key.scalar_string().unwrap_or_default();
            connections.push(connection(name, val));
        }
    }
    DbConfig { table_prefix, connections }
}

/// Extract all Redis usages (cache frontends + session) from a parsed `env.php`.
pub(crate) fn redis_config(env: &PhpValue) -> RedisConfig {
    let mut instances = Vec::new();

    // Cache frontends: `cache/frontend/<id>` with a Redis backend.
    if let Some(frontends) = env.get("cache").and_then(|c| c.get("frontend")).and_then(PhpValue::as_array) {
        for (key, fe) in frontends {
            if let Some(inst) = redis_from_cache(key.scalar_string().unwrap_or_default(), fe) {
                instances.push(inst);
            }
        }
    }

    // Session: `session/save == redis` with `session/redis`.
    let session = env.get("session");
    let uses_redis = session.and_then(|s| s.get("save")).and_then(PhpValue::as_str) == Some("redis");
    if uses_redis {
        if let Some(r) = session.and_then(|s| s.get("redis")) {
            let (host, port, database, password) = redis_endpoint(r);
            instances.push(RedisInstance {
                purpose: "session".into(),
                host,
                port,
                database,
                password,
                backend: None,
            });
        }
    }

    RedisConfig { instances }
}

/// Extract the `session` storage config from a parsed `env.php`.
pub(crate) fn session_config(env: &PhpValue) -> SessionConfig {
    let session = env.get("session");
    let handler =
        session.and_then(|s| s.get("save")).and_then(PhpValue::scalar_string).unwrap_or_else(|| "files".into());

    let (location, database) = match handler.as_str() {
        "redis" => match session.and_then(|s| s.get("redis")) {
            Some(r) => {
                let (host, port, db, _pw) = redis_endpoint(r);
                (non_empty(host_port(host, port)), db)
            }
            None => (None, None),
        },
        "files" => (session.and_then(|s| s.get("save_path")).and_then(PhpValue::scalar_string), None),
        _ => (None, None), // db / other
    };
    SessionConfig { handler, location, database }
}

/// Extract the cache config (frontends + per-type enable flags) from a parsed `env.php`.
pub(crate) fn cache_config(env: &PhpValue) -> CacheConfig {
    let mut frontends = Vec::new();
    if let Some(fes) = env.get("cache").and_then(|c| c.get("frontend")).and_then(PhpValue::as_array) {
        for (key, fe) in fes {
            let id = key.scalar_string().unwrap_or_default();
            let backend = fe.get("backend").and_then(PhpValue::scalar_string).unwrap_or_default();
            let (location, database) = match fe.get("backend_options") {
                Some(opts) => {
                    let (host, port, db, _pw) = redis_endpoint(opts);
                    (non_empty(host_port(host, port)), db)
                }
                None => (None, None),
            };
            frontends.push(CacheFrontend { id, backend, location, database });
        }
    }

    let mut types = Vec::new();
    if let Some(ct) = env.get("cache_types").and_then(PhpValue::as_array) {
        for (k, v) in ct {
            let name = k.scalar_string().unwrap_or_default();
            let enabled = matches!(v.scalar_string().as_deref(), Some("1") | Some("true"));
            types.push(CacheType { name, enabled });
        }
    }
    CacheConfig { frontends, types }
}

/// Extract the `lock` backend config from a parsed `env.php`.
pub(crate) fn lock_config(env: &PhpValue) -> LockConfig {
    let lock = env.get("lock");
    let provider =
        lock.and_then(|l| l.get("provider")).and_then(PhpValue::scalar_string).unwrap_or_else(|| "db".into());

    let mut config = BTreeMap::new();
    if let Some(cfg) = lock.and_then(|l| l.get("config")).and_then(PhpValue::as_array) {
        for (k, v) in cfg {
            // Skip NULL/empty entries (e.g. an unset `prefix`).
            if let (key, Some(val)) = (k.scalar_string().unwrap_or_default(), v.scalar_string()) {
                config.insert(key, val);
            }
        }
    }
    LockConfig { provider, config }
}

/// Extract the message-`queue` connections from a parsed `env.php`.
pub(crate) fn queue_config(env: &PhpValue) -> QueueConfig {
    let queue = env.get("queue");
    let mut connections = Vec::new();

    // The conventional single `amqp` block.
    if let Some(amqp) = queue.and_then(|q| q.get("amqp")) {
        connections.push(queue_conn("amqp".into(), amqp));
    }
    // Plus any `queue/connections/<name>` entries (custom transports).
    if let Some(conns) = queue.and_then(|q| q.get("connections")).and_then(PhpValue::as_array) {
        for (k, v) in conns {
            connections.push(queue_conn(k.scalar_string().unwrap_or_default(), v));
        }
    }

    let consumers_wait_for_messages =
        queue.and_then(|q| q.get("consumers_wait_for_messages")).and_then(PhpValue::scalar_string);
    QueueConfig { connections, consumers_wait_for_messages }
}

fn queue_conn(name: String, v: &PhpValue) -> QueueConnection {
    let s = |k: &str| v.get(k).and_then(PhpValue::scalar_string);
    QueueConnection {
        name,
        host: s("host").unwrap_or_default(),
        port: s("port").and_then(|p| p.parse().ok()),
        user: s("user").unwrap_or_default(),
        password: s("password").unwrap_or_default(),
        virtualhost: s("virtualhost"),
    }
}

/// Render a host + optional port as `host:port`, leaving socket paths (`/…`) and portless
/// hosts untouched.
fn host_port(host: String, port: Option<u16>) -> String {
    match port {
        Some(p) if !host.is_empty() && !host.starts_with('/') => format!("{host}:{p}"),
        _ => host,
    }
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

fn redis_from_cache(id: String, fe: &PhpValue) -> Option<RedisInstance> {
    let is_redis = |b: &Option<String>| b.as_deref().is_some_and(|s| s.contains("Redis"));
    let backend = fe.get("backend").and_then(PhpValue::scalar_string);
    let remote = fe.get("remote_backend").and_then(PhpValue::scalar_string);

    // Direct Redis backend, or a RemoteSynchronizedCache wrapping Redis.
    let (opts, used) = if is_redis(&backend) {
        (fe.get("backend_options")?, backend)
    } else if is_redis(&remote) {
        (fe.get("remote_backend_options")?, remote)
    } else {
        return None;
    };

    let (host, port, database, password) = redis_endpoint(opts);
    Some(RedisInstance { purpose: id, host, port, database, password, backend: used })
}

/// Pull `(host, port, database, password)` from a Redis options block. Cache uses `server`,
/// session uses `host`; `port` may be NULL (socket).
fn redis_endpoint(opts: &PhpValue) -> (String, Option<u16>, Option<String>, String) {
    let host = opts
        .get("server")
        .or_else(|| opts.get("host"))
        .and_then(PhpValue::scalar_string)
        .unwrap_or_default();
    let port = opts.get("port").and_then(PhpValue::scalar_string).and_then(|p| p.parse().ok());
    let database = opts.get("database").and_then(PhpValue::scalar_string);
    let password = opts.get("password").and_then(PhpValue::scalar_string).unwrap_or_default();
    (host, port, database, password)
}

fn connection(name: String, v: &PhpValue) -> DbConnection {
    let s = |k: &str| v.get(k).and_then(PhpValue::scalar_string);
    let raw_host = s("host").unwrap_or_default();

    // Magento accepts `host`, `host:port`, or a unix socket path in `host`.
    let (host, port) = match raw_host.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse::<u16>().ok())
        }
        _ => (raw_host.clone(), None),
    };
    let unix_socket =
        if raw_host.starts_with('/') { Some(raw_host) } else { s("unix_socket") };

    let active = match v.get("active") {
        // `active` is usually the string "1"/"0" or a bool; absent means active.
        Some(a) => matches!(a.scalar_string().as_deref(), Some("1") | Some("true") | None),
        None => true,
    };

    DbConnection {
        name,
        host,
        port,
        dbname: s("dbname").unwrap_or_default(),
        username: s("username").unwrap_or_default(),
        password: s("password").unwrap_or_default(),
        unix_socket,
        model: s("model"),
        engine: s("engine"),
        active,
    }
}
