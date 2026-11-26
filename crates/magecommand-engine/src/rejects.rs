//! The di.xml `setup:di:compile` refuses to compile, so `di compile` refuses it
//! too instead of writing a tree the real compile never would.
//!
//! Magento checks exactly two things about the classes di.xml names, and both
//! with `class_exists`, which is false for an interface or a trait:
//!
//! - `Compiler\Config\Reader::generateCachePerScope`: every class the compile
//!   covers and every preference key is followed through the preferences to
//!   its end. A virtual type there is taken as is. Anything else must exist
//!   ("…but the latter does not exist.") and be part of the compile ("…has not
//!   been included in dependency injection compilation.").
//! - `Interception\PluginListGenerator::inheritPlugins`: every plugin that is
//!   not disabled, its virtual type taken to its class, must exist ("Plugin
//!   class … doesn't exist").
//!
//! Nothing else is checked there. A preference FOR a missing class, a `<type>`
//! of one, a virtual type of one (even one a preference points at) and an
//! object argument of one all compile, and fail at runtime if at all, so they
//! are not refused here.
//!
//! When this cannot tell whether a class exists, it counts it as existing: a
//! refusal Magento would not make breaks a build that works.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use magecommand_php::ClassKind;
use magequery_core::{DiExport, Magento, Source};

use crate::definitions::Definitions;

/// One thing `setup:di:compile` would stop on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub kind: RejectionKind,
    /// The di.xml file of the declaration at fault, relative to the store root
    /// (`/`-separated) where it lies inside it.
    pub file: String,
    /// Its 1-based line; 0 when unknown.
    pub line: u32,
    /// The areas whose configuration has it; empty when every area does.
    pub areas: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectionKind {
    /// `<preference for type>` whose type, followed through any further
    /// preferences, is not a class.
    PreferenceMissing { for_type: String, preference: String, not_a_class: Option<&'static str> },
    /// `<preference for type>` whose type exists but is outside every path the
    /// compile scans.
    PreferenceNotCompiled { for_type: String, preference: String },
    /// `<plugin type>` that is not a class; `via` is the virtual type the
    /// plugin names, when it names one.
    PluginClassMissing { class: String, plugin: String, target: String, via: Option<String> },
}

impl Rejection {
    /// `file:line`, or the file alone when the line is unknown.
    pub fn location(&self) -> String {
        match self.line {
            0 => self.file.clone(),
            line => format!("{}:{line}", self.file),
        }
    }

    /// Magento's message, without the location or areas the full form adds.
    pub fn message(&self) -> String {
        self.kind.to_string()
    }
}

impl RejectionKind {
    /// A stable kebab-case name for the kind, for machine-readable output.
    pub fn name(&self) -> &'static str {
        match self {
            RejectionKind::PreferenceMissing { .. } => "preference-missing",
            RejectionKind::PreferenceNotCompiled { .. } => "preference-not-compiled",
            RejectionKind::PluginClassMissing { .. } => "plugin-class-missing",
        }
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.location(), self.kind)?;
        if !self.areas.is_empty() {
            write!(f, " [{}]", self.areas.join(", "))?;
        }
        Ok(())
    }
}

impl fmt::Display for RejectionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Magento's own wording first, so a search for the message finds both.
        match self {
            RejectionKind::PreferenceMissing { for_type, preference, not_a_class } => {
                write!(
                    f,
                    "Preference declared for \"{for_type}\" as \"{preference}\", but the latter does not exist."
                )?;
                if let Some(kind) = not_a_class {
                    write!(f, " (it is {kind})")?;
                }
            }
            RejectionKind::PreferenceNotCompiled { for_type, preference } => write!(
                f,
                "Preference declared for \"{for_type}\" as \"{preference}\", but the latter has not been \
                 included in dependency injection compilation."
            )?,
            RejectionKind::PluginClassMissing { class, plugin, target, via } => {
                write!(f, "Plugin class {class} doesn't exist (plugin \"{plugin}\" on {target}")?;
                if let Some(via) = via {
                    write!(f, ", through virtual type {via}")?;
                }
                write!(f, ")")?;
            }
        }
        Ok(())
    }
}

