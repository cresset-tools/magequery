//! Static extensions indexes.

use super::*;

// ---------- extension attributes (extension_attributes.xml) ----------

pub(crate) struct ExtAttrIndex {
    by_type: HashMap<ClassName, ExtendedType>,
}

impl ExtAttrIndex {
    pub fn build(modules: &[Module], vfs: &Vfs) -> Self {
        let mut by_type: HashMap<ClassName, ExtendedType> = HashMap::new();
        for (i, path, raws) in
            read_parse(modules, vfs, Area::Global, "extension_attributes.xml", parse::extension_attributes_xml)
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
    pub fn build(modules: &[Module], vfs: &Vfs) -> Self {
        let mut by_id: HashMap<String, MenuItem> = HashMap::new();

        for (i, path, ops) in read_parse(modules, vfs, Area::Adminhtml, "menu.xml", parse::menu_xml) {
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
