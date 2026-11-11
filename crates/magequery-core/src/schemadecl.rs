//! The declarative schema as Magento's setup code reads it, without a database: the
//! merged `db_schema.xml` (`Config\Dom` merge semantics), the name-abbreviation table
//! behind generated index and foreign-key names, and Magento's foreign-key clause
//! parser. Shared by `db status` (`schemadiff`) and the `doctor` schema lints.

use std::collections::HashMap;
use std::path::PathBuf;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::parse::{local_name, LineMap};
use crate::Magento;

type Res<T> = Result<T, String>;

/// `ExpressionConverter::$_translateMap`.
pub(crate) const ABBREVIATIONS: &[(&str, &str)] = &[
    ("address", "addr"),
    ("admin", "adm"),
    ("attribute", "attr"),
    ("enterprise", "ent"),
    ("catalog", "cat"),
    ("category", "ctgr"),
    ("customer", "cstr"),
    ("notification", "ntfc"),
    ("product", "prd"),
    ("session", "sess"),
    ("user", "usr"),
    ("entity", "entt"),
    ("datetime", "dtime"),
    ("decimal", "dec"),
    ("varchar", "vchr"),
    ("index", "idx"),
    ("compare", "cmp"),
    ("bundle", "bndl"),
    ("option", "opt"),
    ("gallery", "glr"),
    ("media", "mda"),
    ("value", "val"),
    ("link", "lnk"),
    ("title", "ttl"),
    ("super", "spr"),
    ("label", "lbl"),
    ("website", "ws"),
    ("aggregat", "aggr"),
    ("minimal", "min"),
    ("inventory", "inv"),
    ("status", "sts"),
    ("agreement", "agrt"),
    ("layout", "lyt"),
    ("resource", "res"),
    ("directory", "dir"),
    ("downloadable", "dl"),
    ("element", "elm"),
    ("fieldset", "fset"),
    ("checkout", "chkt"),
    ("newsletter", "nlttr"),
    ("shipping", "shpp"),
    ("calculation", "calc"),
    ("search", "srch"),
    ("query", "qr"),
];

