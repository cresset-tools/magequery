//! Data types for the runtime domain.

use super::*;

/// Where a resolved config value came from (its source layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSourceKind {
    /// Module `config.xml` `<default>` default.
    ConfigXml,
    /// `app/etc/config.php` `system` node (locked/dumped).
    ConfigPhp,
    /// `app/etc/env.php` `system` node.
    EnvPhp,
    /// A `CONFIG__*` environment variable.
    EnvVar,
    /// `core_config_data` (database).
    Database,
}

/// A resolved system-config value at a given scope, with its source.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ConfigValue {
    /// Config path, e.g. `web/secure/base_url`.
    pub path: String,
    /// Scope: `default`, `websites/<code>`, or `stores/<code>`.
    pub scope: String,
    pub value: String,
    pub source: ConfigSourceKind,
    /// File the value was declared in (`None` for env var / database).
    pub file: Option<std::path::PathBuf>,
    /// 1-based line, or `0` if unknown (config.php/env.php aren't line-tracked).
    pub line: u32,
}

/// A database connection from `env.php` (`db/connection/<name>`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DbConnection {
    /// Connection name (`default`, `indexer`, split-db `checkout`/`sales`, …).
    pub name: String,
    pub host: String,
    pub port: Option<u16>,
    pub dbname: String,
    pub username: String,
    /// Raw password — callers should mask it for display unless explicitly revealing.
    pub password: String,
    /// `unix_socket` if the connection uses a socket instead of host/port.
    pub unix_socket: Option<String>,
    pub model: Option<String>,
    pub engine: Option<String>,
    pub active: bool,
}

/// The `db` section of `env.php`: the table prefix and all configured connections.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DbConfig {
    pub table_prefix: String,
    pub connections: Vec<DbConnection>,
}

/// Result of testing a database connection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DbPing {
    pub connection: String,
    pub ok: bool,
    pub server_version: Option<String>,
    pub error: Option<String>,
    pub elapsed_ms: u128,
}

/// One Redis (or Valkey) instance Magento is configured to use.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RedisInstance {
    /// What it backs: `default` (cache), `page_cache`, or `session`.
    pub purpose: String,
    /// Server — a hostname or a socket path (starts with `/`).
    pub host: String,
    pub port: Option<u16>,
    /// Redis database number.
    pub database: Option<String>,
    pub password: String,
    /// The cache backend class (for cache instances); `None` for session.
    pub backend: Option<String>,
}

/// All Redis usages found in `env.php` (cache / page cache / session).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RedisConfig {
    pub instances: Vec<RedisInstance>,
}

/// Result of pinging one Redis instance.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RedisPing {
    pub purpose: String,
    pub host: String,
    pub database: Option<String>,
    pub ok: bool,
    pub server_version: Option<String>,
    pub error: Option<String>,
    pub elapsed_ms: u128,
}

/// Session storage configuration (the `session` section of `env.php`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SessionConfig {
    /// Save handler: `files`, `db`, or `redis`.
    pub handler: String,
    /// Where sessions live: the server/socket for `redis`, the save path for `files`
    /// (`None` ⇒ Magento's default `var/session`); `None` for `db`.
    pub location: Option<String>,
    /// Redis database number, when the handler is `redis`.
    pub database: Option<String>,
}

/// One configured cache frontend (`cache/frontend/<id>` in `env.php`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CacheFrontend {
    /// Frontend id: `default` (the general cache) or `page_cache` (full-page cache).
    pub id: String,
    /// Backend class, e.g. `\Magento\Framework\Cache\Backend\Redis` or a file backend.
    pub backend: String,
    /// Server/socket for a Redis backend (`host:port` or a socket path); `None` otherwise.
    pub location: Option<String>,
    pub database: Option<String>,
}

/// Whether one Magento cache type is enabled (`cache_types` map in `env.php`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CacheType {
    pub name: String,
    pub enabled: bool,
}

/// Cache configuration: the backend per frontend, plus the per-type enable flags.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CacheConfig {
    pub frontends: Vec<CacheFrontend>,
    pub types: Vec<CacheType>,
}

/// Locking backend configuration (the `lock` section of `env.php`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LockConfig {
    /// Provider: `db` (default), `file`, `zookeeper`, or `cache`.
    pub provider: String,
    /// Provider-specific settings (`path`, `prefix`, `host`, …), sorted by key. NULL/empty
    /// entries are omitted.
    pub config: std::collections::BTreeMap<String, String>,
}

/// One message-queue connection (`queue/amqp` or `queue/connections/<name>` in `env.php`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct QueueConnection {
    /// Connection name, e.g. `amqp`.
    pub name: String,
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    /// Raw password (no masking, matching `db info`).
    pub password: String,
    pub virtualhost: Option<String>,
}

/// Message-queue configuration (the `queue` section of `env.php`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct QueueConfig {
    pub connections: Vec<QueueConnection>,
    /// The `consumers_wait_for_messages` flag, if set.
    pub consumers_wait_for_messages: Option<String>,
}

/// The kind of a GraphQL schema type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GqlKind {
    Object,
    Interface,
    Input,
    Enum,
    Union,
    Scalar,
}

impl std::fmt::Display for GqlKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            GqlKind::Object => "type",
            GqlKind::Interface => "interface",
            GqlKind::Input => "input",
            GqlKind::Enum => "enum",
            GqlKind::Union => "union",
            GqlKind::Scalar => "scalar",
        })
    }
}