/// Whether a class exists, as far as PHP's `class_exists` would say.
enum Exists {
    Yes,
    /// Declared, but as something `class_exists` is false for.
    NotAClass(&'static str),
    No,
}

struct Lookup<'a> {
    magento: &'a Magento,
    defs: &'a Definitions,
    classmap: HashMap<String, PathBuf>,
}

impl Lookup<'_> {
    fn class_exists(&self, name: &str) -> Exists {
        if let Some(record) = self.defs.get(name) {
            return kind_exists(record.meta.kind);
        }
        if crate::definitions::is_internal_class(name) {
            return Exists::Yes;
        }
        // A global-namespace name is a PHP built-in we may not model, and a
        // name the compile generates is generated on demand by `class_exists`.
        if !name.contains('\\') || crate::codegen::classify(name).is_some() {
            return Exists::Yes;
        }
        let file = self
            .magento
            .class_file(&magequery_core::ClassName::new(name.to_owned()))
            .or_else(|| self.classmap.get(name).cloned());
        let Some(file) = file else { return Exists::No };
        let Ok(src) = std::fs::read(&file) else { return Exists::Yes };
        // A file that does not declare the class may still do so in a way the
        // parser does not follow; count it as declared.
        magecommand_php::parse_file(&src)
            .declarations
            .iter()
            .find(|d| d.fqcn.eq_ignore_ascii_case(name))
            .map_or(Exists::Yes, |d| kind_exists(d.kind))
    }
}

fn kind_exists(kind: ClassKind) -> Exists {
    match kind {
        ClassKind::Interface => Exists::NotAClass("an interface"),
        ClassKind::Trait => Exists::NotAClass("a trait"),
        _ => Exists::Yes,
    }
}

/// Follow `map` from `name` to its end, as `ObjectManager\Config` does; stops
/// at a cycle (which Magento reports on its own). Returns every hop.
fn chain<'a>(map: &HashMap<&'a str, &'a str>, name: &'a str) -> Vec<&'a str> {
    let mut hops = vec![name];
    let mut seen = HashSet::from([name]);
    while let Some(next) = map.get(hops[hops.len() - 1]) {
        if !seen.insert(*next) {
            break;
        }
        hops.push(next);
    }
    hops
}