/// PHP `strtr($s, $map)`: left to right, longest key first at each position, replaced
/// text never rescanned.
pub(crate) fn strtr_abbrev(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let hit = ABBREVIATIONS.iter().filter(|(k, _)| rest.starts_with(k)).max_by_key(|(k, _)| k.len());
        match hit {
            Some((k, v)) => {
                out.push_str(v);
                i += k.len();
            }
            None => {
                let ch = rest.chars().next().expect("non-empty");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// `TableNameResolver::getNameOfOriginTable`: strip a trailing `_replica`.
pub(crate) fn origin_table(t: &str) -> &str {
    let suffix = "_replica";
    if t.len() > suffix.len() && t[t.len() - suffix.len()..].eq_ignore_ascii_case(suffix) {
        &t[..t.len() - suffix.len()]
    } else {
        t
    }
}

/// One merged XML element: its attributes (last writer wins; `xsi:type` as `type`) and,
/// for indexes and constraints, the union of their `<column name>` children.
#[derive(Debug, Clone, Default)]
pub(crate) struct XNode {
    pub(crate) attrs: Vec<(String, String)>,
    pub(crate) columns: Vec<String>,
    /// Where the last declaration that touched this element is (file, 1-based line).
    pub(crate) file: PathBuf,
    pub(crate) line: u32,
}

impl XNode {
    pub(crate) fn get(&self, k: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    pub(crate) fn set(&mut self, k: &str, v: String) {
        match self.attrs.iter_mut().find(|(n, _)| n == k) {
            Some(slot) => slot.1 = v,
            None => self.attrs.push((k.to_string(), v)),
        }
    }

    /// `Dom::_mergeNode` for a matched element: a different `xsi:type` on both sides
    /// replaces it wholesale; otherwise attributes overwrite and children union.
    pub(crate) fn merge(&mut self, new: XNode) {
        if let (Some(a), Some(b)) = (self.get("type"), new.get("type")) {
            if a != b {
                *self = new;
                return;
            }
        }
        self.file = new.file;
        self.line = new.line;
        for (k, v) in new.attrs {
            self.set(&k, v);
        }
        for c in new.columns {
            if !self.columns.contains(&c) {
                self.columns.push(c);
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct XTable {
    pub(crate) node: XNode,
    pub(crate) columns: Vec<(String, XNode)>,
    pub(crate) indexes: Vec<(String, XNode)>,
    pub(crate) constraints: Vec<(String, XNode)>,
}

impl XTable {
    /// Merge a later file's declaration of the same table into this one.
    pub(crate) fn merge(&mut self, other: XTable) {
        // Tables carry no xsi:type, so the table node itself always merges.
        for (k, v) in other.node.attrs {
            self.node.set(&k, v);
        }
        self.node.file = other.node.file;
        self.node.line = other.node.line;
        for (k, n) in other.columns {
            merge_keyed(&mut self.columns, k, n);
        }
        for (k, n) in other.indexes {
            merge_keyed(&mut self.indexes, k, n);
        }
        for (k, n) in other.constraints {
            merge_keyed(&mut self.constraints, k, n);
        }
    }
}

pub(crate) fn merge_keyed(into: &mut Vec<(String, XNode)>, key: String, node: XNode) {
    match into.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1.merge(node),
        None => into.push((key, node)),
    }
}

pub(crate) fn node_attrs(e: &BytesStart, line: u32) -> XNode {
    let mut n = XNode { line, ..XNode::default() };
    for a in e.attributes().flatten() {
        let key = a.key.as_ref();
        let name = if key == b"xsi:type" || key.ends_with(b":type") {
            "type".to_string()
        } else {
            String::from_utf8_lossy(key).into_owned()
        };
        n.set(&name, attr_value(&a));
    }
    n
}

/// An attribute value as libxml (and so Magento's `Config\Dom`) reads it: line breaks
/// and tabs typed into the value become spaces *before* entity decoding — XML attribute
/// value normalization — so a comment wrapped across lines compares equal to the
/// single-line comment Magento wrote to the database. `&#10;` stays a newline.
pub(crate) fn attr_value(a: &quick_xml::events::attributes::Attribute) -> String {
    let raw = String::from_utf8_lossy(&a.value).replace("\r\n", "\n").replace(['\r', '\n', '\t'], " ");
    quick_xml::escape::unescape(&raw).map(|v| v.into_owned()).unwrap_or(raw)
}

#[derive(Clone, Copy, PartialEq)]
enum Child {
    Index,
    Constraint,
}

/// Parse one `db_schema.xml` into tables in document order. A `<column>` directly under a
/// `<table>` is a definition; under an `<index>`/`<constraint>` it is a reference.
pub(crate) fn parse_schema_file(xml: &str) -> Res<Vec<(String, XTable)>> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut tables: Vec<(String, XTable)> = Vec::new();
    let mut open: Option<(Child, String)> = None;
    let mut depth = 0usize;
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(|e| e.to_string())?;
        let (e, empty) = match ev {
            Event::Start(e) => (e.into_owned(), false),
            Event::Empty(e) => (e.into_owned(), true),
            Event::End(_) => {
                if depth == 3 {
                    open = None;
                }
                depth = depth.saturating_sub(1);
                buf.clear();
                continue;
            }
            Event::Eof => break,
            _ => {
                buf.clear();
                continue;
            }
        };
        let level = depth + 1;
        let line = lines.line(reader.buffer_position() as usize);
        match (level, local_name(&e).as_str()) {
            (2, "table") => {
                let node = node_attrs(&e, line);
                let key = node.get("name").unwrap_or_default().to_string();
                tables.push((key, XTable { node, ..XTable::default() }));
            }
            (3, kind @ ("column" | "index" | "constraint")) => {
                if let Some((_, t)) = tables.last_mut() {
                    let node = node_attrs(&e, line);
                    let id = if kind == "column" { "name" } else { "referenceId" };
                    let key = node.get(id).unwrap_or_default().to_string();
                    let (list, child) = match kind {
                        "column" => (&mut t.columns, None),
                        "index" => (&mut t.indexes, Some(Child::Index)),
                        _ => (&mut t.constraints, Some(Child::Constraint)),
                    };
                    merge_keyed(list, key.clone(), node);
                    if !empty {
                        open = child.map(|c| (c, key));
                    }
                }
            }
            (4, "column") => {
                if let (Some((child, key)), Some((_, t))) = (&open, tables.last_mut()) {
                    let list = if *child == Child::Index { &mut t.indexes } else { &mut t.constraints };
                    let col = e.attributes().flatten().find(|a| a.key.as_ref() == b"name").map(|a| attr_value(&a));
                    if let (Some(col), Some((_, n))) = (col, list.iter_mut().find(|(k, _)| k == key)) {
                        if !n.columns.contains(&col) {
                            n.columns.push(col);
                        }
                    }
                }
            }
            _ => {}
        }
        if !empty {
            depth = level;
        }
        buf.clear();
    }
    Ok(tables)
}

/// `FileResolverByModule` + `Reader\Filesystem`: every enabled module's
/// `etc/db_schema.xml` in `config.php` order, then `app/etc/db_schema.xml` (required)
/// and `app/etc/*/db_schema.xml`, merged into one document.
pub(crate) fn merged_schema_xml(mage: &Magento) -> Res<Vec<(String, XTable)>> {
    let mut files: Vec<std::path::PathBuf> = mage
        .index
        .modules
        .iter()
        .filter(|m| m.enabled)
        .map(|m| m.path.join("etc/db_schema.xml"))
        .filter(|p| p.is_file())
        .collect();
    let primary = mage.root().join("app/etc/db_schema.xml");
    if !primary.is_file() {
        return Err("app/etc/db_schema.xml is missing".into());
    }
    files.push(primary);
    if let Ok(entries) = std::fs::read_dir(mage.root().join("app/etc")) {
        let mut extra: Vec<_> =
            entries.flatten().map(|e| e.path().join("db_schema.xml")).filter(|p| p.is_file()).collect();
        extra.sort();
        files.extend(extra);
    }

    let mut merged: Vec<(String, XTable)> = Vec::new();
    for f in files {
        let text = mage.index.vfs.read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?;
        for (name, mut t) in parse_schema_file(&text).map_err(|e| format!("{}: {e}", f.display()))? {
            t.node.file = f.clone();
            for (_, n) in t.columns.iter_mut().chain(&mut t.indexes).chain(&mut t.constraints) {
                n.file = f.clone();
            }
            match merged.iter_mut().find(|(n, _)| *n == name) {
                Some((_, into)) => into.merge(t),
                None => merged.push((name, t)),
            }
        }
    }
    Ok(merged)
}

/// The JSON override's input: per XML table name, the last column declared `json`
/// (disabled tables and columns included, exactly as `Db\SchemaBuilder` reads it).
#[cfg_attr(not(feature = "db"), allow(dead_code))] // used by `db status`
pub(crate) fn last_json_columns(xml: &[(String, XTable)]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (name, t) in xml {
        for (col, n) in &t.columns {
            if n.get("type") == Some("json") {
                out.insert(name.clone(), col.clone());
            }
        }
    }
    out
}

/// One `CONSTRAINT … FOREIGN KEY` clause from `SHOW CREATE TABLE`.
#[derive(Debug, PartialEq)]
pub(crate) struct ParsedFk {
    pub(crate) name: String,
    pub(crate) column: String,
    pub(crate) ref_table: String,
    pub(crate) ref_column: String,
    pub(crate) on_delete: String,
}

/// `ForeignKey::fromDefinition`'s regex, by hand:
/// ``,\s*CONSTRAINT\s*`n`\s*FOREIGN KEY\s*?\(`c`\)\s*REFERENCES\s*(`db`\.)?`t`\s*\(`rc`\)``
/// then optional `ON DELETE x` and `ON UPDATE y`. A multi-column key doesn't match, so
/// Magento never sees it.
pub(crate) fn parse_foreign_keys(ddl: &str) -> Vec<ParsedFk> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(off) = ddl[pos..].find(',') {
        let at = pos + off + 1;
        match fk_at(&ddl[at..]) {
            Some((fk, len)) => {
                out.push(fk);
                pos = at + len;
            }
            None => pos = at,
        }
    }
    out
}

struct Cursor<'a> {
    s: &'a str,
    i: usize,
}

impl Cursor<'_> {
    fn ws(&mut self) -> bool {
        let before = self.i;
        self.i += self.s[self.i..].len() - self.s[self.i..].trim_start().len();
        self.i > before
    }

    fn lit(&mut self, l: &str) -> bool {
        let hit = self.s[self.i..].starts_with(l);
        if hit {
            self.i += l.len();
        }
        hit
    }

    fn quoted(&mut self) -> Option<String> {
        if !self.lit("`") {
            return None;
        }
        let end = self.s[self.i..].find('`')?;
        let v = self.s[self.i..self.i + end].to_string();
        self.i += end + 1;
        Some(v)
    }

    /// `(\s*ON\s+<keyword>\s*(RESTRICT|CASCADE|SET NULL|NO ACTION|SET DEFAULT))?`
    fn action(&mut self, keyword: &str) -> Option<String> {
        let save = self.i;
        self.ws();
        if self.lit("ON") && self.ws() && self.lit(keyword) {
            self.ws();
            for a in ["RESTRICT", "CASCADE", "SET NULL", "NO ACTION", "SET DEFAULT"] {
                if self.lit(a) {
                    return Some(a.to_string());
                }
            }
        }
        self.i = save;
        None
    }
}

fn fk_at(s: &str) -> Option<(ParsedFk, usize)> {
    let mut p = Cursor { s, i: 0 };
    p.ws();
    p.lit("CONSTRAINT").then_some(())?;
    p.ws();
    let name = p.quoted()?;
    p.ws();
    p.lit("FOREIGN KEY").then_some(())?;
    p.ws();
    p.lit("(").then_some(())?;
    let column = p.quoted()?;
    p.lit(")").then_some(())?;
    p.ws();
    p.lit("REFERENCES").then_some(())?;
    p.ws();
    let mut ref_table = p.quoted()?;
    if p.lit(".") {
        ref_table = p.quoted()?;
    }
    p.ws();
    p.lit("(").then_some(())?;
    let ref_column = p.quoted()?;
    p.lit(")").then_some(())?;
    let on_delete = match (p.action("DELETE"), p.action("UPDATE")) {
        (Some(d), _) => d,
        // `$m[7]` is set-but-empty when only ON UPDATE follows.
        (None, Some(_)) => String::new(),
        (None, None) => "NO ACTION".into(),
    };
    Some((ParsedFk { name, column, ref_table, ref_column, on_delete }, p.i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_keys_parse_like_the_regex() {
        let ddl = "CREATE TABLE `a` (\n  `id` int,\n  KEY `X` (`b_id`),\n  \
                   CONSTRAINT `FK_A` FOREIGN KEY (`b_id`) REFERENCES `b` (`id`) ON DELETE CASCADE,\n  \
                   CONSTRAINT `FK_B` FOREIGN KEY (`c_id`) REFERENCES `c` (`id`),\n  \
                   CONSTRAINT `FK_C` FOREIGN KEY (`d_id`) REFERENCES `d` (`id`) ON UPDATE CASCADE,\n  \
                   CONSTRAINT `FK_M` FOREIGN KEY (`x`, `y`) REFERENCES `e` (`x`, `y`)\n) ENGINE=InnoDB";
        let fks = parse_foreign_keys(ddl);
        let got: Vec<(&str, &str)> = fks.iter().map(|f| (f.name.as_str(), f.on_delete.as_str())).collect();
        assert_eq!(got, [("FK_A", "CASCADE"), ("FK_B", "NO ACTION"), ("FK_C", "")]);
        assert_eq!(fks[0].ref_table, "b");
    }

    #[test]
    fn dom_merge_unions_index_columns_and_replaces_on_type_change() {
        let a = r#"<schema xmlns:xsi="x"><table name="t" comment="A">
            <column xsi:type="int" name="c" nullable="false" comment="first"/>
            <index referenceId="I"><column name="c"/></index></table></schema>"#;
        let b = r#"<schema xmlns:xsi="x"><table name="t" comment="B">
            <column xsi:type="varchar" name="c" length="5"/>
            <index referenceId="I"><column name="d"/></index></table></schema>"#;
        let mut merged = parse_schema_file(a).unwrap();
        let (_, tb) = parse_schema_file(b).unwrap().remove(0);
        merged[0].1.merge(tb);
        let t = &merged[0].1;
        assert_eq!(t.node.get("comment"), Some("B"));
        let c = &t.columns[0].1;
        assert_eq!((c.get("type"), c.get("nullable"), c.get("length")), (Some("varchar"), None, Some("5")));
        assert_eq!(t.indexes[0].1.columns, ["c", "d"]);
        // Provenance follows the latest declaration.
        assert_eq!(t.indexes[0].1.line, 3);
    }

    #[test]
    fn attribute_values_are_normalized_like_libxml() {
        let xml = "<schema><table name=\"t\" comment=\"one\n\ttwo&#10;three\"/></schema>";
        let t = parse_schema_file(xml).unwrap();
        assert_eq!(t[0].1.node.get("comment"), Some("one  two\nthree"));
    }
}
