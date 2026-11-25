//! How the compiled area files are laid out: each one whole, or (Mage-OS 4.0)
//! every area but `global` as the entries that differ from `global`.
//!
//! Mage-OS 4.0's `Area::doOperation` writes `global.php` in full and passes
//! every other area through `extractDiff`, which keeps, per merged section
//! (`Compiled::MERGED_SECTIONS`: `arguments`, `instanceTypes`, `preferences`,
//! `lazyTypes`), the keys global lacks or holds a different (`!==`) value for,
//! copies any other section whole, and marks the file
//! `'_extends' => 'global'`. When an area lacks one of global's sections, or a
//! key of a merged one, a diff could not say so, and the area is written in
//! full. `ConfigLoader\Compiled::load` merges a marked file onto its base with
//! `array_replace` per merged section; a file without the mark is used as is.
//!
//! So a 4.0 runtime reads either layout, and the choice is memory against
//! nothing: on a 1,000-module store the diffs took OPcache from 377 MB to
//! 292 MB and `generated/metadata` from 70 MB to 12 MB, for a second include
//! and an `array_replace` on each request (about 0.1 ms). An older runtime
//! reads `_extends` as a section and drops everything global holds, so it
//! must never get a diff.
//!
//! `di verify` compares configuration, not layout: when either tree holds a
//! diff, both trees' area files are brought to one form first, see
//! [`CompareTrees`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::areaconfig::AreaFile;
use crate::lazytypes::{Platform, Target};
use crate::phpexport::PhpValue;

/// The key a diff file names its base area under (`CompiledLoader::EXTENDS_KEY`).
pub const EXTENDS_KEY: &str = "_extends";

/// `Compiled::MERGED_SECTIONS`: the sections a diff holds per key.
const MERGED_SECTIONS: [&str; 4] = ["arguments", "instanceTypes", "preferences", "lazyTypes"];

/// The class whose `extractDiff` makes a checkout's compiler write diffs,
/// relative to `setup/src`.
const AREA_OPERATION: &str = "Magento/Setup/Module/Di/App/Task/Operation/Area.php";

/// The layout `di compile --area-config` takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AreaLayout {
    /// Every area file whole: what every release before Mage-OS 4.0 writes.
    Full,
    /// `global` whole, every other area as its differences from it.
    Diff,
}

impl AreaLayout {
    /// Every layout, by the name `--area-config` takes.
    pub const ALL: [(&'static str, AreaLayout); 2] = [("full", AreaLayout::Full), ("diff", AreaLayout::Diff)];

    /// The names `--area-config` takes.
    pub fn names() -> impl Iterator<Item = &'static str> {
        Self::ALL.iter().map(|(n, _)| *n)
    }

    /// The layout a compile for `target` writes: the one asked for, else what
    /// the platform's own compiler writes. `None` when [`AreaLayout::Diff`] is
    /// asked for a platform whose runtime cannot read it.
    pub fn resolve(root: &Path, target: &Target) -> Option<AreaLayout> {
        let reads_diffs = target.platform.map_or_else(|| setup_writes_diffs(root), Platform::reads_area_diffs);
        match target.area_layout {
            None if reads_diffs => Some(AreaLayout::Diff),
            None => Some(AreaLayout::Full),
            Some(AreaLayout::Diff) if !reads_diffs => None,
            Some(layout) => Some(layout),
        }
    }
}

impl std::str::FromStr for AreaLayout {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Self::ALL.iter().find(|(n, _)| *n == s).map(|(_, l)| *l).ok_or_else(|| {
            let names: Vec<_> = Self::names().collect();
            format!("`{s}`: expected one of {}", names.join(", "))
        })
    }
}

/// Whether the checkout's `setup:di:compile` writes diffs: its `Area`
/// operation has `extractDiff`. Read off `setup/` the way [`crate::lazytypes`]
/// reads the lazy pass; the runtime that reads them ships in the same release.
fn setup_writes_diffs(root: &Path) -> bool {
    ["setup/src", "vendor/magento/magento2-base/setup/src"].iter().any(|dir| {
        std::fs::read_to_string(root.join(dir).join(AREA_OPERATION)).is_ok_and(|src| src.contains("extractDiff"))
    })
}

