//! String fns: `e(str), escape(str), %(fmt, …), replace(str,pat,repl,flags?)`
//! (plan §2.7, §2.14). Mirrors less.js `functions/string.js`: `e()` returns an
//! *escaped* Quoted (raw output, quote char kept for later `replace`/`%`);
//! `escape()` is `encodeURI` plus `=:#;()`; `%()` substitutes `/%[sda]/i` one
//! arg at a time (uppercase token → `encodeURIComponent`), then `%%` → `%`;
//! `replace()` is a JS-regex replace honoring the `g`/`i` flags, preserving the
//! subject's quote char + escaped flag.

use crate::ast::Node;
use crate::css::render_value;

/// less.js `e(str)` — `new Quoted('"', str.value, true)`.
pub(super) fn e(args: &[Node]) -> Option<Node> {
    let value = match args.first()? {
        Node::Quoted { value, .. } => value.clone(),
        Node::Keyword(k) => k.clone(),
        Node::Anonymous(s) => s.clone(),
        other => render_value(other, 8),
    };
    Some(Node::Quoted {
        escaped: true,
        quote: '"',
        value,
    })
}

/// less.js `escape(str)` — `encodeURI` + `= : # ; ( )` percent-escapes.
pub(super) fn escape(args: &[Node]) -> Option<Node> {
    let value = string_value(args.first()?)?;
    let mut out = encode_uri(&value);
    for (from, to) in [
        ("=", "%3D"),
        (":", "%3A"),
        ("#", "%23"),
        (";", "%3B"),
        ("(", "%28"),
        (")", "%29"),
    ] {
        out = out.replace(from, to);
    }
    Some(Node::Anonymous(out))
}

/// less.js `replace(string, pattern, replacement, flags?)`.
pub(super) fn replace(args: &[Node], np: u8) -> Option<Node> {
    let (quote, escaped, subject) = quoted_parts(args.first()?)?;
    let pattern = string_value(args.get(1)?)?;
    let replacement = match args.get(2)? {
        Node::Quoted { value, .. } => value.clone(),
        other => render_value(other, np),
    };
    let flags = args
        .get(3)
        .and_then(string_value)
        .unwrap_or_default();

    let mut pat = String::new();
    if flags.contains('i') {
        pat.push_str("(?i)");
    }
    if flags.contains('m') {
        pat.push_str("(?m)");
    }
    if flags.contains('s') {
        pat.push_str("(?s)");
    }
    pat.push_str(&pattern);
    let re = regex::Regex::new(&pat).ok()?;
    let result = if flags.contains('g') {
        re.replace_all(&subject, replacement.as_str()).into_owned()
    } else {
        re.replace(&subject, replacement.as_str()).into_owned()
    };
    Some(Node::Quoted {
        escaped,
        quote,
        value: result,
    })
}

/// less.js `'%'(string, arg…)` — sequential `/%[sda]/i` substitution.
pub(super) fn format(args: &[Node], np: u8) -> Option<Node> {
    let (quote, escaped, fmt) = quoted_parts(args.first()?)?;
    let mut result = fmt;
    for arg in &args[1..] {
        // Find the first %s/%d/%a (any case) and substitute this argument.
        let Some((pos, token)) = find_format_token(&result) else { break };
        let value = match arg {
            Node::Quoted { value, .. } if token.eq_ignore_ascii_case("s") => value.clone(),
            other => render_value(other, np),
        };
        let value = if token.chars().all(|c| c.is_ascii_uppercase()) {
            encode_uri_component(&value)
        } else {
            value
        };
        result.replace_range(pos..pos + 2, &value);
    }
    let result = result.replace("%%", "%");
    Some(Node::Quoted {
        escaped,
        quote,
        value: result,
    })
}

/// The first `/%[sda]/i` match: `(byte_pos, token_letter)`.
fn find_format_token(s: &str) -> Option<(usize, String)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'%' && matches!(bytes[i + 1].to_ascii_lowercase(), b's' | b'd' | b'a') {
            return Some((i, (bytes[i + 1] as char).to_string()));
        }
        i += 1;
    }
    None
}

/// The subject string's `(quote, escaped, value)` — less.js uses
/// `string.quote || ''` so keywords/anonymous subjects get an empty quote
/// (rendered raw).
fn quoted_parts(n: &Node) -> Option<(char, bool, String)> {
    match n {
        Node::Quoted {
            quote,
            escaped,
            value,
        } => Some((*quote, *escaped, value.clone())),
        Node::Keyword(k) => Some(('\0', true, k.clone())),
        Node::Anonymous(s) => Some(('\0', true, s.clone())),
        _ => None,
    }
}

/// A plain string argument (quoted/keyword/anonymous).
pub(super) fn string_value(n: &Node) -> Option<String> {
    match n {
        Node::Quoted { value, .. } => Some(value.clone()),
        Node::Keyword(k) => Some(k.clone()),
        Node::Anonymous(s) => Some(s.clone()),
        _ => None,
    }
}

/// JS `encodeURI` — percent-encode UTF-8 bytes, keeping the URI character set.
pub(crate) fn encode_uri(s: &str) -> String {
    percent_encode(s, |c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')'
                    | ';' | '/' | '?' | ':' | '@' | '&' | '=' | '+' | '$' | ',' | '#'
            )
    })
}

/// JS `encodeURIComponent` — like `encodeURI` minus the reserved set.
pub(crate) fn encode_uri_component(s: &str) -> String {
    percent_encode(s, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')')
    })
}

fn percent_encode(s: &str, keep: fn(char) -> bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if keep(c) {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quoted(q: char, v: &str) -> Node {
        Node::Quoted {
            escaped: false,
            quote: q,
            value: v.to_string(),
        }
    }

    #[test]
    fn replace_first_vs_global_and_captures() {
        let out = replace(
            &[quoted('"', "This is a string."), quoted('"', "(string)\\.$"), quoted('"', "new $1.")],
            8,
        )
        .unwrap();
        let Node::Quoted { value, .. } = out else { panic!() };
        assert_eq!(value, "This is a new string.");

        let out = replace(
            &[quoted('"', "One + one = 4"), quoted('"', "one"), quoted('"', "2"), quoted('"', "gi")],
            8,
        )
        .unwrap();
        let Node::Quoted { value, .. } = out else { panic!() };
        assert_eq!(value, "2 + 2 = 4");
    }

    #[test]
    fn format_pads_and_encodes() {
        // %A (uppercase) → encodeURIComponent of the rendered value.
        let color = Node::Color(crate::color::Color::from_hex("#ff0000").unwrap());
        let out = format(&[quoted('"', "red is %A"), color], 8).unwrap();
        let Node::Quoted { value, .. } = out else { panic!() };
        assert_eq!(value, "red is %23ff0000");

        // %% collapses after substitution.
        let out = format(&[quoted('"', "100%%")], 8).unwrap();
        let Node::Quoted { value, .. } = out else { panic!() };
        assert_eq!(value, "100%");
    }

    #[test]
    fn e_preserves_raw_value_for_later_replace() {
        let out = e(&[quoted('"', "qux-1")]).unwrap();
        assert!(matches!(&out, Node::Quoted { escaped: true, quote: '"', value } if value == "qux-1"));
    }
}
