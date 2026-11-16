//! Data types for the schema domain.

use super::*;

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

/// The answer to `bin/magento setup:db:status`: whether `setup:upgrade` has work to do.
/// One section per Magento validator, in the order Magento runs them; up to date only
/// when every section is empty.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DbStatus {
    /// `UpToDateDeclarativeSchema`: what the declarative-schema diff would change.
    pub schema_changes: Vec<SchemaChange>,
    /// `UpToDateSchema`: schema patches not in `patch_list` and not skipped by version.
    pub schema_patches: Vec<PendingPatch>,
    /// `UpToDateData`: data patches not in `patch_list` and not skipped by version.
    pub data_patches: Vec<PendingPatch>,
    /// `OldDbValidator`: `setup_module` versions that differ from `module.xml`.
    pub module_versions: Vec<ModuleVersionMismatch>,
    /// Places where the answer rests on a static reading Magento would get by running
    /// PHP (a patch `getVersion()` that isn't a literal). Each one can only make the
    /// status say "upgrade needed" when Magento might not — never the reverse.
    pub notes: Vec<String>,
    /// Checks that could not run (a missing `setup_module` table on an empty database, a
    /// query error). Magento counts a validator that throws as "upgrade required", and so
    /// does this.
    pub errors: Vec<String>,
}

impl DbStatus {
    pub fn is_up_to_date(&self) -> bool {
        self.errors.is_empty()
            && self.schema_changes.is_empty()
            && self.schema_patches.is_empty()
            && self.data_patches.is_empty()
            && self.module_versions.is_empty()
    }

    /// Why applying this upgrade needs maintenance mode, or nothing when it could run
    /// while the previous release keeps serving: every schema change is
    /// [`Online::Yes`] or [`Online::Slow`], and no PHP (patches, old-style setup scripts)
    /// is pending. Recurring setup scripts run on every upgrade and are not counted.
    pub fn maintenance_reasons(&self) -> Vec<String> {
        let mut out: Vec<String> = self.errors.iter().map(|e| format!("could not check: {e}")).collect();
        for c in self.schema_changes.iter().filter(|c| c.online == Online::No) {
            let target = c.element.as_ref().map_or_else(|| c.table.clone(), |e| format!("{}.{e}", c.table));
            out.push(format!("{} {target}: {}", c.operation, c.why));
        }
        let patches = self.schema_patches.len() + self.data_patches.len();
        if patches > 0 {
            out.push(format!("{patches} pending setup patch(es) run PHP"));
        }
        if !self.module_versions.is_empty() {
            out.push("a module's setup_version changed, so old-style setup scripts may run".into());
        }
        out
    }
}

/// A setup patch `setup:upgrade` would still apply.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PendingPatch {
    pub class: ClassName,
    pub module: ModuleName,
    pub file: std::path::PathBuf,
}

/// A module whose `setup_module` row disagrees with its `module.xml` `setup_version`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ModuleVersionMismatch {
    pub module: ModuleName,
    /// `schema` (`schema_version`) or `data` (`data_version`).
    pub kind: PatchKind,
    /// What `setup_module` records; `None` when there is no row or no value.
    pub current: Option<String>,
    /// The `module.xml` `setup_version`.
    pub required: String,
}

/// One change the declarative-schema diff would make, named the way Magento's own
/// `setup:db:status` details name it (`create_table`, `add_column`, `modify_column`, …).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SchemaChange {
    pub operation: String,
    pub table: String,
    /// The column / index / constraint name; `None` for table-level operations.
    pub element: Option<String>,
    /// What differs, e.g. `type: varchar(64) → varchar(32)`; empty when nothing more
    /// specific than the operation applies.
    pub detail: Vec<String>,
    /// Whether the change can be applied while the previous release keeps serving.
    pub online: Online,
    /// Why, in a few words.
    pub why: String,
    /// `information_schema.TABLES.TABLE_ROWS` of the live table (an estimate for InnoDB),
    /// when the change rebuilds, scans or copies it.
    pub rows: Option<u64>,
}

/// Whether a schema change can be applied with the previous release still serving.
///
/// Judged on two things: whether code written for the old schema keeps working on the
/// new one (the upgrade runs before the swap, so the new code never sees the old
/// schema), and whether the server applies the change without blocking writes. Anything
/// not shown to be safe is [`Online::No`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Online {
    /// Old code keeps working and the server changes only metadata, or drops an index.
    Yes,
    /// As `Yes`, but the server rebuilds or scans the table in place without blocking
    /// writes; the time grows with the table.
    Slow,
    /// Needs maintenance: old code may break on it, or writes block while the table is
    /// copied.
    No,
}

impl std::fmt::Display for Online {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Online::Yes => "online",
            Online::Slow => "online, slow",
            Online::No => "maintenance",
        })
    }
}