/// `area` rendered as its differences from `global`, the way `extractDiff`
/// writes it; whole when it lacks a section or a merged key of global's.
pub fn render_diff(area: &AreaFile, global: &AreaFile) -> String {
    if !keeps_every_global_entry(area, global) {
        return area.render();
    }
    let arguments: Vec<(&String, &PhpValue)> = area
        .arguments
        .iter()
        .filter(|(k, v)| global.arguments.get(*k).is_none_or(|g| !identical(g, v)))
        .collect();
    let differing = |entries: &[(String, String)], base: &[(String, String)]| -> Vec<(String, String)> {
        let base: HashMap<&str, &str> = base.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        entries.iter().filter(|(k, v)| base.get(k.as_str()) != Some(&v.as_str())).cloned().collect()
    };
    let preferences = differing(&area.preferences, &global.preferences);
    let instance_types = differing(&area.instance_types, &global.instance_types);
    let lazy = area.lazy.as_ref().map(|(name, types)| {
        let types = match &global.lazy {
            // Merged: only the classes global does not list. Every value is `true`.
            Some((g_name, g_types)) if MERGED_SECTIONS.contains(name) && g_name == name => {
                let base: HashSet<&str> = g_types.iter().map(String::as_str).collect();
                types.iter().filter(|t| !base.contains(t.as_str())).cloned().collect()
            }
            // `nonLazyTypes` is not merged, so the area's is copied whole.
            _ => types.clone(),
        };
        (*name, types)
    });
    crate::phpexport::area_file_to_php(
        Some("global"),
        arguments.into_iter(),
        &preferences,
        &instance_types,
        lazy.as_ref().map(|(name, types)| (*name, types.as_slice())),
    )
}

/// `keepsEveryGlobalEntry`: the area has every section global has, and every
/// key of global's merged sections.
fn keeps_every_global_entry(area: &AreaFile, global: &AreaFile) -> bool {
    let keys = |entries: &[(String, String)], base: &[(String, String)]| {
        let have: HashSet<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
        base.iter().all(|(k, _)| have.contains(k.as_str()))
    };
    let lazy = match (&global.lazy, &area.lazy) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some((g_name, _)), Some((name, _))) if g_name != name => false,
        (Some((name, g_types)), Some((_, types))) => {
            !MERGED_SECTIONS.contains(name) || {
                let have: HashSet<&str> = types.iter().map(String::as_str).collect();
                g_types.iter().all(|t| have.contains(t.as_str()))
            }
        }
    };
    lazy && global.arguments.keys().all(|k| area.arguments.contains_key(k))
        && keys(&area.preferences, &global.preferences)
        && keys(&area.instance_types, &global.instance_types)
}

/// PHP's `===` on two compiled values. An object is a fresh instance on each
/// side, so never identical, nor is an array holding one.
fn identical(a: &PhpValue, b: &PhpValue) -> bool {
    match (a, b) {
        (PhpValue::Object { .. }, _) | (_, PhpValue::Object { .. }) => false,
        (PhpValue::Array(x), PhpValue::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|((kx, vx), (ky, vy))| kx == ky && identical(vx, vy))
        }
        (PhpValue::Float(x), PhpValue::Float(y)) => x == y,
        _ => a == b,
    }
}

// ---------------------------------------------------------------------------
// Comparing trees: `di verify`.
// ---------------------------------------------------------------------------

/// The two trees `di verify` compares, with their area files in one form when
/// either holds a diff. Each such tree is mirrored into a temporary directory
/// (symlinks, apart from the area files), deleted when this is dropped.
pub struct CompareTrees {
    pub archive: PathBuf,
    pub output: PathBuf,
    /// Whether each tree held a diff, to tell the reader the layouts differed.
    pub archive_diffs: bool,
    pub output_diffs: bool,
    _mirrors: Vec<tempfile::TempDir>,
}