/// Everything `setup:di:compile` would refuse in `magento`'s configuration,
/// over the compile's final class universe `defs` (generated classes folded
/// in, so a preference for a factory finds it).
pub fn check(magento: &Magento, defs: &Definitions, root: &Path) -> Vec<Rejection> {
    let lookup = Lookup { magento, defs, classmap: crate::definitions::composer_classmap(root) };
    let scanned: HashSet<String> = defs.scanned.iter().map(|n| n.to_ascii_lowercase()).collect();

    // (kind, file, line) -> areas it was found in.
    let mut found: Vec<((RejectionKind, String, u32), BTreeSet<String>)> = Vec::new();
    let mut record = |kind: RejectionKind, source: &Source, area: &str| {
        let key = (kind, relative(&source.file, root), source.line);
        match found.iter_mut().find(|(k, _)| *k == key) {
            Some((_, areas)) => {
                areas.insert(area.to_owned());
            }
            None => found.push((key, BTreeSet::from([area.to_owned()]))),
        }
    };

    let mut areas: Vec<(String, std::borrow::Cow<'_, DiExport>)> = crate::areaconfig::AREA_CODES
        .iter()
        .map(|(area, code)| ((*code).to_owned(), std::borrow::Cow::Borrowed(magento.di_export_ref(*area))))
        .collect();
    for code in crate::areaconfig::custom_area_codes(magento) {
        let export = magento.di_export_custom_area(&code);
        areas.push((code, std::borrow::Cow::Owned(export)));
    }

    for (area, export) in &areas {
        let preferences: HashMap<&str, &str> =
            export.preferences.iter().map(|p| (p.for_type.as_str(), p.prefer.as_str())).collect();
        let preference_keys: HashSet<String> =
            preferences.keys().map(|k| k.to_ascii_lowercase()).collect();
        let virtual_types: HashMap<&str, &str> =
            export.virtual_types.iter().map(|v| (v.name.as_str(), v.base.as_str())).collect();

        for decl in &export.preferences {
            let hops = chain(&preferences, decl.for_type.as_str());
            // Attributed to the declaration that names the end, once.
            let end = hops[hops.len() - 1];
            if hops.len() < 2 || hops[hops.len() - 2] != decl.for_type.as_str() {
                continue;
            }
            if virtual_types.contains_key(end) {
                continue;
            }
            let (for_type, preference) = (decl.for_type.as_str().to_owned(), end.to_owned());
            match lookup.class_exists(end) {
                Exists::No => record(
                    RejectionKind::PreferenceMissing { for_type, preference, not_a_class: None },
                    &decl.source,
                    area,
                ),
                Exists::NotAClass(kind) => record(
                    RejectionKind::PreferenceMissing { for_type, preference, not_a_class: Some(kind) },
                    &decl.source,
                    area,
                ),
                Exists::Yes => {
                    let lower = end.to_ascii_lowercase();
                    let outside = defs.get(end).is_none_or(|r| r.meta.kind == ClassKind::Class)
                        && !crate::definitions::is_internal_class(end)
                        && end.contains('\\')
                        && crate::codegen::classify(end).is_none()
                        && !scanned.contains(&lower)
                        && !preference_keys.contains(&lower);
                    if outside {
                        record(
                            RejectionKind::PreferenceNotCompiled { for_type, preference },
                            &decl.source,
                            area,
                        );
                    }
                }
            }
        }

        for plugin in &export.plugins {
            let Some(class) = &plugin.class else { continue };
            if plugin.disabled {
                continue;
            }
            let hops = chain(&virtual_types, class.as_str());
            let resolved = hops[hops.len() - 1];
            if !matches!(lookup.class_exists(resolved), Exists::No | Exists::NotAClass(_)) {
                continue;
            }
            record(
                RejectionKind::PluginClassMissing {
                    class: resolved.to_owned(),
                    plugin: plugin.name.clone(),
                    target: plugin.target.as_str().to_owned(),
                    via: (hops.len() > 1).then(|| class.as_str().to_owned()),
                },
                &plugin.source,
                area,
            );
        }
    }

    let every_area = areas.len();
    found
        .into_iter()
        .map(|((kind, file, line), in_areas)| Rejection {
            kind,
            file,
            line,
            areas: if in_areas.len() == every_area { Vec::new() } else { in_areas.into_iter().collect() },
        })
        .collect()
}

fn relative(file: &Path, root: &Path) -> String {
    file.strip_prefix(root).unwrap_or(file).to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejection(line: u32, areas: &[&str]) -> Rejection {
        Rejection {
            kind: RejectionKind::PluginClassMissing {
                class: "Acme\\Gone".into(),
                plugin: "p".into(),
                target: "Acme\\Real".into(),
                via: None,
            },
            file: "app/code/Acme/Probe/etc/di.xml".into(),
            line,
            areas: areas.iter().map(|a| (*a).to_owned()).collect(),
        }
    }

    #[test]
    fn the_message_is_magentos_without_where() {
        let r = rejection(3, &["frontend"]);
        assert_eq!(r.message(), "Plugin class Acme\\Gone doesn't exist (plugin \"p\" on Acme\\Real)");
        assert_eq!(r.kind.name(), "plugin-class-missing");
        assert_eq!(r.to_string(), format!("app/code/Acme/Probe/etc/di.xml:3: {} [frontend]", r.message()));
    }

    #[test]
    fn an_unknown_line_is_left_out() {
        assert_eq!(rejection(0, &[]).location(), "app/code/Acme/Probe/etc/di.xml");
    }
}
