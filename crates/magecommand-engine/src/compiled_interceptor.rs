//! `<Class>\Interceptor` in creatuity/magento2-interceptors' "compiled" format,
//! byte-identical to `Creatuity\Interception\Generator\CompiledInterceptor`
//! (1.3.8), which Mage-OS 4 prefers over Magento's own interceptor generator.
//!
//! The plugins are baked into the class: each intercepted method unrolls its
//! before/around/after chain, with a `switch` on the current config scope when
//! the scopes disagree, and every plugin comes from a lazy private getter over
//! a constructor-injected object manager (`____om`) and scope (`____scope`).
//!
//! The per-scope chains are what creatuity's `AreasPluginList` computes at
//! runtime: one `CompiledPluginList` per scope of `Config\Scope::getAllScopes()`
//! (`primary`, `global`, then `AreaList::getCodes()`), iterated as
//! `global, <areas…>, primary` because the `primary` list is assigned last.
//! [`AreasPluginList`] computes them once for many subjects.

use std::collections::HashSet;
use std::sync::Arc;

use magequery_core::{Area, Magento};

use crate::definitions::Definitions;
use crate::laminas::{Class, DocBlock, Method, Param, Property, Visibility};
use crate::pluginlist::{runtime_chains, RuntimeChains, RuntimeScope};
use crate::reflect::{self, RMethod, RParam};

const SCOPE_INTERFACE: &str = "Magento\\Framework\\Config\\ScopeInterface";
const OM_INTERFACE: &str = "Magento\\Framework\\ObjectManagerInterface";

/// `propertiesToInjectToConstructor()`: type => property, in this order.
const INJECTED: [(&str, &str); 2] = [(SCOPE_INTERFACE, "____scope"), (OM_INTERFACE, "____om")];

/// creatuity's `AreasPluginList::getPluginsConfigForAllAreas()`: the scope
/// codes in iteration order, each with its runtime plugin-list state.
pub struct AreasPluginList {
    scopes: Vec<(String, Arc<RuntimeChains>)>,
}

impl AreasPluginList {
    /// Compute every scope's plugin list, as left after `getNext` was asked
    /// about each of `subjects` in order (a long-lived PHP process generates
    /// interceptors in autoload order through ONE shared `AreasPluginList`, and
    /// the inherit walk is order-dependent in one corner: an ancestor already
    /// cached hands its subclasses its disabled plugins too). Scopes run in
    /// parallel.
    ///
    /// Every class whose methods the walk reads — the subjects, their
    /// ancestors, the plugin classes declared on them — must already be in
    /// `defs`; [`AreasPluginList::required_classes`] lists them.
    pub fn new(magento: &Magento, defs: &Definitions, subjects: &[String]) -> AreasPluginList {
        use rayon::prelude::*;
        let areas = crate::pluginlist::area_list_codes(magento);
        let scope_of = |code: &str| -> RuntimeScope {
            match code {
                "frontend" => RuntimeScope::Area(Area::Frontend),
                "adminhtml" => RuntimeScope::Area(Area::Adminhtml),
                "crontab" => RuntimeScope::Area(Area::Crontab),
                "webapi_rest" => RuntimeScope::Area(Area::WebapiRest),
                "webapi_soap" => RuntimeScope::Area(Area::WebapiSoap),
                "graphql" => RuntimeScope::Area(Area::Graphql),
                other => RuntimeScope::Custom(other.to_owned()),
            }
        };
        // `primary` loads [global, primary]; `global` is a clone of it with
        // nothing left to load, so the two lists are identical: compute once.
        let mut jobs: Vec<RuntimeScope> = vec![RuntimeScope::Global];
        jobs.extend(areas.iter().map(|c| scope_of(c)));
        let computed: Vec<Arc<RuntimeChains>> = jobs
            .par_iter()
            .map(|scope| Arc::new(runtime_chains(magento, defs, scope, subjects)))
            .collect();
        let global = computed[0].clone();
        let mut scopes = vec![("global".to_owned(), global.clone())];
        for (code, chains) in areas.iter().zip(computed.into_iter().skip(1)) {
            scopes.push((code.clone(), chains));
        }
        scopes.push(("primary".to_owned(), global));
        AreasPluginList { scopes }
    }

    /// The scope codes in `getPluginsConfigForAllAreas()` order.
    pub fn scope_codes(&self) -> Vec<&str> {
        self.scopes.iter().map(|(c, _)| c.as_str()).collect()
    }

