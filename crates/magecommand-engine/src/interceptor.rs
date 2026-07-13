//! `<Class>\Interceptor` — the plugin dispatch subclass. The setup-specific
//! `Magento\Setup\Module\Di\Code\Generator\Interceptor`: extend (or implement)
//! the subject, `use` the `Interceptor` trait, forward the constructor, and
//! emit one dispatching method per intercepted method.
//!
//! The intercepted-method set per class is the union — across every scope — of
//! the target methods declared by every plugin applying to the class (its own
//! plus inherited from ancestors/interfaces), reproducing
//! `InterceptionConfigurationBuilder`.

use std::collections::{HashMap, HashSet};

use magequery_core::{Area, Magento};

use crate::definitions::Definitions;
use crate::laminas::{Class, DocBlock, Method, Param, Visibility};
use crate::reflect::{self, RMethod, RParam};

const INTERCEPTOR_TRAIT: &str = "Magento\\Framework\\Interception\\Interceptor";
const INTERCEPTOR_INTERFACE: &str = "Magento\\Framework\\Interception\\InterceptorInterface";
const NONINTERCEPTABLE: &str = "Magento\\Framework\\ObjectManager\\NoninterceptableInterface";

/// Everything the interceptor operation needs: which classes get an
/// interceptor, and for each the set of intercepted method names.
pub struct InterceptionPlan {
    /// class -> intercepted method names (may be empty: constructor only).
    pub methods: HashMap<String, HashSet<String>>,
}

/// Build the plan: `getInterceptedClasses` (concrete + has-plugins +
/// interceptable, over the app+lib+generated scan) then `getInterceptedMethods`.
pub fn plan(magento: &Magento, defs: &Definitions) -> InterceptionPlan {
    let has_plugins = crate::interception::interception_map(magento, defs);

    // Seeds = getInterceptedClasses: scanned (app+lib+generated) classes that
    // pass has-plugins + concrete + interceptable.
    let mut seeds: HashSet<String> = HashSet::new();
    for class in &defs.scanned {
        if defs.setup_classes.contains(class) {
            continue;
        }
        if is_intercepted_class(defs, &has_plugins, class) {
            seeds.insert(class.clone());
        }
    }

    // The plugin-config keyset the seeds populate (seeds + their concrete
    // ancestors carrying plugins), each with its applying plugin instances.
    let instances = crate::pluginlist::plugin_instances_across_scopes(magento, defs, &seeds);

    // filterNullInheritance keeps a key only when the KEY class is concrete;
    // the interceptable/seed validation does NOT re-apply to ancestor keys.
    // (Whether a class survives at all is already decided by the inherit walk:
    // a class inheriting only disabled plugins from an ancestor gets a null
    // config and never appears here.)
    let set: HashSet<String> = instances
        .keys()
        .filter(|k| defs.is_concrete(k))
        .cloned()
        .collect();

    // Deref map for turning a plugin instance (possibly a virtual type) into
    // the concrete plugin class whose methods define the interception.
    let global_vtypes: HashMap<String, String> = magento
        .di_export(Area::Global)
        .virtual_types
        .iter()
        .map(|v| (v.name.as_str().to_owned(), v.base.as_str().to_owned()))
        .collect();

    // Per plugin class, its target methods — memoized (plugins repeat a lot).
    let mut target_cache: HashMap<String, Vec<String>> = HashMap::new();
    let mut methods: HashMap<String, HashSet<String>> = HashMap::new();
    for class in &set {
        let mut targets: HashSet<String> = HashSet::new();
        if let Some(plugin_instances) = instances.get(class) {
            for instance in &plugin_instances.instances {
                let plugin_type = chase(&global_vtypes, instance.trim_start_matches('\\'));
                let entry = target_cache
                    .entry(plugin_type.clone())
                    .or_insert_with(|| plugin_targets(defs, &plugin_type));
                targets.extend(entry.iter().cloned());
            }
        }
        // Canonicalize the key to the class's DECLARED case. A di.xml
        // `<type name=…>` may spell a class in a different case than its
        // source declares (PHP class names are case-insensitive), but the
        // generated `<Class>/Interceptor.php` path and `@see`/`extends` follow
        // the reflection name — the declared case. `defs.get` resolves
        // case-insensitively; its record carries the canonical fqcn. Two
        // spellings collapsing to one class union their intercepted methods.
        let canon = defs.get(class).map(|r| r.meta.fqcn.clone()).unwrap_or_else(|| class.clone());
        methods.entry(canon).or_default().extend(targets);
    }

    InterceptionPlan { methods }
}

