//! Running a `magequery sql` statement: read-only transaction, statement timeout, and
//! column-typed values.

use crate::model::DbConnection;

use super::{clean_err, connect};

/// `(column names, numeric column flags, rows)`.
pub(crate) type SelectRows = (Vec<String>, Vec<bool>, Vec<Vec<serde_json::Value>>);

/// Column names and rows of one read-only statement. The statement runs inside
/// `START TRANSACTION READ ONLY` with a server-side timeout, then the transaction is
/// rolled back. Every row is read: the row cap is the statement's own `LIMIT`.
pub(crate) fn run_select(
    conn: &DbConnection,
    sql: &str,
    timeout_secs: u64,
) -> Result<SelectRows, String> {
    let mut c = connect(conn)?;
    let version: Option<String> = c.query_first("SELECT VERSION()")?;
    let timeout = if version.unwrap_or_default().contains("MariaDB") {
        format!("SET SESSION max_statement_time = {timeout_secs}")
    } else {
        format!(
            "SET SESSION max_execution_time = {}",
            timeout_secs.saturating_mul(1000)
        )
    };
    if timeout_secs > 0 {
        c.query_drop(timeout)?;
    }
    // GROUP_CONCAT truncates silently at 1 KB by default; multiselect labels and
    // `website_ids` are built with it.
    c.query_drop("SET SESSION group_concat_max_len = 1048576")?;
    c.query_drop("START TRANSACTION READ ONLY")?;

    // Values are read raw (no `FromRow`), so this is the one place outside `query.rs`
    // that touches the connection directly.
    let out = (|| {
        use mysql::prelude::Queryable;
        let mut result = c.raw().query_iter(sql).map_err(clean_err)?;
        let cols: Vec<mysql::Column> = result.columns().as_ref().to_vec();
        let mut rows = Vec::new();
        for row in result.by_ref() {
            let row = row.map_err(clean_err)?;
            let values = row.unwrap();
            rows.push(
                values
                    .into_iter()
                    .zip(&cols)
                    .map(|(v, col)| to_json(v, col))
                    .collect(),
            );
        }
        let names = cols.iter().map(|col| col.name_str().into_owned()).collect();
        Ok((names, cols.iter().map(is_numeric).collect(), rows))
    })();
    // Nothing was written; rolling back just ends the transaction.
    let _ = c.query_drop("ROLLBACK");
    out
}

fn is_numeric(col: &mysql::Column) -> bool {
    use mysql::consts::ColumnType as T;
    matches!(
        col.column_type(),
        T::MYSQL_TYPE_TINY
            | T::MYSQL_TYPE_SHORT
            | T::MYSQL_TYPE_INT24
            | T::MYSQL_TYPE_LONG
            | T::MYSQL_TYPE_LONGLONG
            | T::MYSQL_TYPE_YEAR
            | T::MYSQL_TYPE_FLOAT
            | T::MYSQL_TYPE_DOUBLE
            | T::MYSQL_TYPE_DECIMAL
            | T::MYSQL_TYPE_NEWDECIMAL
    )
}

/// A text-protocol value as JSON, using the column type: integers and floats become
/// numbers, decimals stay strings (exact), NULL is null.
fn to_json(v: mysql::Value, col: &mysql::Column) -> serde_json::Value {
    use mysql::consts::ColumnType as T;
    use serde_json::Value as J;
    let text = match v {
        mysql::Value::NULL => return J::Null,
        mysql::Value::Bytes(b) => String::from_utf8_lossy(&b).into_owned(),
        mysql::Value::Int(i) => return J::from(i),
        mysql::Value::UInt(u) => return J::from(u),
        mysql::Value::Float(f) => return J::from(f64::from(f)),
        mysql::Value::Double(d) => return J::from(d),
        other => return J::String(other.as_sql(true).trim_matches('\'').to_string()),
    };
    match col.column_type() {
        T::MYSQL_TYPE_TINY
        | T::MYSQL_TYPE_SHORT
        | T::MYSQL_TYPE_INT24
        | T::MYSQL_TYPE_LONG
        | T::MYSQL_TYPE_LONGLONG
        | T::MYSQL_TYPE_YEAR => text
            .parse::<i64>()
            .map(J::from)
            .or_else(|_| text.parse::<u64>().map(J::from))
            .unwrap_or(J::String(text)),
        T::MYSQL_TYPE_FLOAT | T::MYSQL_TYPE_DOUBLE => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or(J::String(text), J::Number),
        _ => J::String(text),
    }
}
