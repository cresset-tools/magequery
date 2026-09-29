//! `magequery sql`: virtual-table descriptions, expansion, and (live DB) execution.

use super::super::*;
use crate::sql::{self as vsql, EavColumn, OptionLabels, VirtualTable};

impl Magento {
    /// `env.php`'s table prefix, or none when there is no readable `env.php` (expansion
    /// of the non-EAV tables and `--describe` work without one).
    fn sql_prefix(&self) -> String {
        self.db_config().map(|c| c.table_prefix).unwrap_or_default()
    }

    /// Every virtual table `magequery sql` offers, with its columns. Base columns come
    /// from `db_schema.xml`; attribute columns need the database and are left out (with a
    /// `warning`) when it is unreachable.
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
        let note_of = |name: &str| {
            vt.notes
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, note)| squash(note))
        };
        let fixed = vt.columns.iter().map(|c| SqlColumn {
            name: c.name.to_string(),
            col_type: None,
            added: true,
            source: squash(&c.expr.replace("{p}", prefix)),
            note: Some(squash(c.note)),
            label: None,
        });
        let base = self
            .table(vt.base)
            .map(|t| t.columns)
            .unwrap_or_default()
            .into_iter()
            .map(|c| SqlColumn {
                note: note_of(&c.name),
                source: format!("{}.{}", vt.base, c.name),
                name: c.name,
                col_type: Some(c.col_type),
                added: false,
                label: None,
            });
        let (attrs, warning) = match vt.eav.map(|spec| self.sql_eav_columns(spec.entity_type)) {
            None => (Vec::new(), None),
            Some(Ok(attrs)) => (attrs, None),
            Some(Err(e)) => (
                Vec::new(),
                Some(format!(
                    "attribute columns need the database ({e}); attributes not listed"
                )),
            ),
        };
        let mut attr_cols = Vec::new();
        for a in attrs
            .iter()
            .filter(|a| !vt.columns.iter().any(|c| c.name == a.code))
        {
            let multi_note = || {
                format!(
                    "multiselect: comma-separated option ids as stored, e.g. '12,15'; filter \
                     with FIND_IN_SET('12', {c}), or read labels from {c}_labels",
                    c = a.code
                )
            };
            attr_cols.push(SqlColumn {
                note: note_of(&a.code)
                    .or_else(|| (a.options == OptionLabels::Multi).then(multi_note)),
                source: format!("{} (attribute {})", a.value_table, a.attribute_id),
                name: a.code.clone(),
                col_type: Some(a.backend_type.clone()),
                added: true,
                label: a.label.clone(),
            });
            if let Some(name) = a.options.column(&a.code) {
                let note = (a.options == OptionLabels::Multi).then(|| {
                    format!(
                        "JSON array of the option labels in admin order, e.g. \
                         [\"Cotton\",\"Linen\"]; NULL when none is set. Filter with \
                         JSON_CONTAINS({name}, '\"Cotton\"'). Labels follow --store, so a label \
                         filter matches one language: filter on the ids to match any. A string \
                         in --json output"
                    )
                });
                attr_cols.push(SqlColumn {
                    note,
                    source: format!("eav_attribute_option_value of {}", a.code),
                    name,
                    col_type: Some("varchar".to_string()),
                    added: true,
                    label: None,
                });
            }
        }

        // Fixed added columns, then every noted column in note order (the ones people
        // get wrong), then the rest.
        let mut rest: Vec<SqlColumn> = base.chain(attr_cols).collect();
        let mut columns: Vec<SqlColumn> = fixed.collect();
        for (n, _) in vt.notes {
            if let Some(i) = rest.iter().position(|c| c.name == *n) {
                columns.push(rest.remove(i));
            }
        }
        columns.extend(rest);
        SqlTable {
            name: vt.name.to_string(),
            grain: vt.grain.to_string(),
            base_table: format!("{prefix}{}", vt.base),
            filter: vt.filter.map(str::to_string),
            columns,
            warning,
        }
    }

    /// The non-static attributes of an entity type as virtual columns. Live DB.
    fn sql_eav_columns(&self, entity_type: &str) -> Result<Vec<EavColumn>> {
        #[cfg(feature = "db")]
        {
            Ok(self
                .eav_attributes(Some(entity_type))?
                .into_iter()
                .filter_map(|a| {
                    let value_table = a.value_table?;
                    // Labels come from the option table only when no custom source
                    // model supplies them (status, visibility, … have their own).
                    let table_options = a.source_model.as_ref().is_none_or(|m| {
                        m.to_string().ends_with("Entity\\Attribute\\Source\\Table")
                    });
                    let options = match a.frontend_input.as_deref() {
                        Some("select") if table_options => OptionLabels::Single,
                        Some("multiselect") if table_options => OptionLabels::Multi,
                        _ => OptionLabels::None,
                    };
                    Some(EavColumn {
                        code: a.code,
                        attribute_id: a.attribute_id,
                        backend_type: a.backend_type,
                        value_table,
                        label: a.label,
                        options,
                    })
                })
                .collect())
        }
        #[cfg(not(feature = "db"))]
        {
            let _ = entity_type;
            Err(Error::Db("built without the `db` feature".to_string()))
        }
    }

    /// The expansion context for `stmt`: attribute metadata for the EAV tables it uses,
    /// and the `--store` view they resolve for. Touches the database only when needed.
    fn sql_context(&self, stmt: &str, store: Option<&str>) -> Result<vsql::Context> {
        let entities = vsql::eav_entities(stmt)?;
        if entities.is_empty() {
            if store.is_some() {
                return Err(Error::Sql(
                    "--store sets which store view's attribute values `products` shows; to \
                     filter orders by store, use WHERE store_code = '…'"
                        .to_string(),
                ));
            }
            return Ok(vsql::Context::default());
        }
        let mut ctx = vsql::Context {
            store_id: match store {
                Some(s) => self.sql_store_id(s)?,
                None => 0,
            },
            ..Default::default()
        };
        for e in entities {
            ctx.attributes
                .insert(e.to_string(), self.sql_eav_columns(e)?);
        }
        Ok(ctx)
    }

    /// A store view code (or id) → its id; `admin` / `0` is the default scope.
    fn sql_store_id(&self, store: &str) -> Result<u32> {
        if store == "admin" || store == "0" {
            return Ok(0);
        }
        #[cfg(feature = "db")]
        {
            let views: Vec<StoreViewNode> = self
                .store_tree()?
                .websites
                .into_iter()
                .flat_map(|w| w.groups)
                .flat_map(|g| g.views)
                .collect();
            views
                .iter()
                .find(|v| v.code == store || v.id.to_string() == store)
                .map(|v| v.id)
                .ok_or_else(|| {
                    let codes: Vec<&str> = views.iter().map(|v| v.code.as_str()).collect();
                    Error::Sql(format!(
                        "no store view `{store}`; there are: admin, {}",
                        codes.join(", ")
                    ))
                })
        }
        #[cfg(not(feature = "db"))]
        Err(Error::Db("built without the `db` feature".to_string()))
    }

    /// Check `stmt` is a single read statement and expand the virtual tables it names —
    /// exactly what [`Magento::sql_run`] would send. `limit` caps the rows unless the
    /// statement has its own top-level `LIMIT`; `store` is the store view attribute values
    /// resolve for. Needs the database only for EAV tables (`products`).
    pub fn sql_expand(
        &self,
        stmt: &str,
        limit: Option<u64>,
        store: Option<&str>,
    ) -> Result<SqlExpansion> {
        let ctx = self.sql_context(stmt, store)?;
        let x = vsql::expand(stmt, &self.sql_prefix(), limit, &ctx)?;
        Ok(SqlExpansion {
            sql: x.sql,
            tables: x.tables.iter().map(|t| t.to_string()).collect(),
            cap: x.cap,
        })
    }

    /// Run `stmt` read-only against the `env.php` database, after expansion. The server
    /// aborts it after `timeout_secs` (0 = the server's own limit). Live DB.
    #[cfg(feature = "db")]
    pub fn sql_run(
        &self,
        stmt: &str,
        limit: Option<u64>,
        timeout_secs: u64,
        store: Option<&str>,
    ) -> Result<SqlResult> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let ctx = self.sql_context(stmt, store)?;
        let x = vsql::expand(stmt, &cfg.table_prefix, limit, &ctx)?;
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
