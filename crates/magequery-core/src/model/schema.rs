//! Data types for the schema domain.

/// A `(table, column)` pair in a schema drift report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TableColumn {
    pub table: String,
    pub column: String,
}

/// Presence-level drift between the declared schema (`db_schema.xml`) and the live
/// database — the schema half of "is this environment in sync with the code".
/// Runtime-managed tables (mview `*_cl` changelogs, `sequence_*`, setup bookkeeping) are
/// excluded from the undeclared side; `runtime_tables_skipped` counts them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SchemaDrift {
    /// Declared but absent live — what `setup:upgrade` would create.
    pub missing_tables: Vec<String>,
    pub missing_columns: Vec<TableColumn>,
    /// Live, no longer declared, but **whitelisted** (`db_schema_whitelist.json`) — the
    /// declarative system owns these, so `setup:upgrade` would DROP them.
    pub would_drop_tables: Vec<String>,
    pub would_drop_columns: Vec<TableColumn>,
    /// Declared but missing from every `db_schema_whitelist.json` — someone forgot
    /// `setup:db-declaration:generate-whitelist` (removals of these won't be applied).
    pub not_whitelisted_tables: Vec<String>,
    pub not_whitelisted_columns: Vec<TableColumn>,
    /// Live but declared nowhere and unwhitelisted: unmanaged (legacy install scripts,
    /// non-declarative modules) — declarative schema won't touch these.
    pub undeclared_tables: Vec<String>,
    pub undeclared_columns: Vec<TableColumn>,
    pub runtime_tables_skipped: usize,
}

impl SchemaDrift {
    pub fn is_clean(&self) -> bool {
        self.missing_tables.is_empty()
            && self.missing_columns.is_empty()
            && self.would_drop_tables.is_empty()
            && self.would_drop_columns.is_empty()
            && self.not_whitelisted_tables.is_empty()
            && self.not_whitelisted_columns.is_empty()
            && self.undeclared_tables.is_empty()
            && self.undeclared_columns.is_empty()
    }
}
