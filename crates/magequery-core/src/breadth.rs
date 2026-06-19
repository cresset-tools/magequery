//! "Breadth" indexes — events/observers, cron, routes, webapi. Each is a thin projection
//! of a per-module XML file, merged in load order (per-area for events/routes; global for
//! cron/webapi). Built lazily (on first query) so they don't slow the common commands.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::ids::{Area, EventName};
use crate::model::{CronJob, Module, Observer, Route, WebapiRoute};
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

fn read(path: &PathBuf) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

// ---------- events / observers ----------

type EventMap = HashMap<EventName, HashMap<String, Observer>>;

pub(crate) struct EventIndex {
    by_area: HashMap<Area, EventMap>,
}

impl EventIndex {
    pub fn build(modules: &[Module]) -> Self {
        let mut global = EventMap::new();
        merge_events(&mut global, modules, Area::Global);
        let mut by_area = HashMap::new();
        for area in REAL_AREAS {
            let mut cfg = global.clone();
            merge_events(&mut cfg, modules, area);
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

fn merge_events(out: &mut EventMap, modules: &[Module], area: Area) {
    for m in modules {
        let path = area_path(m, area, "events.xml");
        let Some(text) = read(&path) else { continue };
        for (event, obs) in parse::events_xml(&text) {
            let source = Source { module: m.name.clone(), file: path.clone(), line: obs.line, area };
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
        for m in modules {
            let path = m.path.join("etc/crontab.xml");
            let Some(text) = read(&path) else { continue };
            for job in parse::crontab_xml(&text) {
                let source =
                    Source { module: m.name.clone(), file: path.clone(), line: job.line, area: Area::Crontab };
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
            for m in modules {
                let path = area_path(m, area, "routes.xml");
                let Some(text) = read(&path) else { continue };
                for r in parse::routes_xml(&text) {
                    let key = (r.router.clone(), r.id.clone());
                    let entry = map.entry(key).or_insert_with(|| Route {
                        area,
                        router: r.router.clone(),
                        id: r.id.clone(),
                        front_name: r.front_name.clone(),
                        modules: Vec::new(),
                        source: Source {
                            module: m.name.clone(),
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
        for m in modules {
            let path = m.path.join("etc/webapi.xml");
            let Some(text) = read(&path) else { continue };
            for r in parse::webapi_xml(&text) {
                let source =
                    Source { module: m.name.clone(), file: path.clone(), line: r.line, area: Area::WebapiRest };
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
