//! `magequery sql`: virtual-table descriptions, expansion, and (live DB) execution.

use super::super::*;
use crate::sql::{self as vsql, VirtualTable};

impl Magento {
    /// `env.php`'s table prefix, or none when there is no readable `env.php` (expansion
    /// and `--describe` work without one).
    fn sql_prefix(&self) -> String {
        self.db_config().map(|c| c.table_prefix).unwrap_or_default()
    }

    /// Every virtual table `magequery sql` offers, with its columns. Static: the base
    /// columns come from `db_schema.xml`, so this works without a database.
    pub fn sql_tables(&self) -> Vec<SqlTable> {
        let prefix = self.sql_prefix();
        vsql::VIRTUAL_TABLES
            .iter()
            .map(|vt| self.describe_virtual(vt, &prefix))
            .collect()
    }

    /// One virtual table by name (case-insensitive).
    pub fn sql_table(&self, name: &str) -> Option<SqlTable> {
        vsql::virtual_table(name).map(|vt| self.describe_virtual(vt, &self.sql_prefix()))
    }

    fn describe_virtual(&self, vt: &VirtualTable, prefix: &str) -> SqlTable {
        let mut columns: Vec<SqlColumn> = vt
            .columns
            .iter()
            .map(|c| SqlColumn {
                name: c.name.to_string(),
                col_type: None,
                added: true,
                source: c.expr.to_string(),
                note: Some(squash(c.note)),
            })
            .collect();
        let base: Vec<SqlColumn> = self
            .table(vt.base)
            .map(|t| t.columns)
            .unwrap_or_default()
            .into_iter()
            .map(|c| SqlColumn {
                note: vt
                    .notes
                    .iter()
                    .find(|(n, _)| *n == c.name)
                    .map(|(_, note)| squash(note)),
                source: format!("{}.{}", vt.base, c.name),
                name: c.name,
                col_type: Some(c.col_type),
                added: false,
            })
            .collect();
        // Noted base columns (the ones people get wrong) before the rest, in note order.
        for (n, _) in vt.notes {
            if let Some(c) = base.iter().find(|c| c.name == *n) {
                columns.push(c.clone());
            }
        }
        columns.extend(base.into_iter().filter(|c| c.note.is_none()));
        SqlTable {
            name: vt.name.to_string(),
            grain: vt.grain.to_string(),
            base_table: format!("{prefix}{}", vt.base),
            filter: vt.filter.map(str::to_string),
            columns,
        }
    }

    /// Check `stmt` is a single read statement and expand the virtual tables it names —
    /// exactly what [`Magento::sql_run`] would send. `limit` caps the rows unless the
    /// statement has its own top-level `LIMIT`.
    pub fn sql_expand(&self, stmt: &str, limit: Option<u64>) -> Result<SqlExpansion> {
        let x = vsql::expand(stmt, &self.sql_prefix(), limit)?;
        Ok(SqlExpansion {
            sql: x.sql,
            tables: x.tables.iter().map(|t| t.to_string()).collect(),
            cap: x.cap,
        })
    }

    /// Run `stmt` read-only against the `env.php` database, after expansion. The server
    /// aborts it after `timeout_secs` (0 = the server's own limit). Live DB.
    #[cfg(feature = "db")]
    pub fn sql_run(&self, stmt: &str, limit: Option<u64>, timeout_secs: u64) -> Result<SqlResult> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let x = vsql::expand(stmt, &cfg.table_prefix, limit)?;
        let start = std::time::Instant::now();
        let (columns, numeric, mut rows) =
            crate::db::run_select(conn, &x.sql, timeout_secs).map_err(Error::Db)?;
        let elapsed_ms = start.elapsed().as_millis();
        let truncated = x.cap.is_some_and(|n| rows.len() as u64 > n);
        if let (true, Some(n)) = (truncated, x.cap) {
            rows.truncate(n as usize);
        }
        Ok(SqlResult {
            columns,
            numeric,
            rows,
            truncated,
            cap: x.cap,
            elapsed_ms,
        })
    }
}

/// Notes are written across source lines; collapse the continuation whitespace.
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
