//! A small parser and writer for the PHP array literals in `app/etc/env.php` /
//! `config.php`. These files are machine-generated `<?php return [ ... ];` — plain
//! nested literals (strings, ints, floats, bools, null, arrays), occasionally with a
//! `\Class::CONST` reference (kept verbatim as `Const`). We never execute PHP; this is
//! a focused literal parser, and [`PhpValue::to_php_file`] writes a tree back out so a
//! deploy tool can derive one env.php from another.

/// A parsed PHP value.
#[derive(Debug, Clone, PartialEq)]
pub enum PhpValue {
    /// Ordered `(key, value)` pairs. Auto-indexed items get an `Int` key.
    Array(Vec<(PhpValue, PhpValue)>),
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    /// A bareword / `\Class::CONST` reference, kept as written.
    Const(String),
}

impl PhpValue {
    /// Look up a string key in an array value.
    pub fn get(&self, key: &str) -> Option<&PhpValue> {
        match self {
            PhpValue::Array(items) => items.iter().find_map(|(k, v)| match k {
                PhpValue::Str(s) if s == key => Some(v),
                _ => None,
            }),
            _ => None,
        }
    }

    /// Look up a string key in an array value, mutably.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut PhpValue> {
        match self {
            PhpValue::Array(items) => items.iter_mut().find_map(|(k, v)| match k {
                PhpValue::Str(s) if s == key => Some(v),
                _ => None,
            }),
            _ => None,
        }
    }

    /// Set a string key in an array value: replaces the value in place when the key
    /// exists (keeping its position), appends it otherwise. A non-array is replaced
    /// by an array holding just this key.
    pub fn set(&mut self, key: &str, value: PhpValue) {
        if !matches!(self, PhpValue::Array(_)) {
            *self = PhpValue::Array(Vec::new());
        }
        if let Some(slot) = self.get_mut(key) {
            *slot = value;
        } else if let PhpValue::Array(items) = self {
            items.push((PhpValue::Str(key.to_string()), value));
        }
    }

    /// Write this value as a whole `<?php return …;` file, in the short-array style
    /// Magento's own config writer uses (four-space indent, every key explicit).
    /// Parsing the output gives back an equal tree.
    pub fn to_php_file(&self) -> String {
        let mut out = String::from("<?php\nreturn ");
        self.write_php(&mut out, 0);
        out.push_str(";\n");
        out
    }

    fn write_php(&self, out: &mut String, depth: usize) {
        match self {
            PhpValue::Array(items) if items.is_empty() => out.push_str("[]"),
            PhpValue::Array(items) => {
                out.push_str("[\n");
                for (k, v) in items {
                    out.push_str(&"    ".repeat(depth + 1));
                    k.write_php(out, depth + 1);
                    out.push_str(" => ");
                    v.write_php(out, depth + 1);
                    out.push_str(",\n");
                }
                out.push_str(&"    ".repeat(depth));
                out.push(']');
            }
            PhpValue::Str(s) => {
                out.push('\'');
                for c in s.chars() {
                    if c == '\\' || c == '\'' {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push('\'');
            }
            PhpValue::Int(i) => out.push_str(&i.to_string()),
            PhpValue::Float(f) if f.is_nan() => out.push_str("NAN"),
            PhpValue::Float(f) if f.is_infinite() => {
                out.push_str(if *f > 0.0 { "INF" } else { "-INF" })
            }
            // Debug keeps a decimal point or exponent, so PHP reads it back as a float.
            PhpValue::Float(f) => out.push_str(&format!("{f:?}")),
            PhpValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            PhpValue::Null => out.push_str("null"),
            PhpValue::Const(c) => out.push_str(c),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            PhpValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[(PhpValue, PhpValue)]> {
        match self {
            PhpValue::Array(items) => Some(items),
            _ => None,
        }
    }

    /// A string view of a scalar (string/int/float/bool), for display/coercion.
    pub fn scalar_string(&self) -> Option<String> {
        match self {
            PhpValue::Str(s) => Some(s.clone()),
            PhpValue::Int(i) => Some(i.to_string()),
            PhpValue::Float(f) => Some(f.to_string()),
            PhpValue::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq)]
enum Tok {
    LBracket,
    RBracket,
    LParen,
    RParen,
    Comma,
    Semicolon,
    Arrow,
    DoubleColon,
    Str(String),
    Int(i64),
    Float(f64),
    Ident(String),
}

/// Parse a `<?php return [...];` file into its `PhpValue`.
pub fn parse(src: &str) -> Result<PhpValue, String> {
    let toks = tokenize(src)?;
    let mut p = Parser { toks: &toks, pos: 0 };
    // Skip a leading `return`.
    if matches!(p.peek(), Some(Tok::Ident(s)) if s == "return") {
        p.pos += 1;
    }
    p.value()
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn value(&mut self) -> Result<PhpValue, String> {
        match self.peek() {
            Some(Tok::LBracket) => {
                self.pos += 1;
                self.array(Tok::RBracket)
            }
            Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("array") => {
                self.pos += 1;
                if self.peek() == Some(&Tok::LParen) {
                    self.pos += 1;
                    self.array(Tok::RParen)
                } else {
                    Ok(PhpValue::Const("array".into()))
                }
            }
            Some(Tok::Str(_)) => {
                let Some(Tok::Str(s)) = self.take() else { unreachable!() };
                Ok(PhpValue::Str(s))
            }
            Some(Tok::Int(_)) => {
                let Some(Tok::Int(i)) = self.take() else { unreachable!() };
                Ok(PhpValue::Int(i))
            }
            Some(Tok::Float(_)) => {
                let Some(Tok::Float(f)) = self.take() else { unreachable!() };
                Ok(PhpValue::Float(f))
            }
            Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("true") => {
                self.pos += 1;
                Ok(PhpValue::Bool(true))
            }
            Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("false") => {
                self.pos += 1;
                Ok(PhpValue::Bool(false))
            }
            Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("null") => {
                self.pos += 1;
                Ok(PhpValue::Null)
            }
            Some(Tok::Ident(_)) => self.const_expr(),
            other => Err(format!("unexpected token in value: {other:?}")),
        }
    }

    /// A `\Class::CONST` / bareword reference, kept verbatim.
    fn const_expr(&mut self) -> Result<PhpValue, String> {
        let mut s = String::new();
        loop {
            match self.peek() {
                Some(Tok::Ident(id)) => {
                    s.push_str(id);
                    self.pos += 1;
                }
                Some(Tok::DoubleColon) => {
                    s.push_str("::");
                    self.pos += 1;
                }
                _ => break,
            }
        }
        Ok(PhpValue::Const(s))
    }

    fn array(&mut self, close: Tok) -> Result<PhpValue, String> {
        let mut items = Vec::new();
        let mut auto = 0i64;
        loop {
            match self.peek() {
                Some(t) if *t == close => {
                    self.pos += 1;
                    return Ok(PhpValue::Array(items));
                }
                None => return Err("unterminated array".into()),
                _ => {}
            }
            let first = self.value()?;
            if self.peek() == Some(&Tok::Arrow) {
                self.pos += 1;
                let val = self.value()?;
                items.push((first, val));
            } else {
                items.push((PhpValue::Int(auto), first));
                auto += 1;
            }
            match self.peek() {
                Some(Tok::Comma) => self.pos += 1,
                Some(t) if *t == close => {}
                other => return Err(format!("expected ',' or close in array, got {other:?}")),
            }
        }
    }

    fn take(&mut self) -> Option<Tok> {
        // Cheap "take by clone of the variant we already matched"; only called for owned
        // variants right after a successful peek.
        let t = self.toks.get(self.pos)?;
        self.pos += 1;
        Some(match t {
            Tok::Str(s) => Tok::Str(s.clone()),
            Tok::Int(i) => Tok::Int(*i),
            Tok::Float(f) => Tok::Float(*f),
            _ => return None,
        })
    }
}

fn tokenize(s: &str) -> Result<Vec<Tok>, String> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'<' if s[i..].starts_with("<?php") => i += 5,
            b'<' if b.get(i + 1) == Some(&b'?') => i += 2,
            b'?' if b.get(i + 1) == Some(&b'>') => i += 2,
            b'[' => {
                out.push(Tok::LBracket);
                i += 1;
            }
            b']' => {
                out.push(Tok::RBracket);
                i += 1;
            }
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            b';' => {
                out.push(Tok::Semicolon);
                i += 1;
            }
            b'=' if b.get(i + 1) == Some(&b'>') => {
                out.push(Tok::Arrow);
                i += 2;
            }
            b':' if b.get(i + 1) == Some(&b':') => {
                out.push(Tok::DoubleColon);
                i += 2;
            }
            b'\'' => {
                let (val, next) = read_string(b, i + 1, b'\'', true)?;
                out.push(Tok::Str(val));
                i = next;
            }
            b'"' => {
                let (val, next) = read_string(b, i + 1, b'"', false)?;
                out.push(Tok::Str(val));
                i = next;
            }
            b'-' | b'0'..=b'9' if c != b'-' || b.get(i + 1).is_some_and(|d| d.is_ascii_digit()) => {
                let (tok, next) = read_number(b, i)?;
                out.push(tok);
                i = next;
            }
            _ if is_ident_start(c) => {
                let start = i;
                while i < b.len() && is_ident(b[i]) {
                    i += 1;
                }
                out.push(Tok::Ident(s[start..i].to_string()));
            }
            _ => return Err(format!("unexpected character `{}`", c as char)),
        }
    }
    Ok(out)
}

fn read_string(b: &[u8], mut i: usize, quote: u8, single: bool) -> Result<(String, usize), String> {
    // Collect bytes, not chars: a multi-byte UTF-8 character (a password, a store
    // name) must survive as itself, not as one Latin-1 char per byte.
    let mut out: Vec<u8> = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c == b'\\' {
            let n = *b.get(i + 1).ok_or("unterminated string")?;
            if single {
                // PHP single quotes only escape \\ and \'.
                match n {
                    b'\\' | b'\'' => out.push(n),
                    _ => {
                        out.push(b'\\');
                        out.push(n);
                    }
                }
            } else {
                match n {
                    b'\\' | b'"' | b'$' | b'/' => out.push(n),
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    _ => {
                        out.push(b'\\');
                        out.push(n);
                    }
                }
            }
            i += 2;
        } else if c == quote {
            return Ok((String::from_utf8_lossy(&out).into_owned(), i + 1));
        } else {
            out.push(c);
            i += 1;
        }
    }
    Err("unterminated string".into())
}

