//! generated/code — WHICH classes the compile generates, and (per kind) the
//! exact bytes. Generation in Magento is autoloader-driven: every scanner
//! merely `class_exists`es candidate names, and the registered
//! `Code\Generator` writes the file (recursively generating the source chain
//! first). This module models that as an explicit `ensure()` closure over
//! the same candidate streams, in the same operation order:
//!
//!   1. ProxyGenerator      — XmlScanner over every di.xml (proxies AND
//!                            object-argument factories),
//!   2. RepositoryGenerator — RepositoryScanner over every di.xml,
//!   3. ServiceDataAttributesGenerator — extension_attributes.xml `for=`,
//!   4. ApplicationCodeGenerator — PhpScanner: ctor params ending `Factory`
//!                            + the getExtensionAttributes docblock sweep,
//!   5. incidental — any later `class_exists` on a generatable name
//!                            (NonLazyTypes candidates, interception walks);
//!                            fed by the caller via `ensure_all`.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::PathBuf;

use magequery_core::{Area, ClassName, Magento};

use crate::definitions::Definitions;

/// One entry of the `generatedEntities` registry (primary di.xml).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GenKind {
    ExtensionInterfaceFactory,
    Factory,
    Proxy,
    Interceptor,
    Logger,
    Mapper,
    Persistor,
    Repository,
    Convertor,
    SearchResults,
    ExtensionInterface,
    Extension,
    Remote,
    ProxyDeferred,
}

/// Registry order matters: `Generator::generateClass` takes the FIRST
/// suffix that matches (case-sensitive substr compare), so a name ending
/// `ExtensionInterfaceFactory` never reaches the plain `factory` entry.
const REGISTRY: &[(GenKind, &str)] = &[
    (GenKind::ExtensionInterfaceFactory, "ExtensionInterfaceFactory"),
    (GenKind::Factory, "Factory"),
    (GenKind::Proxy, "Proxy"),
    (GenKind::Interceptor, "Interceptor"),
    (GenKind::Logger, "Logger"),
    (GenKind::Mapper, "Mapper"),
    (GenKind::Persistor, "Persistor"),
    (GenKind::Repository, "Repository"),
    (GenKind::Convertor, "Convertor"),
    (GenKind::SearchResults, "SearchResults"),
    (GenKind::ExtensionInterface, "ExtensionInterface"),
    (GenKind::Extension, "Extension"),
    (GenKind::Remote, "Remote"),
    (GenKind::ProxyDeferred, "ProxyDeferred"),
];

/// Suffix-classify a generatable name: its kind and the SOURCE class the
/// generator loads before emitting (the dispatch source — name minus suffix,
/// trailing backslashes trimmed — refined per generator constructor:
/// `FooExtension`/`FooExtensionInterface` load `FooInterface`,
/// `FooExtensionInterfaceFactory` loads `FooExtension`).
pub fn classify(name: &str) -> Option<(GenKind, String)> {
    for (kind, suffix) in REGISTRY {
        if name.len() > suffix.len() && name.ends_with(suffix) {
            let dispatch = name[..name.len() - suffix.len()].trim_end_matches('\\');
            if dispatch.is_empty() {
                return None;
            }
            let source = match kind {
                GenKind::ExtensionInterfaceFactory => format!("{dispatch}Extension"),
                GenKind::Extension | GenKind::ExtensionInterface => {
                    format!("{dispatch}Interface")
                }
                _ => dispatch.to_owned(),
            };
            return Some((*kind, source));
        }
    }
    None
}

const FACTORY_TPL: &str = "/**
 * @LABEL@ class for @see \\@SOURCE@
 */
class @SHORT@
{
    /**
     * Object Manager instance
     *
     * @var \\Magento\\Framework\\ObjectManagerInterface
     */
    protected $_objectManager = null;

    /**
     * Instance name to create
     *
     * @var string
     */
    protected $_instanceName = null;

