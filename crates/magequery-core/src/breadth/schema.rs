//! Static schema indexes.

use super::*;

// ---------- declarative schema (db_schema.xml) ----------

pub(crate) struct SchemaIndex {
    tables: HashMap<String, DbTable>,
}

impl SchemaIndex {
    pub fn build(modules: &[Module], vfs: &Vfs) -> Self {
        let mut tables: HashMap<String, DbTable> = HashMap::new();
        for (i, path, raw_tables) in read_parse(modules, vfs, Area::Global, "db_schema.xml", parse::db_schema_xml) {
            let module = &modules[i].name;
            for rt in raw_tables {
                // A disabled table is dropped entirely (a later module can re-add it).
                if rt.disabled {
                    tables.remove(&rt.name);
                    continue;
                }
                let entry = tables.entry(rt.name.clone()).or_insert_with(|| DbTable {
                    name: rt.name.clone(),
                    engine: None,
                    resource: None,
                    comment: None,
                    columns: Vec::new(),
                    constraints: Vec::new(),
                    indexes: Vec::new(),
                    source: Source { module: module.clone(), file: path.clone(), line: rt.line, area: Area::Global },
                });
                // Table-level attributes are last-wins; `source` keeps the first declaration.
                if rt.engine.is_some() {
                    entry.engine = rt.engine;
                }
                if rt.resource.is_some() {
                    entry.resource = rt.resource;
                }
                if rt.comment.is_some() {
                    entry.comment = rt.comment;
                }
                for rc in rt.columns {
                    merge_column(entry, rc, module, &path);
                }
                for rcon in rt.constraints {
                    merge_constraint(entry, rcon, module, &path);
                }
                for ri in rt.indexes {
                    merge_index(entry, ri, module, &path);
                }
            }
        }
        Self { tables }
    }

    /// One table by exact name.
    pub fn table(&self, name: &str) -> Option<DbTable> {
        self.tables.get(name).cloned()
    }

    /// All tables whose name contains `filter` (case-insensitive; all when `None`),
    /// sorted by name.
    pub fn tables(&self, filter: Option<&str>) -> Vec<DbTable> {
        let needle = filter.map(str::to_lowercase);
        let mut v: Vec<DbTable> = self
            .tables
            .values()
            .filter(|t| needle.as_ref().is_none_or(|n| t.name.to_lowercase().contains(n)))
            .cloned()
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

fn merge_column(t: &mut DbTable, rc: parse::RawColumn, module: &ModuleName, path: &Path) {
    if rc.disabled {
        t.columns.retain(|c| c.name != rc.name);
        return;
    }
    let source = Source { module: module.clone(), file: path.to_path_buf(), line: rc.line, area: Area::Global };
    let col = DbColumn {
        name: rc.name.clone(),
        col_type: rc.col_type,
        nullable: rc.nullable,
        unsigned: rc.unsigned,
        length: rc.length,
        precision: rc.precision,
        scale: rc.scale,
        default: rc.default,
        identity: rc.identity,
        comment: rc.comment,
        source,
    };
    match t.columns.iter_mut().find(|c| c.name == rc.name) {
        Some(existing) => *existing = col,
        None => t.columns.push(col),
    }
}

fn merge_constraint(t: &mut DbTable, rc: parse::RawConstraint, module: &ModuleName, path: &Path) {
    if rc.disabled {
        t.constraints.retain(|c| c.id != rc.id);
        return;
    }
    let source = Source { module: module.clone(), file: path.to_path_buf(), line: rc.line, area: Area::Global };
    let con = DbConstraint {
        id: rc.id.clone(),
        kind: rc.kind,
        columns: rc.columns,
        reference_table: rc.reference_table,
        reference_column: rc.reference_column,
        on_delete: rc.on_delete,
        source,
    };
    match t.constraints.iter_mut().find(|c| c.id == rc.id) {
        Some(existing) => *existing = con,
        None => t.constraints.push(con),
    }
}

fn merge_index(t: &mut DbTable, ri: parse::RawIndex, module: &ModuleName, path: &Path) {
    if ri.disabled {
        t.indexes.retain(|i| i.id != ri.id);
        return;
    }
    let source = Source { module: module.clone(), file: path.to_path_buf(), line: ri.line, area: Area::Global };
    let idx = DbIndex { id: ri.id.clone(), index_type: ri.index_type, columns: ri.columns, source };
    match t.indexes.iter_mut().find(|i| i.id == ri.id) {
        Some(existing) => *existing = idx,
        None => t.indexes.push(idx),
    }
}

// ---------- catalog attribute groups (etc/catalog_attributes.xml) ----------

pub(crate) struct CatalogAttrIndex {
    groups: HashMap<String, CatalogAttributeGroup>,
}

impl CatalogAttrIndex {
    pub fn build(modules: &[Module], vfs: &Vfs) -> Self {
        let mut groups: HashMap<String, CatalogAttributeGroup> = HashMap::new();
        for (i, path, raws) in
            read_parse(modules, vfs, Area::Global, "catalog_attributes.xml", parse::catalog_attributes_xml)
        {
            let module = &modules[i].name;
            for r in raws {
                let entry = groups
                    .entry(r.group.clone())
                    .or_insert_with(|| CatalogAttributeGroup { name: r.group, attributes: Vec::new() });
                if !entry.attributes.iter().any(|a| a.name == r.attribute) {
                    entry.attributes.push(CatalogAttribute {
                        name: r.attribute,
                        source: Source {
                            module: module.clone(),
                            file: path.clone(),
                            line: r.line,
                            area: Area::Global,
                        },
                    });
                }
            }
        }
        for g in groups.values_mut() {
            g.attributes.sort_by(|a, b| a.name.cmp(&b.name));
        }
        Self { groups }
    }

    pub fn group(&self, name: &str) -> Option<CatalogAttributeGroup> {
        self.groups.get(name).cloned()
    }

    /// All groups, sorted by name.
    pub fn groups(&self) -> Vec<CatalogAttributeGroup> {
        let mut v: Vec<CatalogAttributeGroup> = self.groups.values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}
