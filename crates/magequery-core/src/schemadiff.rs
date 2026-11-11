//! The declarative-schema diff behind Magento's `UpToDateDeclarativeSchema` validator.
//!
//! Magento builds two schemas of DTOs, one from the merged `db_schema.xml` files and one
//! from the live database, then registers a change for every difference; the schema is
//! up to date when nothing is registered. Removals (dropping a table, column, index or
//! constraint, or recreating a table) only register when `db_schema_whitelist.json`
//! names the element; everything else always does.
//!
//! The port follows the PHP rule by rule, including where Magento's own reading is lossy
//! (integer display width is ignored, every `tinyint(1)` reads back as a boolean, a
//! timestamp's `nullable` is never compared, index and foreign-key names are generated
//! rather than read). Wherever Magento's build would throw — an unsupported column type
//! anywhere in the database, a functional index, a foreign key to a table it can't see
//! — this returns `Err`, which the caller counts as "upgrade required", exactly as
//! `setup:db:status` does.

use std::collections::{HashMap, HashSet};

use crate::db::status::{fetch_raw_schema, RawColumn, RawSchema};
use crate::db::Db;
use crate::model::SchemaChange;
use crate::phpver::version_compare;
use crate::schemadecl::{
    last_json_columns, merged_schema_xml, origin_table, parse_foreign_keys, strtr_abbrev, XNode, XTable,
};
use crate::Magento;

type Res<T> = Result<T, String>;

/// Every declared table and every live table, compared. `Err` means Magento's own
/// validator would have thrown.
pub(crate) fn diff(mage: &Magento, db: &mut Db, table_prefix: &str) -> Res<Vec<SchemaChange>> {
    let raw = fetch_raw_schema(db)?;
    let cfg = mage.db_config().map_err(|e| e.to_string())?;
    let connections: Vec<String> = cfg.connections.iter().map(|c| c.name.clone()).collect();
    let server = Server::new(&raw.version)?;
    let names = Names { prefix: table_prefix.to_string() };

    let xml = merged_schema_xml(mage)?;
    let declared = build_declared(&xml, &names, &server, &connections)?;
    let json_columns = if table_prefix.is_empty() { last_json_columns(&xml) } else { HashMap::new() };
    let live = build_live(&raw, &names, &server, &json_columns)?;
    let mut whitelist = Whitelist::load(mage);
    compare(&declared, &live, &mut whitelist)
}

// ---------------------------------------------------------------------------------------
// PHP value semantics
// ---------------------------------------------------------------------------------------

/// A value in a `getDiffSensitiveParams()` array, typed as PHP types it: Magento compares
/// with `===`, so `0` vs `"0"` and `null` vs `"NULL"` differ.
#[derive(Debug, Clone, PartialEq)]
enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<String>),
}

impl Val {
    fn opt_str(v: &Option<String>) -> Val {
        v.clone().map_or(Val::Null, Val::Str)
    }

    fn render(&self) -> String {
        match self {
            Val::Null => "null".into(),
            Val::Bool(b) => b.to_string(),
            Val::Int(i) => i.to_string(),
            Val::Float(f) => f.to_string(),
            // Control characters (a NUL MariaDB stored for a declared `\0`) shown escaped.
            Val::Str(s) => format!(
                "'{}'",
                s.chars()
                    .map(|c| if c.is_control() { format!("\\x{:02x}", c as u32) } else { c.to_string() })
                    .collect::<String>()
            ),
            Val::List(l) => format!("({})", l.join(", ")),
        }
    }
}

/// `BooleanUtils::toBoolean`: only `true`/`1`/`false`/`0`; anything else throws.
fn to_bool(v: &str, what: &str) -> Res<bool> {
    match v {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(format!("{what}: '{v}' is not a boolean")),
    }
}

/// PHP `trim()`'s character set.
fn php_trim(s: &str) -> &str {
    s.trim_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
}

/// `ElementFactory`: a missing, empty or whitespace-only comment is `null`.
fn comment(c: Option<&str>) -> Option<String> {
    c.filter(|c| !php_trim(c).is_empty()).map(str::to_string)
}

/// The leading numeric part of a string, as PHP's `(float)` reads it.
fn php_float(s: &str) -> f64 {
    let t = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0B', '\x0C']);
    let b = t.as_bytes();
    let mut end = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let start = end;
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
    }
    let int_digits = end - start;
    let mut frac_digits = 0;
    if end < b.len() && b[end] == b'.' {
        let dot = end;
        end += 1;
        while end < b.len() && b[end].is_ascii_digit() {
            end += 1;
        }
        frac_digits = end - dot - 1;
        if frac_digits == 0 {
            end = dot + usize::from(int_digits > 0);
        }
    }
    if int_digits == 0 && frac_digits == 0 {
        return 0.0;
    }
    if end < b.len() && matches!(b[end], b'e' | b'E') {
        let mut e = end + 1;
        if matches!(b.get(e), Some(b'+' | b'-')) {
            e += 1;
        }
        if b.get(e).is_some_and(u8::is_ascii_digit) {
            while e < b.len() && b[e].is_ascii_digit() {
                e += 1;
            }
            end = e;
        }
    }
    t[..end].trim_end_matches('.').parse().unwrap_or(0.0)
}

/// PHP `(int)` on a string: an integer prefix exactly, otherwise the truncated float.
fn php_int(s: &str) -> i64 {
    let t = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0B', '\x0C']);
    let b = t.as_bytes();
    let mut end = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let start = end;
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
    }
    if end > start && !matches!(b.get(end), Some(b'.' | b'e' | b'E')) {
        return t[..end].parse().unwrap_or(if t.starts_with('-') { i64::MIN } else { i64::MAX });
    }
    let f = php_float(t);
    if f.is_finite() { f as i64 } else { 0 }
}

/// PHP `(bool)` on a string: false only for `""` and `"0"`.
fn php_bool_cast(s: &str) -> bool {
    !(s.is_empty() || s == "0")
}

// ---------------------------------------------------------------------------------------
// Server version: charset defaults and MariaDB quirks
// ---------------------------------------------------------------------------------------

struct Server {
    mariadb: bool,
    /// `DefaultCharsetCollationMap` for this server's version key.
    charset: &'static str,
    collation: &'static str,
}

impl Server {
    fn new(version: &str) -> Res<Self> {
        let mariadb = version.to_ascii_lowercase().contains("mariadb");
        let key = version_key(version, mariadb)?;
        let utf8mb4 = matches!(
            key.as_str(),
            "10.4." | "10.6." | "10.11." | "11.4." | "11.8." | "12.2." | "12.3." | "mysql_8_29"
        );
        let (charset, collation) =
            if utf8mb4 { ("utf8mb4", "utf8mb4_general_ci") } else { ("utf8", "utf8_general_ci") };
        Ok(Server { mariadb, charset, collation })
    }
}