    /// Class names the walk over `subjects` needs reflected: the plugin
    /// classes declared on any subject or ancestor. Load them (with their
    /// closure) into `defs` before [`AreasPluginList::new`]; iterate until
    /// nothing new appears if the ancestors themselves were not loaded yet.
    pub fn required_classes(magento: &Magento, defs: &Definitions, subjects: &[String]) -> Vec<String> {
        let mut types: HashSet<String> = HashSet::new();
        let mut frontier: Vec<String> =
            subjects.iter().map(|s| s.trim_start_matches('\\').to_owned()).collect();
        while let Some(t) = frontier.pop() {
            if !types.insert(t.clone()) {
                continue;
            }
            for r in defs.relations_of(&t).iter() {
                frontier.push(r.clone());
            }
            if let (None, Some(rel)) = (defs.get(&t), crate::interception::internal_relations(&t)) {
                frontier.extend(rel.iter().map(|r| r.to_string()));
            }
        }
        crate::pluginlist::plugin_classes_on(magento, &types)
    }
}

/// One plugin as `getPluginInfo` registers it.
#[derive(Clone, PartialEq, Debug)]
struct PluginInfo {
    code: String,
    class: String,
    clean_name: String,
}

/// `getPluginsChain`'s result: one `_processed` node with its names resolved
/// to plugin infos and the around plugin's `next` chain nested.
#[derive(Clone, PartialEq, Debug)]
struct Chain {
    before: Option<Vec<PluginInfo>>,
    around: Option<(PluginInfo, Option<Box<Chain>>)>,
    after: Option<Vec<PluginInfo>>,
}

/// `$allPlugins`: code => (class => info), both in insertion order.
#[derive(Default)]
struct AllPlugins(Vec<(String, Vec<PluginInfo>)>);

impl AllPlugins {
    /// `getPluginInfo`: register `(code, class)` on first sight — a second
    /// class under the same code gets suffix `2`, a third `3`, …
    fn info(&mut self, code: &str, class: &str) -> PluginInfo {
        let slot = match self.0.iter().position(|(c, _)| c == code) {
            Some(i) => i,
            None => {
                self.0.push((code.to_owned(), Vec::new()));
                self.0.len() - 1
            }
        };
        let classes = &mut self.0[slot].1;
        if let Some(found) = classes.iter().find(|p| p.class == class) {
            return found.clone();
        }
        let suffix = if classes.is_empty() { String::new() } else { (classes.len() + 1).to_string() };
        let info = PluginInfo {
            code: code.to_owned(),
            class: class.to_owned(),
            clean_name: clean_name(&format!("{code}{suffix}")),
        };
        classes.push(info.clone());
        info
    }
}

/// `preg_replace("/[^A-Za-z0-9_]/", '_', …)` — per byte, so a multibyte
/// character becomes one underscore per byte.
fn clean_name(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b == b'_' { b as char } else { '_' })
        .collect()
}

fn ucfirst(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) if f.is_ascii() => f.to_ascii_uppercase().to_string() + c.as_str(),
        Some(f) => f.to_string() + c.as_str(),
        None => String::new(),
    }
}

fn lcfirst(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) if f.is_ascii() => f.to_ascii_lowercase().to_string() + c.as_str(),
        Some(f) => f.to_string() + c.as_str(),
        None => String::new(),
    }
}

/// `getPluginsChain` for one scope. Registration order follows PHP's
/// evaluation order: befores, afters, then the around's `next` chain (an
/// argument, so evaluated first) and only then the around plugin itself.
fn plugins_chain(
    chains: &RuntimeChains,
    subject: &str,
    method: &str,
    next: &str,
    all: &mut AllPlugins,
) -> Result<Option<Chain>, String> {
    let key = format!("{subject}_{}_{next}", lcfirst(method));
    let Some((before, around, after)) = chains.nodes.get(&key) else {
        return Ok(None);
    };
    let instances = chains.instances.get(subject);
    let plugin_type = |code: &str| -> Result<String, String> {
        instances
            .and_then(|list| list.iter().find(|(n, _)| n == code))
            .map(|(_, i)| i.clone())
            .ok_or_else(|| format!("plugin {code} of {subject} has no instance"))
    };
    let mut chain = Chain { before: None, around: None, after: None };
    if !before.is_empty() {
        let mut list = Vec::new();
        for code in before {
            let class = plugin_type(code)?;
            list.push(all.info(code, &class));
        }
        chain.before = Some(list);
    }
    if !after.is_empty() {
        let mut list = Vec::new();
        for code in after {
            let class = plugin_type(code)?;
            list.push(all.info(code, &class));
        }
        chain.after = Some(list);
    }
    if let Some(code) = around {
        let class = plugin_type(code)?;
        let nested = plugins_chain(chains, subject, method, code, all)?.map(Box::new);
        chain.around = Some((all.info(code, &class), nested));
    }
    Ok(Some(chain))
}