    /**
     * @LABEL@ constructor
     *
     * @param \\Magento\\Framework\\ObjectManagerInterface $objectManager
     * @param string $instanceName
     */
    public function __construct(\\Magento\\Framework\\ObjectManagerInterface $objectManager, $instanceName = '@ESCAPED@')
    {
        $this->_objectManager = $objectManager;
        $this->_instanceName = $instanceName;
    }

    /**
     * Create class instance with specified parameters
     *
     * @param array $data
     * @return \\@SOURCE@
     */
    public function create(array $data = [])
    {
        return $this->_objectManager->create($this->_instanceName, $data);
    }
}
";

/// The generated factory file, byte-exact (verified uniform across all
/// 1,093 factories of the oracle archive). Two variants share the template:
/// the plain Factory and the ExtensionInterfaceFactory, which labels itself
/// and instantiates the concrete `…Extension` class (name minus
/// `InterfaceFactory`).
pub fn factory_bytes(fqcn: &str, kind: GenKind) -> String {
    let (label, instance) = match kind {
        GenKind::ExtensionInterfaceFactory => (
            "ExtensionInterfaceFactory",
            &fqcn[..fqcn.len() - "InterfaceFactory".len()],
        ),
        _ => ("Factory", &fqcn[..fqcn.len() - "Factory".len()]),
    };
    let (ns, short) = match fqcn.rfind('\\') {
        Some(i) => (Some(&fqcn[..i]), &fqcn[i + 1..]),
        None => (None, fqcn),
    };
    let escaped = format!("\\\\{}", instance.replace('\\', "\\\\"));
    let mut out = String::from("<?php\n");
    if let Some(ns) = ns {
        out.push_str("namespace ");
        out.push_str(ns);
        out.push_str(";\n\n");
    }
    out.push_str(
        &FACTORY_TPL
            .replace("@LABEL@", label)
            .replace("@SOURCE@", instance)
            .replace("@SHORT@", short)
            .replace("@ESCAPED@", &escaped),
    );
    out
}

/// A `<preference for= type=>` as scanned raw from one di.xml file (the
/// repository scanner reads files, not merged config).
struct RawPreference {
    for_type: String,
    type_attr: Option<String>,
}

/// What one di.xml file contributes to the code-generation scanners.
#[derive(Default)]
struct DiFileScan {
    /// `virtualType name=` (ltrimmed) — XmlScanner subtracts these from the
    /// factory candidates.
    vtype_names: Vec<String>,
    /// `preference type=` / `virtualType type=` attribute values, raw.
    type_attrs: Vec<String>,
    /// text of `<argument xsi:type="object">` / `<item xsi:type="object">`.
    object_values: Vec<String>,
    preferences: Vec<RawPreference>,
}

pub struct Codegen<'a> {
    magento: &'a Magento,
    defs: &'a Definitions,
    root: PathBuf,
    /// virtualType names of the loaded (primary + global) config — the
    /// generator's `shouldSkipGeneration` refuses to shadow these.
    global_vtypes: HashSet<String>,
    /// The result: generated class -> kind.
    pub emitted: BTreeMap<String, GenKind>,
    /// lowercase name -> kind (PHP class names are case-insensitive; the
    /// first requested spelling owns the file).
    emitted_ci: std::collections::HashMap<String, GenKind>,
    failed: HashSet<String>,
}