/// One argument of a GraphQL field, as written (`pageSize: Int`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GqlArg {
    pub name: String,
    pub ty: String,
}

/// One field of a GraphQL type, with its resolver and the module that declared it — the
/// per-field provenance is the point: `Query` is assembled from dozens of modules.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GqlField {
    pub name: String,
    pub args: Vec<GqlArg>,
    /// Return type as written, e.g. `[ProductInterface!]!`.
    pub ty: String,
    /// `@resolver(class:)` — the class that computes the field. `None` for plain data
    /// fields served from the parent's output array.
    pub resolver: Option<ClassName>,
    /// `@doc(description:)`, or the SDL description string.
    pub description: Option<String>,
    /// `@deprecated` — the reason (possibly empty).
    pub deprecated: Option<String>,
    /// `@cache(cacheable:)`, when stated.
    pub cacheable: Option<bool>,
    pub source: Source,
}

/// A GraphQL schema type merged from every module's `schema.graphqls` (fields union by
/// name across modules, matching Magento's schema stitching).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GqlType {
    pub name: String,
    pub kind: GqlKind,
    pub implements: Vec<String>,
    /// `@typeResolver(class:)` on interfaces/types — maps a runtime value to its concrete
    /// schema type.
    pub type_resolver: Option<ClassName>,
    pub description: Option<String>,
    pub fields: Vec<GqlField>,
    /// Enum values.
    pub values: Vec<String>,
    /// Union member types.
    pub members: Vec<String>,
    /// Where the type was first declared.
    pub source: Source,
}

/// A message-queue topic from `communication.xml`, with its handlers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MqTopic {
    pub name: String,
    /// Request payload type (a class/interface name, or a primitive like `string`).
    pub request: Option<String>,
    pub response: Option<String>,
    /// `schema="Class::method"` — request/response derived from a service method.
    pub schema: Option<String>,
    pub handlers: Vec<MqHandler>,
    pub source: Source,
}

/// One handler of a topic (`<handler>` in `communication.xml`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MqHandler {
    pub name: String,
    pub class: ClassName,
    pub method: String,
    pub disabled: bool,
    pub source: Source,
}

/// A queue consumer from `queue_consumer.xml`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MqConsumer {
    pub name: String,
    pub queue: String,
    /// Declared connection; absent ⇒ Magento's runtime default (amqp, falling back to db).
    pub connection: Option<String>,
    pub consumer_instance: Option<ClassName>,
    /// `handler="Class::method"`; absent ⇒ handlers come from `communication.xml`.
    pub handler: Option<String>,
    pub max_messages: Option<String>,
    pub source: Source,
}

/// A topic's publisher from `queue_publisher.xml`, flattened to its enabled connection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MqPublisher {
    pub topic: String,
    /// Direct-to-queue shorthand (`queue=` attribute), bypassing exchange routing.
    pub queue: Option<String>,
    /// The enabled `<connection>`'s name/exchange, when the element form is used.
    pub connection: Option<String>,
    pub exchange: Option<String>,
    pub disabled: bool,
    pub source: Source,
}

/// How a message travels from a topic into a queue.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum MqVia {
    /// The topic's publisher names the queue directly (`<publisher queue=…>`).
    PublisherQueue { source: Source },
    /// An exchange binding whose topic pattern matches.
    Binding {
        exchange: String,
        /// The exchange's connection (`amqp` when not declared — the XSD default).
        connection: String,
        id: String,
        pattern: String,
        source: Source,
    },
}

/// One queue a topic's messages land in, how they get there, and who reads it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MqRoute {
    pub queue: String,
    /// Every path leading here (a direct publisher queue and/or matching bindings).
    pub via: Vec<MqVia>,
    /// Consumers reading this queue (joined by queue name).
    pub consumers: Vec<MqConsumer>,
}

/// The full journey of one topic: definition + handlers (`communication.xml`), its
/// publisher (`queue_publisher.xml`), and each queue it reaches with that queue's
/// consumers (`queue_topology.xml` + `queue_consumer.xml`) — the answer to "who processes
/// a message published on this topic".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MqTopicRoute {
    pub topic: MqTopic,
    pub publisher: Option<MqPublisher>,
    pub routes: Vec<MqRoute>,
}

/// One URL rewrite from the `url_rewrite` table (live DB only — these are runtime data,
/// generated from products/categories/CMS pages plus manual entries; no static source).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UrlRewrite {
    /// The public path requested, e.g. `my-product.html`.
    pub request_path: String,
    /// What it resolves to: an internal route (`catalog/product/view/id/42`) or, for a
    /// redirect, another request path.
    pub target_path: String,
    /// `product`, `category`, `cms-page`, or `custom`.
    pub entity_type: String,
    pub entity_id: u32,
    /// `0` = internal rewrite (no redirect), else the HTTP redirect code (`301`/`302`).
    pub redirect_type: u16,
    /// Store-view code the rewrite applies to, or `store/<id>` if the id is unknown.
    pub store: String,
    pub description: Option<String>,
    /// `true` for system-generated rewrites; `false` for manually-added ones.
    pub autogenerated: bool,
}

/// A page of URL rewrites, with whether more existed beyond the requested limit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UrlRewrites {
    pub rewrites: Vec<UrlRewrite>,
    /// `true` if the `url_rewrite` table held more matching rows than `limit` (dropped).
    pub truncated: bool,
}
