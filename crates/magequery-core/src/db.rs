//! Live database connection testing (behind the `db` feature). Connects with the `env.php`
//! credentials and runs a trivial query, returning the server version. A short TCP pre-check
//! makes an unreachable host fail fast instead of hanging on the default connect timeout.

use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::model::{DbConnection, DbPing, UrlRewrite};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn ping(conn: &DbConnection) -> DbPing {
    let start = Instant::now();
    let result = try_ping(conn);
    let elapsed_ms = start.elapsed().as_millis();
    match result {
        Ok(version) => DbPing {
            connection: conn.name.clone(),
            ok: true,
            server_version: Some(version),
            error: None,
            elapsed_ms,
        },
        Err(error) => DbPing {
            connection: conn.name.clone(),
            ok: false,
            server_version: None,
            error: Some(error),
            elapsed_ms,
        },
    }
}

fn try_ping(conn: &DbConnection) -> Result<String, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let version: Option<String> = c.query_first("SELECT VERSION()").map_err(clean_err)?;
    Ok(version.unwrap_or_default())
}

/// Connect to a MySQL connection, with a fast reachability pre-check.
fn connect(conn: &DbConnection) -> Result<mysql::Conn, String> {
    let mut builder = mysql::OptsBuilder::new()
        .user(Some(conn.username.as_str()))
        .pass(Some(conn.password.as_str()))
        .db_name(Some(conn.dbname.as_str()));

    if let Some(socket) = &conn.unix_socket {
        if !std::path::Path::new(socket).exists() {
            return Err(format!("socket file not found: {socket}"));
        }
        builder = builder.socket(Some(socket.as_str()));
    } else {
        let port = conn.port.unwrap_or(3306);
        let addr = format!("{}:{}", conn.host, port);
        let sock = addr
            .to_socket_addrs()
            .map_err(|e| format!("cannot resolve {addr}: {e}"))?
            .next()
            .ok_or_else(|| format!("no address for {addr}"))?;
        TcpStream::connect_timeout(&sock, CONNECT_TIMEOUT)
            .map_err(|e| format!("cannot reach {addr}: {e}"))?;
        builder = builder.ip_or_hostname(Some(conn.host.as_str())).tcp_port(port);
    }
    mysql::Conn::new(builder).map_err(clean_err)
}

