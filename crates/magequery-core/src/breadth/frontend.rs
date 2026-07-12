//! Static frontend indexes.

use super::*;

// ---------- email templates (etc/email_templates.xml) ----------

pub(crate) struct EmailTemplateIndex {
    by_id: HashMap<String, EmailTemplate>,
}

impl EmailTemplateIndex {
    /// `themes` as in [`LayoutIndex::build`].
    pub fn build(modules: &[Module], vfs: &Vfs, themes: &[(String, PathBuf)]) -> Self {
        let paths: HashMap<&str, &Path> =
            modules.iter().map(|m| (m.name.as_str(), m.path.as_path())).collect();

        let mut by_id: HashMap<String, EmailTemplate> = HashMap::new();
        for (i, path, raws) in
            read_parse(modules, vfs, Area::Global, "email_templates.xml", parse::email_templates_xml)
        {
            let declaring = &modules[i].name;
            for r in raws {
                let area = r.area.parse::<Area>().unwrap_or(Area::Frontend);
                // The declared file lives in the *referenced* module's view dir.
                let resolved = paths.get(r.module.as_str()).and_then(|mp| {
                    let p = mp
                        .join("view")
                        .join(area.dir().unwrap_or("frontend"))
                        .join("email")
                        .join(&r.file);
                    p.is_file().then_some(p)
                });
                // Theme overrides: `<theme>/<Module>/email/<file>`, themes of the same area.
                let theme_overrides: Vec<EmailTemplateOverride> = themes
                    .iter()
                    .filter(|(id, _)| id.starts_with(&format!("{area}/")))
                    .filter_map(|(id, dir)| {
                        let p = dir.join(&r.module).join("email").join(&r.file);
                        p.is_file()
                            .then(|| EmailTemplateOverride { theme: id.clone(), file: p })
                    })
                    .collect();
                // Last declaration wins (a module can re-register another's template id).
                by_id.insert(
                    r.id.clone(),
                    EmailTemplate {
                        id: r.id,
                        label: r.label,
                        file: r.file,
                        kind: r.kind,
                        module: ModuleName::new(r.module),
                        area,
                        path: resolved,
                        theme_overrides,
                        source: Source {
                            module: declaring.clone(),
                            file: path.clone(),
                            line: r.line,
                            area,
                        },
                    },
                );
            }
        }
        Self { by_id }
    }

    pub fn template(&self, id: &str) -> Option<EmailTemplate> {
        self.by_id.get(id).cloned()
    }

    /// Templates whose id or label contains `filter` (case-insensitive), sorted by id.
    pub fn templates(&self, filter: Option<&str>) -> Vec<EmailTemplate> {
        let needle = filter.map(str::to_lowercase);
        let mut v: Vec<EmailTemplate> = self
            .by_id
            .values()
            .filter(|t| match &needle {
                Some(n) => t.id.to_lowercase().contains(n) || t.label.to_lowercase().contains(n),
                None => true,
            })
            .cloned()
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }
}

// ---------- widgets (etc/widget.xml) ----------

pub(crate) struct WidgetIndex {
    by_id: HashMap<String, Widget>,
}

impl WidgetIndex {
    pub fn build(modules: &[Module], vfs: &Vfs) -> Self {
        let mut by_id: HashMap<String, Widget> = HashMap::new();
        for (i, path, raws) in read_parse(modules, vfs, Area::Global, "widget.xml", parse::widget_xml) {
            let module = &modules[i].name;
            for r in raws {
                let source = Source {
                    module: module.clone(),
                    file: path.clone(),
                    line: r.line,
                    area: Area::Global,
                };
                let entry = by_id.entry(r.id.clone()).or_insert_with(|| Widget {
                    id: r.id.clone(),
                    label: String::new(),
                    description: None,
                    class: ClassName::new(String::new()),
                    parameters: Vec::new(),
                    containers: Vec::new(),
                    source,
                });
                // Merge non-empty; parameters by name, last declaration wins.
                if let Some(c) = r.class {
                    entry.class = c;
                }
                if !r.label.is_empty() {
                    entry.label = r.label;
                }
                if r.description.is_some() {
                    entry.description = r.description;
                }
                for p in r.parameters {
                    let param = WidgetParam {
                        name: p.name,
                        param_type: p.param_type,
                        required: p.required,
                        label: p.label,
                        source_model: p.source_model,
                        default: p.default,
                    };
                    match entry.parameters.iter_mut().find(|x| x.name == param.name) {
                        Some(existing) => *existing = param,
                        None => entry.parameters.push(param),
                    }
                }
                for c in r.containers {
                    if !entry.containers.contains(&c) {
                        entry.containers.push(c);
                    }
                }
            }
        }
        Self { by_id }
    }

