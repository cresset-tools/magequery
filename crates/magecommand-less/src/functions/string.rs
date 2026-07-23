//! String fns: `e(str), escape(str), %(fmt, …), replace(str,pat,repl,flags?)`
//! (plan §2.7, §2.14). Mirrors less.js `functions/string.js`: `e()` returns an
//! *escaped* Quoted of the node's raw JS `.value` (a Dimension contributes its
//! bare number — unit dropped, FULL float digits; a Color its internal
//! `rgb`/`hsl`/keyword/hex marker; array-ish nodes JS-stringify to
//! `[object Object]`); `escape()` is `encodeURI` of the same `.value` plus
//! `=:#;()`; `%()` substitutes `/%[sda]/i` one arg at a time (uppercase token →
//! `encodeURIComponent`; non-Quoted args via **context-less** `toCSS()` — no
//! fround), then `%%` → `%`; `replace()` is a real JS-regex replace (fancy-regex
//! for lookaround/backrefs) with JS replacement-pattern semantics and the
//! `g`/`i`/`m`/`s` flags, preserving the subject's quote char + escaped flag.
//!
//! Error parity: these functions are uncaught in less.js — missing/non-string
//! arguments are compile errors (`Cannot read properties of undefined`,
//! `result.replace is not a function`, invalid-RegExp throws), never
//! passthrough (F15/F16/F20).

use super::{undef_err, FnResult};
use crate::ast::Node;
use crate::css::{render_value, render_value_cz};
use crate::error::{ErrorKind, LessError};
use crate::value::js_number_string;

fn rt(msg: impl Into<String>) -> LessError {
    LessError::new(ErrorKind::Runtime, msg)
}

/// JS's view of a node's `.value` — what `str.value` yields in less.js.
enum JsValue {
    /// No `.value` property (an operated Color, a deferred Operation, …).
    Undef,
    /// A raw number (Dimension.value).
    Num(f64),
    /// A string (Quoted/Keyword/Anonymous value; a Color's original literal).
    Str(String),
    /// `n` node objects — JS stringifies each to `[object Object]` (an array
    /// value joins them with `,`).
    Objects(usize),
}

fn js_value(n: &Node) -> JsValue {
    match n {
        Node::Quoted { value, .. } => JsValue::Str(value.clone()),
        Node::Keyword(k) => JsValue::Str(k.clone()),
        Node::Anonymous(s) => JsValue::Str(s.clone()),
        Node::Dimension(d) => JsValue::Num(d.value),
        Node::Color(c) => match &c.original {
            Some(o) => JsValue::Str(o.to_string()),
            None => JsValue::Undef,
        },
        Node::Url(_) => JsValue::Objects(1),
        Node::Expression(v) | Node::Value(v) => JsValue::Objects(v.len().max(1)),
        _ => JsValue::Undef,
    }
}

fn obj_join(n: usize) -> String {
    vec!["[object Object]"; n].join(",")
}

/// less.js `e(str)` — `new Quoted('"', str.value, true)`. The Quoted ctor's
/// `content || ''` turns a falsy raw value (undefined, the NUMBER 0, NaN) into
/// the empty string — `e(0px)` prints nothing, `e(hsl(…))` prints `hsl`.
pub(super) fn e(args: &[Node]) -> FnResult {
    let value = match args.first() {
        None => return Err(undef_err("value")),
        Some(n) => match js_value(n) {
            JsValue::Str(s) => s,
            JsValue::Num(v) if v == 0.0 || v.is_nan() => String::new(),
            JsValue::Num(v) => js_number_string(v),
            JsValue::Objects(n) => obj_join(n),
            JsValue::Undef => String::new(),
        },
    };
    Ok(Some(Node::Quoted {
        escaped: true,
        quote: '"',
        value,
    }))
}

/// less.js `escape(str)` — `encodeURI(str.value)` + `= : # ; ( )` escapes.
/// `encodeURI` String-coerces: an undefined `.value` prints `undefined`, a
/// Dimension its bare full-precision number.
pub(super) fn escape(args: &[Node]) -> FnResult {
    let value = match args.first() {
        None => return Err(undef_err("value")),
        Some(n) => match js_value(n) {
            JsValue::Str(s) => s,
            JsValue::Num(v) => js_number_string(v),
            JsValue::Objects(n) => obj_join(n),
            JsValue::Undef => "undefined".to_string(),
        },
    };
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
    Ok(Some(Node::Anonymous(out)))
}