/// `getInterceptedClasses`'s filter for one class.
fn is_intercepted_class(
    defs: &Definitions,
    has_plugins: &std::collections::BTreeMap<String, bool>,
    class: &str,
) -> bool {
    if !has_plugins.get(class).copied().unwrap_or(false) {
        return false;
    }
    // Type::isConcrete — class exists, not abstract, not interface.
    if !defs.is_concrete(class) {
        return false;
    }
    // InterceptableValidator: not itself an interceptor, not marked
    // non-interceptable (proxies implement NoninterceptableInterface).
    if class.ends_with("\\Interceptor") {
        return false;
    }
    if defs
        .all_interfaces(class)
        .iter()
        .any(|i| i.trim_start_matches('\\') == NONINTERCEPTABLE)
    {
        return false;
    }
    true
}

/// `Runtime::getMethodList` keys: the TARGET methods a plugin class defines
/// via its public `before*`/`around*`/`after*` methods (`lcfirst` of the
/// stripped name). Deduped, order-insensitive (used as a set).
fn plugin_targets(defs: &Definitions, plugin_type: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for method in defs.public_methods(plugin_type) {
        let prefix = &method[..method.len().min(5)];
        let (cut, _) = match prefix {
            "befor" => (6, 1),
            "aroun" => (6, 2),
            "after" => (5, 4),
            _ => continue,
        };
        if method.len() < cut {
            continue;
        }
        let target = lcfirst(&method[cut..]);
        if !out.contains(&target) {
            out.push(target);
        }
    }
    out
}

/// The generated `<source>\Interceptor` file, byte-exact. `intercepted` is the
/// method set from [`plan`]. Returns `None` if the subject isn't known.
pub fn interceptor_bytes(
    defs: &Definitions,
    source: &str,
    intercepted: &HashSet<String>,
) -> Option<String> {
    let record = defs.get(source)?;
    let is_interface = record.meta.kind == magecommand_php::ClassKind::Interface;
    let source_bs = format!("\\{source}");

    let mut methods = Vec::new();
    methods.push(constructor(defs, source));

    for rm in reflect::public_methods(defs, source) {
        let name = rm.name.as_str();
        if rm.is_static
            || rm.is_final
            || name.eq_ignore_ascii_case("__construct")
            || name.eq_ignore_ascii_case("__destruct")
            || matches!(name, "__sleep" | "__wakeup" | "__clone" | "_resetState")
        {
            continue;
        }
        if !intercepted.contains(&rm.name) {
            continue;
        }
        methods.push(dispatch_method(&rm));
    }

    let (extends, implements) = if is_interface {
        (None, vec![source_bs.clone(), format!("\\{INTERCEPTOR_INTERFACE}")])
    } else {
        (Some(source_bs.clone()), vec![format!("\\{INTERCEPTOR_INTERFACE}")])
    };

    let class = Class {
        namespace: Some(source.to_owned()),
        name: "Interceptor".into(),
        is_interface: false,
        extends,
        implements,
        traits: vec![format!("\\{INTERCEPTOR_TRAIT}")],
        doc: DocBlock {
            short: Some(format!("Interceptor class for @see {source_bs}")),
            ..Default::default()
        },
        properties: vec![],
        methods,
    };
    Some(class.render())
}

/// `_getDefaultConstructorDefinition`: forward the subject's constructor (the
/// nearest in the hierarchy), calling `___init()` then `parent::__construct`.
fn constructor(defs: &Definitions, source: &str) -> Method {
    let ctor = defs.constructor_of(source).ok().flatten();
    let (params, body) = match ctor {
        Some(info) => {
            let params: Vec<RParam> =
                reflect::resolve_params(defs, info.definer_fqcn, info.params);
            let forwarded = forward_list(&params);
            let call = if params.is_empty() {
                "parent::__construct();".to_owned()
            } else {
                format!("parent::__construct({forwarded});")
            };
            let laminas_params = params.iter().map(to_param).collect();
            (laminas_params, format!("$this->___init();\n{call}"))
        }
        None => (vec![], "$this->___init();".to_owned()),
    };
    Method {
        name: "__construct".into(),
        visibility: Visibility::Public,
        is_static: false,
        is_final: false,
        is_abstract: false,
        returns_ref: false,
        params,
        return_type: None,
        body: Some(body),
        doc: DocBlock::default(),
    }
}

