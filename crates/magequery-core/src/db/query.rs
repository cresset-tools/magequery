//! The one way `db/*.rs` runs a query. [`Db`] mirrors the `mysql` `Queryable` methods the
//! fetchers use, but converts every row with `from_row_opt`, never the panicking
//! `from_row`: a NULL in a non-`Option` field, or a value of the wrong type, used to take
//! the whole command down (exit 101) over one odd row.
//!
//! * List queries ([`Db::query`]/[`Db::exec`]) **skip** a row that doesn't convert and
//!   record a warning naming the table and the NULL column(s) it held.
//! * Single-row lookups ([`Db::query_first`]/[`Db::exec_first`]) can't skip: a bad row is
//!   a clean `Err`.
//! * Wide rows read by index go through [`RowExt::col`], which reads NULL as `None` and a
//!   mismatched value as `None` plus a warning.
//!
//! Warnings land in a per-thread sink that the [`Magento`](crate::Magento) handle drains
//! into its [`diagnostics`](crate::Magento::diagnostics) right after each live query — the
//! fetchers stay plain functions returning `Result<T, String>`.
//!
//! Nothing outside this module (and `sql.rs`'s value-level reader) imports `Queryable`,
//! so no other path can reach `from_row`.

use std::cell::RefCell;

use mysql::prelude::{FromRow, FromValue, Queryable};
use mysql::{Params, Row, Value};

use super::clean_err;

