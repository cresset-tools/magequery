//! Reads for `db status` (`setup:db:status`): the setup bookkeeping tables, over one
//! connection.

use std::collections::{HashMap, HashSet};

use super::{connect, Db};
use crate::model::DbConnection;

/// `setup_module` rows: module → (`schema_version`, `data_version`).
pub(crate) type SetupModuleRows = HashMap<String, (Option<String>, Option<String>)>;

/// Everything `db status` reads live. Each part is its own `Result`: Magento runs its
/// validators independently and counts one that throws as "upgrade required", so a
/// missing table fails only the checks that need it.
pub(crate) struct LiveSetupState {
    pub setup_module: Result<SetupModuleRows, String>,
    /// `patch_list.patch_name`, verbatim — Magento's `isApplied` is an exact `in_array`.
    pub patch_list: Result<HashSet<String>, String>,
}

/// A connection set up the way Magento's `Pdo\Mysql::_connect` sets up its own: the
/// `initStatements` from env.php, or else the version-based `SET NAMES` default. The
/// session charset decides how comments and defaults read back (a CJK comment over
/// `SET NAMES latin1` reads as `??`), so it has to match for the diff to.
pub(crate) fn open(conn: &DbConnection) -> Result<Db, String> {
    let mut db = connect(conn)?;
    let statements = match &conn.init_statements {
        Some(sql) => split_multi_query(sql),
        None => {
            let version: Option<String> = db.query_first("SELECT @@version").ok().flatten();
            vec![default_charset_init(version.as_deref())]
        }
    };
    for stmt in statements {
        db.query_drop(&stmt)?;
    }
    Ok(db)
}

