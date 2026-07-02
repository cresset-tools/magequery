//! "Breadth" indexes — events/observers, cron, routes, webapi. Each is a thin projection
//! of a per-module XML file, merged in load order (per-area for events/routes; global for
//! cron/webapi). Built lazily (on first query) so they don't slow the common commands.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use rayon::prelude::*;

use std::path::Path;

use crate::ids::{Area, ClassName, EventName, ModuleName};
use crate::model::{
    AclResource, CronJob, DbColumn, DbConstraint, DbIndex, DbTable, Indexer, Module,
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

/// Read + parse `etc/[<area>/]<file>` for every module **in parallel**, returning
/// `(module index, path, parsed)` for the files that exist, in module (load) order — so the
/// caller merges sequentially and deterministically. `rayon` preserves the collect order.
fn read_parse<T: Send>(
    modules: &[Module],
    area: Area,
    file: &str,
    parse: impl Fn(&str) -> T + Sync,
) -> Vec<(usize, PathBuf, T)> {
    let jobs: Vec<(usize, PathBuf)> =
        modules.iter().enumerate().map(|(i, m)| (i, area_path(m, area, file))).collect();
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

    /// All tables whose name contains `filter` (or all, when `None`), sorted by name.
    pub fn tables(&self, filter: Option<&str>) -> Vec<DbTable> {
        let mut v: Vec<DbTable> = self
            .tables
            .values()
            .filter(|t| filter.is_none_or(|f| t.name.contains(f)))
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