/// One dispatching method: consult the plugin list, call plugins or fall
/// through to `parent::method(...)`.
fn dispatch_method(rm: &RMethod) -> Method {
    let forwarded = forward_list(&rm.params);
    let without_return = rm.return_type.as_deref() == Some("void");
    let ret = if without_return { "" } else { "return " };
    let m = &rm.name;
    let body = format!(
        "$pluginInfo = $this->pluginList->getNext($this->subjectType, '{m}');\n{ret}$pluginInfo ? $this->___callPlugins('{m}', func_get_args(), $pluginInfo) : parent::{m}({forwarded});"
    );
    Method {
        name: rm.name.clone(),
        visibility: Visibility::Public,
        is_static: false,
        is_final: false,
        is_abstract: false,
        returns_ref: rm.returns_ref,
        params: rm.params.iter().map(to_param).collect(),
        return_type: rm.return_type.clone(),
        body: Some(body),
        doc: DocBlock { short: Some("{@inheritdoc}".into()), ..Default::default() },
    }
}

/// `_getParameterList`: forward params by name, variadic as `... $name`.
fn forward_list(params: &[RParam]) -> String {
    params
        .iter()
        .map(|p| if p.variadic { format!("... ${}", p.name) } else { format!("${}", p.name) })
        .collect::<Vec<_>>()
        .join(", ")
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

// Re-exported small helpers mirroring pluginlist's (kept local to avoid
// widening that module's surface).
fn chase(map: &HashMap<String, String>, name: &str) -> String {
    let mut current = name.trim_start_matches('\\').to_owned();
    let mut seen = HashSet::new();
    while let Some(next) = map.get(&current) {
        let next = next.trim_start_matches('\\').to_owned();
        if !seen.insert(next.clone()) {
            break;
        }
        current = next;
    }
    current
}

fn lcfirst(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rparam(name: &str, variadic: bool) -> RParam {
        RParam {
            name: name.into(),
            type_str: None,
            by_ref: false,
            variadic,
            default: None,
        }
    }

    fn rmethod(name: &str, params: Vec<RParam>, return_type: Option<&str>) -> RMethod {
        RMethod {
            name: name.into(),
            params,
            return_type: return_type.map(str::to_owned),
            returns_ref: false,
            is_static: false,
            is_final: false,
            is_abstract: false,
            declaring: "Foo\\Bar".into(),
        }
    }

    #[test]
    fn forward_list_names_params_and_spreads_variadics() {
        let params = vec![rparam("a", false), rparam("rest", true)];
        assert_eq!(forward_list(&params), "$a, ... $rest");
        assert_eq!(forward_list(&[]), "");
    }

    #[test]
    fn dispatch_body_returns_and_falls_through() {
        // A value-returning method: `return` prefix, plugin fast-path, parent
        // fallthrough forwarding the argument.
        let m = dispatch_method(&rmethod("getCurPage", vec![rparam("displacement", false)], None));
        assert_eq!(
            m.body.as_deref(),
            Some(
                "$pluginInfo = $this->pluginList->getNext($this->subjectType, 'getCurPage');\n\
                 return $pluginInfo ? $this->___callPlugins('getCurPage', func_get_args(), $pluginInfo) : parent::getCurPage($displacement);"
            )
        );
        assert_eq!(m.doc.short.as_deref(), Some("{@inheritdoc}"));
    }

    #[test]
    fn dispatch_body_omits_return_for_void() {
        // A `: void` method drops the `return ` — calling a void method in an
        // expression is a fatal error.
        let m = dispatch_method(&rmethod("execute", vec![], Some("void")));
        let body = m.body.unwrap();
        assert!(body.contains("\n$pluginInfo ? $this->___callPlugins('execute', func_get_args(), $pluginInfo) : parent::execute();"));
        assert!(!body.contains("return "));
    }

    #[test]
    fn plugin_target_from_prefix() {
        assert_eq!(lcfirst("Save"), "save");
        assert_eq!(lcfirst("GetList"), "getList");
        assert_eq!(lcfirst(""), "");
    }
}
