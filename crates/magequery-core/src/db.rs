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