/// Read `core_config_data` and resolve each row's scope to a `default`/`websites/<code>`/
/// `stores/<code>` string (via `store_website` / `store`). Returns `(scope, path, value)`.
pub(crate) fn fetch_config(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<(String, String, String)>, String> {
    use mysql::prelude::Queryable;
    use std::collections::HashMap;

    let mut c = connect(conn)?;
    let p = table_prefix;

    let websites: HashMap<u32, String> = c
        .query(format!("SELECT website_id, code FROM {p}store_website"))
        .map_err(clean_err)?
        .into_iter()
        .collect();
    let stores: HashMap<u32, String> = c
        .query(format!("SELECT store_id, code FROM {p}store"))
        .map_err(clean_err)?
        .into_iter()
        .collect();

    let rows: Vec<(String, u32, String, Option<String>)> = c
        .query(format!("SELECT scope, scope_id, path, value FROM {p}core_config_data"))
        .map_err(clean_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for (scope, scope_id, path, value) in rows {
        let scope = match scope.as_str() {
            "default" => "default".to_string(),
            "websites" => match websites.get(&scope_id) {
                Some(code) => format!("websites/{code}"),
                None => continue,
            },
            "stores" => match stores.get(&scope_id) {
                Some(code) => format!("stores/{code}"),
                None => continue,
            },
            _ => continue,
        };
        out.push((scope, path, value.unwrap_or_default()));
    }
    Ok(out)
}

/// Read the `theme` table: registered themes with their parent links, for resolving the
/// active `design/theme/theme_id` (a numeric id) to a theme path and its ancestry.
/// Returns `(theme_id, parent_id, theme_path, area)`; virtual themes have no path.
pub(crate) fn fetch_themes(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<(u32, Option<u32>, Option<String>, String)>, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    c.query(format!(
        "SELECT theme_id, parent_id, theme_path, area FROM {table_prefix}theme"
    ))
    .map_err(clean_err)
}

/// The live database's tables and columns from `information_schema`, keyed by table name
/// with the configured prefix stripped (tables not matching a non-empty prefix are
/// skipped — they're not Magento's).
pub(crate) fn fetch_live_schema(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<std::collections::HashMap<String, Vec<String>>, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let rows: Vec<(String, String)> = c
        .query(
            "SELECT TABLE_NAME, COLUMN_NAME FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME, ORDINAL_POSITION",
        )
        .map_err(clean_err)?;
    let mut out: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for (table, column) in rows {
        let name = if table_prefix.is_empty() {
            table
        } else {
            match table.strip_prefix(table_prefix) {
                Some(stripped) => stripped.to_string(),
                None => continue,
            }
        };
        out.entry(name).or_default().push(column);
    }
    Ok(out)
}

/// Rows from the `translation` table for one locale whose `string` contains `needle`
/// (case-insensitive): `(string, translate, store_id)`.
pub(crate) fn fetch_translations(
    conn: &DbConnection,
    table_prefix: &str,
    locale: &str,
    needle: &str,
) -> Result<Vec<(String, String, u32)>, String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    c.exec(
        format!(
            "SELECT string, translate, store_id FROM {table_prefix}translation \
             WHERE locale = :loc AND LOWER(string) LIKE :pat"
        ),
        params! { "loc" => locale, "pat" => format!("%{}%", needle.to_lowercase()) },
    )
    .map_err(clean_err)
}

/// Applied patch class names from `patch_list` (leading backslashes normalized away).
pub(crate) fn fetch_patch_list(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<String>, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let rows: Vec<String> =
        c.query(format!("SELECT patch_name FROM {table_prefix}patch_list")).map_err(clean_err)?;
    Ok(rows.into_iter().map(|r| r.trim_start_matches('\\').to_string()).collect())
}

/// Seconds since the last *successful* cron job finished, per the DB server's own clock
/// (`TIMESTAMPDIFF` — no client-side time needed). `None` = no successful runs recorded.
pub(crate) fn fetch_cron_last_success(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Option<i64>, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    c.query_first(format!(
        "SELECT TIMESTAMPDIFF(SECOND, MAX(finished_at), NOW()) FROM {table_prefix}cron_schedule \
         WHERE status = 'success'"
    ))
    .map_err(clean_err)
    .map(Option::flatten)
}

/// Count the store hierarchy — `(websites, store groups, store views)` — excluding the
/// synthetic admin scopes (id 0).
pub(crate) fn fetch_scope_counts(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<(usize, usize, usize), String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let count = |c: &mut mysql::Conn, sql: String| -> Result<usize, String> {
        Ok(c.query_first::<u64, _>(sql).map_err(clean_err)?.unwrap_or(0) as usize)
    };
    let websites = count(&mut c, format!("SELECT COUNT(*) FROM {p}store_website WHERE website_id > 0"))?;
    let groups = count(&mut c, format!("SELECT COUNT(*) FROM {p}store_group WHERE group_id > 0"))?;
    let stores = count(&mut c, format!("SELECT COUNT(*) FROM {p}store WHERE store_id > 0"))?;
    Ok((websites, groups, stores))
}

/// One `indexer_state` row.
pub(crate) struct DbIndexerState {
    pub indexer_id: String,
    /// `valid` / `invalid` / `working` / `suspended`.
    pub status: String,
    pub updated: Option<String>,
}

/// One `mview_state` row, with the pending-changelog count for scheduled views.
pub(crate) struct DbMviewState {
    pub view_id: String,
    /// `enabled` = update by schedule, `disabled` = update on save.
    pub mode: String,
    /// `idle` / `working` / `suspended`.
    pub status: String,
    /// Distinct entities in `<view_id>_cl` past the applied version — what
    /// `bin/magento indexer:status` reports as backlog. `None` = no changelog table.
    pub backlog: Option<u64>,
}

/// Live indexer + mview state. The backlog mirrors Magento's `IndexerStatusCommand`:
/// `COUNT(DISTINCT entity_id)` in the changelog beyond the view's applied `version_id`.
pub(crate) fn fetch_indexer_states(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<(Vec<DbIndexerState>, Vec<DbMviewState>), String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;

    let states: Vec<(String, String, Option<String>)> = c
        .query(format!(
            "SELECT indexer_id, status, CAST(updated AS CHAR) FROM {p}indexer_state"
        ))
        .map_err(clean_err)?;
    let states = states
        .into_iter()
        .map(|(indexer_id, status, updated)| DbIndexerState { indexer_id, status, updated })
        .collect();

    let views: Vec<(String, String, String, Option<u64>)> = c
        .query(format!("SELECT view_id, mode, status, version_id FROM {p}mview_state"))
        .map_err(clean_err)?;
    let mut out = Vec::with_capacity(views.len());
    for (view_id, mode, status, version_id) in views {
        // The changelog table name embeds the view id — identifier-sanitized, and the
        // query tolerated failing: the table is created lazily on first subscribe.
        let backlog = if mode == "enabled"
            && view_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            c.query_first::<u64, _>(format!(
                "SELECT COUNT(DISTINCT entity_id) FROM {p}{view_id}_cl \
                 WHERE version_id > {}",
                version_id.unwrap_or(0)
            ))
            .ok()
            .flatten()
        } else {
            None
        };
        out.push(DbMviewState { view_id, mode, status, backlog });
    }
    Ok((states, out))
}