impl CompareTrees {
    /// The trees as given when neither holds a diff, or both mirrored with
    /// every non-global area file in its canonical form: the diff from the
    /// tree's own `global`, merged back onto it. A whole file and the diff
    /// Mage-OS writes for it come out the same, whichever layout each tree used.
    pub fn new(archive: &Path, output: &Path) -> std::io::Result<Self> {
        let (a_dir, o_dir) = (area_dir(archive), area_dir(output));
        let archive_diffs = a_dir.as_deref().is_some_and(holds_diff);
        let output_diffs = o_dir.as_deref().is_some_and(holds_diff);
        let mut trees = CompareTrees {
            archive: archive.to_path_buf(),
            output: output.to_path_buf(),
            archive_diffs,
            output_diffs,
            _mirrors: Vec::new(),
        };
        if !archive_diffs && !output_diffs {
            return Ok(trees);
        }
        for (root, dir, slot) in [(archive, a_dir, &mut trees.archive), (output, o_dir, &mut trees.output)] {
            let Some(dir) = dir else { continue };
            let mirror = tempfile::tempdir()?;
            mirror_tree(root, &dir, mirror.path())?;
            *slot = mirror.path().to_path_buf();
            trees._mirrors.push(mirror);
        }
        Ok(trees)
    }
}

/// The directory holding a tree's area files: the tree itself (a
/// `generated/metadata` given directly) or its `metadata/`.
fn area_dir(root: &Path) -> Option<PathBuf> {
    [root.to_path_buf(), root.join("metadata")].into_iter().find(|d| d.join("global.php").is_file())
}

fn holds_diff(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    entries.flatten().any(|e| {
        let mut head = [0u8; 64];
        let n = std::fs::File::open(e.path()).and_then(|mut f| std::io::Read::read(&mut f, &mut head)).unwrap_or(0);
        is_diff(&String::from_utf8_lossy(&head[..n]))
    })
}

fn is_diff(text: &str) -> bool {
    text.starts_with(&format!("<?php return array (\n  '{EXTENDS_KEY}' => "))
}

/// Mirror `root` into `to`: every entry a symlink, apart from `area_dir`,
/// which is recreated with its non-global area files canonicalized.
fn mirror_tree(root: &Path, area_dir: &Path, to: &Path) -> std::io::Result<()> {
    let canonical_dir = if area_dir == root {
        to.to_path_buf()
    } else {
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            if entry.path() != area_dir {
                symlink(&entry.path(), &to.join(entry.file_name()))?;
            }
        }
        let dir = to.join(area_dir.file_name().expect("metadata"));
        std::fs::create_dir(&dir)?;
        dir
    };
    let global = std::fs::read_to_string(area_dir.join("global.php"))?;
    for entry in std::fs::read_dir(area_dir)? {
        let entry = entry?;
        let path = entry.path();
        let canonical = (entry.file_name() != "global.php")
            .then(|| std::fs::read_to_string(&path).ok())
            .flatten()
            .and_then(|text| canonical_area_text(&text, &global, area_dir));
        match canonical {
            Some(text) => std::fs::write(canonical_dir.join(entry.file_name()), text)?,
            None => symlink(&path, &canonical_dir.join(entry.file_name()))?,
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(from: &Path, to: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(from, to)
}

#[cfg(windows)]
fn symlink(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::os::windows::fs::symlink_dir(from, to)
    } else {
        std::os::windows::fs::symlink_file(from, to)
    }
}

/// An area file's canonical text: its diff from `global` (as is, when it is
/// one), merged back onto its base. `None` for a file that is not an area
/// file, or whose base is not in `dir`.
fn canonical_area_text(text: &str, global: &str, dir: &Path) -> Option<String> {
    let file = parse(text)?;
    if file.section("arguments").is_none() && file.section(EXTENDS_KEY).is_none() {
        return None;
    }
    let global = parse(global)?;
    let diff = if file.section(EXTENDS_KEY).is_some() { file } else { extract_diff(file, &global) };
    let Some(base) = diff.extends() else { return Some(render(&diff.sections)) };
    let base_text;
    let base = if base == "global" {
        global
    } else {
        base_text = std::fs::read_to_string(dir.join(format!("{base}.php"))).ok()?;
        parse(&base_text)?
    };
    if base.section(EXTENDS_KEY).is_some() {
        return None;
    }
    Some(render(&merge(base, diff).sections))
}

/// A `var_export`ed area file, split into its top-level entries (sections),
/// each array section further into its entries. Text slices of the file.
struct Parsed<'a> {
    sections: Vec<Section<'a>>,
}