    pub fn widget(&self, id: &str) -> Option<Widget> {
        self.by_id.get(id).cloned()
    }

    /// Widgets whose id or label contains `filter` (case-insensitive), sorted by id.
    pub fn widgets(&self, filter: Option<&str>) -> Vec<Widget> {
        let needle = filter.map(str::to_lowercase);
        let mut v: Vec<Widget> = self
            .by_id
            .values()
            .filter(|w| match &needle {
                Some(n) => w.id.to_lowercase().contains(n) || w.label.to_lowercase().contains(n),
                None => true,
            })
            .cloned()
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }
}

// ---------- layout (view/<area>/layout + theme <Module>/layout) ----------

pub(crate) struct LayoutIndex {
    /// (area, handle) -> contributions: module files in load order, then theme files.
    handles: HashMap<(Area, String), Vec<LayoutContribution>>,
    /// (area, included handle) -> handles whose `<update>` pulls it in.
    included_by: HashMap<(Area, String), Vec<String>>,
    /// (area, `Vendor_Module::path.phtml`) -> physical candidates + layout usages.
    templates: HashMap<(Area, String), Template>,
}

impl LayoutIndex {
    /// `themes` = discovered theme roots as `(id like "frontend/Magento/luma", dir)`.
    pub fn build(modules: &[Module], vfs: &Vfs, themes: &[(String, PathBuf)]) -> Self {
        // Enumerate every layout file with its layer and target areas. `view/base`
        // applies to both frontend and adminhtml.
        struct Job {
            layer: LayoutLayer,
            /// Module namespace used by short `template="path.phtml"` references.
            owner_module: ModuleName,
            areas: Vec<Area>,
            handle: String,
            path: PathBuf,
        }
        let mut jobs: Vec<Job> = Vec::new();
        let mut push_dir = |layer: &LayoutLayer, owner_module: &ModuleName, areas: &[Area], dir: PathBuf| {
            let Ok(entries) = std::fs::read_dir(&dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("xml") {
                    continue;
                }
                let Some(handle) = path.file_stem().and_then(|s| s.to_str()) else { continue };
                jobs.push(Job {
                    layer: layer.clone(),
                    owner_module: owner_module.clone(),
                    areas: areas.to_vec(),
                    handle: handle.to_string(),
                    path,
                });
            }
        };

        for m in modules.iter().filter(|m| m.enabled) {
            let layer = LayoutLayer::Module(m.name.clone());
            for (sub, areas) in [
                ("view/base/layout", vec![Area::Frontend, Area::Adminhtml]),
                ("view/frontend/layout", vec![Area::Frontend]),
                ("view/adminhtml/layout", vec![Area::Adminhtml]),
            ] {
                push_dir(&layer, &m.name, &areas, m.path.join(sub));
            }
        }
        for (id, dir) in themes {
            let area = if id.starts_with("adminhtml/") { Area::Adminhtml } else { Area::Frontend };
            let layer = LayoutLayer::Theme(id.clone());
            // Theme layout lives under `<theme>/<Vendor_Module>/layout/`.
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                let owner_module = ModuleName::new(entry.file_name().to_string_lossy().as_ref());
                let sub = entry.path().join("layout");
                if sub.is_dir() {
                    push_dir(&layer, &owner_module, &[area], sub);
                }
            }
        }

        // Parse everything in parallel; job order (modules in load order, themes after)
        // is preserved by the collect.
        let parsed: Vec<Vec<parse::RawLayoutOp>> = jobs
            .par_iter()
            .map(|j| {
                vfs.read_to_string(&j.path)
                    .map(|t| parse::layout_xml(&t))
                    .unwrap_or_default()
            })
            .collect();

