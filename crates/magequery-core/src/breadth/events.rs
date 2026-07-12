//! Static events indexes.

use super::*;

// ---------- events / observers ----------

type EventMap = HashMap<EventName, HashMap<String, Observer>>;

pub(crate) struct EventIndex {
    by_area: HashMap<Area, EventMap>,
}

impl EventIndex {
    pub fn build(modules: &[Module], vfs: &Vfs) -> Self {
        let mut global = EventMap::new();
        apply_events(&mut global, modules, vfs, Area::Global);
        let mut by_area = HashMap::new();
        for area in REAL_AREAS {
            let mut cfg = global.clone();
            apply_events(&mut cfg, modules, vfs, area);
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

fn apply_events(out: &mut EventMap, modules: &[Module], vfs: &Vfs, area: Area) {
    for (i, path, observers) in read_parse(modules, vfs, area, "events.xml", parse::events_xml) {
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
    pub fn build(modules: &[Module], vfs: &Vfs) -> Self {
        let mut groups: HashMap<String, HashMap<String, CronJob>> = HashMap::new();
        for (i, path, jobs) in read_parse(modules, vfs, Area::Global, "crontab.xml", parse::crontab_xml) {
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
                        live: None,
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