fn read_number(b: &[u8], start: usize) -> Result<(Tok, usize), String> {
    let mut i = start;
    if b[i] == b'-' {
        i += 1;
    }
    let mut is_float = false;
    while i < b.len() {
        match b[i] {
            b'0'..=b'9' | b'_' => i += 1,
            b'.' if !is_float => {
                is_float = true;
                i += 1;
            }
            b'e' | b'E' => {
                is_float = true;
                i += 1;
                if matches!(b.get(i), Some(b'+') | Some(b'-')) {
                    i += 1;
                }
            }
            _ => break,
        }
    }
    let raw: String = b[start..i].iter().map(|&c| c as char).filter(|c| *c != '_').collect();
    if is_float {
        raw.parse::<f64>().map(|f| (Tok::Float(f), i)).map_err(|e| e.to_string())
    } else {
        raw.parse::<i64>().map(|n| (Tok::Int(n), i)).map_err(|e| e.to_string())
    }
}

fn is_ident_start(c: u8) -> bool {
    c == b'_' || c == b'\\' || c.is_ascii_alphabetic()
}

fn is_ident(c: u8) -> bool {
    c == b'_' || c == b'\\' || c.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENV: &str = r#"<?php
return [
    'backend' => ['frontName' => 'admin'],
    'crypt' => array('key' => 'base64abc'),
    'db' => [
        'connection' => [
            'default' => [
                'host' => 'localhost',
                'password' => 'pa\'ss\\wörd',
                'active' => '1',
            ],
        ],
    ],
    'cache' => [
        'frontend' => [
            'default' => [
                'id_prefix' => 'abc_',
                'backend' => 'Magento\\Framework\\Cache\\Backend\\Redis',
                'backend_options' => ['database' => '0', 'port' => 6379, 'compress_data' => true],
            ],
        ],
    ],
    'list' => ['a', 'b'],
    'ratio' => 0.5,
    'nothing' => null,
    'mode' => \Magento\Framework\App\State::MODE_PRODUCTION,
    'empty' => [],
];
"#;

    #[test]
    fn written_file_parses_back_to_the_same_tree() {
        let v = parse(ENV).unwrap();
        let again = parse(&v.to_php_file()).unwrap();
        assert_eq!(v, again);
    }

    #[test]
    fn multibyte_strings_survive_parsing() {
        let v = parse(ENV).unwrap();
        let pw = v
            .get("db")
            .and_then(|d| d.get("connection"))
            .and_then(|c| c.get("default"))
            .and_then(|d| d.get("password"));
        assert_eq!(pw.and_then(PhpValue::as_str), Some("pa'ss\\wörd"));
    }

    #[test]
    fn set_replaces_in_place_and_appends_new_keys() {
        let mut v = parse(ENV).unwrap();
        let fe = v
            .get_mut("cache")
            .and_then(|c| c.get_mut("frontend"))
            .and_then(|f| f.get_mut("default"))
            .unwrap();
        fe.set("id_prefix", PhpValue::Str("r36_".into()));
        fe.set("new_key", PhpValue::Int(1));
        let keys: Vec<_> = fe
            .as_array()
            .unwrap()
            .iter()
            .map(|(k, _)| k.scalar_string().unwrap())
            .collect();
        assert_eq!(keys, ["id_prefix", "backend", "backend_options", "new_key"]);
        assert_eq!(fe.get("id_prefix").and_then(PhpValue::as_str), Some("r36_"));
    }

    #[test]
    fn writer_uses_short_arrays_and_explicit_keys() {
        let v = parse("<?php return ['a' => ['x', true], 'b' => 1.0];").unwrap();
        assert_eq!(
            v.to_php_file(),
            "<?php\nreturn [\n    'a' => [\n        0 => 'x',\n        1 => true,\n    ],\n    'b' => 1.0,\n];\n"
        );
    }
}