/// One `eav_entity_type` row with its attribute count.
pub(crate) struct DbEavEntity {
    pub code: String,
    pub entity_table: Option<String>,
    pub value_table_prefix: Option<String>,
    pub attributes: usize,
}

pub(crate) fn fetch_eav_entities(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<DbEavEntity>, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let rows: Vec<(String, Option<String>, Option<String>, u64)> = c
        .query(format!(
            "SELECT t.entity_type_code, t.entity_table, t.value_table_prefix, \
             COUNT(a.attribute_id) \
             FROM {p}eav_entity_type t \
             LEFT JOIN {p}eav_attribute a ON a.entity_type_id = t.entity_type_id \
             GROUP BY t.entity_type_id, t.entity_type_code, t.entity_table, \
             t.value_table_prefix ORDER BY t.entity_type_code"
        ))
        .map_err(clean_err)?;
    Ok(rows
        .into_iter()
        .map(|(code, entity_table, value_table_prefix, n)| DbEavEntity {
            code,
            entity_table: entity_table.filter(|t| !t.is_empty()),
            value_table_prefix: value_table_prefix.filter(|t| !t.is_empty()),
            attributes: n as usize,
        })
        .collect())
}

/// Catalog-specific flags (`catalog_eav_attribute`), raw.
pub(crate) struct DbCatalogFlags {
    pub is_global: i64,
    pub searchable: bool,
    pub filterable: bool,
    pub filterable_in_search: bool,
    pub comparable: bool,
    pub used_in_listing: bool,
    pub used_for_sort_by: bool,
    pub visible_on_front: bool,
    pub apply_to: Option<String>,
}

/// One `eav_attribute` row, joined with its entity-type code and catalog flags.
pub(crate) struct DbEavAttribute {
    pub entity_code: String,
    pub attribute_id: u32,
    pub code: String,
    pub label: Option<String>,
    pub backend_type: String,
    pub frontend_input: Option<String>,
    pub required: bool,
    pub unique: bool,
    pub user_defined: bool,
    pub default_value: Option<String>,
    pub source_model: Option<String>,
    pub backend_model: Option<String>,
    pub frontend_model: Option<String>,
    pub catalog: Option<DbCatalogFlags>,
}