/// The subject of `replace()`/`%()`: less.js reads `string.value` and later
/// re-wraps with `string.quote || ''` + `string.escaped`. A non-string value
/// has no `.replace` → TypeError.
fn subject_parts(n: Option<&Node>) -> Result<(char, bool, String), LessError> {
    match n {
        None => Err(undef_err("value")),
        Some(Node::Quoted {
            quote,
            escaped,
            value,
        }) => Ok((*quote, *escaped, value.clone())),
        Some(Node::Keyword(k)) => Ok(('\0', true, k.clone())),
        Some(Node::Anonymous(s)) => Ok(('\0', true, s.clone())),
        Some(Node::Color(c)) if c.original.is_some() => {
            Ok(('\0', true, c.original.as_deref().unwrap_or_default().to_string()))
        }
        Some(_) => Err(rt("result.replace is not a function")),
    }
}

/// less.js `replace(string, pattern, replacement, flags?)` — JS
/// `String.prototype.replace(new RegExp(pattern, flags), replacement)`.
pub(super) fn replace(args: &[Node]) -> FnResult {
    let (quote, escaped, subject) = subject_parts(args.first())?;
    let pattern = match args.get(1) {
        None => return Err(undef_err("value")),
        Some(n) => match js_value(n) {
            JsValue::Str(s) => s,
            JsValue::Num(v) => js_number_string(v), // RegExp(1) matches "1"
            JsValue::Objects(n) => obj_join(n),
            JsValue::Undef => "undefined".to_string(),
        },
    };
    let replacement = match args.get(2) {
        None => return Err(undef_err("type")),
        Some(Node::Quoted { value, .. }) => value.clone(),
        Some(other) => render_value(other, 0), // toCSS with no context — no fround
    };
    let flags = match args.get(3) {
        None => String::new(),
        Some(n) => match js_value(n) {
            JsValue::Str(s) => s,
            JsValue::Num(v) => js_number_string(v),
            JsValue::Objects(n) => obj_join(n),
            JsValue::Undef => "undefined".to_string(),
        },
    };

    // JS RegExp flag validation: [dgimsuvy], no duplicates → SyntaxError.
    let mut seen = [false; 8];
    for ch in flags.chars() {
        let idx = match ch {
            'd' => 0,
            'g' => 1,
            'i' => 2,
            'm' => 3,
            's' => 4,
            'u' => 5,
            'v' => 6,
            'y' => 7,
            _ => 8,
        };
        if idx == 8 || seen[idx] {
            return Err(rt(format!(
                "Invalid flags supplied to RegExp constructor '{flags}'"
            )));
        }
        seen[idx] = true;
    }

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
    let re = fancy_regex::Regex::new(&pat)
        .map_err(|e| rt(format!("Invalid regular expression: /{pattern}/: {e}")))?;

    let result = js_regex_replace(&re, &subject, &replacement, flags.contains('g'))?;
    Ok(Some(Node::Quoted {
        escaped,
        quote,
        value: result,
    }))
}

/// JS `String.prototype.replace` over a compiled regex: first match only, or
/// every match with `g`; each match's replacement goes through the spec's
/// `GetSubstitution` `$`-pattern expansion.
fn js_regex_replace(
    re: &fancy_regex::Regex,
    subject: &str,
    replacement: &str,
    global: bool,
) -> Result<String, LessError> {
    let has_named = re.capture_names().any(|n| n.is_some());
    let mut out = String::with_capacity(subject.len());
    let mut last = 0usize;
    for caps in re.captures_iter(subject) {
        let caps = caps.map_err(|e| rt(e.to_string()))?;
        let m = caps.get(0).expect("group 0 always participates");
        out.push_str(&subject[last..m.start()]);
        expand_substitution(replacement, subject, &caps, has_named, &mut out);
        last = m.end();
        if !global {
            break;
        }
    }
    out.push_str(&subject[last..]);
    Ok(out)
}

