//! "Breadth" indexes — events/observers, cron, routes, webapi. Each is a thin projection
//! of a per-module XML file, merged in load order (per-area for events/routes; global for
//! cron/webapi). Built lazily (on first query) so they don't slow the common commands.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use rayon::prelude::*;

use std::path::Path;

use crate::ids::{Area, ClassName, EventName, ModuleName};
use crate::model::{
    AclResource, CronJob, DbColumn, DbConstraint, DbIndex, DbTable, ExtendedType,
    ExtensionAttribute, ExtensionJoin, GqlArg, GqlField, GqlKind,
    GqlType, Indexer, MenuItem, Module,
    MqConsumer, MqHandler, MqPublisher, MqRoute, MqTopic, MqTopicRoute, MqVia,
    MviewSubscription, Observer, Route, SystemField, WebapiRoute,
};
use crate::parse;
use crate::source::Source;

const REAL_AREAS: [Area; 6] = [
    Area::Frontend,
    Area::Adminhtml,
    Area::Crontab,
    Area::WebapiRest,
    Area::WebapiSoap,
    Area::Graphql,
];

fn area_path(m: &Module, area: Area, file: &str) -> PathBuf {
    match area.dir() {
        Some(dir) => m.path.join("etc").join(dir).join(file),
        None => m.path.join("etc").join(file),
    }
}

/// Read + parse `etc/[<area>/]<file>` for every **enabled** module **in parallel**
/// (Magento only loads enabled modules' configuration), returning `(module index, path,
/// parsed)` for the files that exist, in module (load) order — so the caller merges
/// sequentially and deterministically. `rayon` preserves the collect order.
fn read_parse<T: Send>(
    modules: &[Module],
    area: Area,
    file: &str,
    parse: impl Fn(&str) -> T + Sync,
) -> Vec<(usize, PathBuf, T)> {
    let jobs: Vec<(usize, PathBuf)> = modules
        .iter()
        .enumerate()
        .filter(|(_, m)| m.enabled)
        .map(|(i, m)| (i, area_path(m, area, file)))
        .collect();
    let parsed: Vec<Option<T>> = jobs
        .par_iter()
        .map(|(_, p)| std::fs::read_to_string(p).ok().map(|t| parse(t.as_str())))
        .collect();
    jobs.into_iter()
        .zip(parsed)
        .filter_map(|((i, p), r)| r.map(|t| (i, p, t)))
        .collect()
}

// ---------- events / observers ----------

type EventMap = HashMap<EventName, HashMap<String, Observer>>;

pub(crate) struct EventIndex {
    by_area: HashMap<Area, EventMap>,
}

impl EventIndex {
    pub fn build(modules: &[Module]) -> Self {
        let mut global = EventMap::new();
        apply_events(&mut global, modules, Area::Global);
        let mut by_area = HashMap::new();
        for area in REAL_AREAS {
            let mut cfg = global.clone();
            apply_events(&mut cfg, modules, area);
            by_area.insert(area, cfg);
        }
        by_area.insert(Area::Global, global);
        Self { by_area }
    }

    fn config(&self, area: Area) -> &EventMap {
        self.by_area.get(&area).unwrap_or_else(|| &self.by_area[&Area::Global])
    }

    pub fn observers(&self, event: &EventName, area: Area) -> Vec<Observer> {
        let mut v: Vec<Observer> = self
            .config(area)
            .get(event)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// All events in `area` with their observer count, sorted by event name.
    pub fn events(&self, area: Area) -> Vec<(EventName, usize)> {
        let mut v: Vec<(EventName, usize)> =
            self.config(area).iter().map(|(e, m)| (e.clone(), m.len())).collect();
        v.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        v
    }
}

fn apply_events(out: &mut EventMap, modules: &[Module], area: Area) {
    for (i, path, observers) in read_parse(modules, area, "events.xml", parse::events_xml) {
        let module = &modules[i].name;
        for (event, obs) in observers {
            let source = Source { module: module.clone(), file: path.clone(), line: obs.line, area };
            let by_name = out.entry(event.clone()).or_default();
            match by_name.get_mut(&obs.name) {
                Some(existing) => {
                    existing.instance = obs.instance;
                    if let Some(d) = obs.disabled {
                        existing.disabled = d;
                    }
                    if let Some(s) = obs.shared {
                        existing.shared = s;
                    }
                    existing.source = source;
                }
                None => {
                    by_name.insert(
                        obs.name.clone(),
                        Observer {
                            event,
                            name: obs.name,
                            instance: obs.instance,
                            disabled: obs.disabled.unwrap_or(false),
                            shared: obs.shared.unwrap_or(true),
                            source,
                        },
                    );
                }
            }
        }
    }
}

// ---------- cron ----------

pub(crate) struct CronIndex {
    /// group -> (job name -> job)
    groups: HashMap<String, HashMap<String, CronJob>>,
}

impl CronIndex {
    pub fn build(modules: &[Module]) -> Self {
        let mut groups: HashMap<String, HashMap<String, CronJob>> = HashMap::new();
        for (i, path, jobs) in read_parse(modules, Area::Global, "crontab.xml", parse::crontab_xml) {
            let module = &modules[i].name;
            for job in jobs {
                let source =
                    Source { module: module.clone(), file: path.clone(), line: job.line, area: Area::Crontab };
                groups.entry(job.group.clone()).or_default().insert(
                    job.name.clone(),
                    CronJob {
                        group: job.group,
                        name: job.name,
                        instance: job.instance,
                        method: job.method,
                        schedule: job.schedule,
                        config_path: job.config_path,
                        source,
                    },
                );
            }
        }
        Self { groups }
    }

