//! Redis/Valkey connectivity test over the raw RESP protocol — no client crate needed.
//! Connects (TCP or unix socket), optionally `AUTH`/`SELECT`s, `PING`s, and reads the server
//! version from `INFO server`. Always available (pure std::net).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::model::{RedisInstance, RedisPing};

const TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn ping(inst: &RedisInstance) -> RedisPing {
    let start = Instant::now();
    let result = try_ping(inst);
    let elapsed_ms = start.elapsed().as_millis();
    match result {
        Ok(version) => RedisPing {
            purpose: inst.purpose.clone(),
            host: inst.host.clone(),
            database: inst.database.clone(),
            ok: true,
            server_version: version,
            error: None,
            elapsed_ms,
        },
        Err(error) => RedisPing {
            purpose: inst.purpose.clone(),
            host: inst.host.clone(),
            database: inst.database.clone(),
            ok: false,
            server_version: None,
            error: Some(error),
            elapsed_ms,
        },
    }
}

fn try_ping(inst: &RedisInstance) -> Result<Option<String>, String> {
    if inst.host.starts_with('/') {
        connect_socket(inst)
    } else {
        let port = inst.port.unwrap_or(6379);
        let addr = format!("{}:{}", inst.host, port);
        let sock = addr
            .to_socket_addrs()
            .map_err(|e| format!("cannot resolve {addr}: {e}"))?
            .next()
            .ok_or_else(|| format!("no address for {addr}"))?;
        let s = TcpStream::connect_timeout(&sock, TIMEOUT)
            .map_err(|e| format!("cannot reach {addr}: {e}"))?;
        s.set_read_timeout(Some(TIMEOUT)).ok();
        s.set_write_timeout(Some(TIMEOUT)).ok();
        talk(s, inst)
    }
}

/// Connect over a unix domain socket. Unix-only; the DB/redis sockets Magento
/// configures don't exist on Windows, where this reports cleanly instead.
#[cfg(unix)]
fn connect_socket(inst: &RedisInstance) -> Result<Option<String>, String> {
    let s = UnixStream::connect(&inst.host)
        .map_err(|e| format!("cannot connect to socket {}: {e}", inst.host))?;
    s.set_read_timeout(Some(TIMEOUT)).ok();
    s.set_write_timeout(Some(TIMEOUT)).ok();
    talk(s, inst)
}

#[cfg(not(unix))]
fn connect_socket(inst: &RedisInstance) -> Result<Option<String>, String> {
    Err(format!(
        "unix socket connections are not supported on this platform: {}",
        inst.host
    ))
}

fn talk<S: Read + Write>(mut s: S, inst: &RedisInstance) -> Result<Option<String>, String> {
    if !inst.password.is_empty() {
        write_cmd(&mut s, &["AUTH", &inst.password])?;
        if let Reply::Error(e) = read_reply(&mut s)? {
            return Err(format!("AUTH failed: {e}"));
        }
    }
    if let Some(db) = &inst.database {
        write_cmd(&mut s, &["SELECT", db])?;
        if let Reply::Error(e) = read_reply(&mut s)? {
            return Err(format!("SELECT {db} failed: {e}"));
        }
    }
    write_cmd(&mut s, &["PING"])?;
    match read_reply(&mut s)? {
        Reply::Status(st) if st.eq_ignore_ascii_case("PONG") => {}
        Reply::Error(e) => return Err(e),
        other => return Err(format!("unexpected PING reply: {other:?}")),
    }
    write_cmd(&mut s, &["INFO", "server"])?;
    let version = match read_reply(&mut s)? {
        Reply::Bulk(Some(info)) => parse_version(&info),
        _ => None,
    };
    Ok(version)
}

#[derive(Debug)]
enum Reply {
    Status(String),
    Error(String),
    Bulk(Option<String>),
    Int,
}

fn write_cmd<S: Write>(s: &mut S, args: &[&str]) -> Result<(), String> {
    let mut out = format!("*{}\r\n", args.len());
    for a in args {
        out.push_str(&format!("${}\r\n{a}\r\n", a.len()));
    }
    s.write_all(out.as_bytes()).map_err(|e| e.to_string())?;
    s.flush().map_err(|e| e.to_string())
}

fn read_reply<S: Read>(s: &mut S) -> Result<Reply, String> {
    let line = read_line(s)?;
    let mut chars = line.chars();
    let tag = chars.next().ok_or("empty reply")?;
    let rest: String = chars.collect();
    match tag {
        '+' => Ok(Reply::Status(rest)),
        '-' => Ok(Reply::Error(rest)),
        ':' => Ok(Reply::Int),
        '$' => {
            let len: i64 = rest.trim().parse().map_err(|_| "bad bulk length".to_string())?;
            if len < 0 {
                return Ok(Reply::Bulk(None));
            }
            let mut data = vec![0u8; len as usize];
            s.read_exact(&mut data).map_err(|e| e.to_string())?;
            let mut crlf = [0u8; 2];
            s.read_exact(&mut crlf).ok();
            Ok(Reply::Bulk(Some(String::from_utf8_lossy(&data).into_owned())))
        }
        other => Err(format!("unexpected reply tag `{other}`")),
    }
}

fn read_line<S: Read>(s: &mut S) -> Result<String, String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        s.read_exact(&mut byte).map_err(|e| e.to_string())?;
        if byte[0] == b'\r' {
            s.read_exact(&mut byte).ok(); // consume the '\n'
            break;
        }
        buf.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Pull a friendly version (`valkey 8.0.1` / `redis 7.2.4`) out of `INFO server` output.
fn parse_version(info: &str) -> Option<String> {
    for line in info.lines() {
        for (key, name) in
            [("valkey_version:", "valkey"), ("redis_version:", "redis"), ("server_version:", "server")]
        {
            if let Some(v) = line.strip_prefix(key) {
                return Some(format!("{name} {}", v.trim()));
            }
        }
    }
    None
}