/// Every attribute across all entity types (a few hundred rows — filtering happens in
/// Rust). The row is too wide for a tuple, so columns are taken by index.
pub(crate) fn fetch_eav_attributes(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<DbEavAttribute>, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let rows: Vec<mysql::Row> = c
        .query(format!(
            "SELECT t.entity_type_code, a.attribute_id, a.attribute_code, a.frontend_label, \
             a.backend_type, a.frontend_input, a.is_required, a.is_unique, a.is_user_defined, \
             a.default_value, a.source_model, a.backend_model, a.frontend_model, \
             c.attribute_id, c.is_global, c.is_searchable, c.is_filterable, \
             c.is_filterable_in_search, c.is_comparable, c.used_in_product_listing, \
             c.used_for_sort_by, c.is_visible_on_front, c.apply_to \
             FROM {p}eav_attribute a \
             JOIN {p}eav_entity_type t ON t.entity_type_id = a.entity_type_id \
             LEFT JOIN {p}catalog_eav_attribute c ON c.attribute_id = a.attribute_id \
             ORDER BY t.entity_type_code, a.attribute_code"
        ))
        .map_err(clean_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for mut row in rows {
        let s = |r: &mut mysql::Row, i: usize| r.take::<Option<String>, _>(i).flatten();
        let n = |r: &mut mysql::Row, i: usize| {
            r.take::<Option<i64>, _>(i).flatten().unwrap_or(0)
        };
        let catalog = row
            .take::<Option<u32>, _>(13)
            .flatten()
            .is_some()
            .then(|| DbCatalogFlags {
                is_global: n(&mut row, 14),
                searchable: n(&mut row, 15) != 0,
                filterable: n(&mut row, 16) != 0,
                filterable_in_search: n(&mut row, 17) != 0,
                comparable: n(&mut row, 18) != 0,
                used_in_listing: n(&mut row, 19) != 0,
                used_for_sort_by: n(&mut row, 20) != 0,
                visible_on_front: n(&mut row, 21) != 0,
                apply_to: s(&mut row, 22).filter(|a| !a.is_empty()),
            });
        out.push(DbEavAttribute {
            entity_code: s(&mut row, 0).unwrap_or_default(),
            attribute_id: n(&mut row, 1) as u32,
            code: s(&mut row, 2).unwrap_or_default(),
            label: s(&mut row, 3).filter(|l| !l.is_empty()),
            backend_type: s(&mut row, 4).unwrap_or_default(),
            frontend_input: s(&mut row, 5).filter(|i| !i.is_empty()),
            required: n(&mut row, 6) != 0,
            unique: n(&mut row, 7) != 0,
            user_defined: n(&mut row, 8) != 0,
            default_value: s(&mut row, 9).filter(|d| !d.is_empty()),
            source_model: s(&mut row, 10).filter(|m| !m.is_empty()),
            backend_model: s(&mut row, 11).filter(|m| !m.is_empty()),
            frontend_model: s(&mut row, 12).filter(|m| !m.is_empty()),
            catalog,
        });
    }
    Ok(out)
}

/// An attribute's set memberships as `(set name, group name)`, plus the entity's total
/// set count (the "in 3 of 5 sets" denominator).
pub(crate) fn fetch_eav_sets(
    conn: &DbConnection,
    table_prefix: &str,
    attribute_id: u32,
    entity_code: &str,
) -> Result<(Vec<(String, String)>, usize), String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let sets: Vec<(String, String)> = c
        .exec(
            format!(
                "SELECT s.attribute_set_name, g.attribute_group_name \
                 FROM {p}eav_entity_attribute ea \
                 JOIN {p}eav_attribute_set s ON s.attribute_set_id = ea.attribute_set_id \
                 JOIN {p}eav_attribute_group g ON g.attribute_group_id = ea.attribute_group_id \
                 WHERE ea.attribute_id = :id ORDER BY s.attribute_set_name"
            ),
            params! { "id" => attribute_id },
        )
        .map_err(clean_err)?;
    let total: Option<u64> = c
        .exec_first(
            format!(
                "SELECT COUNT(*) FROM {p}eav_attribute_set s \
                 JOIN {p}eav_entity_type t ON t.entity_type_id = s.entity_type_id \
                 WHERE t.entity_type_code = :code"
            ),
            params! { "code" => entity_code },
        )
        .map_err(clean_err)?;
    Ok((sets, total.unwrap_or(0) as usize))
}

