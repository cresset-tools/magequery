//! Layout-layer navigation: template resolution through the theme-override candidates,
//! handle and block-name lookups — thin scans over core's merged layout index. Which
//! theme override *applies* is active-theme runtime state, so candidates are reported,
//! never resolved to one (the same honesty rule as `email-templates`).

use std::path::{Path, PathBuf};

use magequery_core::{Area, LayoutOp, LayoutOpKind, Magento, ModuleName};

/// `Vendor_Module::rel/path.phtml` split into its halves.
pub(crate) fn parse_template_ref(reference: &str) -> Option<(ModuleName, &str)> {
    let (module, rel) = reference.split_once("::")?;
    (!module.is_empty() && !rel.is_empty()).then(|| (ModuleName::new(module), rel))
}

/// The area a view file belongs to, from its path: `view/<area>/…` for module files,
/// the theme id's prefix for theme files. `base` (and anything unrecognized) queries as
/// frontend — the layout index folds base files into both areas anyway.
pub(crate) fn area_of_file(magento: &Magento, path: &Path) -> Area {
    let text = path.to_string_lossy();
    if text.contains("/view/adminhtml/") {
        return Area::Adminhtml;
    }
    if text.contains("/view/frontend/") || text.contains("/view/base/") {
        return Area::Frontend;
    }
    for (id, dir) in magento.themes() {
        if path.starts_with(&dir) {
            return if id.starts_with("adminhtml/") { Area::Adminhtml } else { Area::Frontend };
        }
    }
    Area::Frontend
}

/// A template reference normalized the way core's template index keys it: short paths
/// (`path.phtml`, no `::`) get the declaring layout file's owning module prefixed —
/// a module layout file's own module, or the `<Vendor_Module>` path segment for a
/// theme layout file. Mirrors core's `normalize_template_ref`.
pub(crate) fn normalize_ref(magento: &Magento, layout_file: &Path, reference: &str) -> String {
    let reference = reference.trim().trim_start_matches('/');
    if reference.contains("::") {
        return reference.to_string();
    }
    let module = magento
        .modules()
        .iter()
        .filter(|m| layout_file.starts_with(&m.path))
        .max_by_key(|m| m.path.as_os_str().len())
        .map(|m| m.name.clone())
        .or_else(|| {
            magento.themes().into_iter().find_map(|(_, dir)| {
                layout_file
                    .strip_prefix(&dir)
                    .ok()
                    .and_then(|rel| rel.components().next())
                    .and_then(|c| c.as_os_str().to_str())
                    .filter(|segment| segment.contains('_'))
                    .map(ModuleName::new)
            })
        });
    match module {
        Some(module) => format!("{module}::{reference}"),
        None => reference.to_string(),
    }
}

/// Every existing file a (normalized) template reference can resolve to. The template
/// index answers when any layout references it; the physical probe is the fallback for
/// templates no layout uses (an unreferenced theme override still deserves its lens).
pub(crate) fn resolve_template(
    magento: &Magento,
    reference: &str,
    area: Area,
) -> Vec<(String, PathBuf)> {
    if let Some(template) = magento.template(reference, area) {
        if !template.files.is_empty() {
            return template
                .files
                .into_iter()
                .map(|f| {
                    let label = match &f.layer {
                        magequery_core::LayoutLayer::Module(m) => format!("module {m}"),
                        magequery_core::LayoutLayer::Theme(id) => format!("theme {id}"),
                    };
                    (label, f.file)
                })
                .collect();
        }
    }
    resolve_physical(magento, reference, area)
}

/// Filesystem-probe resolution (module area/base file + every theme override).
fn resolve_physical(magento: &Magento, reference: &str, area: Area) -> Vec<(String, PathBuf)> {
    let Some((module_name, rel)) = parse_template_ref(reference) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(module) = magento.modules().iter().find(|m| m.name == module_name) {
        let area_dir = if area == Area::Adminhtml { "adminhtml" } else { "frontend" };
        for sub in [area_dir, "base"] {
            let candidate = module.path.join("view").join(sub).join("templates").join(rel);
            if candidate.is_file() {
                out.push((format!("module view/{sub}"), candidate));
            }
        }
    }
    let prefix = if area == Area::Adminhtml { "adminhtml/" } else { "frontend/" };
    for (id, dir) in magento.themes() {
        if !id.starts_with(prefix) {
            continue;
        }
        let candidate = dir.join(module_name.as_str()).join("templates").join(rel);
        if candidate.is_file() {
            out.push((format!("theme {id}"), candidate));
        }
    }
    out
}

/// The `Vendor_Module::rel` reference a `.phtml` file answers to: `(reference, area,
/// theme id when the file is a theme override)`.
pub(crate) fn template_ref_of_file(
    magento: &Magento,
    path: &Path,
) -> Option<(String, Area, Option<String>)> {
    let parts = |p: &Path| -> Vec<String> {
        p.components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect()
    };
    for module in magento.modules() {
        let Ok(rel) = path.strip_prefix(&module.path) else { continue };
        let segments = parts(rel);
        if segments.len() < 4 || segments[0] != "view" || segments[2] != "templates" {
            continue;
        }
        let area = match segments[1].as_str() {
            "adminhtml" => Area::Adminhtml,
            "frontend" | "base" => Area::Frontend,
            _ => continue,
        };
        return Some((
            format!("{}::{}", module.name, segments[3..].join("/")),
            area,
            None,
        ));
    }
    for (id, dir) in magento.themes() {
        let Ok(rel) = path.strip_prefix(&dir) else { continue };
        let segments = parts(rel);
        if segments.len() < 3 || !segments[0].contains('_') || segments[1] != "templates" {
            continue;
        }
        let area = if id.starts_with("adminhtml/") { Area::Adminhtml } else { Area::Frontend };
        return Some((
            format!("{}::{}", segments[0], segments[2..].join("/")),
            area,
            Some(id),
        ));
    }
    None
}

/// Every `(handle, op)` in `area` matching `pred` — a scan over the merged index
/// (module contributions in load order, then theme files).
pub(crate) fn ops_where(
    magento: &Magento,
    area: Area,
    mut pred: impl FnMut(&LayoutOp) -> bool,
) -> Vec<(String, LayoutOp)> {
    let mut out = Vec::new();
    for (handle, _) in magento.layout_handles(area) {
        let Some(view) = magento.layout(&handle, area) else { continue };
        for contribution in view.contributions {
            for op in contribution.ops {
                if pred(&op) {
                    out.push((handle.clone(), op));
                }
            }
        }
    }
    out
}

pub(crate) fn is_declaration(op: &LayoutOp) -> bool {
    matches!(op.kind, LayoutOpKind::Block | LayoutOpKind::Container)
}