/// ES `GetSubstitution`: `$$` `$&` ``$` `` `$'` `$N`/`$NN` (out-of-range stays
/// LITERAL — JS never empty-expands a missing group number) and `$<name>`
/// (literal unless the regex has named groups; an unknown name is empty).
fn expand_substitution(
    rep: &str,
    subject: &str,
    caps: &fancy_regex::Captures,
    has_named: bool,
    out: &mut String,
) {
    let m = caps.get(0).expect("group 0");
    let group_count = caps.len() - 1;
    let rep_bytes = rep.as_bytes();
    let mut i = 0;
    while i < rep_bytes.len() {
        if rep_bytes[i] != b'$' || i + 1 >= rep_bytes.len() {
            // copy one full UTF-8 char
            let ch_len = utf8_len(rep_bytes[i]);
            out.push_str(&rep[i..i + ch_len]);
            i += ch_len;
            continue;
        }
        match rep_bytes[i + 1] {
            b'$' => {
                out.push('$');
                i += 2;
            }
            b'&' => {
                out.push_str(m.as_str());
                i += 2;
            }
            b'`' => {
                out.push_str(&subject[..m.start()]);
                i += 2;
            }
            b'\'' => {
                out.push_str(&subject[m.end()..]);
                i += 2;
            }
            d1 @ b'0'..=b'9' => {
                let d1v = (d1 - b'0') as usize;
                let d2 = rep_bytes.get(i + 2).filter(|b| b.is_ascii_digit());
                let two = d2.map(|b| d1v * 10 + (b - b'0') as usize);
                if let Some(nn) = two.filter(|&nn| nn >= 1 && nn <= group_count) {
                    out.push_str(caps.get(nn).map(|g| g.as_str()).unwrap_or(""));
                    i += 3;
                } else if d1v >= 1 && d1v <= group_count {
                    out.push_str(caps.get(d1v).map(|g| g.as_str()).unwrap_or(""));
                    i += 2;
                } else {
                    out.push('$'); // out of range — the whole `$N` stays literal
                    i += 1;
                }
            }
            b'<' if has_named => match rep[i + 2..].find('>') {
                Some(end) => {
                    let name = &rep[i + 2..i + 2 + end];
                    out.push_str(caps.name(name).map(|g| g.as_str()).unwrap_or(""));
                    i += 2 + end + 1;
                }
                None => {
                    out.push('$');
                    i += 1;
                }
            },
            _ => {
                out.push('$');
                i += 1;
            }
        }
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// less.js `'%'(string, arg…)` — sequential `/%[sda]/i` substitution. In less.js
/// (default profile) non-Quoted args render via context-less `toCSS()` (full
/// float digits, no compression — F8), so `compress` is `false` there. less.php's
/// `%d`/`%a` instead call `toCSS()` under the live environment, so under compress
/// the substituted value is squeezed (`%d` of `rgba(255, 255, 255, 0)` becomes
/// `rgba(255,255,255,0)` — the backend `_utilities.less` gradient-filter mixin
/// bakes an `rgba` color into a `progid:…gradient(startColorstr='%d', …)` string).
pub(super) fn format(
    args: &[Node],
    np: u8,
    compress: bool,
    keep_zero_units: bool,
    php_numbers: bool,
) -> FnResult {
    let (quote, escaped, fmt) = subject_parts(args.first())?;
    let mut result = fmt;
    for arg in &args[1..] {
        // Find the first %s/%d/%a (any case) and substitute this argument.
        let Some((pos, token)) = find_format_token(&result) else { break };
        let value = match arg {
            Node::Quoted { value, .. } if token.eq_ignore_ascii_case("s") => value.clone(),
            other if compress => render_value_cz(other, np, true, keep_zero_units, php_numbers),
            other => render_value(other, 0),
        };
        let value = if token.chars().all(|c| c.is_ascii_uppercase()) {
            encode_uri_component(&value)
        } else {
            value
        };
        result.replace_range(pos..pos + 2, &value);
    }
    let result = result.replace("%%", "%");
    Ok(Some(Node::Quoted {
        escaped,
        quote,
        value: result,
    }))
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

/// A plain string argument (quoted/keyword/anonymous) — used by the resource
/// functions (`data-uri`, `image-size`).
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
    fn qval(r: FnResult) -> String {
        let Node::Quoted { value, .. } = r.unwrap().unwrap() else { panic!() };
        value
    }

    #[test]
    fn replace_first_vs_global_and_captures() {
        let out = qval(replace(&[
            quoted('"', "This is a string."),
            quoted('"', "(string)\\.$"),
            quoted('"', "new $1."),
        ]));
        assert_eq!(out, "This is a new string.");

        let out = qval(replace(&[
            quoted('"', "One + one = 4"),
            quoted('"', "one"),
            quoted('"', "2"),
            quoted('"', "gi"),
        ]));
        assert_eq!(out, "2 + 2 = 4");
    }

    #[test]
    fn replace_js_dollar_patterns() {
        // $& = whole match, $` = before, $' = after (F2).
        let out = qval(replace(&[
            quoted('"', "amp"),
            quoted('"', "(a)"),
            quoted('"', "$&$1"),
        ]));
        assert_eq!(out, "aamp");
        let out = qval(replace(&[
            quoted('"', "abcd"),
            quoted('"', "b(c)"),
            quoted('"', "[$']"),
        ]));
        assert_eq!(out, "a[d]d");
        // Out-of-range $N stays LITERAL (JS), never empty-expands.
        let out = qval(replace(&[
            quoted('"', "um"),
            quoted('"', "(u)"),
            quoted('"', "$2"),
        ]));
        assert_eq!(out, "$2m");
        // $$& → literal "$" then literal "&".
        let out = qval(replace(&[
            quoted('"', "hi"),
            quoted('"', "h"),
            quoted('"', "$$&"),
        ]));
        assert_eq!(out, "$&i");
        // $<name> with a named group.
        let out = qval(replace(&[
            quoted('"', "hello"),
            quoted('"', "(?<first>h)"),
            quoted('"', "$<first>!"),
        ]));
        assert_eq!(out, "h!ello");
    }

    #[test]
    fn replace_js_only_regex_features() {
        // Lookahead / backreference must WORK, not fall through (F3).
        let out = qval(replace(&[
            quoted('"', "look"),
            quoted('"', "o(?=k)"),
            quoted('"', "0"),
        ]));
        assert_eq!(out, "lo0k");
        let out = qval(replace(&[
            quoted('"', "aa b"),
            quoted('"', "(a)\\1"),
            quoted('"', "X"),
        ]));
        assert_eq!(out, "X b");
    }

    #[test]
    fn replace_error_parity() {
        // Invalid flags → compile error (F20).
        assert!(replace(&[
            quoted('"', "ab"),
            quoted('"', "a"),
            quoted('"', "X"),
            quoted('"', "G"),
        ])
        .is_err());
        // Invalid pattern (`+` = nothing to repeat) → compile error.
        assert!(replace(&[quoted('"', "a+b"), quoted('"', "+"), quoted('"', "-")]).is_err());
        // Numeric subject → `result.replace is not a function` (F15).
        let n = Node::Dimension(crate::value::Dimension::number(3.0));
        assert!(replace(&[n, quoted('"', "3"), quoted('"', "4")]).is_err());
        // Numeric PATTERN is coerced: replace("a1b", 1, "x") → "axb".
        let one = Node::Dimension(crate::value::Dimension::number(1.0));
        let out = qval(replace(&[quoted('"', "a1b"), one, quoted('"', "x")]));
        assert_eq!(out, "axb");
    }

    #[test]
    fn format_pads_and_encodes() {
        // %A (uppercase) → encodeURIComponent of the rendered value.
        let color = Node::Color(crate::color::Color::from_hex("#ff0000").unwrap());
        let out = qval(format(&[quoted('"', "red is %A"), color], 0, false, false, false));
        assert_eq!(out, "red is %23ff0000");

        // %% collapses after substitution.
        let out = qval(format(&[quoted('"', "100%%")], 0, false, false, false));
        assert_eq!(out, "100%");

        // Full float digits — no fround in the context-less toCSS (F8).
        let d = Node::Dimension(crate::value::Dimension::with_unit(9.876543219, "px"));
        let out = qval(format(&[quoted('"', "%a"), d], 0, false, false, false));
        assert_eq!(out, "9.876543219px");
    }

    #[test]
    fn e_and_escape_value_semantics() {
        let out = qval(e(&[quoted('"', "qux-1")]));
        assert_eq!(out, "qux-1");
        // e(1.234567891px) → bare number, unit dropped, full digits (F10).
        let d = Node::Dimension(crate::value::Dimension::with_unit(1.234567891, "px"));
        assert_eq!(qval(e(&[d.clone()])), "1.234567891");
        // e(0px) → Quoted ctor `0 || ''` → empty.
        let zero = Node::Dimension(crate::value::Dimension::with_unit(0.0, "px"));
        assert_eq!(qval(e(&[zero])), "");
        // e(hsl-marker color) → the marker; operated color → empty (C16).
        let hsl = Node::Color(crate::color::Color::with_original([64.0, 191.0, 64.0], 1.0, "hsl"));
        assert_eq!(qval(e(&[hsl])), "hsl");
        // escape(#fff) → %23fff; escape(red) → red (C17).
        let hex = Node::Color(crate::color::Color::from_hex("#fff").unwrap());
        let Ok(Some(Node::Anonymous(s))) = escape(&[hex]) else { panic!() };
        assert_eq!(s, "%23fff");
        let mixed = Node::Color(crate::color::Color { rgb: [1.0, 2.0, 3.0], alpha: 1.0, original: None });
        let Ok(Some(Node::Anonymous(s))) = escape(&[mixed]) else { panic!() };
        assert_eq!(s, "undefined");
        // e() / escape() with no argument → compile error (F16).
        assert!(e(&[]).is_err());
        assert!(escape(&[]).is_err());
    }
}