/// `SqlVersionProvider`. Every path first runs `getSqlVersion()`, which throws unless the
/// version matches one of Magento's supported-version patterns (tried in order:
/// `^8\.0\.`, `^8\.4\.`, `^5\.7\.`, `^10\.[2-6]\.`, `^10\.11\.`, `^11\.[4|8]\.`,
/// `^12\.[2-3]\.`, `^8\.[0-4]\.`, `^10\.([2-9]|10|11)\.`, `^11\.`, `^12\.`) — so MySQL 9
/// is unsupported even though it is "≥ 8.0.29". The key is then `mysql_8_29` for MySQL
/// ≥ 8.0.29, else the matched text.
fn version_key(version: &str, mariadb: bool) -> Res<String> {
    let matched = supported_version(version)
        .ok_or_else(|| format!("current version of RDBMS is not supported: {version}"))?;
    if !mariadb && version_compare(version, "8.0.29") != std::cmp::Ordering::Less {
        return Ok("mysql_8_29".into());
    }
    Ok(matched)
}

/// The text Magento's supported-version pattern matches, if any.
fn supported_version(version: &str) -> Option<String> {
    let (major, rest) = version.split_once('.')?;
    let minor: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let dotted = rest.as_bytes().get(minor.len()) == Some(&b'.');
    let exact = format!("{major}.{minor}.");
    let n: Option<u32> = if dotted { minor.parse().ok() } else { None };
    Some(match (major, n) {
        ("8", Some(0 | 4)) | ("5", Some(7)) | ("10", Some(2..=6 | 11)) => exact,
        ("11", Some(4 | 8)) | ("12", Some(2 | 3)) | ("8", Some(0..=4)) | ("10", Some(7..=10)) => exact,
        ("11", _) => "11.".into(),
        ("12", _) => "12.".into(),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------
// Names: Magento generates index and foreign-key names; it never reads `referenceId`
// ---------------------------------------------------------------------------------------

/// `ExpressionConverter::shortenEntityName`.
fn shorten(s: &str, prefix: &str) -> String {
    if s.len() <= 64 {
        return s.to_string();
    }
    let short = strtr_abbrev(s);
    if short.len() <= 64 {
        return short;
    }
    use md5::{Digest, Md5};
    let hex: String = Md5::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}{hex}")
}

/// `Pdo\Mysql::getIndexName`.
fn index_name(table: &str, columns: &[String], index_type: &str) -> String {
    let prefix = match index_type.to_ascii_lowercase().as_str() {
        "unique" => "unq_",
        "fulltext" => "fti_",
        _ => "idx_",
    };
    shorten(&format!("{table}_{}", columns.join("_")), prefix).to_ascii_uppercase()
}

/// `Pdo\Mysql::getForeignKeyName`.
fn fk_name(table: &str, column: &str, ref_table: &str, ref_column: &str) -> String {
    shorten(&format!("{table}_{column}_{ref_table}_{ref_column}"), "fk_").to_ascii_uppercase()
}

/// `ElementNameResolver`'s type normalization: anything but fulltext/unique is "index".
fn name_type(t: &str) -> &str {
    match t {
        "fulltext" | "unique" => t,
        _ => "index",
    }
}

struct Names {
    prefix: String,
}

impl Names {
    /// `(name, nameWithoutPrefix)` of a table, as `Factories\Table` derives them.
    fn table(&self, raw: &str) -> (String, String) {
        if self.prefix.is_empty() {
            return (raw.to_string(), raw.to_string());
        }
        if raw.starts_with(&self.prefix) {
            (raw.to_string(), raw[self.prefix.len()..].to_string())
        } else {
            (format!("{}{raw}", self.prefix), raw.to_string())
        }
    }

    /// `ResourceConnection::getTableName`: prefix, then shorten with `t_`.
    fn full_table(&self, t: &str) -> String {
        let full = if t.starts_with(&self.prefix) { t.to_string() } else { format!("{}{t}", self.prefix) };
        shorten(&full, "t_")
    }

    /// Full name and matching key of an index or unique key on `t`.
    fn index(&self, t: &Table, columns: &[String], ty: &str) -> (String, String) {
        let ty = name_type(ty);
        let full = index_name(&self.full_table(origin_table(&t.name)), columns, ty);
        let key = if self.prefix.is_empty() { full.clone() } else { index_name(origin_table(&t.nwp), columns, ty) };
        (full, key)
    }

    /// Full name and matching key of a foreign key from `t.column` to `rt.ref_column`.
    fn fk(&self, t: &Table, column: &str, rt: &Table, ref_column: &str) -> (String, String) {
        let full = fk_name(&self.full_table(origin_table(&t.name)), column, &self.full_table(&rt.name), ref_column);
        let key =
            if self.prefix.is_empty() { full.clone() } else { fk_name(origin_table(&t.nwp), column, &rt.nwp, ref_column) };
        (full, key)
    }
}

// ---------------------------------------------------------------------------------------
// The DTO model: one value per element, compared by class + getDiffSensitiveParams()
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct Element {
    /// The DTO class (`Integer`, `Boolean`, `Index`, `Internal`, `Reference`, …).
    class: &'static str,
    /// The real or generated full name (`excludeAutoIndexes` compares these).
    name: String,
    /// `getDiffSensitiveParams()`, in Magento's key order.
    params: Vec<(&'static str, Val)>,
}

impl Element {
    fn same(&self, other: &Element) -> bool {
        self.class == other.class && self.params == other.params
    }

    /// What differs, as `param: live → declared` lines.
    fn describe_change(live: &Element, declared: &Element) -> Vec<String> {
        if live.class != declared.class {
            let ty = |e: &Element| {
                e.params.iter().find(|(k, _)| *k == "type").map_or_else(
                    || e.class.to_ascii_lowercase(),
                    |(_, v)| v.render().trim_matches('\'').to_string(),
                )
            };
            return vec![format!("type: {} → {}", ty(live), ty(declared))];
        }
        declared
            .params
            .iter()
            .filter_map(|(k, dv)| {
                let lv = live.params.iter().find(|(lk, _)| lk == k).map(|(_, v)| v);
                (lv != Some(dv)).then(|| format!("{k}: {} → {}", lv.map_or_else(|| "–".into(), Val::render), dv.render()))
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
struct Table {
    name: String,
    nwp: String,
    resource: String,
    /// `[resource, engine, comment, charset, collation]`.
    params: Vec<(&'static str, Val)>,
    columns: Vec<(String, Element)>,
    /// Primary, unique and foreign keys: one namespace, keyed by `nameWithoutPrefix`.
    constraints: Vec<(String, Element)>,
    /// Keyed by `nameWithoutPrefix`, after `excludeAutoIndexes`.
    indexes: Vec<(String, Element)>,
    /// Declared side only, for the validation rules: foreign keys as
    /// `(column, referenced XML table, referenced column)`, and the column lists of every
    /// primary/unique key and index (auto-indexes included).
    fks: Vec<(String, String, String)>,
    key_columns: Vec<Vec<String>>,
}

/// Insert keyed by name; a later element with the same key replaces the earlier one in
/// place, as a PHP array assignment does.
fn put(list: &mut Vec<(String, Element)>, key: String, e: Element) {
    match list.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = e,
        None => list.push((key, e)),
    }
}

/// Everything the column factories and DTO classes read, after each side's own
/// normalization.
struct ColumnSpec {
    ty: String,
    nullable: bool,
    unsigned: bool,
    identity: bool,
    default: Option<String>,
    length: Option<String>,
    precision: Option<String>,
    scale: Option<String>,
    /// Timestamp `onUpdate` after the factory.
    on_update: Option<String>,
    comment: Option<String>,
    charset: Option<String>,
    collation: Option<String>,
    /// A boolean column's default, already a bool.
    bool_default: Option<bool>,
}

/// The column DTO a factory builds for `spec.ty`, as class + diff params. Mirrors the
/// `Dto/Factories/*` + `Dto/Columns/*` pairs.
fn column_element(name: &str, c: &ColumnSpec) -> Res<Element> {
    let comment = Val::opt_str(&c.comment);
    let ty = Val::Str(c.ty.clone());
    let (class, params): (&'static str, Vec<(&'static str, Val)>) = match c.ty.as_str() {
        "int" | "smallint" | "tinyint" | "bigint" => {
            let default = match c.default.as_deref() {
                None | Some("null") => Val::Null,
                Some(d) => Val::Int(php_int(d)),
            };
            (
                "Integer",
                vec![
                    ("type", ty),
                    // An identity column is never nullable.
                    ("nullable", Val::Bool(c.nullable && !c.identity)),
                    ("padding", Val::Null),
                    ("unsigned", Val::Bool(c.unsigned)),
                    ("identity", Val::Bool(c.identity)),
                    ("default", default),
                    ("comment", comment),
                ],
            )
        }
        "boolean" => (
            "Boolean",
            vec![
                ("nullable", Val::Bool(c.nullable)),
                ("default", c.bool_default.map_or(Val::Null, Val::Bool)),
                ("comment", comment),
            ],
        ),
        "decimal" | "float" | "double" => {
            let decimal = c.ty == "decimal";
            let precision = c.precision.as_deref().map_or(if decimal { 10 } else { 0 }, php_int);
            let scale = c.scale.as_deref().map_or(0, php_int);
            (
                "Real",
                vec![
                    ("type", ty),
                    ("nullable", Val::Bool(c.nullable)),
                    ("precision", Val::Int(precision)),
                    ("scale", Val::Int(scale)),
                    ("unsigned", Val::Bool(c.unsigned)),
                    ("default", c.default.as_deref().map_or(Val::Null, |d| Val::Float(php_float(d)))),
                    ("comment", comment),
                ],
            )
        }
        "varchar" | "char" | "varbinary" => {
            let mut params = vec![
                ("type", ty),
                ("nullable", Val::Bool(c.nullable)),
                ("default", Val::opt_str(&c.default)),
                ("length", Val::Int(c.length.as_deref().map_or(255, php_int))),
                ("comment", comment),
            ];
            if c.ty == "varchar" {
                params.push(("collation", Val::opt_str(&c.collation)));
                params.push(("charset", Val::opt_str(&c.charset)));
            }
            ("StringBinary", params)
        }
        "text" | "mediumtext" | "longtext" => (
            "Text",
            vec![
                ("type", ty),
                ("nullable", Val::Bool(c.nullable)),
                ("comment", comment),
                ("collation", Val::opt_str(&c.collation)),
                ("charset", Val::opt_str(&c.charset)),
            ],
        ),
        "blob" | "mediumblob" | "longblob" | "json" => {
            ("Blob", vec![("type", ty), ("nullable", Val::Bool(c.nullable)), ("comment", comment)])
        }
        "date" => ("Date", vec![("type", ty), ("nullable", Val::Bool(c.nullable)), ("comment", comment)]),
        "datetime" | "timestamp" => (
            "Timestamp",
            vec![
                ("type", ty),
                ("default", Val::Str(c.default.clone().unwrap_or_else(|| "NULL".into()))),
                ("onUpdate", Val::opt_str(&c.on_update)),
                ("comment", comment),
            ],
        ),
        other => return Err(format!("column {name}: types {other} is not declared")),
    };
    Ok(Element { class, name: name.to_string(), params })
}

fn reference(full: String, column: &str, ref_column: &str, ref_table: &str, table: &str, on_delete: &str) -> Element {
    Element {
        class: "Reference",
        name: full,
        params: vec![
            ("type", Val::Str("reference".into())),
            ("column", Val::Str(column.into())),
            ("referenceColumn", Val::Str(ref_column.into())),
            ("referenceTableName", Val::Str(ref_table.into())),
            ("tableName", Val::Str(table.into())),
            ("onDelete", Val::Str(on_delete.into())),
            // Only `default` is ever read or kept (cross-resource keys are skipped).
            ("resource", Val::Str("default".into())),
        ],
    }
}

fn index_element(full: String, columns: Vec<String>, index_type: String) -> Element {
    Element {
        class: "Index",
        name: full,
        params: vec![
            ("type", Val::Str("index".into())),
            ("columns", Val::List(columns)),
            ("indexType", Val::Str(index_type)),
        ],
    }
}

fn internal_element(full: String, ty: &str, columns: Vec<String>) -> Element {
    Element {
        class: "Internal",
        name: full,
        params: vec![("type", Val::Str(ty.into())), ("columns", Val::List(columns))],
    }
}

// ---------------------------------------------------------------------------------------
// Declared side: the merged db_schema.xml, with Config\Dom semantics
// ---------------------------------------------------------------------------------------

fn is_disabled(n: &XNode, what: &str) -> Res<bool> {
    n.get("disabled").map_or(Ok(false), |v| to_bool(v, &format!("{what} disabled")))
}

fn declared_bool(n: &XNode, key: &str, default: bool, what: &str) -> Res<bool> {
    n.get(key).map_or(Ok(default), |v| to_bool(v, &format!("{what} {key}")))
}

/// The declared table DTOs (`Declaration\SchemaBuilder`).
fn build_declared(xml: &[(String, XTable)], names: &Names, server: &Server, connections: &[String]) -> Res<Vec<Table>> {
    let xml_by_name: HashMap<&str, &XTable> = xml.iter().map(|(n, t)| (n.as_str(), t)).collect();
    // First pass: every enabled table with its columns (foreign keys need the referenced
    // table's name and resource).
    let mut shells: HashMap<&str, Table> = HashMap::new();
    for (raw, t) in xml {
        if !is_disabled(&t.node, &format!("table {raw}"))? {
            shells.insert(raw, declared_shell(raw, t, names, server, connections)?);
        }
    }

    let mut out = Vec::new();
    for (raw, t) in xml {
        let Some(shell) = shells.get(raw.as_str()) else { continue };
        let mut table = shell.clone();
        let mut fk_full_names = Vec::new();
        for (id, n) in &t.constraints {
            let what = format!("table {raw} constraint {id}");
            if is_disabled(n, &what)? {
                continue;
            }
            let has_col = |c: &str| shell.columns.iter().any(|(name, _)| name == c);
            match n.get("type").unwrap_or_default() {
                ty @ ("primary" | "unique") => {
                    if let Some(c) = n.columns.iter().find(|c| !has_col(c)) {
                        return Err(format!("{what}: column {c} does not exist"));
                    }
                    let (full, key) = if ty == "primary" {
                        ("PRIMARY".to_string(), "PRIMARY".to_string())
                    } else {
                        names.index(&table, &n.columns, "unique")
                    };
                    table.key_columns.push(n.columns.clone());
                    put(&mut table.constraints, key, internal_element(full, ty, n.columns.clone()));
                }
                "foreign" => {
                    let column = n.get("column").unwrap_or_default();
                    let ref_raw = n.get("referenceTable").unwrap_or_default();
                    let ref_column = n.get("referenceColumn").unwrap_or_default();
                    if !xml_by_name.contains_key(ref_raw) {
                        return Err(format!("{what}: references undeclared table {ref_raw}"));
                    }
                    let Some(rt) = shells.get(ref_raw) else {
                        return Err(format!("{what}: references disabled table {ref_raw}"));
                    };
                    if !has_col(column) || !rt.columns.iter().any(|(c, _)| c == ref_column) {
                        return Err(format!("{what}: column {column} or {ref_raw}.{ref_column} does not exist"));
                    }
                    // A foreign key across resources is silently skipped.
                    if rt.resource != table.resource {
                        continue;
                    }
                    table.fks.push((column.to_string(), ref_raw.to_string(), ref_column.to_string()));
                    let (full, key) = names.fk(&table, column, rt, ref_column);
                    fk_full_names.push(full.clone());
                    let on_delete = n.get("onDelete").unwrap_or("CASCADE");
                    let e = reference(full, column, ref_column, &rt.name, &table.name, on_delete);
                    put(&mut table.constraints, key, e);
                }
                other => return Err(format!("{what}: types {other} is not declared")),
            }
        }

        for (id, n) in &t.indexes {
            let what = format!("table {raw} index {id}");
            if is_disabled(n, &what)? {
                continue;
            }
            if let Some(c) = n.columns.iter().find(|c| !shell.columns.iter().any(|(name, _)| name == *c)) {
                return Err(format!("{what}: column {c} does not exist"));
            }
            let index_type = n.get("indexType").unwrap_or("btree").to_string();
            let (full, key) = names.index(&table, &n.columns, &index_type);
            table.key_columns.push(n.columns.clone());
            // excludeAutoIndexes: an index named like a foreign key is InnoDB's own.
            if !fk_full_names.contains(&full) {
                put(&mut table.indexes, key, index_element(full, n.columns.clone(), index_type));
            }
        }
        out.push((raw.as_str(), table));
    }
    validate(&out)?;
    Ok(out.into_iter().map(|(_, t)| t).collect())
}

fn param<'a>(e: &'a Element, key: &str) -> Option<&'a Val> {
    e.params.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
}

/// `getType()`: the `type` param, or `boolean` for the Boolean DTO (which doesn't diff it).
fn column_type(e: &Element) -> Val {
    param(e, "type").cloned().unwrap_or_else(|| Val::Str("boolean".into()))
}

/// The `Declaration/ValidationRules` Magento runs on the declared schema; any failure
/// throws (so: upgrade required).
fn validate(tables: &[(&str, Table)]) -> Res<()> {
    let by_raw: HashMap<&str, &Table> = tables.iter().map(|(r, t)| (*r, t)).collect();
    let mut errors = Vec::new();
    for (_, t) in tables {
        let col = |name: &str| t.columns.iter().find(|(n, _)| n == name).map(|(_, e)| e);
        for (name, e) in &t.columns {
            // RealTypes: scale <= precision.
            if let (Some(Val::Int(p)), Some(Val::Int(s))) = (param(e, "precision"), param(e, "scale")) {
                if s > p {
                    errors.push(format!("{}.{name}: scale {s} is greater than precision {p}", t.name));
                }
            }
            // AutoIncrementColumnValidation: an identity column needs a key or index.
            if param(e, "identity") == Some(&Val::Bool(true)) && !t.key_columns.iter().any(|c| c.contains(name)) {
                errors.push(format!("{}.{name}: auto-increment column has no index", t.name));
            }
        }
        // PrimaryKeyCanBeCreated: no nullable primary key column (Timestamp has no
        // nullable to check).
        for (_, c) in &t.constraints {
            if param(c, "type") != Some(&Val::Str("primary".into())) {
                continue;
            }
            for name in param(c, "columns").into_iter().flat_map(|v| match v {
                Val::List(l) => l.as_slice(),
                _ => &[],
            }) {
                if col(name).and_then(|e| param(e, "nullable")) == Some(&Val::Bool(true)) {
                    errors.push(format!("{}: primary key column {name} is nullable", t.name));
                }
            }
        }
        for (column, ref_raw, ref_column) in &t.fks {
            let Some(rt) = by_raw.get(ref_raw.as_str()) else { continue };
            // CheckReferenceColumnHasIndex.
            if !rt.key_columns.iter().any(|c| c.contains(ref_column)) {
                errors.push(format!("{}.{ref_column}: referenced column has no index", rt.name));
            }
            // IncosistentReferenceDefinition: same type, unsignedness and length.
            let b = rt.columns.iter().find(|(n, _)| n == ref_column).map(|(_, e)| e);
            let (Some(a), Some(b)) = (col(column), b) else { continue };
            let unsigned_aware = matches!(a.class, "Integer" | "Real");
            if column_type(a) != column_type(b)
                || (unsigned_aware && param(a, "unsigned") != param(b, "unsigned"))
                || (a.class == "StringBinary" && param(a, "length") != param(b, "length"))
            {
                errors.push(format!(
                    "{}.{column} and {}.{ref_column}: foreign key column definitions differ",
                    t.name, rt.name
                ));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("declared schema is invalid: {}", errors.join("; ")))
    }
}

/// A declared table with its params and columns (no keys yet).
fn declared_shell(raw: &str, t: &XTable, names: &Names, server: &Server, connections: &[String]) -> Res<Table> {
    let (name, nwp) = names.table(raw);
    // A resource is honoured only when env.php defines that connection.
    let resource = t.node.get("resource").filter(|r| connections.iter().any(|c| c == r)).unwrap_or("default");
    let params = vec![
        ("resource", Val::Str(resource.into())),
        ("engine", Val::Str(t.node.get("engine").unwrap_or("innodb").into())),
        ("comment", Val::opt_str(&comment(t.node.get("comment")))),
        ("charset", Val::Str(t.node.get("charset").unwrap_or(server.charset).into())),
        ("collation", Val::Str(t.node.get("collation").unwrap_or(server.collation).into())),
    ];
    let mut columns = Vec::new();
    for (cname, n) in &t.columns {
        let what = format!("table {raw} column {cname}");
        if is_disabled(n, &what)? {
            continue;
        }
        let ty = n.get("type").unwrap_or_default().to_string();
        // `Factories\Timestamp`: a truthy value other than CURRENT_TIMESTAMP goes through
        // toBoolean; a falsy one ('0', '') is passed through untouched.
        let on_update = match n.get("on_update") {
            None => None,
            Some(v) if v == "CURRENT_TIMESTAMP" || !php_bool_cast(v) => Some(v.to_string()),
            Some(v) => to_bool(v, &format!("{what} on_update"))?.then(|| "CURRENT_TIMESTAMP".to_string()),
        };
        let bool_default = match (ty.as_str(), n.get("default")) {
            ("boolean", Some(d)) => Some(to_bool(d, &format!("{what} default"))?),
            _ => None,
        };
        let spec = ColumnSpec {
            nullable: declared_bool(n, "nullable", true, &what)?,
            unsigned: declared_bool(n, "unsigned", false, &what)?,
            identity: declared_bool(n, "identity", false, &what)?,
            default: n.get("default").map(str::to_string),
            length: n.get("length").map(str::to_string),
            precision: n.get("precision").map(str::to_string),
            scale: n.get("scale").map(str::to_string),
            on_update,
            comment: comment(n.get("comment")),
            // Declared varchar/text always carry the DTO constructor defaults: the XSD has
            // no column charset, and no factory sets one.
            charset: Some("utf8mb4".into()),
            collation: Some("utf8mb4_general_ci".into()),
            bool_default,
            ty,
        };
        put(&mut columns, cname.clone(), column_element(cname, &spec)?);
    }
    Ok(Table {
        name,
        nwp,
        resource: resource.to_string(),
        params,
        columns,
        constraints: Vec::new(),
        indexes: Vec::new(),
        fks: Vec::new(),
        key_columns: Vec::new(),
    })
}

// ---------------------------------------------------------------------------------------
// Live side: Db\SchemaBuilder over every base table
// ---------------------------------------------------------------------------------------

/// `DefinitionAggregator::processDefaultValue`.
fn process_default(d: &str, mariadb: bool) -> Option<String> {
    if d == "'NULL'" {
        return Some("NULL".into());
    }
    if d == "NULL" && mariadb {
        return None;
    }
    let d = d
        .replace("current_timestamp()", "CURRENT_TIMESTAMP")
        .replace("curdate()", "CURRENT_DATE")
        .replace("curtime()", "CURRENT_TIME");
    Some(d.replace('\'', ""))
}

/// `^(?<type>(?:big|small|tiny|medium)?int)(?:\((?<padding>\d+)\))?` → `(type, padding)`.
fn int_definition(def: &str) -> Option<(&str, Option<&str>)> {
    let ty = ["bigint", "smallint", "tinyint", "mediumint", "int"].into_iter().find(|t| def.starts_with(t))?;
    let padding = def[ty.len()..].strip_prefix('(').and_then(|r| {
        let digits = r.len() - r.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        (digits > 0 && r[digits..].starts_with(')')).then(|| &r[..digits])
    });
    Some((ty, padding))
}

/// `Real` converter: precision and scale from `float|decimal|double(p,s)` or `decimal(p)`.
fn real_definition(def: &str) -> (Option<String>, Option<String>) {
    let lower = def.to_ascii_lowercase();
    let Some(ty) = ["float", "decimal", "double"].into_iter().find(|t| lower.starts_with(t)) else {
        return (None, None);
    };
    let Some(inner) = lower[ty.len()..].trim_start().strip_prefix('(').and_then(|r| r.split_once(')')).map(|(i, _)| i)
    else {
        return (None, None);
    };
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match parts.as_slice() {
        [p, s] if digits(p) && digits(s) => (Some(p.to_string()), Some(s.to_string())),
        [p] if ty == "decimal" && digits(p) => (Some(p.to_string()), Some("0".into())),
        _ => (None, None),
    }
}

/// `StringBinary` converter: the length after `type(`, unless it is empty or `0`
/// (`!empty($matches['padding'])`).
fn string_length(def: &str) -> Option<String> {
    let ty = ["varbinary", "varchar", "binary", "char"].into_iter().find(|t| def.starts_with(t))?;
    let rest = def[ty.len()..].strip_prefix('(').unwrap_or(&def[ty.len()..]);
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (!digits.is_empty() && digits != "0").then_some(digits)
}

/// `/^(?:0x|x)([a-f0-9]+)$/i` → `hex2bin` of the digits.
fn hex_default(d: &str) -> Option<String> {
    let lower = d.to_ascii_lowercase();
    let hex = lower.strip_prefix("0x").or_else(|| lower.strip_prefix('x'))?;
    if hex.is_empty() || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let bytes: Vec<u8> = (0..hex.len()).step_by(2).filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect();
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// `OnUpdate` converter: `/(?:on update)\s([\_\-\s\w\d]+)/` on EXTRA, case-sensitive.
fn has_on_update(extra: &str) -> bool {
    extra.match_indices("on update").any(|(i, m)| {
        let mut rest = extra[i + m.len()..].chars();
        rest.next().is_some_and(char::is_whitespace)
            && rest.next().is_some_and(|c| c == '_' || c == '-' || c.is_whitespace() || c.is_alphanumeric())
    })
}

fn live_column(c: &RawColumn, server: &Server, json: bool) -> Res<Element> {
    let what = format!("table {} column {}", c.table, c.name);
    let data_type = c.data_type.as_str();
    let mut spec = ColumnSpec {
        ty: data_type.to_string(),
        nullable: c.nullable,
        unsigned: false,
        identity: false,
        default: c.default.as_deref().and_then(|d| process_default(d, server.mariadb)),
        length: None,
        precision: None,
        scale: None,
        on_update: None,
        comment: comment(c.comment.as_deref()),
        charset: c.charset.clone(),
        collation: c.collation.clone(),
        bool_default: None,
    };
    let unsigned = c.definition.to_ascii_lowercase().contains("unsigned");
    match data_type {
        "boolean" | "int" | "smallint" | "tinyint" | "bigint" => {
            if let Some((ty, padding)) = int_definition(&c.definition) {
                // "we have an agreement that tinyint(1) is Boolean"
                if data_type == "tinyint" && ty == "tinyint" && padding == Some("1") {
                    spec.ty = "boolean".into();
                    spec.bool_default = spec.default.as_deref().map(php_bool_cast);
                } else {
                    spec.unsigned = unsigned;
                    spec.identity = c.extra.to_ascii_lowercase().contains("auto_increment");
                }
            }
        }
        "decimal" | "float" | "double" => {
            (spec.precision, spec.scale) = real_definition(&c.definition);
            spec.unsigned = unsigned;
        }
        "text" | "mediumtext" | "longtext" | "blob" | "mediumblob" | "longblob" | "date" | "json" => {}
        "datetime" | "timestamp" => {
            if spec.default.as_deref() == Some("0000-00-00 00:00:00") {
                spec.default = Some("0".into());
            }
            if has_on_update(&c.extra) {
                spec.on_update = Some("CURRENT_TIMESTAMP".into());
            }
        }
        "char" | "varchar" | "binary" | "varbinary" => {
            if data_type == "binary" {
                return Err(format!("{what}: types binary is not declared"));
            }
            spec.length = string_length(&c.definition);
            if data_type == "varbinary" {
                if let Some(bytes) = spec.default.as_deref().and_then(hex_default) {
                    spec.default = Some(bytes);
                }
            }
        }
        other => return Err(format!("{what}: cannot process definition to array for type {other}")),
    }
    if json {
        spec.ty = "json".into();
    }
    column_element(&c.name, &spec)
}

/// The live table DTOs.
fn build_live(raw: &RawSchema, names: &Names, server: &Server, json_columns: &HashMap<String, String>) -> Res<Vec<Table>> {
    let mut tables: Vec<Table> = Vec::new();
    let mut by_name: HashMap<&str, usize> = HashMap::new();
    for (real, engine, collation, table_comment) in &raw.tables {
        if !names.prefix.is_empty() && !real.starts_with(&names.prefix) {
            // Magento prefixes the name and then fails to SHOW CREATE the table.
            return Err(format!("table {real} is outside the table prefix '{}'", names.prefix));
        }
        let Some(collation) = collation else {
            return Err(format!("table {real} has no collation"));
        };
        let Some(charset) = raw.collation_charsets.get(collation) else {
            return Err(format!("table {real}: no character set for collation {collation}"));
        };
        let (name, nwp) = names.table(real);
        let params = vec![
            ("resource", Val::Str("default".into())),
            ("engine", Val::Str(engine.clone().unwrap_or_default().to_ascii_lowercase())),
            ("comment", Val::opt_str(&comment(Some(table_comment)))),
            ("charset", Val::Str(charset.clone())),
            ("collation", Val::Str(collation.clone())),
        ];
        by_name.insert(real, tables.len());
        tables.push(Table {
            name,
            nwp,
            resource: "default".into(),
            params,
            columns: Vec::new(),
            constraints: Vec::new(),
            indexes: Vec::new(),
            fks: Vec::new(),
            key_columns: Vec::new(),
        });
    }

    for c in &raw.columns {
        let Some(&i) = by_name.get(c.table.as_str()) else { continue };
        let json = json_columns.get(&c.table).is_some_and(|j| *j == c.name);
        put(&mut tables[i].columns, c.name.clone(), live_column(c, server, json)?);
    }

    // Foreign keys first: their names decide which indexes are InnoDB's auto-indexes.
    let mut fk_names: HashMap<usize, HashSet<String>> = HashMap::new();
    let mut fk_tables: Vec<(&String, &String)> = raw.create_table.iter().collect();
    fk_tables.sort();
    for (real, ddl) in fk_tables {
        let Some(&i) = by_name.get(real.as_str()) else { continue };
        for fk in parse_foreign_keys(ddl) {
            let Some(&ri) = by_name.get(fk.ref_table.as_str()) else {
                return Err(format!("table {real} foreign key {}: references unknown table {}", fk.name, fk.ref_table));
            };
            let col_exists = |t: &Table, c: &str| t.columns.iter().any(|(n, _)| n == c);
            if !col_exists(&tables[i], &fk.column) || !col_exists(&tables[ri], &fk.ref_column) {
                return Err(format!("table {real} foreign key {}: unknown column", fk.name));
            }
            let key = if names.prefix.is_empty() {
                fk.name.clone()
            } else {
                fk_name(origin_table(&tables[i].nwp), &fk.column, &tables[ri].nwp, &fk.ref_column)
            };
            let e = reference(fk.name.clone(), &fk.column, &fk.ref_column, &tables[ri].name, &tables[i].name, &fk.on_delete);
            fk_names.entry(i).or_default().insert(fk.name);
            put(&mut tables[i].constraints, key, e);
        }
    }

    // `SHOW INDEXES`, grouped per key in column order.
    let mut keys: Vec<(usize, &str, bool, String, Vec<String>)> = Vec::new();
    for r in &raw.indexes {
        let Some(&i) = by_name.get(r.table.as_str()) else { continue };
        let Some(col) = &r.column else {
            return Err(format!("table {} index {}: a functional key part has no column", r.table, r.name));
        };
        if !tables[i].columns.iter().any(|(n, _)| n == col) {
            return Err(format!("table {} index {}: column {col} does not exist", r.table, r.name));
        }
        match keys.iter_mut().find(|k| k.0 == i && k.1 == r.name) {
            Some(k) if !k.4.contains(col) => k.4.push(col.clone()),
            Some(_) => {}
            None => keys.push((i, &r.name, r.non_unique, r.index_type.to_ascii_lowercase(), vec![col.clone()])),
        }
    }
    for (i, name, non_unique, index_type, columns) in keys {
        let nwp = tables[i].nwp.clone();
        if non_unique {
            if fk_names.get(&i).is_some_and(|f| f.contains(name)) {
                continue; // excludeAutoIndexes
            }
            let key = if names.prefix.is_empty() {
                name.to_string()
            } else {
                index_name(origin_table(&nwp), &columns, name_type(&index_type))
            };
            put(&mut tables[i].indexes, key, index_element(name.to_string(), columns, index_type));
        } else {
            let primary = name == "PRIMARY";
            let key = if primary || names.prefix.is_empty() {
                name.to_string()
            } else {
                index_name(origin_table(&nwp), &columns, "unique")
            };
            let ty = if primary { "primary" } else { "unique" };
            put(&mut tables[i].constraints, key, internal_element(name.to_string(), ty, columns));
        }
    }
    Ok(tables)
}

// ---------------------------------------------------------------------------------------
// Whitelist
// ---------------------------------------------------------------------------------------

/// A decoded whitelist as PHP holds it after `json_decode($json, true)`: arrays (a JSON
/// object, or a list keyed `"0"`, `"1"`, …) and scalars, of which only `null` fails `isset`.
#[derive(Debug, Clone, PartialEq)]
enum Php {
    Array(Vec<(String, Php)>),
    Null,
    Scalar,
}

impl Php {
    fn from_json(v: serde_json::Value) -> Php {
        match v {
            serde_json::Value::Object(o) => Php::Array(o.into_iter().map(|(k, v)| (k, Php::from_json(v))).collect()),
            serde_json::Value::Array(l) => {
                Php::Array(l.into_iter().enumerate().map(|(i, v)| (i.to_string(), Php::from_json(v))).collect())
            }
            serde_json::Value::Null => Php::Null,
            _ => Php::Scalar,
        }
    }

    /// `array_replace_recursive($self, $other)` for two arrays.
    fn replace_recursive(&mut self, other: Php) {
        let (Php::Array(mine), Php::Array(theirs)) = (&mut *self, other) else { return };
        for (k, v) in theirs {
            match mine.iter_mut().find(|(mk, _)| *mk == k) {
                Some((_, slot)) if matches!((&*slot, &v), (Php::Array(_), Php::Array(_))) => slot.replace_recursive(v),
                Some((_, slot)) => *slot = v,
                None => mine.push((k, v)),
            }
        }
    }

    /// `isset($this[$k1][$k2]…)`: every step an array holding the key, the last not null.
    fn isset(&self, path: &[&str]) -> bool {
        let Some((first, rest)) = path.split_first() else { return !matches!(self, Php::Null) };
        match self {
            Php::Array(entries) => entries.iter().find(|(k, _)| k == first).is_some_and(|(_, v)| v.isset(rest)),
            _ => false,
        }
    }
}

/// The union of every registered module's `db_schema_whitelist.json` — enabled or not,
/// listed in `config.php` or not — folded with `array_replace_recursive` like
/// `Diff::getWhiteListTables`. Loaded on first use, as in Magento, so an invalid file
/// fails the check only once a removal needs it.
struct Whitelist {
    files: Vec<std::path::PathBuf>,
    loaded: Option<Res<Php>>,
}

impl Whitelist {
    fn load(mage: &Magento) -> Self {
        let mut dirs: Vec<std::path::PathBuf> = mage.index.modules.iter().map(|m| m.path.clone()).collect();
        dirs.extend(mage.module_check().on_disk_not_in_config.iter().map(|m| m.path.clone()));
        let files = dirs.into_iter().map(|d| d.join("etc/db_schema_whitelist.json")).filter(|p| p.is_file()).collect();
        Whitelist { files, loaded: None }
    }

    fn data(&mut self) -> Res<&Php> {
        if self.loaded.is_none() {
            self.loaded = Some(read_whitelists(&self.files));
        }
        self.loaded.as_ref().expect("just loaded").as_ref().map_err(Clone::clone)
    }

    fn has_table(&mut self, nwp: &str) -> Res<bool> {
        Ok(self.data()?.isset(&[nwp]))
    }

    fn has(&mut self, table_nwp: &str, kind: &str, name: &str) -> Res<bool> {
        Ok(self.data()?.isset(&[table_nwp, kind, name]))
    }
}

fn read_whitelists(files: &[std::path::PathBuf]) -> Res<Php> {
    let mut out = Php::Array(Vec::new());
    for f in files {
        let text = std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?;
        // json_decode gives null for invalid JSON, and a scalar for a scalar document;
        // either is a TypeError in array_replace_recursive.
        let decoded = serde_json::from_str::<serde_json::Value>(&text).map(Php::from_json);
        match decoded {
            Ok(arr @ Php::Array(_)) => out.replace_recursive(arr),
            _ => return Err(format!("{}: not a JSON object or array", f.display())),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// The diff
// ---------------------------------------------------------------------------------------

fn change(op: &str, table: &str, element: Option<&str>, detail: Vec<String>) -> SchemaChange {
    SchemaChange {
        operation: op.to_string(),
        table: table.to_string(),
        element: element.map(str::to_string),
        detail,
    }
}

fn compare(declared: &[Table], live: &[Table], wl: &mut Whitelist) -> Res<Vec<SchemaChange>> {
    let live_by_name: HashMap<&str, &Table> = live.iter().map(|t| (t.name.as_str(), t)).collect();
    let mut out = Vec::new();
    let mut declared_sorted: Vec<&Table> = declared.iter().collect();
    declared_sorted.sort_by(|a, b| a.name.cmp(&b.name));

    for t in declared_sorted {
        let Some(d) = live_by_name.get(t.name.as_str()) else {
            out.push(change("create_table", &t.name, None, Vec::new()));
            continue;
        };
        // A resource change recreates the table, and nothing else about it is compared.
        if t.resource != d.resource {
            if wl.has_table(&t.nwp)? {
                let detail = vec![format!("resource: '{}' → '{}'", d.resource, t.resource)];
                out.push(change("recreate_table", &t.name, None, detail));
            }
            continue;
        }
        if t.params != d.params {
            let detail = t
                .params
                .iter()
                .zip(&d.params)
                .filter(|(a, b)| a.1 != b.1)
                .map(|(a, b)| format!("{}: {} → {}", a.0, b.1.render(), a.1.render()))
                .collect();
            out.push(change("modify_table", &t.name, None, detail));
        }

        for (name, c) in &t.columns {
            match d.columns.iter().find(|(n, _)| n == name) {
                None => out.push(change("add_column", &t.name, Some(name), Vec::new())),
                Some((_, lc)) if !lc.same(c) => {
                    out.push(change("modify_column", &t.name, Some(name), Element::describe_change(lc, c)))
                }
                Some(_) => {}
            }
        }
        for (name, _) in &d.columns {
            if !t.columns.iter().any(|(n, _)| n == name) && wl.has(&d.nwp, "column", name)? {
                out.push(change("drop_element", &t.name, Some(name), vec!["column".into()]));
            }
        }

        for (kind, decl, liv) in [("constraint", &t.constraints, &d.constraints), ("index", &t.indexes, &d.indexes)] {
            for (key, e) in decl {
                match liv.iter().find(|(k, _)| k == key) {
                    None => out.push(change(
                        "add_complex_element",
                        &t.name,
                        Some(&e.name),
                        vec![format!("{kind} missing live")],
                    )),
                    // A changed index or key is dropped and re-added; the re-add is never
                    // whitelist-gated, so it always counts.
                    Some((_, le)) if !le.same(e) => {
                        // The drop half is gated like any removal; Magento lists it first.
                        if wl.has(&d.nwp, kind, key)? {
                            let op = if le.class == "Reference" { "drop_reference" } else { "drop_element" };
                            out.push(change(op, &t.name, Some(&le.name), vec![kind.to_string()]));
                        }
                        let mut detail = vec![format!("{kind} differs")];
                        detail.extend(Element::describe_change(le, e));
                        out.push(change("add_complex_element", &t.name, Some(&e.name), detail));
                    }
                    Some(_) => {}
                }
            }
            for (key, le) in liv {
                if !decl.iter().any(|(k, _)| k == key) && wl.has(&d.nwp, kind, key)? {
                    let op = if le.class == "Reference" { "drop_reference" } else { "drop_element" };
                    out.push(change(op, &t.name, Some(&le.name), vec![kind.to_string()]));
                }
            }
        }
    }

    let declared_names: HashSet<&str> = declared.iter().map(|t| t.name.as_str()).collect();
    let mut extra: Vec<&Table> = live.iter().filter(|t| !declared_names.contains(t.name.as_str())).collect();
    extra.sort_by(|a, b| a.name.cmp(&b.name));
    for d in extra {
        if wl.has_table(&d.nwp)? {
            out.push(change("drop_table", &d.name, None, Vec::new()));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemadecl::parse_schema_file;

    #[test]
    fn names_match_magentos_generator() {
        // Worked examples from the core whitelist (no prefix).
        assert_eq!(
            index_name("catalog_product_entity", &["attribute_set_id".into()], "index"),
            "CATALOG_PRODUCT_ENTITY_ATTRIBUTE_SET_ID"
        );
        assert_eq!(
            fk_name("catalog_product_entity", "attribute_set_id", "eav_attribute_set", "attribute_set_id"),
            "CAT_PRD_ENTT_ATTR_SET_ID_EAV_ATTR_SET_ATTR_SET_ID"
        );
        assert_eq!(origin_table("catalogsearch_fulltext_replica"), "catalogsearch_fulltext");
        assert_eq!(origin_table("_replica"), "_replica");
    }

    #[test]
    fn long_names_hash_the_original() {
        let n = index_name(&"x".repeat(70), &["y".into()], "unique");
        assert!(n.starts_with("UNQ_") && n.len() == 36, "{n}");
    }

    #[test]
    fn version_keys() {
        assert_eq!(version_key("11.4.4-MariaDB", true).unwrap(), "11.4.");
        assert_eq!(version_key("10.5.27-MariaDB", true).unwrap(), "10.5.");
        assert_eq!(version_key("10.8.1-MariaDB", true).unwrap(), "10.8.");
        assert_eq!(version_key("10.11.9-MariaDB", true).unwrap(), "10.11.");
        assert_eq!(version_key("11.2.0-MariaDB", true).unwrap(), "11.");
        assert_eq!(version_key("8.0.36", false).unwrap(), "mysql_8_29");
        assert_eq!(version_key("8.0.28", false).unwrap(), "8.0.");
        assert_eq!(version_key("5.7.44-log", false).unwrap(), "5.7.");
        assert!(version_key("9.1.0", true).is_err());
        // "≥ 8.0.29", but outside the supported patterns: Magento throws.
        assert!(version_key("9.1.0", false).is_err());
        assert!(version_key("8.5.0", false).is_err());
        assert_eq!(version_key("8.4.3", false).unwrap(), "mysql_8_29");
        assert_eq!(version_key("8.0.36-0ubuntu0.22.04.1", false).unwrap(), "mysql_8_29");
        assert_eq!(Server::new("11.4.4-MariaDB").unwrap().charset, "utf8mb4");
        assert_eq!(Server::new("10.5.27-MariaDB").unwrap().charset, "utf8");
    }

    #[test]
    fn defaults_follow_php_casts() {
        assert_eq!(process_default("'abc'", true).as_deref(), Some("abc"));
        assert_eq!(process_default("NULL", true), None);
        assert_eq!(process_default("NULL", false).as_deref(), Some("NULL"));
        assert_eq!(process_default("'NULL'", true).as_deref(), Some("NULL"));
        assert_eq!(process_default("current_timestamp()", true).as_deref(), Some("CURRENT_TIMESTAMP"));
        assert_eq!(php_float("0.0000"), 0.0);
        assert_eq!(php_float("12.5abc"), 12.5);
        assert_eq!(php_float("abc"), 0.0);
        assert_eq!(php_int("12abc"), 12);
        assert_eq!(php_int("1e3"), 1000);
        assert!(has_on_update("on update current_timestamp()"));
        assert!(has_on_update("DEFAULT_GENERATED on update CURRENT_TIMESTAMP"));
        assert!(!has_on_update("auto_increment"));
        assert_eq!(int_definition("int(10) unsigned"), Some(("int", Some("10"))));
        assert_eq!(int_definition("tinyint(1)"), Some(("tinyint", Some("1"))));
        assert_eq!(real_definition("decimal(12,4) unsigned"), (Some("12".into()), Some("4".into())));
        assert_eq!(string_length("varbinary(32)").as_deref(), Some("32"));
    }

    fn declare(xml: &str) -> Res<Vec<Table>> {
        let server = Server::new("11.4.4-MariaDB").unwrap();
        let names = Names { prefix: String::new() };
        build_declared(&parse_schema_file(xml).unwrap(), &names, &server, &["default".into()])
    }

    #[test]
    fn validation_rules_reject_what_magento_rejects() {
        let ok = r#"<schema xmlns:xsi="x">
            <table name="p"><column xsi:type="int" name="id" unsigned="true" nullable="false" identity="true"/>
              <constraint xsi:type="primary" referenceId="PRIMARY"><column name="id"/></constraint></table>
            <table name="c"><column xsi:type="int" name="p_id" unsigned="true" nullable="false"/>
              <constraint xsi:type="foreign" referenceId="FK" column="p_id" referenceTable="p" referenceColumn="id"/></table>
        </schema>"#;
        let tables = declare(ok).unwrap();
        let c = tables.iter().find(|t| t.name == "c").unwrap();
        assert_eq!(c.constraints[0].1.name, "C_P_ID_P_ID");

        // Signedness differs across the foreign key.
        let bad = ok.replace(r#"name="p_id" unsigned="true""#, r#"name="p_id""#);
        assert!(declare(&bad).unwrap_err().contains("definitions differ"));
        // A nullable primary key column.
        let bad = ok.replace(r#"name="id" unsigned="true" nullable="false" identity="true""#, r#"name="id" unsigned="true""#);
        assert!(declare(&bad).unwrap_err().contains("nullable"));
        // Scale above precision.
        let bad = ok.replace(
            r#"<column name="id"/></constraint></table>"#,
            r#"<column name="id"/></constraint><column xsi:type="decimal" name="d" precision="4" scale="6"/></table>"#,
        );
        assert!(declare(&bad).unwrap_err().contains("scale 6"));
        // A foreign key to an undeclared table.
        let bad = ok.replace(r#"referenceTable="p""#, r#"referenceTable="missing""#);
        assert!(declare(&bad).unwrap_err().contains("undeclared table missing"));
    }

    #[test]
    fn whitelists_merge_like_array_replace_recursive() {
        let mut wl = Php::Array(Vec::new());
        for json in [
            r#"{"t": {"column": {"a": true}}, "gone": {"column": {"x": true}}}"#,
            "[]",
            r#"{"t": {"column": {"b": true, "n": null}}, "gone": true}"#,
        ] {
            wl.replace_recursive(Php::from_json(serde_json::from_str(json).unwrap()));
        }
        assert!(wl.isset(&["t", "column", "a"]) && wl.isset(&["t", "column", "b"]));
        assert!(!wl.isset(&["t", "column", "n"]), "null fails isset");
        // A scalar replaced the array: the table stays set, its columns are gone.
        assert!(wl.isset(&["gone"]) && !wl.isset(&["gone", "column", "x"]));
    }

}