/// Admin-scope (store 0) option labels of a table-source attribute, in sort order.
pub(crate) fn fetch_eav_options(
    conn: &DbConnection,
    table_prefix: &str,
    attribute_id: u32,
) -> Result<Vec<String>, String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    c.exec(
        format!(
            "SELECT v.value FROM {p}eav_attribute_option o \
             JOIN {p}eav_attribute_option_value v \
             ON v.option_id = o.option_id AND v.store_id = 0 \
             WHERE o.attribute_id = :id ORDER BY o.sort_order, v.value"
        ),
        params! { "id" => attribute_id },
    )
    .map_err(clean_err)
}

/// Read the `url_rewrite` table, resolving each row's `store_id` to a store code. Filters
/// (path substring on request/target, store code, redirects-only) are applied **in SQL** —
/// the table is often huge. Fetches `limit + 1` rows to detect truncation; returns
/// `(rows, truncated)`.
pub(crate) fn fetch_url_rewrites(
    conn: &DbConnection,
    table_prefix: &str,
    path_filter: Option<&str>,
    store_filter: Option<&str>,
    redirects_only: bool,
    limit: usize,
) -> Result<(Vec<UrlRewrite>, bool), String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    use std::collections::HashMap;

    let mut c = connect(conn)?;
    let p = table_prefix;

    let stores: HashMap<u32, String> =
        c.query(format!("SELECT store_id, code FROM {p}store")).map_err(clean_err)?.into_iter().collect();

    let mut wheres: Vec<String> = Vec::new();
    if path_filter.is_some() {
        wheres.push("(request_path LIKE :pat OR target_path LIKE :pat)".into());
    }
    if let Some(code) = store_filter {
        let id = stores
            .iter()
            .find(|(_, c)| c.as_str() == code)
            .map(|(id, _)| *id)
            .ok_or_else(|| format!("no store with code `{code}`"))?;
        wheres.push(format!("store_id = {id}"));
    }
    if redirects_only {
        wheres.push("redirect_type <> 0".into());
    }
    let where_sql =
        if wheres.is_empty() { String::new() } else { format!("WHERE {}", wheres.join(" AND ")) };

    let sql = format!(
        "SELECT request_path, target_path, entity_type, entity_id, redirect_type, store_id, \
         description, is_autogenerated FROM {p}url_rewrite {where_sql} \
         ORDER BY request_path LIMIT {}",
        limit + 1
    );

    type Row = (String, String, String, i64, i64, i64, Option<String>, i64);
    let rows: Vec<Row> = match path_filter {
        Some(f) => c.exec(&sql, params! { "pat" => format!("%{f}%") }).map_err(clean_err)?,
        None => c.query(&sql).map_err(clean_err)?,
    };

    let truncated = rows.len() > limit;
    let out = rows
        .into_iter()
        .take(limit)
        .map(|(request_path, target_path, entity_type, entity_id, redirect_type, store_id, description, auto)| {
            let store_id = store_id as u32;
            let store = stores.get(&store_id).cloned().unwrap_or_else(|| format!("store/{store_id}"));
            UrlRewrite {
                request_path,
                target_path,
                entity_type,
                entity_id: entity_id as u32,
                redirect_type: redirect_type as u16,
                store,
                description: description.filter(|d| !d.is_empty()),
                autogenerated: auto != 0,
            }
        })
        .collect();
    Ok((out, truncated))
}

/// The `mysql` crate prints errors as `DriverError { … }` / `MySqlError { … }`; unwrap the
/// outer `Variant { … }` so the message reads cleanly.
fn clean_err(e: mysql::Error) -> String {
    let s = e.to_string();
    match (s.find("{ "), s.rfind(" }")) {
        (Some(open), Some(close)) if open + 2 <= close => s[open + 2..close].to_string(),
        _ => s,
    }
}
