//! Data types for `magequery sql`.

/// One virtual table as `--describe` shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SqlTable {
    pub name: String,
    /// What one row is, in words.
    pub grain: String,
    /// The real table every row comes from, with `env.php`'s prefix applied.
    pub base_table: String,
    /// The `WHERE` the virtual table applies to its base table, if any.
    pub filter: Option<String>,
    /// Added columns first, then the base table's columns (noted ones before the rest).
    pub columns: Vec<SqlColumn>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SqlColumn {
    pub name: String,
    /// `db_schema.xml`'s type for a base column; `None` for an added (computed) one.
    pub col_type: Option<String>,
    /// Added by the virtual table (vs. passed through from the base table).
    pub added: bool,
    /// Where the value comes from: `sales_order.base_grand_total`, or the SQL expression
    /// of an added column.
    pub source: String,
    /// How to use it correctly: currency, timezone, what not to sum.
    pub note: Option<String>,
}

/// A statement after virtual-table expansion: exactly what `sql_run` sends.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SqlExpansion {
    pub sql: String,
    /// The virtual tables that were expanded.
    pub tables: Vec<String>,
    /// The row cap in force; `None` when the statement has its own top-level `LIMIT` or
    /// the cap was lifted.
    pub cap: Option<u64>,
}

/// The rows a `magequery sql` statement returned.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SqlResult {
    pub columns: Vec<String>,
    /// Per column: a numeric type (decimals included), for right-aligning the table view.
    #[serde(skip)]
    pub numeric: Vec<bool>,
    /// One JSON value per column: integers and floats as numbers, NULL as null, and
    /// everything else (decimals included, to keep their precision) as strings.
    pub rows: Vec<Vec<serde_json::Value>>,
    /// More rows existed than the cap; only the first `cap` are returned.
    pub truncated: bool,
    pub cap: Option<u64>,
    pub elapsed_ms: u128,
}