struct Section<'a> {
    /// The key as written, quotes and escapes included.
    key: &'a str,
    value: Value<'a>,
}

enum Value<'a> {
    /// An array: its entries, each `(key, the whole entry's text)`.
    Map(Vec<(&'a str, &'a str)>),
    /// Anything else: the whole entry's text.
    Other(&'a str),
}

impl<'a> Parsed<'a> {
    fn section(&self, name: &str) -> Option<&Section<'a>> {
        self.sections.iter().find(|s| unquote(s.key) == Some(name))
    }

    /// The area a diff names as its base.
    fn extends(&self) -> Option<String> {
        let Value::Other(text) = self.section(EXTENDS_KEY)?.value else { return None };
        let value = text.split_once(" => ")?.1;
        unquote(value).map(str::to_owned)
    }
}

const HEADER: &str = "<?php return array (\n";
const MAP_OPEN: &str = "\n  array (\n";

fn parse(text: &str) -> Option<Parsed<'_>> {
    let body = text.strip_prefix(HEADER)?;
    let body = body.trim_end().strip_suffix(";")?.strip_suffix(")")?;
    let mut sections = Vec::new();
    for item in items(body)? {
        let key = entry_key(item)?;
        let value = &item[item.find(" => ")? + 4..];
        let value = match value.strip_prefix(MAP_OPEN).and_then(|v| v.strip_suffix("  )")) {
            Some(inner) => {
                let entries = items(inner)?;
                Value::Map(entries.into_iter().map(|e| Some((entry_key(e)?, e))).collect::<Option<_>>()?)
            }
            None => Value::Other(item),
        };
        sections.push(Section { key: key.trim_start(), value });
    }
    Some(Parsed { sections })
}

/// Split `var_export`ed array entries: each ends in `,\n` outside any string
/// or bracket. `None` when the text does not split cleanly.
fn items(text: &str) -> Option<Vec<&str>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut depth, mut i) = (0usize, 0i32, 0usize);
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(_) if b == b'\\' => i += 1,
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None => match b {
                b'\'' | b'"' => quote = Some(b),
                b'(' => depth += 1,
                b')' => depth -= 1,
                b',' if depth == 0 && bytes.get(i + 1) == Some(&b'\n') => {
                    out.push(&text[start..i]);
                    start = i + 2;
                    i += 1;
                }
                _ => {}
            },
        }
        i += 1;
    }
    (start == bytes.len() && quote.is_none() && depth == 0).then_some(out)
}

/// The key of one entry, as written: a quoted string or an integer.
fn entry_key(item: &str) -> Option<&str> {
    let trimmed = item.trim_start();
    let offset = item.len() - trimmed.len();
    let bytes = trimmed.as_bytes();
    let end = if bytes.first() == Some(&b'\'') {
        let mut i = 1;
        loop {
            match bytes.get(i)? {
                b'\\' => i += 2,
                b'\'' => break i + 1,
                _ => i += 1,
            }
        }
    } else {
        trimmed.find(' ')?
    };
    trimmed[end..].starts_with(" => ").then(|| &item[offset..offset + end])
}

/// A plain quoted key's content (`'arguments'` → `arguments`).
fn unquote(key: &str) -> Option<&str> {
    key.strip_prefix('\'')?.strip_suffix('\'').filter(|k| !k.contains(['\\', '\'']))
}

fn is_merged(section: &Section) -> bool {
    unquote(section.key).is_some_and(|k| MERGED_SECTIONS.contains(&k))
}