/// The generated `<subject>\Interceptor` file, byte-exact.
///
/// `Err` carries the reason no file can be produced: an unknown subject, an
/// interface (creatuity's generator returns `false` for one), or a plugin
/// whose instance cannot be resolved.
pub fn compiled_interceptor_bytes(
    defs: &Definitions,
    plugins: &AreasPluginList,
    subject: &str,
) -> Result<String, String> {
    let subject = subject.trim_start_matches('\\');
    let record = defs.get(subject).ok_or_else(|| format!("interceptor subject unresolved: {subject}"))?;
    if record.meta.kind == magecommand_php::ClassKind::Interface {
        return Err(format!("interceptor subject is an interface: {subject}"));
    }
    // The class name as requested: `getSourceClassName()` keys every lookup.
    let source_bs = format!("\\{subject}");

    let mut properties: Vec<Property> = INJECTED
        .iter()
        .map(|(ty, name)| Property {
            name: (*name).to_owned(),
            visibility: Visibility::Private,
            default: None,
            doc: DocBlock {
                tags: vec![("var".into(), ty.rsplit('\\').next().unwrap_or(ty).to_owned())],
                ..Default::default()
            },
        })
        .collect();

    let mut methods = vec![constructor(defs, subject)];
    if has_method(defs, subject, "__sleep") {
        methods.push(simple("__sleep", sleep_body()));
    }
    if has_method(defs, subject, "__wakeup") {
        methods.push(simple("__wakeup", wakeup_body()));
    }

    let mut all = AllPlugins::default();
    for rm in reflect::public_methods(defs, subject) {
        if !is_intercepted(&rm) {
            continue;
        }
        // getPluginsConfig: one chain per non-empty scope, `_empty` closing a
        // set that leaves some scope without plugins.
        let mut config: Vec<(&str, Option<Chain>)> = Vec::new();
        let mut has_empty = false;
        for (code, chains) in &plugins.scopes {
            match plugins_chain(chains, subject, &rm.name, "__self", &mut all)? {
                Some(chain) => config.push((code, Some(chain))),
                None => has_empty = true,
            }
        }
        if config.is_empty() {
            continue;
        }
        if has_empty {
            config.push(("_empty", None));
        }
        methods.push(compiled_method(&rm, &config));
    }
    for (_, classes) in &all.0 {
        for p in classes {
            methods.push(plugin_getter(p));
            properties.push(Property {
                name: format!("____plugin_{}", p.clean_name),
                visibility: Visibility::Private,
                default: None,
                doc: DocBlock { tags: vec![("var".into(), format!("\\{}", p.class))], ..Default::default() },
            });
        }
    }

    let class = Class {
        namespace: Some(subject.to_owned()),
        name: "Interceptor".into(),
        is_interface: false,
        extends: Some(source_bs.clone()),
        implements: vec![],
        traits: vec![],
        doc: DocBlock { short: Some(format!("Interceptor class for @see {source_bs}")), ..Default::default() },
        properties,
        methods,
    };
    let uses: Vec<String> = INJECTED.iter().map(|(t, _)| (*t).to_owned()).collect();
    Ok(class.render_with_uses(&uses))
}

/// Render many interceptors at once (in parallel), keyed by subject.
pub fn compiled_interceptors(
    defs: &Definitions,
    plugins: &AreasPluginList,
    subjects: &[String],
) -> Vec<(String, Result<String, String>)> {
    use rayon::prelude::*;
    subjects
        .par_iter()
        .map(|s| (s.clone(), compiled_interceptor_bytes(defs, plugins, s)))
        .collect()
}

/// `isInterceptedMethod`.
fn is_intercepted(rm: &RMethod) -> bool {
    let name = rm.name.as_str();
    !(name.eq_ignore_ascii_case("__construct")
        || name.eq_ignore_ascii_case("__destruct")
        || rm.is_final
        || rm.is_static)
        && !matches!(name, "__sleep" | "__wakeup" | "__clone")
}