        let mut handles: HashMap<(Area, String), Vec<LayoutContribution>> = HashMap::new();
        let mut included_by: HashMap<(Area, String), Vec<String>> = HashMap::new();
        let mut templates: HashMap<(Area, String), Template> = HashMap::new();
        for (job, raw_ops) in jobs.iter().zip(parsed) {
            for &area in &job.areas {
                let module = match &job.layer {
                    LayoutLayer::Module(m) => m.clone(),
                    LayoutLayer::Theme(t) => ModuleName::new(t.as_str()),
                };
                let ops: Vec<LayoutOp> = raw_ops
                    .iter()
                    .map(|r| LayoutOp {
                        kind: layout_kind(r.kind),
                        name: r.name.clone(),
                        class: r.class.clone(),
                        template: r.template.clone(),
                        parent: r.parent.clone(),
                        remove: r.remove,
                        source: Source {
                            module: module.clone(),
                            file: job.path.clone(),
                            line: r.line,
                            area,
                        },
                    })
                    .collect();
                for op in &ops {
                    if op.kind == LayoutOpKind::Update {
                        let list = included_by.entry((area, op.name.clone())).or_default();
                        if !list.contains(&job.handle) {
                            list.push(job.handle.clone());
                        }
                    }
                    if let Some(raw) = op.template.as_deref() {
                        let reference = normalize_template_ref(&job.owner_module, raw);
                        let template = templates
                            .entry((area, reference.clone()))
                            .or_insert_with(|| Template {
                                reference,
                                area,
                                files: Vec::new(),
                                usages: Vec::new(),
                            });
                        template.usages.push(TemplateUsage {
                            handle: job.handle.clone(),
                            block: op.name.clone(),
                            class: op.class.clone(),
                            source: op.source.clone(),
                        });
                    }
                }
                handles.entry((area, job.handle.clone())).or_default().push(LayoutContribution {
                    layer: job.layer.clone(),
                    file: job.path.clone(),
                    ops,
                });
            }
        }
        for list in included_by.values_mut() {
            list.sort();
        }

        // Catalog module templates, including files not currently referenced by layout.
        for m in modules.iter().filter(|m| m.enabled) {
            let layer = LayoutLayer::Module(m.name.clone());
            for (sub, areas) in [
                ("base", vec![Area::Frontend, Area::Adminhtml]),
                ("frontend", vec![Area::Frontend]),
                ("adminhtml", vec![Area::Adminhtml]),
            ] {
                add_template_files(
                    &mut templates,
                    &layer,
                    &m.name,
                    &areas,
                    &m.path.join("view").join(sub).join("templates"),
                );
            }
        }
        // A theme override's first directory is the module namespace it overrides.
        for (id, dir) in themes {
            let area = if id.starts_with("adminhtml/") { Area::Adminhtml } else { Area::Frontend };
            let layer = LayoutLayer::Theme(id.clone());
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                let module = ModuleName::new(entry.file_name().to_string_lossy().as_ref());
                add_template_files(
                    &mut templates,
                    &layer,
                    &module,
                    &[area],
                    &entry.path().join("templates"),
                );
            }
        }
        for template in templates.values_mut() {
            template.files.sort_by(|a, b| {
                let rank = |file: &TemplateFile| match file.layer {
                    LayoutLayer::Module(_) => 0,
                    LayoutLayer::Theme(_) => 1,
                };
                rank(a).cmp(&rank(b)).then_with(|| a.file.cmp(&b.file))
            });
            template.files.dedup_by(|a, b| a.file == b.file);
            template.usages.sort_by(|a, b| {
                (&a.handle, &a.block, &a.source.file, a.source.line)
                    .cmp(&(&b.handle, &b.block, &b.source.file, b.source.line))
            });
        }

        Self { handles, included_by, templates }
    }

    /// Every handle in `area` with its contributing-file count, sorted by handle.
    pub fn handles(&self, area: Area) -> Vec<(String, usize)> {
        let mut v: Vec<(String, usize)> = self
            .handles
            .iter()
            .filter(|((a, _), _)| *a == area)
            .map(|((_, h), c)| (h.clone(), c.len()))
            .collect();
        v.sort();
        v
    }

    /// One handle's full view.
    pub fn view(&self, handle: &str, area: Area) -> Option<LayoutView> {
        let contributions = self.handles.get(&(area, handle.to_string()))?.clone();
        let mut includes: Vec<String> = contributions
            .iter()
            .flat_map(|c| c.ops.iter())
            .filter(|o| o.kind == LayoutOpKind::Update)
            .map(|o| o.name.clone())
            .collect();
        includes.sort();
        includes.dedup();
        let included_by =
            self.included_by.get(&(area, handle.to_string())).cloned().unwrap_or_default();
        Some(LayoutView { handle: handle.to_string(), area, contributions, includes, included_by })
    }

    pub fn template(&self, reference: &str, area: Area) -> Option<Template> {
        self.templates.get(&(area, reference.to_string())).cloned()
    }

    pub fn templates(&self, area: Area, filter: Option<&str>) -> Vec<Template> {
        let needle = filter.map(str::to_lowercase);
        let mut out: Vec<_> = self
            .templates
            .iter()
            .filter(|((a, reference), _)| {
                *a == area
                    && needle
                        .as_ref()
                        .map_or(true, |n| reference.to_lowercase().contains(n))
            })
            .map(|(_, template)| template.clone())
            .collect();
        out.sort_by(|a, b| a.reference.cmp(&b.reference));
        out
    }
}