/// `extractDiff` over text. Entries compare by their text, which an object
/// matches where PHP's `!==` would not; both sides of a comparison go
/// through here, so it is the same either way.
fn extract_diff<'a>(area: Parsed<'a>, global: &Parsed<'a>) -> Parsed<'a> {
    let keeps_every = global.sections.iter().all(|g| match area.sections.iter().find(|s| s.key == g.key) {
        None => false,
        Some(s) => match (&g.value, &s.value) {
            (Value::Map(g_entries), Value::Map(entries)) if is_merged(g) => {
                let have: HashSet<&str> = entries.iter().map(|(k, _)| *k).collect();
                g_entries.iter().all(|(k, _)| have.contains(k))
            }
            _ => true,
        },
    });
    if !keeps_every {
        return area;
    }
    let mut sections = vec![Section { key: "'_extends'", value: Value::Other("  '_extends' => 'global'") }];
    for section in area.sections {
        let base = global.sections.iter().find(|g| g.key == section.key);
        let merged = is_merged(&section);
        let value = match (section.value, base.map(|b| &b.value)) {
            (Value::Map(entries), base) if merged => {
                let base: HashMap<&str, &str> = match base {
                    Some(Value::Map(b)) => b.iter().map(|(k, e)| (*k, *e)).collect(),
                    _ => HashMap::new(),
                };
                Value::Map(entries.into_iter().filter(|(k, e)| base.get(k) != Some(e)).collect())
            }
            (value, _) => value,
        };
        sections.push(Section { key: section.key, value });
    }
    Parsed { sections }
}

/// `ConfigLoader\Compiled::merge`: the diff's merged sections `array_replace`d
/// onto the base's, any other entry replacing the base's.
fn merge<'a>(base: Parsed<'a>, diff: Parsed<'a>) -> Parsed<'a> {
    let mut sections = base.sections;
    for section in diff.sections {
        if unquote(section.key) == Some(EXTENDS_KEY) {
            continue;
        }
        let merged = is_merged(&section);
        match sections.iter_mut().find(|s| s.key == section.key) {
            Some(existing) => match (&mut existing.value, section.value) {
                (Value::Map(entries), Value::Map(delta)) if merged => {
                    let mut at: HashMap<&str, usize> = entries.iter().enumerate().map(|(i, (k, _))| (*k, i)).collect();
                    for (k, e) in delta {
                        match at.get(k) {
                            Some(&i) => entries[i] = (k, e),
                            None => {
                                at.insert(k, entries.len());
                                entries.push((k, e));
                            }
                        }
                    }
                }
                (slot, value) => *slot = value,
            },
            None => sections.push(section),
        }
    }
    Parsed { sections }
}