impl<'a> Codegen<'a> {
    pub fn new(magento: &'a Magento, defs: &'a Definitions, root: PathBuf) -> Codegen<'a> {
        let global_vtypes = magento
            .di_export(Area::Global)
            .virtual_types
            .iter()
            .map(|v| v.name.as_str().to_owned())
            .collect();
        Codegen {
            magento,
            defs,
            root,
            global_vtypes,
            emitted: BTreeMap::new(),
            emitted_ci: std::collections::HashMap::new(),
            failed: HashSet::new(),
        }
    }

    /// Run every scanner in DiCompileCommand's operation order.
    pub fn collect(&mut self) {
        let scans = self.scan_di_files();
        self.collect_xml_scanner(&scans); // op 1: ProxyGenerator
        self.collect_repository_scanner(&scans); // op 2: RepositoryGenerator
        self.collect_extension_attributes(); // op 3: ServiceDataAttributesGenerator
        self.collect_php_scanner(); // op 4: ApplicationCodeGenerator
    }

    /// `class_exists($name)` with the generation autoloader registered:
    /// true when the name is real source, already generated, or generatable
    /// (in which case it is generated now, source chain first).
    pub fn ensure(&mut self, raw: &str) -> bool {
        // PHP normalizes the leading backslash before autoloading.
        let name = raw.trim_start_matches('\\');
        if name.is_empty() {
            return false;
        }
        if self.exists_as_source(name) {
            return true;
        }
        if self.emitted_ci.contains_key(&name.to_ascii_lowercase()) {
            return true;
        }
        if self.failed.contains(name) {
            return false;
        }
        let Some((kind, source)) = classify(name) else {
            self.failed.insert(name.to_owned());
            return false;
        };
        // GENERATION_SKIP: a virtual type by this name exists — no file, and
        // the class stays unloadable.
        if self.global_vtypes.contains(name) {
            self.failed.insert(name.to_owned());
            return false;
        }
        // tryToLoadSourceClass: the source chain generates recursively; a
        // missing source fails the whole request (no file written).
        if !self.ensure(&source) {
            self.failed.insert(name.to_owned());
            return false;
        }
        self.emitted.insert(name.to_owned(), kind);
        self.emitted_ci.insert(name.to_ascii_lowercase(), kind);
        true
    }

    pub fn ensure_all<'i>(&mut self, names: impl IntoIterator<Item = &'i str>) {
        for name in names {
            self.ensure(name);
        }
    }

    /// `DefinedClasses::isClassLoadableFromDisk` — real source reachable by
    /// the autoloader. The archive's generated classes in `defs` are compile
    /// ARTIFACTS, not source (a true compile starts from an empty generated
    /// dir), so they don't count.
    fn exists_as_source(&self, name: &str) -> bool {
        if !name.contains('\\') {
            // Global-namespace names are PHP built-ins in practice
            // (DOMDocument, ArrayObject) — loadable, UNLESS the name carries
            // a generatable suffix that no built-in owns (DOMDocumentFactory
            // is a generated artifact over the built-in DOMDocument).
            return classify(name).is_none() || is_suffixed_builtin(name);
        }
        if let Some(rec) = self.defs.get(name) {
            return !self.defs.generated_classes.contains(&rec.meta.fqcn);
        }
        self.magento
            .class_file(&ClassName::new(name.to_owned()))
            .is_some()
    }

    /// `class_exists($name)` WITHOUT triggering generation — true only for a
    /// loadable CLASS: interfaces (hand-written or generated) and traits load
    /// but still return false. This is the gate the extension-attributes
    /// sweep leans on — it is why `FooExtensionFactory` still generates after
    /// an earlier operation produced `FooExtensionInterface`.
    fn name_is_class(&self, name: &str) -> bool {
        let name = name.trim_start_matches('\\');
        if !name.contains('\\') {
            return classify(name).is_none() || is_suffixed_builtin(name);
        }
        if let Some(rec) = self.defs.get(name) {
            if !self.defs.generated_classes.contains(&rec.meta.fqcn) {
                return matches!(
                    rec.meta.kind,
                    magecommand_php::ClassKind::Class | magecommand_php::ClassKind::Enum
                );
            }
        }
        if let Some(kind) = self.emitted_ci.get(&name.to_ascii_lowercase()) {
            // Every generated artifact is a class except the extension
            // attributes interface.
            return *kind != GenKind::ExtensionInterface;
        }
        // Loadable real source outside the compile paths: assume a class
        // (the sweep's names are module Api\Data types; libs don't occur).
        self.magento
            .class_file(&ClassName::new(name.to_owned()))
            .is_some()
    }

    // ---- op 1 + 2 input: raw di.xml files -------------------------------

    /// Every di.xml the ConfigurationScanner yields: the primary app/etc
    /// glob, then each enabled module's etc/di.xml and etc/<area>/di.xml.
    fn di_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let app_etc = self.root.join("app/etc");
        let mut primary: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = fs::read_dir(&app_etc) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && is_di_xml(&path) {
                    primary.push(path);
                } else if path.is_dir() {
                    if let Ok(subs) = fs::read_dir(&path) {
                        for sub in subs.flatten() {
                            let sub = sub.path();
                            if sub.is_file() && is_di_xml(&sub) {
                                primary.push(sub);
                            }
                        }
                    }
                }
            }
        }
        primary.sort();
        files.extend(primary);
        const AREAS: &[&str] = &[
            "frontend",
            "adminhtml",
            "crontab",
            "webapi_rest",
            "webapi_soap",
            "graphql",
        ];
        for module in self.magento.modules() {
            if !module.enabled {
                continue;
            }
            let etc = module.path.join("etc");
            let global = etc.join("di.xml");
            if global.is_file() {
                files.push(global);
            }
            for area in AREAS {
                let area_file = etc.join(area).join("di.xml");
                if area_file.is_file() {
                    files.push(area_file);
                }
            }
        }
        files
    }

    fn scan_di_files(&self) -> DiFileScan {
        let mut merged = DiFileScan::default();
        for file in self.di_files() {
            let Ok(text) = fs::read_to_string(&file) else { continue };
            scan_di_xml(&text, &mut merged);
        }
        merged
    }

    /// XmlScanner::collectEntities — proxies (preference/argument/item/
    /// virtualType values whose last segment ends `Proxy`) filtered like
    /// `_filterEntities`, plus object-argument values ending `Factory`
    /// minus every virtualType name.
    fn collect_xml_scanner(&mut self, scans: &DiFileScan) {
        let vtypes: HashSet<&str> = scans.vtype_names.iter().map(String::as_str).collect();
        let mut proxies: Vec<String> = Vec::new();
        let mut factories: Vec<String> = Vec::new();
        for value in scans
            .object_values
            .iter()
            .chain(scans.type_attrs.iter())
        {
            let trimmed = value.trim();
            // /^(\s+)?(.*)\\(.*)Proxy(\s+)?$/ — ends Proxy, has a backslash.
            if trimmed.ends_with("Proxy") && trimmed.contains('\\') {
                proxies.push(trimmed.trim_start_matches('\\').to_owned());
            }
        }
        // scanFactories reads only object argument/item values.
        for value in &scans.object_values {
            let trimmed = value.trim();
            if trimmed.ends_with("Factory") {
                factories.push(trimmed.trim_start_matches('\\').to_owned());
            }
        }
        // _filterEntities: only names whose `\Proxy`-stripped base loads
        // survive; a candidate not ending in the `\Proxy` SEGMENT keeps its
        // full name as base (and, not existing, is dropped as invalid).
        for proxy in proxies {
            match proxy.strip_suffix("\\Proxy") {
                Some(_) => {
                    self.ensure(&proxy);
                }
                None => {
                    // base = the unchanged name; class_exists(base) is the
                    // gate, and a real class needs no generation anyway.
                }
            }
        }
        for factory in factories {
            if vtypes.contains(factory.as_str()) {
                continue;
            }
            self.ensure(&factory);
        }
    }

    /// RepositoryScanner: `<preference for="…RepositoryInterface">` whose
    /// type does not exist as source spawns the repository family.
    fn collect_repository_scanner(&mut self, scans: &DiFileScan) {
        let mut candidates: Vec<String> = Vec::new();
        for pref in &scans.preferences {
            let Some(type_attr) = &pref.type_attr else { continue };
            if !pref.for_type.ends_with("RepositoryInterface") {
                continue;
            }
            let type_name = type_attr.trim().trim_start_matches('\\');
            if self.exists_as_source(type_name) {
                continue;
            }
            // str_replace('\Repository', …, $type) — replace-all, faithfully.
            candidates.push(type_name.replace("\\Repository", "InterfacePersistor"));
            candidates.push(type_name.replace("\\Repository", "InterfaceFactory"));
            candidates.push(type_name.replace("\\Repository", "SearchResultInterfaceFactory"));
            candidates.push(type_name.to_owned());
        }
        for name in candidates {
            self.ensure(&name);
        }
    }

    // ---- op 3: extension_attributes.xml ---------------------------------

    fn collect_extension_attributes(&mut self) {
        let mut for_types: Vec<String> = Vec::new();
        const AREAS: &[&str] = &[
            "frontend",
            "adminhtml",
            "crontab",
            "webapi_rest",
            "webapi_soap",
            "graphql",
        ];
        for module in self.magento.modules() {
            if !module.enabled {
                continue;
            }
            let etc = module.path.join("etc");
            let mut paths = vec![etc.join("extension_attributes.xml")];
            paths.extend(AREAS.iter().map(|a| etc.join(a).join("extension_attributes.xml")));
            for path in paths {
                let Ok(text) = fs::read_to_string(&path) else { continue };
                collect_extension_for_types(&text, &mut for_types);
            }
        }
        for for_type in for_types {
            // ServiceDataAttributesScanner: str_replace('Interface', …) —
            // replace-all on the FQCN, exactly as PHP does.
            self.ensure(&for_type.replace("Interface", "ExtensionInterface"));
            self.ensure(&for_type.replace("Interface", "Extension"));
        }
    }

    // ---- op 4: PhpScanner ------------------------------------------------

    /// `_fetchFactories` + `_fetchMissingExtensionAttributesClasses` over
    /// every class/interface the scan paths declare (PhpScanner tokenizes
    /// T_CLASS and T_INTERFACE — traits and enums are invisible to it).
    fn collect_php_scanner(&mut self) {
        let defs = self.defs;
        let mut names: Vec<&String> = defs
            .from_scan
            .iter()
            .filter(|n| {
                defs.get(n).is_some_and(|r| {
                    matches!(
                        r.meta.kind,
                        magecommand_php::ClassKind::Class | magecommand_php::ClassKind::Interface
                    )
                })
            })
            .collect();
        names.sort();
        for name in names {
            // Factories from constructor parameters (reflection walks to the
            // nearest inherited constructor, as constructor_of does).
            if let Ok(Some(ctor)) = defs.constructor_of(name) {
                let candidates: Vec<String> = ctor
                    .params
                    .iter()
                    .filter_map(|p| p.ty.as_deref())
                    .filter_map(factory_param_candidate)
                    .map(str::to_owned)
                    .collect();
                for cand in candidates {
                    self.ensure(&cand);
                }
            }
            // The extension-attributes sweep: interfaces declaring (or
            // inheriting) getExtensionAttributes.
            let record = defs.get(name).expect("filtered above");
            if record.meta.kind != magecommand_php::ClassKind::Interface {
                continue;
            }
            let Some(ret) = self.extension_return(name) else { continue };
            let ret = ret.trim_start_matches('\\').to_owned();
            // shouldGenerateClass(ret, 'ExtensionInterface'):
            //   `class_exists($ret)` is FALSE for interfaces — hand-written
            //   or generated by an earlier operation — so only a loadable
            //   CLASS short-circuits the block.
            if self.name_is_class(&ret) {
                continue;
            }
            //   Source gate: getSourceClassName chops 18 chars blind and
            //   appends `Interface`; that class-or-interface must load.
            if ret.len() <= "ExtensionInterface".len() {
                continue;
            }
            let chopped =
                ret[..ret.len() - "ExtensionInterface".len()].trim_end_matches('\\');
            if chopped.is_empty() || !self.ensure(&format!("{chopped}Interface")) {
                continue;
            }
            // Push the interface (a real file on disk makes generation SKIP,
            // which ensure() models via the exists gate)…
            self.ensure(&ret);
            // …then the concrete extension and its factory, each gated on
            // its own class_exists.
            if ret.len() <= "Interface".len() {
                continue;
            }
            let extension =
                ret[..ret.len() - "Interface".len()].trim_end_matches('\\').to_owned();
            if !self.name_is_class(&extension) {
                self.ensure(&extension);
            }
            let factory = format!("{extension}Factory");
            if !self.name_is_class(&factory) {
                self.ensure(&factory);
            }
        }
    }

    /// TypeProcessor::getGetterReturnType on getExtensionAttributes — the
    /// docblock `@return` of the nearest declaring interface, first type of
    /// the union. Magento's docblocks write it fully qualified; a relative
    /// name resolves against the declaring file.
    fn extension_return(&self, iface: &str) -> Option<String> {
        let defs = self.defs;
        let mut chain: Vec<String> = vec![iface.to_owned()];
        chain.extend(defs.all_interfaces(iface));
        let declaring = chain.iter().find(|c| {
            defs.get(c).is_some_and(|r| {
                r.meta
                    .methods
                    .iter()
                    .any(|m| m.name.eq_ignore_ascii_case("getExtensionAttributes"))
            })
        })?;
        let record = defs.get(declaring)?;
        let text = fs::read_to_string(&record.file).ok()?;
        let ret = docblock_return_of(&text, "getExtensionAttributes")?;
        let first = ret.split('|').next()?.trim();
        if first.is_empty() {
            return None;
        }
        if let Some(abs) = first.strip_prefix('\\') {
            return Some(abs.to_owned());
        }
        // Relative: resolve through the declaring file's use map / namespace.
        let (first_seg, rest) = match first.split_once('\\') {
            Some((f, r)) => (f, Some(r)),
            None => (first, None),
        };
        let lower = first_seg.to_ascii_lowercase();
        if let Some((_, mapped)) = record.meta.uses.iter().find(|(a, _)| *a == lower) {
            return Some(match rest {
                Some(r) => format!("{mapped}\\{r}"),
                None => mapped.clone(),
            });
        }
        let ns = match record.meta.fqcn.rfind('\\') {
            Some(i) => &record.meta.fqcn[..i],
            None => "",
        };
        Some(if ns.is_empty() {
            first.to_owned()
        } else {
            format!("{ns}\\{first}")
        })
    }
}

