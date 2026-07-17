//! Static routes indexes.

use super::*;

// ---------- routes ----------

pub(crate) struct RouteIndex {
    /// area -> ((router, route id) -> route)
    by_area: HashMap<Area, HashMap<(String, String), Route>>,
}

impl RouteIndex {
    pub fn build(modules: &[Module], vfs: &Vfs) -> Self {
        let mut by_area = HashMap::new();
        for area in [Area::Global, Area::Frontend, Area::Adminhtml, Area::WebapiRest, Area::Graphql] {
            let mut map: HashMap<(String, String), Route> = HashMap::new();
            for (i, path, parsed) in read_parse(modules, vfs, area, "routes.xml", parse::routes_xml) {
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
    pub fn build(modules: &[Module], vfs: &Vfs) -> Self {
        let mut routes = HashMap::new();
        for (i, path, parsed) in read_parse(modules, vfs, Area::Global, "webapi.xml", parse::webapi_xml) {
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
