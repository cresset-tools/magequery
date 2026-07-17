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

/// The merged `extension_attributes.xml` config, keyed by `for` interface
/// (leading backslash trimmed, as the generator's `ltrim` lookup does), the
/// value an ordered `(code, raw type)` list. Order is Magento's DOM merge:
/// first-mention position across modules in load order, `type` last-wins.
pub struct ExtConfig {
    map: std::collections::HashMap<String, Vec<(String, String)>>,
}

impl ExtConfig {
    /// Build from every enabled module's global `etc/extension_attributes.xml`
    /// (the Config reader's default scope is `global`; there are no per-area
    /// extension_attributes files).
    pub fn build(magento: &Magento) -> ExtConfig {
        let mut map: std::collections::HashMap<String, Vec<(String, String)>> =
            std::collections::HashMap::new();
        for module in magento.modules() {
            if !module.enabled {
                continue;
            }
            let path = module.path.join("etc/extension_attributes.xml");
            let Ok(text) = fs::read_to_string(&path) else { continue };
            merge_extension_attributes(&text, &mut map);
        }
        ExtConfig { map }
    }

    fn attributes(&self, for_type: &str) -> &[(String, String)] {
        self.map
            .get(for_type.trim_start_matches('\\'))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// One extension_attributes.xml streamed into the merge map. `<attribute
/// code= type=>` under `<extension_attributes for=>`; resources/join ignored
/// (the generator reads only DATA_TYPE).
fn merge_extension_attributes(
    text: &str,
    map: &mut std::collections::HashMap<String, Vec<(String, String)>>,
) {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(text);
    let mut buf = Vec::new();
    let mut current_for: Option<String> = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                match e.local_name().as_ref() {
                    b"extension_attributes" => {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"for" {
                                current_for = Some(
                                    String::from_utf8_lossy(&attr.value)
                                        .trim_start_matches('\\')
                                        .to_owned(),
                                );
                            }
                        }
                    }
                    b"attribute" => {
                        if let Some(for_type) = &current_for {
                            let mut code = None;
                            let mut ty = None;
                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"code" => {
                                        code = Some(String::from_utf8_lossy(&attr.value).into_owned())
                                    }
                                    b"type" => {
                                        ty = Some(String::from_utf8_lossy(&attr.value).into_owned())
                                    }
                                    _ => {}
                                }
                            }
                            if let (Some(code), Some(ty)) = (code, ty) {
                                let entry = map.entry(for_type.clone()).or_default();
                                match entry.iter_mut().find(|(c, _)| *c == code) {
                                    Some(slot) => slot.1 = ty, // in-place, keeps position
                                    None => entry.push((code, ty)),
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                if e.local_name().as_ref() == b"extension_attributes" {
                    current_for = None;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
}

/// The generated Extension / ExtensionInterface file, byte-exact. `kind`
/// selects class-vs-interface framing; the attribute method list is shared
/// (both apply `isValidTypeDeclaration` to gate the setter type hint).
pub fn extension_bytes(fqcn: &str, kind: GenKind, cfg: &ExtConfig) -> String {
    let (_, source) = classify(fqcn).expect("extension name classifies");
    let (ns, short) = match fqcn.rfind('\\') {
        Some(i) => (Some(&fqcn[..i]), &fqcn[i + 1..]),
        None => (None, fqcn),
    };
    let (label, is_iface, decl_line) = match kind {
        GenKind::Extension => (
            "Extension",
            false,
            format!(
                "class {short} extends \\Magento\\Framework\\Api\\AbstractSimpleObject implements {short}Interface"
            ),
        ),
        GenKind::ExtensionInterface => (
            "ExtensionInterface",
            true,
            format!(
                "interface {short} extends \\Magento\\Framework\\Api\\ExtensionAttributesInterface"
            ),
        ),
        _ => unreachable!("extension_bytes called with {kind:?}"),
    };

    let mut methods: Vec<String> = Vec::new();
    for (code, raw_type) in cfg.attributes(&source) {
        let rtype = render_attr_type(raw_type);
        let prop = snake_to_camel(code);
        let ucprop = ucfirst(&prop);
        // getter
        let getter_sig = format!("get{ucprop}()");
        let getter_body = if is_iface {
            ";".to_owned()
        } else {
            format!("\n    {{\n        return $this->_get('{code}');\n    }}")
        };
        methods.push(format!(
            "    /**\n     * @return {rtype}|null\n     */\n    public function {getter_sig}{getter_body}"
        ));
        // setter
        let hint = if is_valid_type_declaration(&rtype) {
            format!("{rtype} ")
        } else {
            String::new()
        };
        let setter_sig = format!("set{ucprop}({hint}${prop})");
        let setter_body = if is_iface {
            ";".to_owned()
        } else {
            format!(
                "\n    {{\n        $this->setData('{code}', ${prop});\n        return $this;\n    }}"
            )
        };
        methods.push(format!(
            "    /**\n     * @param {rtype} ${prop}\n     * @return $this\n     */\n    public function {setter_sig}{setter_body}"
        ));
    }

    let mut out = String::from("<?php\n");
    if let Some(ns) = ns {
        out.push_str("namespace ");
        out.push_str(ns);
        out.push_str(";\n\n");
    }
    out.push_str("/**\n * ");
    out.push_str(label);
    out.push_str(" class for @see \\");
    out.push_str(&source);
    out.push_str("\n */\n");
    out.push_str(&decl_line);
    out.push_str("\n{\n");
    if !methods.is_empty() {
        out.push_str(&methods.join("\n\n"));
        out.push('\n');
    }
    out.push_str("}\n");
    out
}

/// `_getFullyQualifiedClassName` applied by the generator: a type containing
/// a backslash is forced to exactly one leading backslash; scalars/arrays of
/// scalars pass through untouched (`int[]`, `string`).
fn render_attr_type(raw: &str) -> String {
    if raw.contains('\\') {
        format!("\\{}", raw.trim_start_matches('\\'))
    } else {
        raw.to_owned()
    }
}

/// `TypeProcessor::isValidTypeDeclaration` — true only for a class/interface
/// type (not a scalar, `mixed`, or an array-of).
fn is_valid_type_declaration(rendered: &str) -> bool {
    if rendered.ends_with("[]") || rendered.starts_with("ArrayOf") {
        return false;
    }
    let normalized = match rendered {
        "str" => "string",
        "integer" => "int",
        "bool" => "boolean",
        "mixed" => "anyType",
        other => other,
    };
    !matches!(normalized, "string" | "int" | "float" | "double" | "boolean" | "anyType")
}

/// `SimpleDataObjectConverter::snakeCaseToCamelCase` = lcfirst of the
/// upper-camel form (`str_replace('_', '', ucwords($s, '_'))`).
fn snake_to_camel(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut upper_next = true;
    for ch in input.chars() {
        if ch == '_' {
            upper_next = true;
        } else if upper_next {
            out.extend(ch.to_uppercase());
            upper_next = false;
        } else {
            out.push(ch);
        }
    }
    // lcfirst
    let mut chars = out.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => out,
    }
}

fn ucfirst(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The generated `<X>SearchResults` file, byte-exact. A repository-generator
/// artifact: `extends \Magento\Framework\Api\SearchResults` with a single
/// `getItems()` narrowing the docblock return type to `<source>[]`. Unlike the
/// other generators it emits NO class docblock (its `_generateCode` override
/// skips `setClassDocBlock`).
pub fn search_results_bytes(fqcn: &str) -> String {
    use crate::laminas::{Class, DocBlock, Method, Visibility};
    let source = &fqcn[..fqcn.len() - "SearchResults".len()];
    let source_bs = format!("\\{source}");
    let (ns, short) = match fqcn.rfind('\\') {
        Some(i) => (Some(fqcn[..i].to_owned()), fqcn[i + 1..].to_owned()),
        None => (None, fqcn.to_owned()),
    };
    let get_items = Method {
        name: "getItems".into(),
        visibility: Visibility::Public,
        is_static: false,
        is_final: false,
        is_abstract: false,
        returns_ref: false,
        params: vec![],
        return_type: None,
        body: Some("return parent::getItems();".into()),
        doc: DocBlock {
            short: Some("Returns array of items".into()),
            tags: vec![("return".into(), format!("{source_bs}[]"))],
            ..Default::default()
        },
    };
    let class = Class {
        namespace: ns,
        name: short,
        is_interface: false,
        extends: Some("\\Magento\\Framework\\Api\\SearchResults".into()),
        implements: vec![],
        traits: vec![],
        doc: DocBlock::default(),
        properties: vec![],
        methods: vec![get_items],
    };
    class.render()
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
            // A compile ARTIFACT (present only because we scan the frozen
            // archive): at compile start it does not exist on disk, so
            // `class_exists` is false until WE generate it. Only an already
            // emitted one counts, and only if it's a class — never fall
            // through to `class_file`, which would resolve the archive file
            // and wrongly report the artifact as pre-existing.
            return self
                .emitted_ci
                .get(&name.to_ascii_lowercase())
                .is_some_and(|kind| *kind != GenKind::ExtensionInterface);
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
            // Factories AND proxies from constructor parameters (reflection
            // walks to the nearest inherited constructor, as constructor_of
            // does). A `…Factory` param is caught by PhpScanner's
            // `_fetchFactories`; a `…\Proxy` param has no scanner, but the
            // definition compiler resolves every constructor argument's class,
            // and the code-generator autoloader materializes the proxy the
            // first time `<Subject>\Proxy` is reflected — so a proxy injected
            // only via a constructor type-hint (e.g. `use …\Foo\Proxy as Foo;
            // __construct(Foo $foo)`), with no di.xml `<argument>` at all, is
            // still generated. Verified on the oracle: a synthetic class with
            // such a parameter and no di.xml reference produces the proxy.
            if let Ok(Some(ctor)) = defs.constructor_of(name) {
                let types: Vec<&str> = ctor.params.iter().filter_map(|p| p.ty.as_deref()).collect();
                let candidates: Vec<String> = types
                    .iter()
                    .filter_map(|ty| factory_param_candidate(ty))
                    .chain(types.iter().filter_map(|ty| proxy_param_candidate(ty)))
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

/// The full `generated/code` tree: every generated PHP file as
/// `(relative path, content)`, the relative path being the FQCN with `\` →
/// `/` and a `.php` suffix (an interceptor lives at `<Class>/Interceptor.php`).
pub struct GeneratedCode {
    pub files: Vec<(String, String)>,
    pub findings: Vec<String>,
}

/// Generate every `generated/code` file for the compile. Runs the same
/// scanner sweep the SET model does (`collect` + the NonLazyTypes incidental
/// `class_exists` pass over the seven area files), then materializes the bytes
/// per generated entity; interceptors come from the separate interception
/// plan (op 5). The result is the union that `magecommand compare` checks
/// against the frozen `generated/_code`.
pub fn generate_code(
    magento: &Magento,
    defs: &Definitions,
    root: PathBuf,
    area_files: &[crate::areaconfig::CompiledArea],
    has_plugins: &std::collections::BTreeMap<String, bool>,
    fused: bool,
) -> GeneratedCode {
    let _prof = std::env::var_os("MAGECOMMAND_PROFILE").is_some();
    macro_rules! ilap {
        ($t:expr, $label:expr) => {
            if _prof {
                eprintln!("  [profile]     · {:<24} {:>8.1} ms", $label, $t.elapsed().as_secs_f64() * 1000.0);
                $t = std::time::Instant::now();
            }
        };
    }
    let mut _it = std::time::Instant::now();
    let mut cg = Codegen::new(magento, defs, root.clone());
    cg.collect();
    ilap!(_it, "collect");
    // Incidental generation while the config Reader aggregates each area
    // (`Compiler\Config\Reader::generateCachePerScope`): it `isConcrete()`s —
    // hence `class_exists()`, which autoloads and so materializes a generatable
    // name — every instanceType key it resolves arguments for and every
    // virtualType's original type. It does NOT touch preference *targets*: the
    // Reader only records `$config['preferences'][$for] = $type` as a string
    // (line ~92) and resolves constructors solely for concrete types already in
    // the definitions collection plus virtualType originals. So a name reachable
    // ONLY as a `<preference type=…Factory>` (e.g. RemoteStorage's
    // `MetadataProviderFactory`) is never generated at compile time — it's a
    // runtime-generated factory. Mirror that: sweep arguments + instanceType
    // values, never preference targets. `\Proxy`/`\Interceptor` names
    // short-circuit or are op-5 artifacts, handled elsewhere.
    //
    // The area files (fixed + custom) are prebuilt and passed in — the same set
    // main.rs writes as `<code>.php` metadata — so this sweep never rebuilds
    // them. The `isConcrete` runs in each area's OM context, so a name that is a
    // virtualType in this area is skipped by the generator
    // (`shouldSkipGeneration`) — no file. The area's vtype names are exactly the
    // keys of its instanceTypes. Without this, an area-scoped vtype whose name
    // ends in a generatable suffix (e.g. the graphql vtype
    // `amPromoQuoteItemFactory`) leaks a bogus Factory: it isn't in the
    // global-only vtype skip set, and its bare source `amPromoQuoteItem` reads
    // as a PHP built-in, so `ensure` emits it.
    for ca in area_files {
        let file = &ca.file;
        let area_vtypes: HashSet<&str> =
            file.instance_types.iter().map(|(k, _)| k.as_str()).collect();
        let names: Vec<String> = file
            .arguments
            .keys()
            .cloned()
            .chain(file.instance_types.iter().map(|(_, v)| v.clone()))
            .filter(|n| !n.ends_with("\\Proxy") && !n.ends_with("\\Interceptor"))
            .filter(|n| !area_vtypes.contains(n.as_str()))
            .collect();
        cg.ensure_all(names.iter().map(String::as_str));
    }
    ilap!(_it, "area+custom sweep");

    let ext_cfg = ExtConfig::build(magento);
    ilap!(_it, "ExtConfig::build");
    let mut files: Vec<(String, String)> = Vec::new();
    let mut findings: Vec<String> = Vec::new();
    for (name, kind) in &cg.emitted {
        let content = match kind {
            GenKind::Factory | GenKind::ExtensionInterfaceFactory => factory_bytes(name, *kind),
            GenKind::Extension | GenKind::ExtensionInterface => {
                extension_bytes(name, *kind, &ext_cfg)
            }
            GenKind::SearchResults => search_results_bytes(name),
            GenKind::Proxy => match crate::proxy::proxy_bytes(defs, name.trim_end_matches("\\Proxy")) {
                Some(bytes) => bytes,
                None => {
                    findings.push(format!("proxy source unresolved: {name}"));
                    continue;
                }
            },
            GenKind::ProxyDeferred => {
                match crate::proxy::proxy_deferred_bytes(defs, name.trim_end_matches("\\ProxyDeferred")) {
                    Some(bytes) => bytes,
                    None => {
                        findings.push(format!("proxyDeferred source unresolved: {name}"));
                        continue;
                    }
                }
            }
            // Repository/Logger/Mapper/Persistor/Convertor/Remote: the
            // RepositoryScanner family and the never-exercised registry
            // entries. None occur on the oracle (archive `other` = 0); a
            // future store that hits one gets a finding, not a silent gap.
            other => {
                findings.push(format!("unmodeled generator {other:?}: {name}"));
                continue;
            }
        };
        files.push((format!("{}.php", name.replace('\\', "/")), content));
    }
    ilap!(_it, "emit generated bytes");

    // op 5: interceptors (not part of `emitted` — a separate plan). The
    // has-plugins map is computed once by the caller (for interception.php) and
    // reused here — the plan needs the same map, never a fresh one.
    let plan = crate::interceptor::plan(magento, defs, has_plugins);
    ilap!(_it, "interceptor plan");
    // Fused mode (`di compile --fused`): global-only classes render as inlined
    // plugin chains (creatuity's technique); multi-area classes fall back to
    // stock until the per-area switch lands. The global chains are computed once.
    let fused_chains =
        fused.then(|| crate::pluginlist::global_plugin_chains(magento, defs));
    // Each interceptor's bytes are an independent pure function of the (already
    // fully-populated) definitions, and the final `files.sort_by` makes push
    // order irrelevant — so generate them in parallel (2489 on the oracle).
    use rayon::prelude::*;
    let interceptors: Vec<Result<(String, String), String>> = plan
        .methods
        .par_iter()
        .map(|(class, methods)| {
            let bytes = fused_chains
                .as_ref()
                .and_then(|c| crate::fused::fused_interceptor_bytes(magento, defs, c, class, methods))
                .or_else(|| crate::interceptor::interceptor_bytes(defs, class, methods));
            match bytes {
                Some(bytes) => Ok((format!("{}/Interceptor.php", class.replace('\\', "/")), bytes)),
                None => Err(format!("interceptor subject unresolved: {class}")),
            }
        })
        .collect();
    for result in interceptors {
        match result {
            Ok(file) => files.push(file),
            Err(finding) => findings.push(finding),
        }
    }

    ilap!(_it, "interceptor bytes");
    files.sort_by(|a, b| a.0.cmp(&b.0));
    ilap!(_it, "sort");
    GeneratedCode { files, findings }
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

/// A constructor parameter type that names a `<Subject>\Proxy` — captured the
/// same way as `factory_param_candidate` (cut at the first non-name char so a
/// union keeps only its first member, drop a leading `?`), but keyed on the
/// `\Proxy` *segment* (`ends_with("\\Proxy")`), mirroring the di.xml scanner's
/// `_filterEntities` gate. `ensure` then generates it only if the stripped
/// subject is a real class.
fn proxy_param_candidate(ty: &str) -> Option<&str> {
    let s = ty.strip_prefix('?').unwrap_or(ty);
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '\\'))
        .unwrap_or(s.len());
    let cap = &s[..end];
    cap.ends_with("\\Proxy").then_some(cap)
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
    fn proxy_param_candidates() {
        // A `\Proxy`-segment constructor param (already alias-resolved to its
        // FQCN by the parser) is a proxy target; `?` and unions handled like
        // the factory scan.
        assert_eq!(proxy_param_candidate("Foo\\Bar\\Proxy"), Some("Foo\\Bar\\Proxy"));
        assert_eq!(proxy_param_candidate("?Foo\\Bar\\Proxy"), Some("Foo\\Bar\\Proxy"));
        assert_eq!(proxy_param_candidate("Foo\\A\\Proxy|null"), Some("Foo\\A\\Proxy"));
        // "Proxy" only as a name suffix, not its own segment, is not a proxy.
        assert_eq!(proxy_param_candidate("Foo\\MyProxy"), None);
        assert_eq!(proxy_param_candidate("Foo\\Bar"), None);
        assert_eq!(proxy_param_candidate("string"), None);
    }

    #[test]
    fn snake_camel_and_type_predicates() {
        assert_eq!(snake_to_camel("exclude_website_ids"), "excludeWebsiteIds");
        assert_eq!(snake_to_camel("stock_item"), "stockItem");
        assert_eq!(ucfirst("stockItem"), "StockItem");
        // class type → hint; scalar/array/mixed → no hint.
        assert!(is_valid_type_declaration("\\Magento\\Foo\\BarInterface"));
        assert!(!is_valid_type_declaration("int[]"));
        assert!(!is_valid_type_declaration("\\Magento\\Foo\\BarInterface[]"));
        assert!(!is_valid_type_declaration("int"));
        assert!(!is_valid_type_declaration("string"));
        assert!(!is_valid_type_declaration("mixed"));
        assert!(!is_valid_type_declaration("boolean"));
        // FQCN forcing.
        assert_eq!(render_attr_type("int[]"), "int[]");
        assert_eq!(
            render_attr_type("Magento\\Foo\\Bar"),
            "\\Magento\\Foo\\Bar"
        );
        assert_eq!(render_attr_type("\\Magento\\Foo\\Bar"), "\\Magento\\Foo\\Bar");
    }

    #[test]
    fn extension_bytes_shape() {
        let mut map = std::collections::HashMap::new();
        map.insert(
            "Magento\\Customer\\Api\\Data\\GroupInterface".to_owned(),
            vec![("exclude_website_ids".to_owned(), "int[]".to_owned())],
        );
        let cfg = ExtConfig { map };
        let cls = extension_bytes(
            "Magento\\Customer\\Api\\Data\\GroupExtension",
            GenKind::Extension,
            &cfg,
        );
        assert!(cls.starts_with("<?php\nnamespace Magento\\Customer\\Api\\Data;\n\n/**\n * Extension class for @see \\Magento\\Customer\\Api\\Data\\GroupInterface\n */\nclass GroupExtension extends \\Magento\\Framework\\Api\\AbstractSimpleObject implements GroupExtensionInterface\n{\n"));
        assert!(cls.contains("    public function getExcludeWebsiteIds()\n    {\n        return $this->_get('exclude_website_ids');\n    }\n\n"));
        assert!(cls.contains("    public function setExcludeWebsiteIds($excludeWebsiteIds)\n    {\n        $this->setData('exclude_website_ids', $excludeWebsiteIds);\n        return $this;\n    }\n}\n"));
        let iface = extension_bytes(
            "Magento\\Customer\\Api\\Data\\GroupExtensionInterface",
            GenKind::ExtensionInterface,
            &cfg,
        );
        assert!(iface.contains("interface GroupExtensionInterface extends \\Magento\\Framework\\Api\\ExtensionAttributesInterface\n{\n"));
        assert!(iface.contains("    public function getExcludeWebsiteIds();\n\n"));
        assert!(iface.ends_with("    public function setExcludeWebsiteIds($excludeWebsiteIds);\n}\n"));
        // Empty attribute set → bare body.
        let empty = ExtConfig { map: std::collections::HashMap::new() };
        let e = extension_bytes(
            "Magento\\Bundle\\Api\\Data\\LinkExtension",
            GenKind::Extension,
            &empty,
        );
        assert!(e.ends_with("implements LinkExtensionInterface\n{\n}\n"));
    }

    #[test]
    fn extension_setter_type_hint() {
        let mut map = std::collections::HashMap::new();
        map.insert(
            "Foo\\ProductInterface".to_owned(),
            vec![(
                "stock_item".to_owned(),
                "Magento\\CatalogInventory\\Api\\Data\\StockItemInterface".to_owned(),
            )],
        );
        let cfg = ExtConfig { map };
        let cls = extension_bytes("Foo\\ProductExtension", GenKind::Extension, &cfg);
        // Class type ⇒ setter carries the hint; docblock uses the FQCN form.
        assert!(cls.contains("public function setStockItem(\\Magento\\CatalogInventory\\Api\\Data\\StockItemInterface $stockItem)"));
        assert!(cls.contains(" * @return \\Magento\\CatalogInventory\\Api\\Data\\StockItemInterface|null"));
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
