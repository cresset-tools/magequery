//! Parsers for the schema domain.

use super::*;

// ---------- declarative schema (db_schema.xml) ----------

pub(crate) struct RawColumn {
    pub name: String,
    pub col_type: String,
    pub nullable: bool,
    pub unsigned: bool,
    pub length: Option<String>,
    pub precision: Option<String>,
    pub scale: Option<String>,
    pub default: Option<String>,
    pub identity: bool,
    pub comment: Option<String>,
    pub disabled: bool,
    pub line: u32,
}

pub(crate) struct RawConstraint {
    pub id: String,
    pub kind: String,
    pub columns: Vec<String>,
    pub reference_table: Option<String>,
    pub reference_column: Option<String>,
    pub on_delete: Option<String>,
    pub disabled: bool,
    pub line: u32,
}

pub(crate) struct RawIndex {
    pub id: String,
    pub index_type: String,
    pub columns: Vec<String>,
    pub disabled: bool,
    pub line: u32,
}

pub(crate) struct RawTable {
    pub name: String,
    pub engine: Option<String>,
    pub resource: Option<String>,
    pub comment: Option<String>,
    pub disabled: bool,
    pub columns: Vec<RawColumn>,
    pub constraints: Vec<RawConstraint>,
    pub indexes: Vec<RawIndex>,
    pub line: u32,
}

/// Parse a module's `db_schema.xml` into raw tables (each carrying line numbers; module/area
/// provenance is attached at merge). A `<column>` directly under `<table>` is a definition
/// (carries an `xsi:type`); a `<column>` inside a `<constraint>`/`<index>` is a column
/// *reference* (only `name`), so we route it by the current context.
pub(crate) fn db_schema_xml(xml: &str) -> Vec<RawTable> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut tables: Vec<RawTable> = Vec::new();
    // Index into the current table's `constraints`/`indexes` when inside one (for child columns).
    let mut in_constraint: Option<usize> = None;
    let mut in_index: Option<usize> = None;

    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                schema_element(&e, line, false, &mut tables, &mut in_constraint, &mut in_index)
            }
            Ok(Event::Empty(e)) => {
                schema_element(&e, line, true, &mut tables, &mut in_constraint, &mut in_index)
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                // A self-closing (`Empty`) constraint/index never opened a context, so only
                // the matching `End` of a real `Start` clears it.
                b"constraint" => in_constraint = None,
                b"index" => in_index = None,
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }
    tables
}

