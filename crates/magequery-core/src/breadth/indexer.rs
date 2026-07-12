//! Static indexer indexes.

use super::*;

// ---------- indexers (indexer.xml + mview.xml) ----------

pub(crate) struct IndexerIndex {
    by_id: HashMap<String, Indexer>,
}

impl IndexerIndex {
    pub fn build(modules: &[Module], vfs: &Vfs) -> Self {
        // indexer.xml: indexers keyed by id, merged non-empty (a later module may re-state
        // an indexer only to override its class or add dependencies). `source` keeps the
        // first declaration; dependencies accumulate (deduped).
        let mut by_id: HashMap<String, Indexer> = HashMap::new();
        for (i, path, raws) in read_parse(modules, vfs, Area::Global, "indexer.xml", parse::indexer_xml)
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
                    live: None,
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
        for (i, path, raws) in read_parse(modules, vfs, Area::Global, "mview.xml", parse::mview_xml) {
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