/// `ReflectionClass::hasMethod`: any visibility, own, trait-imported or
/// inherited.
fn has_method(defs: &Definitions, fqcn: &str, name: &str) -> bool {
    fn walk(defs: &Definitions, fqcn: &str, name: &str, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        let Some(rec) = defs.get(fqcn) else { return false };
        if rec.meta.methods.iter().any(|m| m.name.eq_ignore_ascii_case(name)) {
            return true;
        }
        if rec.meta.traits.iter().any(|t| walk(defs, t, name, depth + 1)) {
            return true;
        }
        rec.meta.extends.first().is_some_and(|p| walk(defs, p, name, depth + 1))
    }
    walk(defs, fqcn, name, 0)
}

fn to_param(p: &RParam) -> Param {
    Param {
        name: p.name.clone(),
        type_str: p.type_str.clone(),
        by_ref: p.by_ref,
        variadic: p.variadic,
        default: p.default.clone(),
    }
}

fn inheritdoc() -> DocBlock {
    DocBlock { short: Some("{@inheritdoc}".into()), ..Default::default() }
}

fn simple(name: &str, body: String) -> Method {
    Method {
        name: name.into(),
        visibility: Visibility::Public,
        is_static: false,
        is_final: false,
        is_abstract: false,
        returns_ref: false,
        params: vec![],
        return_type: None,
        body: Some(body),
        doc: inheritdoc(),
    }
}

/// `injectPropertiesSettersToConstructor`: the parent constructor's
/// parameters, preceded by `$____om`/`$____scope` unless a parent parameter
/// already has that type (then the property is set from it instead).
fn constructor(defs: &Definitions, subject: &str) -> Method {
    let ctor = defs.constructor_of(subject).ok().flatten();
    let parent_params: Vec<RParam> = match &ctor {
        Some(info) => reflect::resolve_params(defs, info.definer_fqcn, info.params),
        None => Vec::new(),
    };
    let mut body: Vec<String> = Vec::new();
    if ctor.is_some() {
        let call: Vec<String> = parent_params.iter().map(|p| format!("${}", p.name)).collect();
        body.push(format!("parent::__construct({});", call.join(", ")));
    }
    let mut extra_params: Vec<(&str, &str)> = INJECTED.to_vec();
    let mut setters: Vec<(&str, String)> = INJECTED.iter().map(|(_, n)| (*n, (*n).to_owned())).collect();
    for p in &parent_params {
        let Some(ty) = &p.type_str else { continue };
        // `getType()->getName()`: the named type, without its nullable marker.
        let ty = ty.trim_start_matches('?').trim_start_matches('\\');
        if let Some((_, prop)) = INJECTED.iter().find(|(t, _)| *t == ty) {
            if let Some(slot) = setters.iter_mut().find(|(n, _)| n == prop) {
                slot.1 = p.name.clone();
            }
            extra_params.retain(|(t, _)| *t != ty);
        }
    }
    let mut params: Vec<Param> = parent_params.iter().map(to_param).collect();
    for (ty, name) in extra_params {
        params.insert(
            0,
            Param { name: name.to_owned(), type_str: Some(ty.to_owned()), by_ref: false, variadic: false, default: None },
        );
    }
    for (name, param) in setters {
        body.insert(0, format!("$this->{name} = ${param};"));
    }
    Method {
        name: "__construct".into(),
        visibility: Visibility::Public,
        is_static: false,
        is_final: false,
        is_abstract: false,
        returns_ref: false,
        params,
        return_type: None,
        body: Some(body.join("\n")),
        doc: inheritdoc(),
    }
}

fn sleep_body() -> String {
    let mut body = vec![
        "$properties = parent::__sleep();".to_owned(),
        "return \\array_diff(".to_owned(),
        "\t$properties,".to_owned(),
        "\t[".to_owned(),
    ];
    for (_, name) in INJECTED {
        body.push(format!("\t\t'{name}',"));
    }
    body.push("\t]".to_owned());
    body.push(");".to_owned());
    body.join("\n")
}

fn wakeup_body() -> String {
    let mut body = vec![
        "parent::__wakeup();".to_owned(),
        "$objectManager = \\Magento\\Framework\\App\\ObjectManager::getInstance();".to_owned(),
    ];
    for (ty, name) in INJECTED {
        body.push(format!("$this->{name} = $objectManager->get(\\{ty}::class);"));
    }
    body.join("\n")
}

fn getter_name(p: &PluginInfo) -> String {
    format!("____plugin_{}", p.clean_name)
}

