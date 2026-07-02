//! EAV attribute inspection — the static half. Attributes are runtime data (the
//! `eav_attribute` tables), but their *origin* is code: `$eavSetup->addAttribute(...)`
//! calls in setup scripts. This index scans every enabled module's `Setup/` tree
//! (patches + legacy Install/UpgradeData) for literal add/update/removeAttribute calls,
//! giving the DB rows source provenance. Core catalog attributes won't appear — Magento
//! installs those from data arrays (`CategorySetup::getDefaultEntities`), not
//! `addAttribute`; the scan's value is third-party and project attributes.

use rayon::prelude::*;

use crate::doctor;
use crate::ids::Area;
use crate::model::{EavSetupKind, EavSetupProp, EavSetupRef, EavValueKind, Module};
use crate::parse::LineMap;
use crate::php;
use crate::source::Source;

pub(crate) struct EavSetupIndex {
    /// All calls, in module load order (then file path within a module).
    pub refs: Vec<EavSetupRef>,
}

impl EavSetupIndex {
    pub fn build(modules: &[Module]) -> Self {
        let refs: Vec<EavSetupRef> = modules
            .par_iter()
            .filter(|m| m.enabled)
            .map(|m| {
                let base = m.path.join("Setup");
                if !base.is_dir() {
                    return Vec::new();
                }
                let mut files = Vec::new();
                doctor::walk_php(&base, 0, &mut |p| files.push(p.to_path_buf()));
                files.sort();
                let mut out = Vec::new();
                for path in files {
                    let Ok(src) = std::fs::read_to_string(&path) else { continue };
                    if !src.contains("Attribute(") {
                        continue;
                    }
                    let lines = LineMap::new(&src);
                    for call in php::eav_setup_calls(&src) {
                        out.push(EavSetupRef {
                            kind: match call.kind {
                                php::EavCallKind::Add => EavSetupKind::Add,
                                php::EavCallKind::Update => EavSetupKind::Update,
                                php::EavCallKind::Remove => EavSetupKind::Remove,
                            },
                            entity: call.entity,
                            entity_known: call.entity_known,
                            code: call.code,
                            props: call.props.into_iter().map(to_prop).collect(),
                            source: Source {
                                module: m.name.clone(),
                                file: path.clone(),
                                line: lines.line(call.offset),
                                area: Area::Global,
                            },
                        });
                    }
                }
                out
            })
            .flatten()
            .collect();
        Self { refs }
    }
}

fn to_prop((key, v): (String, php::EavValue)) -> EavSetupProp {
    let (kind, value) = match v {
        php::EavValue::Str(s) => (EavValueKind::Str, s),
        php::EavValue::Num(n) => (EavValueKind::Num, n),
        php::EavValue::Bool(b) => (EavValueKind::Bool, b.to_string()),
        php::EavValue::Null => (EavValueKind::Null, "null".to_string()),
        php::EavValue::Class(c) => (EavValueKind::Class, c.as_str().to_string()),
        php::EavValue::Other(t) => (EavValueKind::Other, t),
    };
    EavSetupProp { key, value, kind }
}

/// Everyday aliases for entity-type codes (`eav product` reads better than
/// `eav catalog_product`); unknown strings pass through unchanged.
#[cfg(feature = "db")]
pub(crate) fn resolve_entity_alias(s: &str) -> String {
    match s.to_lowercase().as_str() {
        "product" | "products" => "catalog_product".to_string(),
        "category" | "categories" => "catalog_category".to_string(),
        "address" | "addresses" => "customer_address".to_string(),
        other => other.to_string(),
    }
}