    /// Jobs, optionally restricted to one group, sorted by (group, name).
    pub fn jobs(&self, group: Option<&str>) -> Vec<CronJob> {
        let mut v: Vec<CronJob> = self
            .groups
            .iter()
            .filter(|(g, _)| group.is_none_or(|f| f == g.as_str()))
            .flat_map(|(_, m)| m.values().cloned())
            .collect();
        v.sort_by(|a, b| a.group.cmp(&b.group).then_with(|| a.name.cmp(&b.name)));
        v
    }
}

// ---------- routes ----------

pub(crate) struct RouteIndex {
    /// area -> ((router, route id) -> route)
    by_area: HashMap<Area, HashMap<(String, String), Route>>,
}

impl RouteIndex {
    pub fn build(modules: &[Module]) -> Self {
        let mut by_area = HashMap::new();
        for area in [Area::Global, Area::Frontend, Area::Adminhtml, Area::WebapiRest, Area::Graphql] {
            let mut map: HashMap<(String, String), Route> = HashMap::new();
            for (i, path, parsed) in read_parse(modules, area, "routes.xml", parse::routes_xml) {
                let module = &modules[i].name;
                for r in parsed {
                    let key = (r.router.clone(), r.id.clone());
                    let entry = map.entry(key).or_insert_with(|| Route {
                        area,
                        router: r.router.clone(),
                        id: r.id.clone(),
                        front_name: r.front_name.clone(),
                        modules: Vec::new(),
                        source: Source {
                            module: module.clone(),
                            file: path.clone(),
                            line: r.line,
                            area,
                        },
                    });
                    if !r.front_name.is_empty() {
                        entry.front_name = r.front_name;
                    }
                    for module in r.modules {
                        let mn = crate::ids::ModuleName::new(module);
                        if !entry.modules.contains(&mn) {
                            entry.modules.push(mn);
                        }
                    }
                }
            }
            by_area.insert(area, map);
        }
        Self { by_area }
    }

    pub fn routes(&self, area: Area) -> Vec<Route> {
        let mut v: Vec<Route> =
            self.by_area.get(&area).map(|m| m.values().cloned().collect()).unwrap_or_default();
        v.sort_by(|a, b| a.front_name.cmp(&b.front_name).then_with(|| a.id.cmp(&b.id)));
        v
    }
}

// ---------- webapi ----------

pub(crate) struct WebapiIndex {
    routes: HashMap<(String, String), WebapiRoute>,
}

impl WebapiIndex {
    pub fn build(modules: &[Module]) -> Self {
        let mut routes = HashMap::new();
        for (i, path, parsed) in read_parse(modules, Area::Global, "webapi.xml", parse::webapi_xml) {
            let module = &modules[i].name;
            for r in parsed {
                let source =
                    Source { module: module.clone(), file: path.clone(), line: r.line, area: Area::WebapiRest };
                routes.insert(
                    (r.method.clone(), r.url.clone()),
                    WebapiRoute {
                        method: r.method,
                        url: r.url,
                        service_class: r.service_class,
                        service_method: r.service_method,
                        resources: r.resources,
                        source,
                    },
                );
            }
        }
        Self { routes }
    }