/// `DefaultCharsetCollationMap::getInitStatementFromVersionString`: utf8mb4 from MariaDB
/// 10.4 and MySQL 8.0.29 on, utf8 before; utf8mb4 when the version is unknown.
fn default_charset_init(version: Option<&str>) -> String {
    use crate::phpver::version_compare;
    use std::cmp::Ordering::Less;
    let utf8mb4 = match version {
        None => true,
        Some(v) => {
            // `(\d+\.\d+(?:\.\d+)?)`: the first dotted number.
            let start = v.find(|c: char| c.is_ascii_digit()).unwrap_or(v.len());
            let num: String =
                v[start..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            let num = num.trim_end_matches('.');
            if !num.contains('.') {
                false
            } else if v.to_ascii_lowercase().contains("mariadb") {
                version_compare(num, "10.4.0") != Less
            } else {
                version_compare(num, "8.0.29") != Less
            }
        }
    };
    if utf8mb4 {
        "SET NAMES utf8mb4 COLLATE utf8mb4_general_ci".into()
    } else {
        "SET NAMES utf8 COLLATE utf8_general_ci".into()
    }
}

/// `Pdo\Mysql::_splitMultiQuery`: split on `;` outside quotes and comments.
fn split_multi_query(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut comment: Option<&str> = None;
    let mut prev: Option<char> = None;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        let next = chars.peek().copied();
        match (comment, quote) {
            (Some("line"), _) if c == '\n' => comment = None,
            (Some("block"), _) if c == '*' && next == Some('/') => {
                cur.push(c);
                cur.push(chars.next().expect("peeked"));
                comment = None;
                prev = Some('/');
                continue;
            }
            (Some(_), _) => {}
            (None, Some(q)) if c == q && prev != Some('\\') => quote = None,
            (None, Some(_)) => {}
            (None, None) => match c {
                '\'' | '"' if prev != Some('\\') => quote = Some(c),
                '-' | '/' if next == Some(c) && matches!(prev, None | Some('\n')) => comment = Some("line"),
                '/' if next == Some('*') => comment = Some("block"),
                ';' => {
                    if !cur.trim().is_empty() {
                        out.push(cur.trim().to_string());
                    }
                    cur.clear();
                    prev = Some(c);
                    continue;
                }
                _ => {}
            },
        }
        cur.push(c);
        prev = Some(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

pub(crate) fn fetch_setup_state(db: &mut Db, table_prefix: &str) -> LiveSetupState {
    let setup_module = db
        .query::<(Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>)>(format!(
            "SELECT module, schema_version, data_version FROM {table_prefix}setup_module"
        ))
        .map(|rows| rows.into_iter().map(|(m, s, d)| (txt(m), (opt_txt(s), opt_txt(d)))).collect());
    let patch_list = db
        .query::<Vec<u8>>(format!("SELECT patch_name FROM {table_prefix}patch_list"))
        .map(|rows| rows.into_iter().map(txt).collect());
    LiveSetupState { setup_module, patch_list }
}

/// One live column, as Magento's `DbSchemaReader::readColumns` selects it.
#[derive(Debug, Clone)]
pub(crate) struct RawColumn {
    pub table: String,
    pub name: String,
    /// `DATA_TYPE`.
    pub data_type: String,
    /// `COLUMN_TYPE` (`int(10) unsigned`, `varchar(255)`, …) — Magento's `definition`.
    pub definition: String,
    pub nullable: bool,
    /// `COLUMN_DEFAULT` verbatim (MariaDB quotes strings and writes `NULL`).
    pub default: Option<String>,
    pub extra: String,
    /// `NULL` when empty, as Magento's `IF(COLUMN_COMMENT="", NULL, …)`.
    pub comment: Option<String>,
    pub charset: Option<String>,
    pub collation: Option<String>,
}

/// One `SHOW INDEXES` row (from `information_schema.STATISTICS`, same data).
#[derive(Debug, Clone)]
pub(crate) struct RawIndexColumn {
    pub table: String,
    pub name: String,
    pub non_unique: bool,
    pub index_type: String,
    /// `NULL` for a functional index part.
    pub column: Option<String>,
}

/// Everything the declarative-schema diff reads from the database.
pub(crate) struct RawSchema {
    /// `SHOW variables LIKE "version"`.
    pub version: String,
    /// Base tables: `(name, engine, collation, comment)`, in `information_schema` order.
    pub tables: Vec<(String, Option<String>, Option<String>, String)>,
    /// Table collation → its character set, as Magento's join on
    /// `COLLATION_CHARACTER_SET_APPLICABILITY` resolves it (first row wins).
    pub collation_charsets: HashMap<String, String>,
    pub columns: Vec<RawColumn>,
    pub indexes: Vec<RawIndexColumn>,
    /// `SHOW CREATE TABLE` text of each table that has a foreign key — where Magento
    /// reads foreign keys from.
    pub create_table: HashMap<String, String>,
}

/// Text from the server, read as bytes: after a non-UTF-8 `SET NAMES` (latin1, say) a
/// comment's bytes needn't be valid UTF-8, and Magento compares them as raw bytes. A
/// lossy decode keeps them unequal to the declared text, where `String` would drop the row.
fn txt(b: Vec<u8>) -> String {
    String::from_utf8(b).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn opt_txt(b: Option<Vec<u8>>) -> Option<String> {
    b.map(txt)
}

/// Read the whole schema of the connection's database in a handful of queries, plus one
/// `SHOW CREATE TABLE` per table that has foreign keys.
pub(crate) fn fetch_raw_schema(db: &mut Db) -> Result<RawSchema, String> {
    type B = Vec<u8>;
    let version: String = db
        .query_first::<(B, B)>("SHOW VARIABLES LIKE 'version'")?
        .map(|(_, v)| txt(v))
        .ok_or("the server reported no version")?;

    let tables = db
        .query::<(B, Option<B>, Option<B>, Option<B>)>(
            "SELECT TABLE_NAME, ENGINE, TABLE_COLLATION, TABLE_COMMENT FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE'",
        )?
        .into_iter()
        .map(|(n, e, c, comment)| (txt(n), opt_txt(e), opt_txt(c), opt_txt(comment).unwrap_or_default()))
        .collect();

    // MariaDB 10.10+ joins on FULL_COLLATION_NAME when the column exists.
    let has_full: Option<B> = db.query_first(
        "SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = 'information_schema' \
         AND TABLE_NAME = 'COLLATION_CHARACTER_SET_APPLICABILITY' AND COLUMN_NAME = 'FULL_COLLATION_NAME'",
    )?;
    let key = if has_full.is_some() { "FULL_COLLATION_NAME" } else { "COLLATION_NAME" };
    let mut collation_charsets = HashMap::new();
    for (collation, charset) in db.query::<(B, B)>(format!(
        "SELECT {key}, CHARACTER_SET_NAME FROM information_schema.COLLATION_CHARACTER_SET_APPLICABILITY"
    ))? {
        collation_charsets.entry(txt(collation)).or_insert(txt(charset));
    }

    type ColRow = (B, B, B, B, B, Option<B>, B, B, Option<B>, Option<B>);
    let columns = db
        .query::<ColRow>(
            "SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA, \
             COLUMN_COMMENT, CHARACTER_SET_NAME, COLLATION_NAME FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME, ORDINAL_POSITION",
        )?
        .into_iter()
        .map(|(table, name, data_type, definition, nullable, default, extra, comment, charset, collation)| {
            let comment = txt(comment);
            RawColumn {
                table: txt(table),
                name: txt(name),
                data_type: txt(data_type),
                definition: txt(definition),
                nullable: nullable == b"YES",
                default: opt_txt(default),
                extra: txt(extra),
                comment: (!comment.is_empty()).then_some(comment),
                charset: opt_txt(charset),
                collation: opt_txt(collation),
            }
        })
        .collect();

    let indexes = db
        .query::<(B, B, i64, B, Option<B>)>(
            // No ORDER BY: the natural order is `SHOW INDEXES` order (key by key, columns
            // in sequence), and it decides which of two keys with the same generated name
            // Magento keeps — the later one.
            "SELECT TABLE_NAME, INDEX_NAME, NON_UNIQUE, INDEX_TYPE, COLUMN_NAME FROM information_schema.STATISTICS \
             WHERE TABLE_SCHEMA = DATABASE()",
        )?
        .into_iter()
        .map(|(table, name, non_unique, index_type, column)| RawIndexColumn {
            table: txt(table),
            name: txt(name),
            non_unique: non_unique != 0,
            index_type: txt(index_type),
            column: opt_txt(column),
        })
        .collect();

    // Magento regex-scans every table's `SHOW CREATE TABLE` for foreign keys. Only tables
    // whose DDL can contain the words do anything: those with real foreign keys, and those
    // where some free text (a comment, a default, a generated-column or CHECK expression)
    // says FOREIGN KEY — Magento parses that as a key too. LIKE is case-insensitive, so
    // this over-selects, never under-selects; tables the server can't search are skipped.
    let mut fk_tables: HashSet<String> = db
        .query::<B>(
            "SELECT DISTINCT TABLE_NAME FROM information_schema.KEY_COLUMN_USAGE \
             WHERE TABLE_SCHEMA = DATABASE() AND REFERENCED_TABLE_NAME IS NOT NULL",
        )?
        .into_iter()
        .map(txt)
        .collect();
    for sql in [
        "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() \
         AND TABLE_COMMENT LIKE '%FOREIGN KEY%'",
        "SELECT TABLE_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() \
         AND (COLUMN_COMMENT LIKE '%FOREIGN KEY%' OR COLUMN_DEFAULT LIKE '%FOREIGN KEY%' \
         OR GENERATION_EXPRESSION LIKE '%FOREIGN KEY%')",
        "SELECT TABLE_NAME FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = DATABASE() \
         AND INDEX_COMMENT LIKE '%FOREIGN KEY%'",
        "SELECT TABLE_NAME FROM information_schema.CHECK_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = DATABASE() \
         AND CHECK_CLAUSE LIKE '%FOREIGN KEY%'",
        "SELECT TABLE_NAME FROM information_schema.PARTITIONS WHERE TABLE_SCHEMA = DATABASE() \
         AND PARTITION_COMMENT LIKE '%FOREIGN KEY%'",
    ] {
        fk_tables.extend(db.query::<B>(sql).unwrap_or_default().into_iter().map(txt));
    }
    let mut fk_tables: Vec<String> = fk_tables.into_iter().collect();
    fk_tables.sort();
    let mut create_table = HashMap::new();
    for t in fk_tables {
        let ddl: Option<(B, B)> = db.query_first(format!("SHOW CREATE TABLE `{}`", t.replace('`', "``")))?;
        if let Some((_, ddl)) = ddl {
            create_table.insert(t, txt(ddl));
        }
    }

    Ok(RawSchema { version, tables, collation_charsets, columns, indexes, create_table })
}

#[cfg(test)]
mod tests {
    use super::{default_charset_init, split_multi_query};

    #[test]
    fn init_statements_split_like_magento() {
        assert_eq!(split_multi_query("SET NAMES utf8;"), ["SET NAMES utf8"]);
        assert_eq!(
            split_multi_query("SET NAMES latin1; SET @x = 'a;b';\n-- note;\nSET @y = 1"),
            ["SET NAMES latin1", "SET @x = 'a;b'", "-- note;\nSET @y = 1"]
        );
    }

    #[test]
    fn default_charset_follows_the_server_version() {
        assert!(default_charset_init(Some("11.4.4-MariaDB")).contains("utf8mb4"));
        assert!(default_charset_init(Some("10.3.39-MariaDB")).contains("utf8 "));
        assert!(default_charset_init(Some("8.0.36")).contains("utf8mb4"));
        assert!(default_charset_init(Some("5.7.44-log")).contains("utf8 "));
        assert!(default_charset_init(None).contains("utf8mb4"));
    }
}
