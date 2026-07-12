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

/// The `mysql` crate prints errors as `DriverError { … }` / `MySqlError { … }`; unwrap the
/// outer `Variant { … }` so the message reads cleanly.
pub(crate) fn clean_err(e: mysql::Error) -> String {
    let s = e.to_string();
    match (s.find("{ "), s.rfind(" }")) {
        (Some(open), Some(close)) if open + 2 <= close => s[open + 2..close].to_string(),
        _ => s,
    }
}

mod config;
pub(crate) use config::*;
mod content;
pub(crate) use content::*;
mod rules;
pub(crate) use rules::*;
mod stores;
pub(crate) use stores::*;
mod sales;
pub(crate) use sales::*;
mod catalog;
pub(crate) use catalog::*;
mod runtime;
pub(crate) use runtime::*;
mod admin;
pub(crate) use admin::*;