/// The parameter-type capture of PhpScanner's reflection regex
/// `/\[\s\<\w+?>\s\??([\w\\]+)/`: strip a leading `?`, take the leading
/// word/backslash run (a union stops at `|`), and keep it when it ends in
/// `Factory` — except an exact `\Factory` last segment, which Magento logs
/// as an invalid declaration and skips.
/// PHP built-ins whose names end in a registry suffix — real classes, not
/// generation targets.
fn is_suffixed_builtin(name: &str) -> bool {
    matches!(name, "ReflectionExtension" | "ReflectionZendExtension")
}

fn factory_param_candidate(ty: &str) -> Option<&str> {
    let s = ty.strip_prefix('?').unwrap_or(ty);
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '\\'))
        .unwrap_or(s.len());
    let cap = &s[..end];
    if cap.ends_with("Factory") && !cap.ends_with("\\Factory") {
        Some(cap)
    } else {
        None
    }
}

fn is_di_xml(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with("di.xml"))
}

/// Stream one di.xml for what the code-generation scanners read: virtualType
/// names, preference/virtualType type attributes, object-typed argument and
/// item text, and raw preferences.
fn scan_di_xml(text: &str, out: &mut DiFileScan) {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().expand_empty_elements = false;
    let mut buf = Vec::new();
    // Depth of the currently capturing <argument>/<item xsi:type=object>,
    // with its accumulated text.
    let mut depth = 0usize;
    let mut capture: Option<(usize, String)> = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                open_di_element(&e, out, &mut capture, Some(depth + 1));
                depth += 1;
            }
            Ok(Event::Empty(e)) => {
                // Self-closing: no End event follows, no text to capture.
                open_di_element(&e, out, &mut capture, None);
            }
            Ok(Event::End(_)) => {
                if let Some((cap_depth, value)) = &capture {
                    if *cap_depth == depth {
                        out.object_values.push(value.clone());
                        capture = None;
                    }
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Text(t)) => {
                if let (Some((_, value)), Ok(text)) = (&mut capture, t.unescape()) {
                    value.push_str(&text);
                }
            }
            Ok(Event::CData(c)) => {
                if let Some((_, value)) = &mut capture {
                    value.push_str(&String::from_utf8_lossy(&c));
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
}

/// One opening di.xml element (Start or Empty) routed into the scan.
/// `capture_depth` is `Some` only for Start events of object-typed
/// `<argument>`/`<item>` — the depth their text accumulates at.
fn open_di_element(
    e: &quick_xml::events::BytesStart<'_>,
    out: &mut DiFileScan,
    capture: &mut Option<(usize, String)>,
    capture_depth: Option<usize>,
) {
    let name = e.local_name();
    let name = name.as_ref();
    let mut xsi_object = false;
    let mut vt_name: Option<String> = None;
    let mut type_attr: Option<String> = None;
    let mut for_attr: Option<String> = None;
    for attr in e.attributes().flatten() {
        let key = attr.key.as_ref();
        let value = String::from_utf8_lossy(&attr.value).into_owned();
        match key {
            b"xsi:type" if value == "object" => xsi_object = true,
            b"name" if name == b"virtualType" => vt_name = Some(value),
            b"type" => type_attr = Some(value),
            b"for" => for_attr = Some(value),
            _ => {}
        }
    }
    match name {
        b"virtualType" => {
            if let Some(n) = vt_name {
                out.vtype_names.push(n.trim_start_matches('\\').to_owned());
            }
            if let Some(t) = type_attr {
                out.type_attrs.push(t);
            }
        }
        b"preference" => {
            if let Some(t) = &type_attr {
                out.type_attrs.push(t.clone());
            }
            if let Some(f) = for_attr {
                out.preferences.push(RawPreference {
                    for_type: f.trim_start_matches('\\').to_owned(),
                    type_attr,
                });
            }
        }
        b"argument" | b"item" => {
            if xsi_object {
                if let Some(depth) = capture_depth {
                    *capture = Some((depth, String::new()));
                }
            }
        }
        _ => {}
    }
}

/// `//extension_attributes/@for` of one extension_attributes.xml.
fn collect_extension_for_types(text: &str, out: &mut Vec<String>) {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(text);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if e.local_name().as_ref() == b"extension_attributes" {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"for" {
                            out.push(String::from_utf8_lossy(&attr.value).into_owned());
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
}

/// The `@return` expression of the docblock immediately preceding
/// `function <method>` in `text`, when present.
fn docblock_return_of(text: &str, method: &str) -> Option<String> {
    let mut search_from = 0usize;
    loop {
        let pos = text[search_from..].find("function ")? + search_from;
        let after = &text[pos + "function ".len()..];
        let after = after.trim_start_matches(['&', ' ']);
        if after
            .strip_prefix(method)
            .is_some_and(|rest| rest.trim_start().starts_with('('))
        {
            // Walk back over whitespace/modifiers to the nearest docblock end.
            let before = &text[..pos];
            let doc_end = before.rfind("*/")?;
            let between = &before[doc_end + 2..];
            // Only modifiers/attributes may sit between docblock and method.
            if between
                .split_whitespace()
                .all(|w| matches!(w, "public" | "protected" | "private" | "static" | "abstract" | "final"))
            {
                let doc_start = before[..doc_end].rfind("/**")?;
                let doc = &before[doc_start..doc_end];
                for line in doc.lines() {
                    let line = line.trim_start_matches([' ', '*', '\t']);
                    if let Some(rest) = line.strip_prefix("@return") {
                        let expr = rest.trim().split_whitespace().next()?;
                        return Some(expr.to_owned());
                    }
                }
                return None;
            }
        }
        search_from = pos + "function ".len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_registry_order() {
        // ExtensionInterfaceFactory beats Factory; source chains refine.
        assert_eq!(
            classify("Foo\\BarExtensionInterfaceFactory"),
            Some((GenKind::ExtensionInterfaceFactory, "Foo\\BarExtension".into()))
        );
        assert_eq!(
            classify("Foo\\BarFactory"),
            Some((GenKind::Factory, "Foo\\Bar".into()))
        );
        assert_eq!(
            classify("Foo\\BarExtension"),
            Some((GenKind::Extension, "Foo\\BarInterface".into()))
        );
        assert_eq!(
            classify("Foo\\BarExtensionInterface"),
            Some((GenKind::ExtensionInterface, "Foo\\BarInterface".into()))
        );
        assert_eq!(
            classify("Foo\\Bar\\Proxy"),
            Some((GenKind::Proxy, "Foo\\Bar".into()))
        );
        assert_eq!(
            classify("Foo\\ProductRenderSearchResults"),
            Some((GenKind::SearchResults, "Foo\\ProductRender".into()))
        );
        assert_eq!(classify("Foo\\Bar"), None);
        // The bare suffix itself has an empty source: generation error.
        assert_eq!(classify("Factory"), None);
    }

    #[test]
    fn factory_bytes_shape() {
        let out = factory_bytes("Magento\\Catalog\\Model\\ProductFactory", GenKind::Factory);
        assert!(out.starts_with("<?php\nnamespace Magento\\Catalog\\Model;\n\n/**\n * Factory class for @see \\Magento\\Catalog\\Model\\Product\n */\nclass ProductFactory\n{\n"));
        assert!(out.contains(
            "$instanceName = '\\\\Magento\\\\Catalog\\\\Model\\\\Product')"
        ));
        assert!(out.ends_with("}\n"));
        // Global namespace: no namespace line, no blank line after <?php.
        let dom = factory_bytes("DOMDocumentFactory", GenKind::Factory);
        assert!(dom.starts_with("<?php\n/**\n * Factory class for @see \\DOMDocument\n */\nclass DOMDocumentFactory\n"));
        // The interface-factory variant instantiates the Extension class.
        let eif = factory_bytes(
            "Magento\\Customer\\Api\\Data\\GroupExtensionInterfaceFactory",
            GenKind::ExtensionInterfaceFactory,
        );
        assert!(eif.contains(" * ExtensionInterfaceFactory class for @see \\Magento\\Customer\\Api\\Data\\GroupExtension\n"));
        assert!(eif.contains(" * ExtensionInterfaceFactory constructor\n"));
        assert!(eif.contains("$instanceName = '\\\\Magento\\\\Customer\\\\Api\\\\Data\\\\GroupExtension')"));
    }

    #[test]
    fn factory_param_candidates() {
        assert_eq!(factory_param_candidate("Foo\\BarFactory"), Some("Foo\\BarFactory"));
        assert_eq!(factory_param_candidate("?Foo\\BarFactory"), Some("Foo\\BarFactory"));
        // Union: reflection's __toString shows the first member only.
        assert_eq!(factory_param_candidate("Foo\\AFactory|Foo\\B"), Some("Foo\\AFactory"));
        assert_eq!(factory_param_candidate("Foo\\A|Foo\\BFactory"), None);
        // A literal \Factory segment is an invalid declaration, skipped.
        assert_eq!(factory_param_candidate("Foo\\Factory"), None);
        assert_eq!(factory_param_candidate("array"), None);
        assert_eq!(factory_param_candidate("string"), None);
    }

    #[test]
    fn docblock_return_extraction() {
        let src = r#"<?php
interface Foo
{
    /**
     * Retrieve existing extension attributes object or create a new one.
     *
     * @return \Magento\Catalog\Api\Data\ProductExtensionInterface|null
     */
    public function getExtensionAttributes();
}
"#;
        assert_eq!(
            docblock_return_of(src, "getExtensionAttributes"),
            Some("\\Magento\\Catalog\\Api\\Data\\ProductExtensionInterface|null".into())
        );
    }
}
