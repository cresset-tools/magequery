//! Static admin indexes.

use super::*;

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
    pub fn build(modules: &[Module], vfs: &Vfs) -> Self {
        let mut tabs: HashMap<String, String> = HashMap::new();
        let mut sections: HashMap<String, SectionBuild> = HashMap::new();

        for (i, path, raw) in read_parse(modules, vfs, Area::Adminhtml, "system.xml", parse::system_xml) {
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
    pub fn build(modules: &[Module], vfs: &Vfs) -> Self {
        let mut by_id: HashMap<String, AclResource> = HashMap::new();

        // acl.xml is a global file (`etc/acl.xml`); tag provenance as adminhtml — its domain.
        for (i, path, raws) in read_parse(modules, vfs, Area::Global, "acl.xml", parse::acl_xml) {
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