fn normalize_template_ref(module: &ModuleName, raw: &str) -> String {
    let raw = raw.trim().trim_start_matches('/');
    if raw.contains("::") {
        raw.to_string()
    } else {
        format!("{}::{raw}", module.as_str())
    }
}

fn add_template_files(
    templates: &mut HashMap<(Area, String), Template>,
    layer: &LayoutLayer,
    module: &ModuleName,
    areas: &[Area],
    root: &Path,
) {
    fn walk(out: &mut Vec<PathBuf>, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(out, &path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("phtml") {
                out.push(path);
            }
        }
    }

    let mut files = Vec::new();
    walk(&mut files, root);
    files.sort();
    for file in files {
        let Ok(rel) = file.strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
        let reference = format!("{}::{rel}", module.as_str());
        for &area in areas {
            let template = templates
                .entry((area, reference.clone()))
                .or_insert_with(|| Template {
                    reference: reference.clone(),
                    area,
                    files: Vec::new(),
                    usages: Vec::new(),
                });
            template.files.push(TemplateFile { layer: layer.clone(), file: file.clone() });
        }
    }
}

fn layout_kind(k: parse::RawLayoutOpKind) -> LayoutOpKind {
    use parse::RawLayoutOpKind as R;
    match k {
        R::Block => LayoutOpKind::Block,
        R::Container => LayoutOpKind::Container,
        R::ReferenceBlock => LayoutOpKind::ReferenceBlock,
        R::ReferenceContainer => LayoutOpKind::ReferenceContainer,
        R::Update => LayoutOpKind::Update,
        R::Move => LayoutOpKind::Move,
    }
}

#[cfg(test)]
mod template_tests {
    use super::LayoutIndex;
    use crate::ids::{Area, ModuleName};
    use crate::engine::vfs::Vfs;
    use crate::model::{LayoutLayer, Module, ModuleSource};

    #[test]
    fn joins_short_layout_reference_to_module_and_theme_files() {
        let root = std::env::temp_dir().join(format!(
            "magequery-template-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let module_root = root.join("Acme/Widget");
        let layout = module_root.join("view/frontend/layout/acme_index.xml");
        let module_file = module_root.join("view/frontend/templates/chip/view.phtml");
        let theme_root = root.join("theme");
        let theme_file = theme_root.join("Acme_Widget/templates/chip/view.phtml");
        for file in [&layout, &module_file, &theme_file] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        }
        std::fs::write(
            &layout,
            r#"<page><body><block name="chip" template="chip/view.phtml"/></body></page>"#,
        )
        .unwrap();
        std::fs::write(&module_file, "module").unwrap();
        std::fs::write(&theme_file, "theme").unwrap();

        let module = Module {
            name: ModuleName::new("Acme_Widget"),
            enabled: true,
            source: ModuleSource::App,
            path: module_root,
            sequence: Vec::new(),
            load_order: 0,
        };
        let index = LayoutIndex::build(
            &[module],
            &Vfs::default(),
            &[("frontend/Acme/theme".to_string(), theme_root)],
        );
        let template = index
            .template("Acme_Widget::chip/view.phtml", Area::Frontend)
            .unwrap();
        assert_eq!(template.files.len(), 2);
        assert_eq!(template.usages.len(), 1);
        assert_eq!(template.usages[0].handle, "acme_index");
        assert!(matches!(template.files[0].layer, LayoutLayer::Module(_)));
        assert!(matches!(template.files[1].layer, LayoutLayer::Theme(_)));

        std::fs::remove_dir_all(root).unwrap();
    }
}

// ---------- ui components (view/<area>/ui_component + theme <Module>/ui_component) ----------

pub(crate) struct UiComponentIndex {
    /// (area, component name) -> (kind from the first declaring file, contributions:
    /// module files in load order, then theme files).
    components: HashMap<(Area, String), (String, Vec<UiComponentContribution>)>,
}

impl UiComponentIndex {
    /// `themes` as in [`LayoutIndex::build`]. Component name = file stem; `view/base`
    /// applies to both areas. Only direct children of `ui_component/` are components
    /// (Magento_Ui's `ui_component/etc/definition/` holds component *type* definitions).
    pub fn build(modules: &[Module], vfs: &Vfs, themes: &[(String, PathBuf)]) -> Self {
        struct Job {
            layer: LayoutLayer,
            areas: Vec<Area>,
            name: String,
            path: PathBuf,
        }
        let mut jobs: Vec<Job> = Vec::new();
        let mut push_dir = |layer: &LayoutLayer, areas: &[Area], dir: PathBuf| {
            let Ok(entries) = std::fs::read_dir(&dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("xml") {
                    continue;
                }
                let Some(name) = path.file_stem().and_then(|s| s.to_str()) else { continue };
                jobs.push(Job {
                    layer: layer.clone(),
                    areas: areas.to_vec(),
                    name: name.to_string(),
                    path,
                });
            }
        };

        for m in modules.iter().filter(|m| m.enabled) {
            let layer = LayoutLayer::Module(m.name.clone());
            for (sub, areas) in [
                ("view/base/ui_component", vec![Area::Frontend, Area::Adminhtml]),
                ("view/frontend/ui_component", vec![Area::Frontend]),
                ("view/adminhtml/ui_component", vec![Area::Adminhtml]),
            ] {
                push_dir(&layer, &areas, m.path.join(sub));
            }
        }
        for (id, dir) in themes {
            let area = if id.starts_with("adminhtml/") { Area::Adminhtml } else { Area::Frontend };
            let layer = LayoutLayer::Theme(id.clone());
            // Theme overrides live under `<theme>/<Vendor_Module>/ui_component/`.
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                let sub = entry.path().join("ui_component");
                if sub.is_dir() {
                    push_dir(&layer, &[area], sub);
                }
            }
        }