fn plugin_getter(p: &PluginInfo) -> Method {
    let var = format!("$this->____plugin_{}", p.clean_name);
    let body = [
        format!("if ({var} === null) {{"),
        format!("\t{var} = $this->____om->get(\\{}::class);", p.class),
        "}".to_owned(),
        format!("return {var};"),
    ]
    .join("\n");
    Method {
        name: getter_name(p),
        visibility: Visibility::Private,
        is_static: false,
        is_final: false,
        is_abstract: false,
        returns_ref: false,
        params: vec![],
        return_type: None,
        body: Some(body),
        doc: DocBlock {
            short: Some(format!("plugin \"{}\"\n@return \\{}", p.code, p.class)),
            ..Default::default()
        },
    }
}

fn sub_block(out: &mut Vec<String>, lines: Vec<String>, indent: usize) {
    let tabs = "\t".repeat(indent);
    for line in lines {
        out.push(format!("{tabs}{line}"));
    }
}

/// `getMethodSourceFromConfig`.
fn method_source(name: &str, conf: Option<&Chain>, params: &[RParam], void: bool) -> Vec<String> {
    let cap = ucfirst(name);
    let has_params = !params.is_empty();
    let mut body: Vec<String> = vec!["$arguments = \\func_get_args();".to_owned()];
    if let Some(before) = conf.and_then(|c| c.before.as_ref()) {
        for p in before {
            let call = format!("$this->{}()->before{cap}($this, ...\\array_values($arguments));", getter_name(p));
            if has_params {
                body.push(format!("$beforeResult = {call}"));
                body.push("if ($beforeResult !== null) $arguments = (array)$beforeResult;".to_owned());
            } else {
                body.push(call);
            }
            body.push(String::new());
        }
    }
    let mut chain: Vec<Vec<String>> = Vec::new();
    match conf.and_then(|c| c.around.as_ref()) {
        Some((p, next)) => {
            let mut lines =
                vec![format!("$this->{}()->around{cap}($this, function(...$arguments){{", getter_name(p))];
            sub_block(&mut lines, method_source(name, next.as_deref(), params, void), 1);
            lines.push("}, ...\\array_values($arguments));".to_owned());
            chain.push(lines);
        }
        None => chain.push(vec![format!("parent::{name}(...\\array_values($arguments));")]),
    }
    if let Some(after) = conf.and_then(|c| c.after.as_ref()) {
        for p in after {
            let result = if void { "null" } else { "$result" };
            chain.push(vec![format!(
                "$this->{}()->after{cap}($this, {result}, ...\\array_values($arguments));",
                getter_name(p)
            )]);
        }
    }
    // getResultChainLines
    let count = chain.len();
    for (i, mut piece) in chain.into_iter().enumerate() {
        if i > 0 {
            body.push(String::new());
        }
        if !void {
            let prefix = if i + 1 == count { "return " } else { "$result = " };
            piece[0] = format!("{prefix}{}", piece[0]);
        }
        body.extend(piece);
    }
    body
}

/// `getCompiledMethodInfo`.
fn compiled_method(rm: &RMethod, config: &[(&str, Option<Chain>)]) -> Method {
    let void = rm.return_type.as_deref() == Some("void");
    // getScopeCasesFromConfig: group scopes with equal chains, in first-seen
    // order; the last group becomes `default:`.
    let mut cases: Vec<(Vec<String>, Option<&Chain>)> = Vec::new();
    for (scope, conf) in config {
        let case = format!("\tcase '{scope}':");
        match cases.iter_mut().find(|(_, c)| *c == conf.as_ref()) {
            Some((labels, _)) => labels.push(case),
            None => cases.push((vec![case], conf.as_ref())),
        }
    }
    if let Some(last) = cases.last_mut() {
        last.0 = vec!["\tdefault:".to_owned()];
    }
    let body = if cases.len() == 1 {
        method_source(&rm.name, cases[0].1, &rm.params, void)
    } else {
        let mut body = vec!["switch ($this->____scope->getCurrentScope()) {".to_owned()];
        for (labels, conf) in cases {
            body.extend(labels);
            sub_block(&mut body, method_source(&rm.name, conf, &rm.params, void), 2);
            if void {
                body.push("\t\tbreak;".to_owned());
            }
        }
        body.push("}".to_owned());
        body
    };
    Method {
        name: rm.name.clone(),
        visibility: Visibility::Public,
        is_static: false,
        is_final: false,
        is_abstract: false,
        returns_ref: rm.returns_ref,
        params: rm.params.iter().map(to_param).collect(),
        return_type: rm.return_type.clone(),
        body: Some(body.join("\n")),
        doc: inheritdoc(),
    }
}