    /// Routes, optionally filtered by a URL substring, sorted by (url, method).
    pub fn routes(&self, url_filter: Option<&str>) -> Vec<WebapiRoute> {
        let mut v: Vec<WebapiRoute> = self
            .routes
            .values()
            .filter(|r| url_filter.is_none_or(|f| r.url.contains(f)))
            .cloned()
            .collect();
        v.sort_by(|a, b| a.url.cmp(&b.url).then_with(|| a.method.cmp(&b.method)));
        v
    }
}

// ---------- declarative schema (db_schema.xml) ----------

pub(crate) struct SchemaIndex {
    tables: HashMap<String, DbTable>,
}

impl SchemaIndex {
    pub fn build(modules: &[Module]) -> Self {
        let mut tables: HashMap<String, DbTable> = HashMap::new();
        for (i, path, raw_tables) in read_parse(modules, Area::Global, "db_schema.xml", parse::db_schema_xml) {
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

// ---------- extension attributes (extension_attributes.xml) ----------

pub(crate) struct ExtAttrIndex {
    by_type: HashMap<ClassName, ExtendedType>,
}

impl ExtAttrIndex {
    pub fn build(modules: &[Module]) -> Self {
        let mut by_type: HashMap<ClassName, ExtendedType> = HashMap::new();
        for (i, path, raws) in
            read_parse(modules, Area::Global, "extension_attributes.xml", parse::extension_attributes_xml)
        {
            let module = &modules[i].name;
            for r in raws {
                let source = Source {
                    module: module.clone(),
                    file: path.clone(),
                    line: r.line,
                    area: Area::Global,
                };
                let attr = ExtensionAttribute {
                    code: r.code,
                    ty: r.ty,
                    resources: r.resources,
                    join: r.join.map(|j| ExtensionJoin {
                        reference_table: j.reference_table,
                        reference_field: j.reference_field,
                        join_on_field: j.join_on_field,
                        fields: j.fields,
                    }),
                    source,
                };
                let entry = by_type
                    .entry(r.for_type.clone())
                    .or_insert_with(|| ExtendedType { for_type: r.for_type, attributes: Vec::new() });
                match entry.attributes.iter_mut().find(|a| a.code == attr.code) {
                    Some(existing) => *existing = attr, // last declaration wins wholesale
                    None => entry.attributes.push(attr),
                }
            }
        }
        for t in by_type.values_mut() {
            t.attributes.sort_by(|a, b| a.code.cmp(&b.code));
        }
        Self { by_type }
    }

    /// One extended type by exact name.
    pub fn extended_type(&self, name: &ClassName) -> Option<ExtendedType> {
        self.by_type.get(name).cloned()
    }

    /// All extended types whose name contains `filter` (case-insensitive), by name.
    pub fn types(&self, filter: Option<&str>) -> Vec<ExtendedType> {
        let needle = filter.map(str::to_lowercase);
        let mut v: Vec<ExtendedType> = self
            .by_type
            .values()
            .filter(|t| {
                needle.as_ref().is_none_or(|n| t.for_type.as_str().to_lowercase().contains(n))
            })
            .cloned()
            .collect();
        v.sort_by(|a, b| a.for_type.cmp(&b.for_type));
        v
    }
}

// ---------- admin menu (adminhtml/menu.xml) ----------

pub(crate) struct MenuIndex {
    by_id: HashMap<String, MenuItem>,
    /// Pre-order DFS (roots first, each level by sortOrder then id), like [`AclIndex`].
    order: Vec<String>,
}

impl MenuIndex {
    pub fn build(modules: &[Module]) -> Self {
        let mut by_id: HashMap<String, MenuItem> = HashMap::new();

        for (i, path, ops) in read_parse(modules, Area::Adminhtml, "menu.xml", parse::menu_xml) {
            let module = &modules[i].name;
            for op in ops {
                match op {
                    parse::RawMenuOp::Upsert(r) => {
                        let source = Source {
                            module: module.clone(),
                            file: path.clone(),
                            line: r.line,
                            area: Area::Adminhtml,
                        };
                        let entry = by_id.entry(r.id.clone()).or_insert_with(|| MenuItem {
                            id: r.id.clone(),
                            title: String::new(),
                            parent: None,
                            action: None,
                            resource: None,
                            sort_order: None,
                            depends_on_module: None,
                            depends_on_config: None,
                            children: Vec::new(),
                            source: source.clone(),
                        });
                        // Attribute-level merge; the module giving the title owns `source`.
                        if let Some(t) = r.title {
                            entry.title = t;
                            entry.source = source;
                        }
                        if r.parent.is_some() {
                            entry.parent = r.parent;
                        }
                        if r.action.is_some() {
                            entry.action = r.action;
                        }
                        if r.resource.is_some() {
                            entry.resource = r.resource;
                        }
                        if r.sort_order.is_some() {
                            entry.sort_order = r.sort_order;
                        }
                        if r.depends_on_module.is_some() {
                            entry.depends_on_module = r.depends_on_module;
                        }
                        if r.depends_on_config.is_some() {
                            entry.depends_on_config = r.depends_on_config;
                        }
                    }
                    parse::RawMenuOp::Remove { id } => {
                        by_id.remove(&id);
                    }
                }
            }
        }

        // Children from parent pointers; an item whose parent doesn't exist (removed, or
        // never declared) is treated as a root so it never silently disappears.
        let mut kids: HashMap<String, Vec<(i32, String)>> = HashMap::new();
        for item in by_id.values() {
            if let Some(p) = &item.parent {
                if by_id.contains_key(p) {
                    kids.entry(p.clone())
                        .or_default()
                        .push((item.sort_order.unwrap_or(0), item.id.clone()));
                }
            }
        }
        for (pid, mut list) in kids {
            list.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            if let Some(parent) = by_id.get_mut(&pid) {
                parent.children = list.into_iter().map(|(_, id)| id).collect();
            }
        }

        let mut roots: Vec<&MenuItem> = by_id
            .values()
            .filter(|i| i.parent.as_ref().is_none_or(|p| !by_id.contains_key(p)))
            .collect();
        roots.sort_by(|a, b| {
            a.sort_order.unwrap_or(0).cmp(&b.sort_order.unwrap_or(0)).then_with(|| a.id.cmp(&b.id))
        });
        let root_ids: Vec<String> = roots.iter().map(|i| i.id.clone()).collect();
        let mut order = Vec::with_capacity(by_id.len());
        let mut seen = HashSet::new();
        for root in &root_ids {
            push_menu_preorder(root, &by_id, &mut order, &mut seen);
        }

        Self { by_id, order }
    }

    /// Tree (pre-order) order, or items whose id or title contains `filter`.
    pub fn items(&self, filter: Option<&str>) -> Vec<MenuItem> {
        let needle = filter.map(str::to_lowercase);
        self.order
            .iter()
            .filter_map(|id| self.by_id.get(id))
            .filter(|i| match &needle {
                Some(n) => i.id.to_lowercase().contains(n) || i.title.to_lowercase().contains(n),
                None => true,
            })
            .cloned()
            .collect()
    }

    pub fn item(&self, id: &str) -> Option<MenuItem> {
        self.by_id.get(id).cloned()
    }

    /// Breadcrumb: ancestors from the root down to (excluding) `id`.
    pub fn ancestors(&self, id: &str) -> Vec<MenuItem> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut cur = self.by_id.get(id).and_then(|i| i.parent.clone());
        while let Some(pid) = cur {
            if !seen.insert(pid.clone()) {
                break;
            }
            let Some(p) = self.by_id.get(&pid) else { break };
            chain.push(p.clone());
            cur = p.parent.clone();
        }
        chain.reverse();
        chain
    }

    pub fn children(&self, id: &str) -> Vec<MenuItem> {
        self.by_id
            .get(id)
            .map(|i| i.children.iter().filter_map(|c| self.by_id.get(c).cloned()).collect())
            .unwrap_or_default()
    }
}

fn push_menu_preorder(
    id: &str,
    by_id: &HashMap<String, MenuItem>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    if !seen.insert(id.to_string()) {
        return;
    }
    let Some(item) = by_id.get(id) else { return };
    out.push(id.to_string());
    for child in &item.children {
        push_menu_preorder(child, by_id, out, seen);
    }
}

// ---------- GraphQL schema (schema.graphqls) ----------

pub(crate) struct GqlIndex {
    types: HashMap<String, GqlType>,
}

impl GqlIndex {
    pub fn build(modules: &[Module]) -> Self {
        let mut types: HashMap<String, GqlType> = HashMap::new();
        for (i, path, raws) in
            read_parse(modules, Area::Global, "schema.graphqls", crate::graphql::schema_graphqls)
        {
            let module = &modules[i].name;
            let src = |line: u32| Source {
                module: module.clone(),
                file: path.clone(),
                line,
                area: Area::Graphql,
            };
            for r in raws {
                let entry = types.entry(r.name.clone()).or_insert_with(|| GqlType {
                    name: r.name.clone(),
                    kind: kind_of(r.kind),
                    implements: Vec::new(),
                    type_resolver: None,
                    description: None,
                    fields: Vec::new(),
                    values: Vec::new(),
                    members: Vec::new(),
                    source: src(r.line),
                });
                for imp in r.implements {
                    if !entry.implements.contains(&imp) {
                        entry.implements.push(imp);
                    }
                }
                if let Some(tr) = directive_arg(&r.directives, "typeResolver", "class") {
                    entry.type_resolver = Some(ClassName::new(tr));
                }
                if let Some(d) =
                    directive_arg(&r.directives, "doc", "description").or(r.description)
                {
                    entry.description = Some(d);
                }
                for v in r.values {
                    if !entry.values.contains(&v) {
                        entry.values.push(v);
                    }
                }
                for m in r.members {
                    if !entry.members.contains(&m) {
                        entry.members.push(m);
                    }
                }
                // Fields union by name; a re-declaration replaces (last module wins,
                // matching the stitching reader) and takes the newer provenance.
                for f in r.fields {
                    let field = GqlField {
                        name: f.name,
                        args: f
                            .args
                            .into_iter()
                            .map(|a| GqlArg { name: a.name, ty: a.ty })
                            .collect(),
                        ty: f.ty,
                        resolver: directive_arg(&f.directives, "resolver", "class")
                            .map(ClassName::new),
                        description: directive_arg(&f.directives, "doc", "description")
                            .or(f.description),
                        deprecated: f
                            .directives
                            .iter()
                            .find(|d| d.name == "deprecated")
                            .map(|d| {
                                d.args
                                    .iter()
                                    .find(|(k, _)| k == "reason")
                                    .map(|(_, v)| v.clone())
                                    .unwrap_or_default()
                            }),
                        cacheable: directive_arg(&f.directives, "cache", "cacheable")
                            .map(|v| v != "false"),
                        source: src(f.line),
                    };
                    match entry.fields.iter_mut().find(|e| e.name == field.name) {
                        Some(e) => *e = field,
                        None => entry.fields.push(field),
                    }
                }
            }
        }
        Self { types }
    }

    /// One type by exact name.
    pub fn type_(&self, name: &str) -> Option<GqlType> {
        self.types.get(name).cloned()
    }

    /// Types whose name contains `filter` (case-insensitive; all when `None`), by name.
    pub fn types(&self, filter: Option<&str>) -> Vec<GqlType> {
        let needle = filter.map(str::to_lowercase);
        let mut v: Vec<GqlType> = self
            .types
            .values()
            .filter(|t| needle.as_ref().is_none_or(|n| t.name.to_lowercase().contains(n)))
            .cloned()
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

fn kind_of(k: crate::graphql::RawGqlKind) -> GqlKind {
    use crate::graphql::RawGqlKind as R;
    match k {
        R::Object => GqlKind::Object,
        R::Interface => GqlKind::Interface,
        R::Input => GqlKind::Input,
        R::Enum => GqlKind::Enum,
        R::Union => GqlKind::Union,
        R::Scalar => GqlKind::Scalar,
    }
}

/// The value of `@directive(arg: …)`, when present.
fn directive_arg(
    directives: &[crate::graphql::RawDirective],
    directive: &str,
    arg: &str,
) -> Option<String> {
    directives
        .iter()
        .find(|d| d.name == directive)?
        .args
        .iter()
        .find(|(k, _)| k == arg)
        .map(|(_, v)| v.clone())
}

// ---------- message queues (communication.xml + queue_*.xml) ----------

/// The merged message-queue wiring: topics/handlers, consumers, exchange bindings, and
/// publishers, plus the topic → queue → consumer join.
pub(crate) struct MqIndex {
    topics: HashMap<String, MqTopic>,
    /// consumer name -> consumer.
    consumers: HashMap<String, MqConsumer>,
    /// (connection, exchange name) -> exchange (bindings keyed by id inside).
    exchanges: HashMap<(String, String), MqExchangeBuild>,
    /// topic -> publisher (connections kept raw; flattened in [`publisher`](Self::publisher)).
    publishers: HashMap<String, MqPublisherBuild>,
}

struct MqExchangeBuild {
    bindings: HashMap<String, MqBindingBuild>,
}

struct MqBindingBuild {
    pattern: String,
    destination: String,
    disabled: bool,
    source: Source,
}

struct MqPublisherBuild {
    queue: Option<String>,
    disabled: bool,
    /// connection name -> (exchange, disabled).
    connections: Vec<(String, Option<String>, bool)>,
    source: Source,
}

impl MqIndex {
    pub fn build(modules: &[Module]) -> Self {
        let src = |module: &ModuleName, path: &PathBuf, line: u32| Source {
            module: module.clone(),
            file: path.clone(),
            line,
            area: Area::Global,
        };

        // communication.xml: topics by name (attrs merge non-empty), handlers by name
        // (attribute-level, like plugins — a later `disabled="true"` keeps the class).
        let mut topics: HashMap<String, MqTopic> = HashMap::new();
        for (i, path, raws) in
            read_parse(modules, Area::Global, "communication.xml", parse::communication_xml)
        {
            let module = &modules[i].name;
            for r in raws {
                let entry = topics.entry(r.name.clone()).or_insert_with(|| MqTopic {
                    name: r.name.clone(),
                    request: None,
                    response: None,
                    schema: None,
                    handlers: Vec::new(),
                    source: src(module, &path, r.line),
                });
                if r.request.is_some() {
                    entry.request = r.request;
                }
                if r.response.is_some() {
                    entry.response = r.response;
                }
                if r.schema.is_some() {
                    entry.schema = r.schema;
                }
                for h in r.handlers {
                    let source = src(module, &path, h.line);
                    match entry.handlers.iter_mut().find(|e| e.name == h.name) {
                        Some(e) => {
                            if !h.class.as_str().is_empty() {
                                e.class = h.class;
                            }
                            if !h.method.is_empty() {
                                e.method = h.method;
                            }
                            if let Some(d) = h.disabled {
                                e.disabled = d;
                            }
                            e.source = source;
                        }
                        None => entry.handlers.push(MqHandler {
                            name: h.name,
                            class: h.class,
                            method: h.method,
                            disabled: h.disabled.unwrap_or(false),
                            source,
                        }),
                    }
                }
            }
        }

        // queue_consumer.xml: consumers by name, merge non-empty.
        let mut consumers: HashMap<String, MqConsumer> = HashMap::new();
        for (i, path, raws) in
            read_parse(modules, Area::Global, "queue_consumer.xml", parse::queue_consumer_xml)
        {
            let module = &modules[i].name;
            for r in raws {
                let source = src(module, &path, r.line);
                let entry = consumers.entry(r.name.clone()).or_insert_with(|| MqConsumer {
                    name: r.name.clone(),
                    queue: String::new(),
                    connection: None,
                    consumer_instance: None,
                    handler: None,
                    max_messages: None,
                    source: source.clone(),
                });
                if !r.queue.is_empty() {
                    entry.queue = r.queue;
                }
                if r.connection.is_some() {
                    entry.connection = r.connection;
                }
                if r.consumer_instance.is_some() {
                    entry.consumer_instance = r.consumer_instance;
                }
                if r.handler.is_some() {
                    entry.handler = r.handler;
                }
                if r.max_messages.is_some() {
                    entry.max_messages = r.max_messages;
                }
                entry.source = source;
            }
        }

        // queue_topology.xml: exchanges keyed by (connection, name) — the same exchange
        // name on amqp and db is two different exchanges. Bindings by id, last-wins.
        let mut exchanges: HashMap<(String, String), MqExchangeBuild> = HashMap::new();
        for (i, path, raws) in
            read_parse(modules, Area::Global, "queue_topology.xml", parse::queue_topology_xml)
        {
            let module = &modules[i].name;
            for r in raws {
                let conn = r.connection.clone().unwrap_or_else(|| "amqp".to_string());
                let entry = exchanges
                    .entry((conn, r.name.clone()))
                    .or_insert_with(|| MqExchangeBuild { bindings: HashMap::new() });
                for b in r.bindings {
                    entry.bindings.insert(
                        b.id.clone(),
                        MqBindingBuild {
                            pattern: b.topic,
                            destination: b.destination,
                            disabled: b.disabled,
                            source: src(module, &path, b.line),
                        },
                    );
                }
            }
        }

        // queue_publisher.xml: publishers by topic; connections merged by name.
        let mut publishers: HashMap<String, MqPublisherBuild> = HashMap::new();
        for (i, path, raws) in
            read_parse(modules, Area::Global, "queue_publisher.xml", parse::queue_publisher_xml)
        {
            let module = &modules[i].name;
            for r in raws {
                let source = src(module, &path, r.line);
                let entry = publishers.entry(r.topic.clone()).or_insert_with(|| MqPublisherBuild {
                    queue: None,
                    disabled: false,
                    connections: Vec::new(),
                    source: source.clone(),
                });
                if r.queue.is_some() {
                    entry.queue = r.queue;
                }
                if let Some(d) = r.disabled {
                    entry.disabled = d;
                }
                for c in r.connections {
                    match entry.connections.iter_mut().find(|(n, _, _)| *n == c.name) {
                        Some((_, ex, dis)) => {
                            if c.exchange.is_some() {
                                *ex = c.exchange;
                            }
                            if let Some(d) = c.disabled {
                                *dis = d;
                            }
                        }
                        None => entry.connections.push((
                            c.name,
                            c.exchange,
                            c.disabled.unwrap_or(false),
                        )),
                    }
                }
                entry.source = source;
            }
        }

        Self { topics, consumers, exchanges, publishers }
    }

    /// Topics whose name contains `filter` (or all, when `None`), sorted by name.
    pub fn topics(&self, filter: Option<&str>) -> Vec<MqTopic> {
        let mut v: Vec<MqTopic> = self
            .topics
            .values()
            .filter(|t| filter.is_none_or(|f| t.name.contains(f)))
            .cloned()
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// The publisher for `topic`, flattened to its enabled `<connection>` (Magento allows
    /// exactly one enabled connection per publisher).
    fn publisher(&self, topic: &str) -> Option<MqPublisher> {
        let p = self.publishers.get(topic)?;
        let conn = p.connections.iter().find(|(_, _, disabled)| !disabled);
        Some(MqPublisher {
            topic: topic.to_string(),
            queue: p.queue.clone(),
            connection: conn.map(|(n, _, _)| n.clone()),
            exchange: conn.and_then(|(_, e, _)| e.clone()),
            disabled: p.disabled,
            source: p.source.clone(),
        })
    }

    /// The full journey of one topic (exact name): its queues (via the publisher's direct
    /// `queue=` and/or every enabled binding whose pattern matches) and each queue's
    /// consumers. `None` when the topic appears in neither `communication.xml` nor
    /// `queue_publisher.xml`.
    pub fn topic_route(&self, name: &str) -> Option<MqTopicRoute> {
        let publisher = self.publisher(name);
        // A topic declared only in queue_publisher.xml (no communication.xml entry) still
        // gets a route, with an empty handler list and the publisher's provenance.
        let topic = match self.topics.get(name) {
            Some(t) => t.clone(),
            None => MqTopic {
                name: name.to_string(),
                request: None,
                response: None,
                schema: None,
                handlers: Vec::new(),
                source: publisher.as_ref()?.source.clone(),
            },
        };

        let mut routes: Vec<MqRoute> = Vec::new();
        if let Some(p) = &publisher {
            if let Some(q) = &p.queue {
                let i = route_for(&mut routes, q);
                routes[i].via.push(MqVia::PublisherQueue { source: p.source.clone() });
            }
        }
        let mut keys: Vec<&(String, String)> = self.exchanges.keys().collect();
        keys.sort();
        for key in keys {
            let (conn, ex_name) = key;
            let ex = &self.exchanges[key];
            let mut ids: Vec<&String> = ex.bindings.keys().collect();
            ids.sort();
            for id in ids {
                let b = &ex.bindings[id];
                if b.disabled || !topic_matches(&b.pattern, name) {
                    continue;
                }
                let i = route_for(&mut routes, &b.destination);
                routes[i].via.push(MqVia::Binding {
                    exchange: ex_name.clone(),
                    connection: conn.clone(),
                    id: id.clone(),
                    pattern: b.pattern.clone(),
                    source: b.source.clone(),
                });
            }
        }

        for route in &mut routes {
            route.consumers = self
                .consumers
                .values()
                .filter(|c| c.queue == route.queue)
                .cloned()
                .collect();
            route.consumers.sort_by(|a, b| a.name.cmp(&b.name));
        }
        routes.sort_by(|a, b| a.queue.cmp(&b.queue));

        Some(MqTopicRoute { topic, publisher, routes })
    }
}

/// The index of `queue`'s route in `routes`, appending an empty one on first sight.
fn route_for(routes: &mut Vec<MqRoute>, queue: &str) -> usize {
    match routes.iter().position(|r| r.queue == queue) {
        Some(i) => i,
        None => {
            routes.push(MqRoute { queue: queue.to_string(), via: Vec::new(), consumers: Vec::new() });
            routes.len() - 1
        }
    }
}

/// AMQP topic-exchange pattern match: `.`-separated words, `*` = exactly one word,
/// `#` = zero or more words.
fn topic_matches(pattern: &str, topic: &str) -> bool {
    fn rec(p: &[&str], t: &[&str]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(&"#"), _) => rec(&p[1..], t) || (!t.is_empty() && rec(p, &t[1..])),
            (Some(&"*"), Some(_)) => rec(&p[1..], &t[1..]),
            (Some(&w), Some(&tw)) if w == tw => rec(&p[1..], &t[1..]),
            _ => false,
        }
    }
    rec(
        &pattern.split('.').collect::<Vec<_>>(),
        &topic.split('.').collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod mq_match_tests {
    use super::topic_matches;

    #[test]
    fn amqp_topic_patterns() {
        assert!(topic_matches("a.b.c", "a.b.c"));
        assert!(!topic_matches("a.b.c", "a.b.d"));
        assert!(topic_matches("a.*.c", "a.b.c"));
        assert!(!topic_matches("a.*.c", "a.b.b.c")); // * is exactly one word
        assert!(topic_matches("#", "anything.at.all"));
        assert!(topic_matches("a.#", "a"));
        assert!(topic_matches("a.#", "a.b.c"));
        assert!(!topic_matches("a.#", "b.a"));
        assert!(topic_matches("#.c", "a.b.c"));
    }
}

// ---------- indexers (indexer.xml + mview.xml) ----------

pub(crate) struct IndexerIndex {
    by_id: HashMap<String, Indexer>,
}

impl IndexerIndex {
    pub fn build(modules: &[Module]) -> Self {
        // indexer.xml: indexers keyed by id, merged non-empty (a later module may re-state
        // an indexer only to override its class or add dependencies). `source` keeps the
        // first declaration; dependencies accumulate (deduped).
        let mut by_id: HashMap<String, Indexer> = HashMap::new();
        for (i, path, raws) in read_parse(modules, Area::Global, "indexer.xml", parse::indexer_xml)
        {
            let module = &modules[i].name;
            for r in raws {
                let source = Source {
                    module: module.clone(),
                    file: path.clone(),
                    line: r.line,
                    area: Area::Global,
                };
                let entry = by_id.entry(r.id.clone()).or_insert_with(|| Indexer {
                    id: r.id.clone(),
                    title: String::new(),
                    description: None,
                    class: ClassName::new(String::new()),
                    view_id: None,
                    shared_index: None,
                    dependencies: Vec::new(),
                    subscriptions: Vec::new(),
                    source,
                });
                if let Some(c) = r.class {
                    entry.class = c;
                }
                if r.view_id.is_some() {
                    entry.view_id = r.view_id;
                }
                if r.shared_index.is_some() {
                    entry.shared_index = r.shared_index;
                }
                if !r.title.is_empty() {
                    entry.title = r.title;
                }
                if r.description.is_some() {
                    entry.description = r.description;
                }
                for d in r.dependencies {
                    if !entry.dependencies.contains(&d) {
                        entry.dependencies.push(d);
                    }
                }
            }
        }

        // mview.xml: views keyed by id; subscriptions merged by table name, each keeping the
        // *adding* module's source (a module can subscribe extra tables to another's view).
        let mut views: HashMap<String, Vec<MviewSubscription>> = HashMap::new();
        for (i, path, raws) in read_parse(modules, Area::Global, "mview.xml", parse::mview_xml) {
            let module = &modules[i].name;
            for v in raws {
                let subs = views.entry(v.id).or_default();
                for s in v.subscriptions {
                    let sub = MviewSubscription {
                        table: s.table,
                        entity_column: s.entity_column,
                        source: Source {
                            module: module.clone(),
                            file: path.clone(),
                            line: s.line,
                            area: Area::Global,
                        },
                    };
                    match subs.iter_mut().find(|x| x.table == sub.table) {
                        Some(existing) => *existing = sub,
                        None => subs.push(sub),
                    }
                }
            }
        }

        // Join: an indexer's subscriptions come from its view (matched on view_id).
        for idx in by_id.values_mut() {
            if let Some(subs) = idx.view_id.as_ref().and_then(|vid| views.get(vid)) {
                idx.subscriptions = subs.clone();
            }
        }

        Self { by_id }
    }

    /// One indexer by exact id.
    pub fn indexer(&self, id: &str) -> Option<Indexer> {
        self.by_id.get(id).cloned()
    }

    /// All indexers whose id or title contains `filter` (case-insensitive; all when `None`),
    /// sorted by id.
    pub fn indexers(&self, filter: Option<&str>) -> Vec<Indexer> {
        let needle = filter.map(str::to_lowercase);
        let mut v: Vec<Indexer> = self
            .by_id
            .values()
            .filter(|ix| match &needle {
                Some(n) => ix.id.to_lowercase().contains(n) || ix.title.to_lowercase().contains(n),
                None => true,
            })
            .cloned()
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }
}

// ---------- admin system configuration (adminhtml/system.xml) ----------

#[derive(Default)]
struct FieldBuild {
    label: String,
    field_type: String,
    config_path: String,
    scopes: Vec<String>,
    source_model: Option<String>,
    backend_model: Option<String>,
    source: Option<Source>,
}

#[derive(Default)]
struct GroupBuild {
    label: String,
    fields: HashMap<String, FieldBuild>,
}

#[derive(Default)]
struct SectionBuild {
    label: String,
    tab: Option<String>,
    groups: HashMap<String, GroupBuild>,
}

pub(crate) struct SystemConfigIndex {
    tabs: HashMap<String, String>,
    sections: HashMap<String, SectionBuild>,
}

impl SystemConfigIndex {
    pub fn build(modules: &[Module]) -> Self {
        let mut tabs: HashMap<String, String> = HashMap::new();
        let mut sections: HashMap<String, SectionBuild> = HashMap::new();

        for (i, path, raw) in read_parse(modules, Area::Adminhtml, "system.xml", parse::system_xml) {
            let module = &modules[i].name;
            for tab in raw.tabs {
                if !tab.label.is_empty() {
                    tabs.insert(tab.id, tab.label);
                }
            }
            for sec in raw.sections {
                let sec_id = sec.id;
                let sb = sections.entry(sec_id.clone()).or_default();
                if !sec.label.is_empty() {
                    sb.label = sec.label;
                }
                if sec.tab.is_some() {
                    sb.tab = sec.tab;
                }
                for grp in sec.groups {
                    let grp_id = grp.id;
                    let gb = sb.groups.entry(grp_id.clone()).or_default();
                    if !grp.label.is_empty() {
                        gb.label = grp.label;
                    }
                    for f in grp.fields {
                        let config_path =
                            f.config_path.unwrap_or_else(|| format!("{sec_id}/{grp_id}/{}", f.id));
                        let mut scopes = Vec::new();
                        if f.show_default {
                            scopes.push("default".to_string());
                        }
                        if f.show_website {
                            scopes.push("website".to_string());
                        }
                        if f.show_store {
                            scopes.push("store".to_string());
                        }
                        let source =
                            Source { module: module.clone(), file: path.clone(), line: f.line, area: Area::Adminhtml };
                        // Merge non-empty over any prior declaration (a later module may only
                        // tweak a field — e.g. add a scope — without re-stating everything).
                        let entry = gb.fields.entry(f.id).or_default();
                        if !f.label.is_empty() {
                            entry.label = f.label;
                        }
                        if !f.field_type.is_empty() {
                            entry.field_type = f.field_type;
                        }
                        entry.config_path = config_path;
                        if !scopes.is_empty() {
                            entry.scopes = scopes;
                        }
                        if f.source_model.is_some() {
                            entry.source_model = f.source_model;
                        }
                        if f.backend_model.is_some() {
                            entry.backend_model = f.backend_model;
                        }
                        entry.source = Some(source);
                    }
                }
            }
        }

        Self { tabs, sections }
    }

    /// Every field whose config path or label contains `filter` (or all, when `None`), sorted
    /// by config path. Each carries its admin breadcrumb (tab/section/group labels resolved).
    pub fn fields(&self, filter: Option<&str>) -> Vec<SystemField> {
        let needle = filter.map(str::to_lowercase);
        let mut out = Vec::new();
        for section in self.sections.values() {
            let tab = section.tab.as_ref().and_then(|id| self.tabs.get(id)).cloned();
            for group in section.groups.values() {
                for fb in group.fields.values() {
                    let Some(source) = &fb.source else { continue };
                    if let Some(n) = &needle {
                        if !fb.config_path.to_lowercase().contains(n)
                            && !fb.label.to_lowercase().contains(n)
                        {
                            continue;
                        }
                    }
                    out.push(SystemField {
                        path: fb.config_path.clone(),
                        label: fb.label.clone(),
                        field_type: fb.field_type.clone(),
                        tab: tab.clone(),
                        section: section.label.clone(),
                        group: group.label.clone(),
                        scopes: fb.scopes.clone(),
                        source_model: fb.source_model.clone(),
                        backend_model: fb.backend_model.clone(),
                        source: source.clone(),
                    });
                }
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }
}

// ---------- admin ACL (acl.xml) ----------

pub(crate) struct AclIndex {
    by_id: HashMap<String, AclResource>,
    /// Pre-order DFS of the whole forest (roots first; each level sorted by sortOrder then id).
    order: Vec<String>,
}

impl AclIndex {
    pub fn build(modules: &[Module]) -> Self {
        let mut by_id: HashMap<String, AclResource> = HashMap::new();

        // acl.xml is a global file (`etc/acl.xml`); tag provenance as adminhtml — its domain.
        for (i, path, raws) in read_parse(modules, Area::Global, "acl.xml", parse::acl_xml) {
            let module = &modules[i].name;
            for r in raws {
                let source =
                    Source { module: module.clone(), file: path.clone(), line: r.line, area: Area::Adminhtml };
                let entry = by_id.entry(r.id.clone()).or_insert_with(|| AclResource {
                    id: r.id.clone(),
                    title: String::new(),
                    parent: None,
                    children: Vec::new(),
                    sort_order: None,
                    disabled: false,
                    source: source.clone(),
                });
                // Merge non-empty over prior declarations: a later file may re-state an ancestor
                // as a bare path anchor (no title/sortOrder). The module that gives it a title is
                // the declarer, so `source` follows the title.
                if !r.title.is_empty() {
                    entry.title = r.title;
                    entry.source = source;
                }
                if r.parent.is_some() {
                    entry.parent = r.parent;
                }
                if r.sort_order.is_some() {
                    entry.sort_order = r.sort_order;
                }
                if r.disabled {
                    entry.disabled = true;
                }
            }
        }

        // Attach children (sorted by sortOrder then id) from the parent pointers.
        let mut kids: HashMap<String, Vec<(i32, String)>> = HashMap::new();
        for r in by_id.values() {
            if let Some(p) = &r.parent {
                kids.entry(p.clone()).or_default().push((r.sort_order.unwrap_or(0), r.id.clone()));
            }
        }
        for (pid, mut list) in kids {
            list.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            if let Some(parent) = by_id.get_mut(&pid) {
                parent.children = list.into_iter().map(|(_, id)| id).collect();
            }
        }

        // Pre-order DFS from the roots (resources with no parent), roots sorted likewise.
        let mut roots: Vec<&AclResource> = by_id.values().filter(|r| r.parent.is_none()).collect();
        roots.sort_by(|a, b| {
            a.sort_order.unwrap_or(0).cmp(&b.sort_order.unwrap_or(0)).then_with(|| a.id.cmp(&b.id))
        });
        let root_ids: Vec<String> = roots.iter().map(|r| r.id.clone()).collect();
        let mut order = Vec::with_capacity(by_id.len());
        let mut seen = HashSet::new();
        for root in &root_ids {
            push_preorder(root, &by_id, &mut order, &mut seen);
        }

        Self { by_id, order }
    }

    /// List mode: every resource in tree (pre-order) order, or those whose id or title contains
    /// `filter` (case-insensitive), keeping tree order.
    pub fn resources(&self, filter: Option<&str>) -> Vec<AclResource> {
        let needle = filter.map(str::to_lowercase);
        self.order
            .iter()
            .filter_map(|id| self.by_id.get(id))
            .filter(|r| match &needle {
                Some(n) => r.id.to_lowercase().contains(n) || r.title.to_lowercase().contains(n),
                None => true,
            })
            .cloned()
            .collect()
    }

    pub fn resource(&self, id: &str) -> Option<AclResource> {
        self.by_id.get(id).cloned()
    }

    /// The breadcrumb: ancestors from the root down to (but excluding) `id`.
    pub fn ancestors(&self, id: &str) -> Vec<AclResource> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut cur = self.by_id.get(id).and_then(|r| r.parent.clone());
        while let Some(pid) = cur {
            if !seen.insert(pid.clone()) {
                break; // malformed cycle guard
            }
            let Some(p) = self.by_id.get(&pid) else { break };
            chain.push(p.clone());
            cur = p.parent.clone();
        }
        chain.reverse();
        chain
    }

    /// Direct children of `id`, in their stored (sortOrder, id) order.
    pub fn children(&self, id: &str) -> Vec<AclResource> {
        self.by_id
            .get(id)
            .map(|r| r.children.iter().filter_map(|c| self.by_id.get(c).cloned()).collect())
            .unwrap_or_default()
    }
}

fn push_preorder(id: &str, by_id: &HashMap<String, AclResource>, out: &mut Vec<String>, seen: &mut HashSet<String>) {
    if !seen.insert(id.to_string()) {
        return; // already emitted (guards against a malformed parent cycle)
    }
    let Some(r) = by_id.get(id) else { return };
    out.push(id.to_string());
    for child in &r.children {
        push_preorder(child, by_id, out, seen);
    }
}