fn render(sections: &[Section]) -> String {
    let mut out = String::from(HEADER);
    for section in sections {
        match &section.value {
            Value::Map(entries) => {
                out.push_str("  ");
                out.push_str(section.key);
                out.push_str(" => ");
                out.push_str(MAP_OPEN);
                for (_, entry) in entries {
                    out.push_str(entry);
                    out.push_str(",\n");
                }
                out.push_str("  ),\n");
            }
            Value::Other(text) => {
                out.push_str(text);
                out.push_str(",\n");
            }
        }
    }
    out.push_str(");");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phpexport::PhpKey;

    fn area(arguments: &[(&str, PhpValue)], preferences: &[(&str, &str)], lazy: Option<&[&str]>) -> AreaFile {
        AreaFile {
            arguments: arguments.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect(),
            preferences: preferences.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
            instance_types: Vec::new(),
            lazy: lazy.map(|t| ("lazyTypes", t.iter().map(|s| (*s).to_owned()).collect())),
            findings: Vec::new(),
            omitted: Vec::new(),
            lost_arguments: Vec::new(),
        }
    }

    fn object() -> PhpValue {
        PhpValue::Object { class: "Acme\\Thing".into(), props: Vec::new() }
    }

    #[test]
    fn a_diff_keeps_what_differs_from_global() {
        let global = area(&[("A", PhpValue::Null), ("B", PhpValue::Int(1))], &[("I", "G")], Some(&["A", "B"]));
        let front = area(
            &[("A", PhpValue::Null), ("B", PhpValue::Int(2)), ("C", PhpValue::Null)],
            &[("I", "F")],
            Some(&["A", "B", "C"]),
        );
        let diff = render_diff(&front, &global);
        assert_eq!(
            diff,
            "<?php return array (\n  '_extends' => 'global',\n  'arguments' => \n  array (\n    'B' => 2,\n    \
             'C' => NULL,\n  ),\n  'preferences' => \n  array (\n    'I' => 'F',\n  ),\n  'instanceTypes' => \n  \
             array (\n  ),\n  'lazyTypes' => \n  array (\n    'C' => true,\n  ),\n);"
        );
    }

    #[test]
    fn an_object_is_never_identical() {
        let holds = |v| PhpValue::Array(vec![(PhpKey::Int(0), v)]);
        let global = area(&[("A", object()), ("B", holds(object())), ("C", holds(PhpValue::Int(1)))], &[], None);
        let diff = render_diff(&global, &global);
        assert!(diff.contains("'A' =>") && diff.contains("'B' =>"), "{diff}");
        assert!(!diff.contains("'C' =>"), "{diff}");
    }

    #[test]
    fn an_area_missing_a_global_entry_is_written_whole() {
        let global = area(&[("A", PhpValue::Null), ("B", PhpValue::Null)], &[], None);
        let front = area(&[("A", PhpValue::Null)], &[], None);
        assert_eq!(render_diff(&front, &global), front.render());

        let global = area(&[], &[], Some(&["A"]));
        assert_eq!(render_diff(&area(&[], &[], Some(&[])), &global), area(&[], &[], Some(&[])).render());
        assert_eq!(render_diff(&area(&[], &[], None), &global), area(&[], &[], None).render());
    }

    #[test]
    fn a_diff_merged_back_is_the_whole_file() {
        let global = area(&[("A", PhpValue::Null), ("B", PhpValue::Int(1))], &[("I", "G")], Some(&["A", "B"]));
        let front = area(
            &[("A", PhpValue::Null), ("B", PhpValue::Int(2)), ("C", object())],
            &[("I", "F")],
            Some(&["B", "A", "C"]),
        );
        let (g, whole, diff) = (global.render(), front.render(), render_diff(&front, &global));
        let dir = Path::new("/nonexistent");
        let from_whole = canonical_area_text(&whole, &g, dir).unwrap();
        let from_diff = canonical_area_text(&diff, &g, dir).unwrap();
        assert_eq!(from_whole, from_diff);
        // The canonical form is global's order, then what the area adds.
        assert!(from_whole.contains("'B' => 2,\n    'C' => \n"), "{from_whole}");
        assert!(from_whole.contains("'A' => true,\n    'B' => true,\n    'C' => true,"), "{from_whole}");
        // A real difference survives.
        let other = area(&[("A", PhpValue::Null), ("B", PhpValue::Int(3)), ("C", object())], &[("I", "F")], Some(&["C"]));
        assert_ne!(canonical_area_text(&other.render(), &g, dir).unwrap(), from_diff);
    }

    #[test]
    fn entries_split_around_strings_and_brackets() {
        let text = "    'a' => 'x,\n)',\n    'b' => \n    array (\n      0 => \"\\0\",\n    ),\n    5 => NULL,\n";
        let got = items(text).unwrap();
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got.iter().map(|i| entry_key(i).unwrap()).collect::<Vec<_>>(), ["'a'", "'b'", "5"]);
        assert!(items("    'a' => 'open,\n").is_none());
    }

    #[test]
    fn layouts() {
        let t = |platform, layout| Target { php: None, platform, area_layout: layout };
        let root = Path::new("/nonexistent");
        assert_eq!(AreaLayout::resolve(root, &t(None, None)), Some(AreaLayout::Full));
        assert_eq!(AreaLayout::resolve(root, &t(Some(Platform::MageOs4_0), None)), Some(AreaLayout::Diff));
        assert_eq!(
            AreaLayout::resolve(root, &t(Some(Platform::MageOs4_0), Some(AreaLayout::Full))),
            Some(AreaLayout::Full)
        );
        assert_eq!(AreaLayout::resolve(root, &t(Some(Platform::MageOs3_3), Some(AreaLayout::Diff))), None);
        assert_eq!(AreaLayout::resolve(root, &t(None, Some(AreaLayout::Diff))), None);
        assert!("diffs".parse::<AreaLayout>().is_err());
    }
}