/// Handle one `Start`/`Empty` schema element. `is_empty` marks a self-closing element, which
/// must not open a child context (it has no matching `End`).
fn schema_element(
    e: &BytesStart,
    line: u32,
    is_empty: bool,
    tables: &mut Vec<RawTable>,
    in_constraint: &mut Option<usize>,
    in_index: &mut Option<usize>,
) {
    match local_name(e).as_str() {
        "table" => tables.push(RawTable {
            name: attr(e, b"name").unwrap_or_default(),
            engine: attr(e, b"engine"),
            resource: attr(e, b"resource"),
            comment: attr(e, b"comment"),
            disabled: attr_true(e, b"disabled"),
            columns: Vec::new(),
            constraints: Vec::new(),
            indexes: Vec::new(),
            line,
        }),
        "column" => {
            let Some(t) = tables.last_mut() else { return };
            if let Some(ci) = *in_index {
                if let Some(name) = attr(e, b"name") {
                    t.indexes[ci].columns.push(name);
                }
            } else if let Some(ci) = *in_constraint {
                if let Some(name) = attr(e, b"name") {
                    t.constraints[ci].columns.push(name);
                }
            } else {
                t.columns.push(RawColumn {
                    name: attr(e, b"name").unwrap_or_default(),
                    col_type: xsi_type(e).unwrap_or_default(),
                    nullable: attr_true(e, b"nullable"),
                    unsigned: attr_true(e, b"unsigned"),
                    length: attr(e, b"length"),
                    precision: attr(e, b"precision"),
                    scale: attr(e, b"scale"),
                    default: attr(e, b"default"),
                    identity: attr_true(e, b"identity"),
                    comment: attr(e, b"comment"),
                    disabled: attr_true(e, b"disabled"),
                    line,
                });
            }
        }
        "constraint" => {
            let Some(t) = tables.last_mut() else { return };
            // Foreign keys reference a single local column via the `column` attr.
            let columns = attr(e, b"column").into_iter().collect();
            t.constraints.push(RawConstraint {
                id: attr(e, b"referenceId").unwrap_or_default(),
                kind: xsi_type(e).unwrap_or_default(),
                columns,
                reference_table: attr(e, b"referenceTable"),
                reference_column: attr(e, b"referenceColumn"),
                on_delete: attr(e, b"onDelete"),
                disabled: attr_true(e, b"disabled"),
                line,
            });
            if !is_empty {
                *in_constraint = Some(t.constraints.len() - 1);
            }
        }
        "index" => {
            let Some(t) = tables.last_mut() else { return };
            t.indexes.push(RawIndex {
                id: attr(e, b"referenceId").unwrap_or_default(),
                index_type: attr(e, b"indexType").unwrap_or_else(|| "btree".into()),
                columns: Vec::new(),
                disabled: attr_true(e, b"disabled"),
                line,
            });
            if !is_empty {
                *in_index = Some(t.indexes.len() - 1);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod schema_tests {
    use super::db_schema_xml;

    const XML: &str = r#"<?xml version="1.0"?>
<schema xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
    <table name="store_group" resource="default" engine="innodb" comment="Store Groups">
        <column xsi:type="smallint" name="group_id" unsigned="true" nullable="false" identity="true"/>
        <column xsi:type="varchar" name="code" nullable="true" length="32"/>
        <column xsi:type="decimal" name="rate" scale="4" precision="12" nullable="true"/>
        <column xsi:type="int" name="legacy" disabled="true"/>
        <constraint xsi:type="primary" referenceId="PRIMARY">
            <column name="group_id"/>
        </constraint>
        <constraint xsi:type="foreign" referenceId="FK_GROUP_WEBSITE" table="store_group"
                    column="website_id" referenceTable="store_website" referenceColumn="website_id"
                    onDelete="CASCADE"/>
        <index referenceId="IDX_CODE" indexType="btree">
            <column name="code"/>
        </index>
    </table>
</schema>"#;

    #[test]
    fn parses_columns_constraints_indexes() {
        let tables = db_schema_xml(XML);
        assert_eq!(tables.len(), 1);
        let t = &tables[0];
        assert_eq!(t.name, "store_group");
        assert_eq!(t.engine.as_deref(), Some("innodb"));

        // Column definitions (the `disabled` one is still parsed; it's dropped at merge time).
        let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["group_id", "code", "rate", "legacy"]);
        let group_id = &t.columns[0];
        assert_eq!(group_id.col_type, "smallint");
        assert!(group_id.unsigned && group_id.identity && !group_id.nullable);
        assert_eq!(t.columns[1].length.as_deref(), Some("32"));
        assert_eq!((t.columns[2].precision.as_deref(), t.columns[2].scale.as_deref()), (Some("12"), Some("4")));
        assert!(t.columns[3].disabled);

        // A `<column>` inside a constraint/index is a *reference*, not a new column definition.
        assert_eq!(t.columns.len(), 4);

        // Primary (child column) vs foreign (self-closing, attrs only).
        let primary = t.constraints.iter().find(|c| c.id == "PRIMARY").unwrap();
        assert_eq!(primary.kind, "primary");
        assert_eq!(primary.columns, ["group_id"]);
        let fk = t.constraints.iter().find(|c| c.id == "FK_GROUP_WEBSITE").unwrap();
        assert_eq!(fk.kind, "foreign");
        assert_eq!(fk.columns, ["website_id"]); // from the `column` attr
        assert_eq!(fk.reference_table.as_deref(), Some("store_website"));
        assert_eq!(fk.on_delete.as_deref(), Some("CASCADE"));

        // The index's child column must NOT have leaked into the table columns.
        let idx = &t.indexes[0];
        assert_eq!(idx.id, "IDX_CODE");
        assert_eq!(idx.columns, ["code"]);
    }

    #[test]
    fn self_closing_foreign_does_not_capture_following_columns() {
        // A foreign constraint is `Empty` (no End); the next table's columns must stay separate.
        let xml = r#"<schema xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
            <table name="a">
                <constraint xsi:type="foreign" referenceId="FK" column="x" referenceTable="b" referenceColumn="y"/>
                <column xsi:type="int" name="after_fk"/>
            </table>
        </schema>"#;
        let tables = db_schema_xml(xml);
        let t = &tables[0];
        // `after_fk` is a real column def, not swallowed as the FK's reference column.
        assert_eq!(t.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["after_fk"]);
        assert_eq!(t.constraints[0].columns, ["x"]);
    }
}