thread_local! {
    static WARNINGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn warn(message: String) {
    WARNINGS.with(|w| w.borrow_mut().push(message));
}

/// Warnings recorded on this thread since the last call.
pub(crate) fn take_warnings() -> Vec<String> {
    WARNINGS.with(|w| std::mem::take(&mut *w.borrow_mut()))
}

/// A connection whose queries never panic on a row that doesn't fit its Rust type.
pub(crate) struct Db(mysql::Conn);

impl Db {
    pub(super) fn new(conn: mysql::Conn) -> Self {
        Db(conn)
    }

    /// The raw connection, for `sql.rs`, which reads values without `FromRow`.
    pub(super) fn raw(&mut self) -> &mut mysql::Conn {
        &mut self.0
    }

    /// Every row of a text query; rows that don't convert are skipped with a warning.
    pub(crate) fn query<T: FromRow>(&mut self, sql: impl AsRef<str>) -> Result<Vec<T>, String> {
        let sql = sql.as_ref();
        let rows = self.0.query_iter(sql).map_err(clean_err)?;
        collect(sql, rows)
    }

    /// Every row of a prepared statement; rows that don't convert are skipped with a
    /// warning.
    pub(crate) fn exec<T: FromRow>(
        &mut self,
        sql: impl AsRef<str>,
        params: impl Into<Params>,
    ) -> Result<Vec<T>, String> {
        let sql = sql.as_ref();
        let rows = self.0.exec_iter(sql, params).map_err(clean_err)?;
        collect(sql, rows)
    }

    /// The first row of a text query; one that doesn't convert is an `Err`.
    pub(crate) fn query_first<T: FromRow>(
        &mut self,
        sql: impl AsRef<str>,
    ) -> Result<Option<T>, String> {
        let sql = sql.as_ref();
        let mut rows = self.0.query_iter(sql).map_err(clean_err)?;
        let first = rows.next().transpose().map_err(clean_err)?;
        first.map(|row| convert_one(sql, row)).transpose()
    }

    /// The first row of a prepared statement; one that doesn't convert is an `Err`.
    pub(crate) fn exec_first<T: FromRow>(
        &mut self,
        sql: impl AsRef<str>,
        params: impl Into<Params>,
    ) -> Result<Option<T>, String> {
        let sql = sql.as_ref();
        let mut rows = self.0.exec_iter(sql, params).map_err(clean_err)?;
        let first = rows.next().transpose().map_err(clean_err)?;
        first.map(|row| convert_one(sql, row)).transpose()
    }

    /// A statement whose result is ignored.
    pub(crate) fn query_drop(&mut self, sql: impl AsRef<str>) -> Result<(), String> {
        self.0.query_drop(sql.as_ref()).map_err(clean_err)
    }
}

fn collect<T: FromRow, P: mysql::prelude::Protocol>(
    sql: &str,
    rows: mysql::QueryResult<'_, '_, '_, P>,
) -> Result<Vec<T>, String> {
    let (out, skipped) = convert_all(rows.map(|r| r.map_err(clean_err)))?;
    if let Some((count, first)) = skipped {
        warn(format!(
            "skipped {count} row(s) of `{}` that don't fit the expected shape ({})",
            table_of(sql),
            describe(&first)
        ));
    }
    Ok(out)
}

/// How many rows were skipped, and the first of them (for the warning).
type Skipped = Option<(usize, Row)>;

/// Convert every row, keeping the ones that fit. Returns the converted rows plus, when any
/// were skipped, the skip count and the first skipped row.
fn convert_all<T: FromRow>(
    rows: impl Iterator<Item = Result<Row, String>>,
) -> Result<(Vec<T>, Skipped), String> {
    let mut out = Vec::new();
    let mut skipped: Skipped = None;
    for row in rows {
        match mysql::from_row_opt::<T>(row?) {
            Ok(v) => out.push(v),
            Err(mysql::FromRowError(row)) => match &mut skipped {
                Some((n, _)) => *n += 1,
                None => skipped = Some((1, row)),
            },
        }
    }
    Ok((out, skipped))
}

fn convert_one<T: FromRow>(sql: &str, row: Row) -> Result<T, String> {
    mysql::from_row_opt::<T>(row).map_err(|mysql::FromRowError(row)| {
        format!(
            "unexpected row shape from `{}` ({})",
            table_of(sql),
            describe(&row)
        )
    })
}

/// Why a row probably didn't convert. `FromRowError` doesn't say which column failed, so
/// name the NULLs (the usual cause); without any, it was a type or arity mismatch.
fn describe(row: &Row) -> String {
    let nulls: Vec<String> = row
        .columns_ref()
        .iter()
        .enumerate()
        .filter(|(i, _)| matches!(row.as_ref(*i), Some(Value::NULL)))
        .map(|(_, col)| column_label(col))
        .collect();
    if nulls.is_empty() {
        "a value of unexpected type".to_string()
    } else {
        format!("NULL in {}", nulls.join(", "))
    }
}

fn column_label(col: &mysql::Column) -> String {
    let table = col.org_table_str();
    let name = col.org_name_str();
    let name = if name.is_empty() { col.name_str() } else { name };
    if table.is_empty() {
        format!("`{name}`")
    } else {
        format!("`{table}.{name}`")
    }
}

/// The first table a statement reads, for messages: the identifier after `FROM`.
fn table_of(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    upper
        .find(" FROM ")
        .and_then(|at| sql[at + 6..].split_whitespace().next())
        .map(|t| t.trim_matches(|c| c == '`' || c == '(' || c == ')').to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "query".to_string())
}

/// Index-based reads of a wide [`Row`] that never panic.
pub(crate) trait RowExt {
    /// Column `i` as `T`: NULL (or an already-taken column) is `None`; a value that
    /// doesn't fit `T` is `None` plus a warning.
    fn col<T: FromValue>(&mut self, i: usize) -> Option<T>;
}

impl RowExt for Row {
    fn col<T: FromValue>(&mut self, i: usize) -> Option<T> {
        match self.take_opt::<Option<T>, _>(i)? {
            Ok(v) => v,
            Err(_) => {
                let col = self.columns_ref().get(i).map(column_label);
                warn(format!(
                    "ignored a value of unexpected type in {}",
                    col.unwrap_or_else(|| format!("column {i}"))
                ));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql::consts::ColumnType;
    use mysql::Column;

    fn row(values: Vec<Value>, names: &[&str]) -> Row {
        let cols: Vec<Column> = names
            .iter()
            .map(|n| {
                Column::new(ColumnType::MYSQL_TYPE_VAR_STRING)
                    .with_name(n.as_bytes())
                    .with_org_name(n.as_bytes())
                    .with_org_table(b"tax_calculation_rate")
            })
            .collect();
        mysql_common::row::new_row(values, cols.into())
    }

    fn ok(r: Row) -> Result<Row, String> {
        Ok(r)
    }

    #[test]
    fn a_null_in_a_required_field_skips_the_row_not_the_query() {
        let rows = vec![
            ok(row(vec![Value::Int(1), Value::Bytes(b"NL".to_vec())], &["id", "code"])),
            ok(row(vec![Value::Int(2), Value::NULL], &["id", "code"])),
            ok(row(vec![Value::Int(3), Value::NULL], &["id", "code"])),
            ok(row(vec![Value::Int(4), Value::Bytes(b"BE".to_vec())], &["id", "code"])),
        ];
        let (out, skipped) = convert_all::<(u32, String)>(rows.into_iter()).unwrap();
        assert_eq!(out, vec![(1, "NL".to_string()), (4, "BE".to_string())]);
        let (n, first) = skipped.unwrap();
        assert_eq!(n, 2);
        assert_eq!(describe(&first), "NULL in `tax_calculation_rate.code`");
    }

    #[test]
    fn an_option_field_takes_the_null() {
        let rows = vec![ok(row(vec![Value::Int(2), Value::NULL], &["id", "code"]))];
        let (out, skipped) = convert_all::<(u32, Option<String>)>(rows.into_iter()).unwrap();
        assert_eq!(out, vec![(2, None)]);
        assert!(skipped.is_none());
    }

    #[test]
    fn a_type_mismatch_is_skipped_and_described() {
        let rows = vec![ok(row(vec![Value::Bytes(b"abc".to_vec())], &["n"]))];
        let (out, skipped) = convert_all::<u64>(rows.into_iter()).unwrap();
        assert!(out.is_empty());
        assert_eq!(describe(&skipped.unwrap().1), "a value of unexpected type");
    }

    #[test]
    fn a_driver_error_still_fails_the_query() {
        let rows = vec![Err::<Row, _>("lost connection".to_string())];
        assert!(convert_all::<u64>(rows.into_iter()).is_err());
    }

    #[test]
    fn a_single_row_that_does_not_convert_is_an_error() {
        // `SELECT MAX(sequence_value) FROM sequence_order_1` on an empty table.
        let r = row(vec![Value::NULL], &["MAX(sequence_value)"]);
        let err = convert_one::<u64>("SELECT MAX(sequence_value) FROM sequence_order_1", r)
            .unwrap_err();
        assert!(err.contains("`sequence_order_1`"), "{err}");
        let r = row(vec![Value::NULL], &["MAX(sequence_value)"]);
        assert_eq!(convert_one::<Option<u64>>("SELECT 1", r).unwrap(), None);
    }

    #[test]
    fn wide_row_reads_never_panic() {
        take_warnings();
        let mut r = row(
            vec![Value::NULL, Value::Bytes(b"x1".to_vec()), Value::Bytes(b"7".to_vec())],
            &["a", "b", "c"],
        );
        assert_eq!(r.col::<String>(0), None);
        assert_eq!(r.col::<i64>(1), None);
        assert_eq!(r.col::<i64>(2), Some(7));
        assert_eq!(r.col::<i64>(2), None, "a taken column reads as None");
        assert_eq!(r.col::<i64>(9), None, "out of range reads as None");
        let w = take_warnings();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("`tax_calculation_rate.b`"), "{w:?}");
    }

    #[test]
    fn table_names_come_from_the_from_clause() {
        assert_eq!(table_of("SELECT a FROM mg_store WHERE x"), "mg_store");
        assert_eq!(table_of("SELECT a, b\n FROM `quote` q"), "quote");
        assert_eq!(table_of("SELECT VERSION()"), "query");
    }
}
