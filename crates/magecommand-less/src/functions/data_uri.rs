//! `data-uri(mime?, url)` (plan §2.7, §C-assets) — a port of less.js
//! `functions/data-uri.js` (v4.6.7 has NO size cap — the old IE-32KB check was
//! dropped upstream): mime from the table (or given; `;base64` suffix decides
//! encoding), the file read through the [`crate::resolver::ImportResolver`]'s
//! binary hook, base64 or `encodeURIComponent` payload, `#fragment` preserved.
//! A missing file or unreadable path falls back to a plain `url(path)`.

use super::string::{encode_uri_component, string_value};
use crate::ast::Node;
use crate::data::mime;
use crate::resolver::ImportResolver;

pub(crate) fn data_uri(
    args: &[Node],
    resolver: &dyn ImportResolver,
    current_dir: &str,
) -> Option<Node> {
    let (mime_node, path_node) = match args.len() {
        0 => return None,
        1 => (None, args.first()?),
        _ => (args.first(), args.get(1)?),
    };
    let fallback = || Some(Node::Url(Box::new(path_node.clone())));

    let mut mimetype = match mime_node {
        Some(n) => string_value(n)?,
        None => String::new(),
    };
    let mut file_path = string_value(path_node)?;

    // Split a #fragment off before reading.
    let mut fragment = String::new();
    if let Some(pos) = file_path.find('#') {
        fragment = file_path[pos..].to_string();
        file_path.truncate(pos);
    }

    let use_base64 = if mime_node.is_none() {
        let Some(m) = mime::mime_lookup(&file_path) else {
            return fallback();
        };
        mimetype = m.to_string();
        let b64 = m != "image/svg+xml" && !mime::is_text(m);
        if b64 {
            mimetype.push_str(";base64");
        }
        b64
    } else {
        mimetype.ends_with(";base64")
    };

    let Some(bytes) = resolver.load_binary(&file_path, current_dir) else {
        return fallback();
    };

    let payload = if use_base64 {
        base64(&bytes)
    } else {
        encode_uri_component(&String::from_utf8_lossy(&bytes))
    };
    let uri = format!("data:{mimetype},{payload}{fragment}");
    Some(Node::Url(Box::new(Node::Quoted {
        escaped: false,
        quote: '"',
        value: uri,
    })))
}

/// Standard base64 (RFC 4648, with padding).
pub(crate) fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_rfc_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