        let parsed: Vec<parse::RawUiComponent> = jobs
            .par_iter()
            .map(|j| {
                vfs.read_to_string(&j.path)
                    .map(|t| parse::ui_component_xml(&t))
                    .unwrap_or(parse::RawUiComponent { kind: None, ops: Vec::new() })
            })
            .collect();

        let mut components: HashMap<(Area, String), (String, Vec<UiComponentContribution>)> =
            HashMap::new();
        for (job, raw) in jobs.iter().zip(parsed) {
            for &area in &job.areas {
                let module = match &job.layer {
                    LayoutLayer::Module(m) => m.clone(),
                    LayoutLayer::Theme(t) => ModuleName::new(t.as_str()),
                };
                let ops: Vec<UiComponentOp> = raw
                    .ops
                    .iter()
                    .map(|r| UiComponentOp {
                        element: r.element.clone(),
                        name: r.name.clone(),
                        class: r.class.clone(),
                        component: r.component.clone(),
                        form_element: r.form_element.clone(),
                        sort_order: r.sort_order.clone(),
                        label: r.label.clone(),
                        disabled: r.disabled,
                        visible: r.visible,
                        parent: r.parent.clone(),
                        depth: r.depth,
                        source: Source {
                            module: module.clone(),
                            file: job.path.clone(),
                            line: r.line,
                            area,
                        },
                    })
                    .collect();
                let entry = components
                    .entry((area, job.name.clone()))
                    .or_insert_with(|| (String::new(), Vec::new()));
                // Kind = the first declaring file's root element (load order).
                if entry.0.is_empty() {
                    if let Some(k) = &raw.kind {
                        entry.0 = k.clone();
                    }
                }
                entry.1.push(UiComponentContribution {
                    layer: job.layer.clone(),
                    file: job.path.clone(),
                    ops,
                });
            }
        }

        Self { components }
    }

    /// Every component in `area` as `(name, kind, contributing files)`, sorted by name.
    pub fn list(&self, area: Area) -> Vec<(String, String, usize)> {
        let mut v: Vec<(String, String, usize)> = self
            .components
            .iter()
            .filter(|((a, _), _)| *a == area)
            .map(|((_, n), (kind, c))| (n.clone(), kind.clone(), c.len()))
            .collect();
        v.sort();
        v
    }

    /// One component's full view.
    pub fn view(&self, name: &str, area: Area) -> Option<UiComponentView> {
        let (kind, contributions) = self.components.get(&(area, name.to_string()))?.clone();
        Some(UiComponentView { name: name.to_string(), kind, area, contributions })
    }
}
