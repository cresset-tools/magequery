//! The `arguments` section: a port of Magento's compile-time argument
//! resolution — `ObjectManager\Config::_collectConfiguration` (vtype chains,
//! relations inheritance, SortItems) feeding `Setup\Di\Compiler\
//! ArgumentsResolver` (the `_i_/_ins_/_v_/_vn_/_vac_/_a_/_d_` encodings) —
//! over the parser's constructors and statically evaluated defaults.

use std::collections::{BTreeMap, HashMap, HashSet};

use magecommand_php::constexpr::{
    eval, parse_const_expr, ConstLookup, ConstValue, EvalCtx, ParsedExpr,
};
use magecommand_php::{ClassKind, ParamMeta};
use magequery_core::{ArgValue, DiExport, Magento};

use crate::definitions::Definitions;
use crate::phpexport::{PhpKey, PhpValue};

/// A merged-config value in PHP shape (`['instance' => …]` maps included).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Cfg {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    Map(Vec<(CfgKey, Cfg)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CfgKey {
    Int(i64),
    Str(String),
}

impl CfgKey {
    /// XML item names land in PHP arrays, which coerce int-like keys.
    fn from_xml(name: &str) -> CfgKey {
        match name.parse::<i64>() {
            Ok(i) if i.to_string() == name => CfgKey::Int(i),
            _ => CfgKey::Str(name.to_owned()),
        }
    }
    fn s(name: &str) -> CfgKey {
        CfgKey::Str(name.to_owned())
    }
}

fn map_get<'a>(entries: &'a [(CfgKey, Cfg)], key: &str) -> Option<&'a Cfg> {
    entries
        .iter()
        .find(|(k, _)| matches!(k, CfgKey::Str(s) if s == key))
        .map(|(_, v)| v)
}

pub struct Findings {
    /// Things reflection would know that static analysis couldn't — each one
    /// a hard fact to fix, never silently guessed around.
    pub issues: Vec<String>,
}

pub(crate) struct ArgsCtx<'a> {
    pub defs: &'a Definitions,
    /// Scan-set membership (the Reader's collection) — the extended
    /// hierarchy must not mint argument rows of its own.
    pub scanned: &'a HashSet<String>,
    /// DiCompileCommand::configureObjectManager's runtime overrides — they
    /// extend the very config object the Reader clones, so the compiled
    /// output bakes them (machine-specific exclude regexes included).
    overrides: Vec<(String, String, Cfg)>,
    type_args: HashMap<String, Vec<(String, &'a ArgValue)>>,
    non_shared: HashSet<String>,
    vtypes: HashMap<String, String>,
    pref_keys: Vec<String>,
    merged_cache: std::cell::RefCell<HashMap<String, Vec<(CfgKey, Cfg)>>>,
    findings: std::cell::RefCell<Vec<String>>,
}

impl<'a> ArgsCtx<'a> {
    pub(crate) fn new(
        defs: &'a Definitions,
        scanned: &'a HashSet<String>,
        export: &'a DiExport,
        overrides: Vec<(String, String, Cfg)>,
    ) -> Self {
        let mut type_args: HashMap<String, Vec<(String, &ArgValue)>> = HashMap::new();
        for decl in &export.arguments {
            type_args
                .entry(decl.type_name.as_str().to_owned())
                .or_default()
                .push((decl.arg.clone(), &decl.value));
        }
        let non_shared = export
            .shared
            .iter()
            .filter(|s| !s.shared)
            .map(|s| s.type_name.as_str().to_owned())
            .collect();
        let vtypes = export
            .virtual_types
            .iter()
            .map(|v| (v.name.as_str().to_owned(), v.base.as_str().to_owned()))
            .collect();
        let pref_keys = export
            .preferences
            .iter()
            .map(|p| p.for_type.as_str().to_owned())
            .collect();
        ArgsCtx {
            defs,
            scanned,
            overrides,
            type_args,
            non_shared,
            vtypes,
            pref_keys,
            merged_cache: Default::default(),
            findings: Default::default(),
        }
    }

    fn finding(&self, msg: String) {
        self.findings.borrow_mut().push(msg);
    }

    pub fn take_findings(&self) -> Vec<String> {
        std::mem::take(&mut self.findings.borrow_mut())
    }

    fn is_shared_type(&self, name: &str) -> bool {
        !self.non_shared.contains(name)
    }

    // ---- Config::_collectConfiguration -------------------------------------

    fn own_config(&self, name: &str) -> Option<Vec<(CfgKey, Cfg)>> {
        let mut entries: Vec<(CfgKey, Cfg)> = self
            .type_args
            .get(name)
            .map(|args| {
                args.iter()
                    .map(|(arg_name, value)| (CfgKey::s(arg_name), self.to_cfg(value, name)))
                    .collect()
            })
            .unwrap_or_default();
        // configureObjectManager layer: array_replace at argument level.
        for (ty, arg, value) in &self.overrides {
            if ty == name {
                match entries.iter_mut().find(
                    |(k, _)| matches!(k, CfgKey::Str(s) if s == arg),
                ) {
                    Some(slot) => slot.1 = value.clone(),
                    None => entries.push((CfgKey::s(arg), value.clone())),
                }
            }
        }
        if entries.is_empty() {
            return None;
        }
        Some(entries)
    }

    fn merged_config(&self, name: &str) -> Vec<(CfgKey, Cfg)> {
        if let Some(cached) = self.merged_cache.borrow().get(name) {
            return cached.clone();
        }
        let mut arguments: Vec<(CfgKey, Cfg)> = Vec::new();
        if let Some(base) = self.vtypes.get(name) {
            arguments = self.merged_config(base);
            sort_items(&mut arguments);
        } else if self.defs.contains(name) {
            for relation in self.defs.relations_of(name) {
                let relation_args = self.merged_config(&relation);
                if !relation_args.is_empty() {
                    array_replace(&mut arguments, relation_args);
                    sort_items(&mut arguments);
                }
            }
        }
        if let Some(own) = self.own_config(name) {
            if arguments.is_empty() {
                arguments = own;
            } else {
                array_replace_recursive(&mut arguments, own);
                sort_items(&mut arguments);
            }
        }
        self.merged_cache
            .borrow_mut()
            .insert(name.to_owned(), arguments.clone());
        arguments
    }

    // ---- di.xml value → PHP config shape -----------------------------------

    fn to_cfg(&self, value: &ArgValue, context: &str) -> Cfg {
        match value {
            ArgValue::Object(o) => {
                // DataObject interpreter order: instance, sortOrder, shared.
                let mut map = vec![(CfgKey::s("instance"), Cfg::Str(o.class.as_str().to_owned()))];
                if let Some(so) = o.sort_order {
                    map.push((CfgKey::s("sortOrder"), Cfg::Int(so as i64)));
                }
                if let Some(shared) = o.shared {
                    map.push((CfgKey::s("shared"), Cfg::Bool(shared)));
                }
                Cfg::Map(map)
            }
            ArgValue::Null => Cfg::Null,
            ArgValue::Array(items) => {
                // ArrayType interpreter: items stably sorted by their
                // sortOrder XML attribute at conversion time, each level
                // independently.
                let mut sorted: Vec<&magequery_core::ArgItem> = items.iter().collect();
                sorted.sort_by_key(|item| item.sort_order.unwrap_or(0));
                Cfg::Map(
                    sorted
                        .into_iter()
                        .map(|item| {
                            (CfgKey::from_xml(&item.key), self.to_cfg(&item.value, context))
                        })
                        .collect(),
                )
            }
            ArgValue::Scalar { xsi_type, text } => match xsi_type.as_str() {
                "boolean" => match text.trim() {
                    "true" | "1" => Cfg::Bool(true),
                    "false" | "0" => Cfg::Bool(false),
                    other => {
                        self.finding(format!("{context}: non-boolean boolean '{other}'"));
                        Cfg::Bool(false)
                    }
                },
                // The Number interpreter passes the raw (numeric) string through.
                "number" => Cfg::Str(text.clone()),
                "const" => self.eval_const_text(text, context),
                "init_parameter" => Cfg::Map(vec![(
                    CfgKey::s("argument"),
                    self.eval_const_text(text, context),
                )]),
                _ => Cfg::Str(text.clone()),
            },
        }
    }

    fn eval_const_text(&self, text: &str, context: &str) -> Cfg {
        // di.xml constants are written fully qualified; no use map applies.
        let parsed = parse_const_expr(text, "", &[]);
        let lookup = DefsLookup { defs: self.defs };
        match eval(&parsed, &EvalCtx::new(&lookup, None)) {
            Ok(v) => const_to_cfg(&v),
            Err(e) => {
                self.finding(format!("{context}: const '{text}': {}", e.message));
                Cfg::Null
            }
        }
    }

    // ---- ArgumentsResolver ---------------------------------------------------

    /// `getResolvedConstructorArguments` for one instance name whose
    /// constructor is `params` defined by `definer_fqcn`. `None` params (no
    /// constructor) yields `PhpValue::Null`.
    pub fn resolve_instance(
        &self,
        instance_name: &str,
        definer_fqcn: &str,
        definer_uses: &[(String, String)],
        params: &[ParamMeta],
    ) -> PhpValue {
        if params.is_empty() {
            return PhpValue::Null;
        }
        let configured = self.merged_config(instance_name);
        let definer_ns = definer_fqcn
            .rsplit_once('\\')
            .map(|(ns, _)| ns)
            .unwrap_or("");
        let mut out: Vec<(PhpKey, PhpValue)> = Vec::with_capacity(params.len());
        for param in params {
            let required = param.default.is_none() && !param.variadic;
            let class_ty = self.param_class(param, definer_fqcn);
            let mut arg = if !required {
                let default = self.eval_default(param, definer_ns, definer_uses, definer_fqcn);
                self.non_object_argument(&default)
            } else if let Some(class) = &class_ty {
                self.instance_pattern(class)
            } else {
                vn_pattern()
            };
            if let Some(cfg) = map_get(&configured, &param.name) {
                arg = if class_ty.is_some() {
                    self.configured_instance(cfg, instance_name, &param.name)
                } else if let Cfg::Map(entries) = cfg {
                    if let Some(argument) = map_get(entries, "argument") {
                        // ['_a_' => value, '_d_' => default]
                        let default =
                            self.eval_default(param, definer_ns, definer_uses, definer_fqcn);
                        PhpValue::Array(vec![
                            (PhpKey::str("_a_"), cfg_to_php(argument)),
                            (PhpKey::str("_d_"), cfg_to_php(&default)),
                        ])
                    } else {
                        self.non_object_argument(cfg)
                    }
                } else {
                    self.non_object_argument(cfg)
                };
            }
            out.push((PhpKey::str(param.name.clone()), arg));
        }
        PhpValue::Array(out)
    }

    /// The parameter's CLASS type per Magento's GetParameterClassTrait:
    /// a single named non-builtin type (nullable ok); unions, intersections,
    /// and DNF yield None; self/parent/static resolve against the definer.
    fn param_class(&self, param: &ParamMeta, definer: &str) -> Option<String> {
        let ty = param.ty.as_deref()?;
        let ty = ty.strip_prefix('?').unwrap_or(ty);
        if ty.contains('|') || ty.contains('&') || ty.contains('(') {
            return None;
        }
        match ty {
            "array" | "callable" | "bool" | "float" | "int" | "string" | "iterable"
            | "object" | "mixed" | "never" | "void" | "null" | "false" | "true" => None,
            "self" | "static" => Some(definer.to_owned()),
            "parent" => self
                .defs
                .get(definer)
                .and_then(|r| r.meta.extends.first().cloned()),
            // Swoole/OpenSwoole are InterfaceValidator::$optionalPackages.
            _ if ty.starts_with("Swoole\\") || ty.starts_with("OpenSwoole\\") => None,
            other => Some(
                // Reflection reports the DECLARED case, not the use-site
                // spelling (PageBuilder's Gt\Dom vs phpgt's GT\Dom).
                self.defs
                    .canonical_case(other)
                    .map(str::to_owned)
                    .unwrap_or_else(|| other.to_owned()),
            ),
        }
    }

    fn eval_default(
        &self,
        param: &ParamMeta,
        definer_ns: &str,
        definer_uses: &[(String, String)],
        definer_fqcn: &str,
    ) -> Cfg {
        if param.variadic {
            return Cfg::Map(Vec::new()); // ClassReader: variadic default = []
        }
        let Some(default) = &param.default else {
            return Cfg::Null; // getDefaultValue: null when unavailable
        };
        let parsed = parse_const_expr(default, definer_ns, definer_uses);
        let lookup = DefsLookup { defs: self.defs };
        match eval(&parsed, &EvalCtx::new(&lookup, Some(definer_fqcn))) {
            Ok(v) => const_to_cfg(&v),
            Err(e) => {
                self.finding(format!(
                    "{definer_fqcn}::${}: default '{default}': {}",
                    param.name, e.message
                ));
                Cfg::Null
            }
        }
    }

    fn instance_pattern(&self, name: &str) -> PhpValue {
        if self.is_shared_type(name) {
            PhpValue::Array(vec![(PhpKey::str("_i_"), PhpValue::str(name))])
        } else {
            PhpValue::Array(vec![(PhpKey::str("_ins_"), PhpValue::str(name))])
        }
    }

    /// getConfiguredInstanceArgument: type-level sharedness of the configured
    /// instance, overridden by an explicit `shared` on the argument.
    fn configured_instance(&self, cfg: &Cfg, context: &str, arg: &str) -> PhpValue {
        let Cfg::Map(entries) = cfg else {
            self.finding(format!(
                "{context}: argument '{arg}' configured non-object for class-typed param"
            ));
            return vn_pattern();
        };
        let Some(Cfg::Str(instance)) = map_get(entries, "instance") else {
            self.finding(format!("{context}: argument '{arg}' lacks instance"));
            return vn_pattern();
        };
        match map_get(entries, "shared") {
            Some(Cfg::Bool(true)) => {
                PhpValue::Array(vec![(PhpKey::str("_i_"), PhpValue::str(instance.clone()))])
            }
            Some(Cfg::Bool(false)) => {
                PhpValue::Array(vec![(PhpKey::str("_ins_"), PhpValue::str(instance.clone()))])
            }
            _ => self.instance_pattern(instance),
        }
    }

    /// getNonObjectArgument: null → `_vn_`; arrays containing configured
    /// entries → `_vac_` (transformed); else `_v_` verbatim.
    fn non_object_argument(&self, value: &Cfg) -> PhpValue {
        match value {
            Cfg::Null => vn_pattern(),
            Cfg::Map(entries) if is_configured_array(entries) => {
                let transformed = self.configured_array(entries);
                PhpValue::Array(vec![(PhpKey::str("_vac_"), transformed)])
            }
            other => PhpValue::Array(vec![(PhpKey::str("_v_"), cfg_to_php(other))]),
        }
    }

    fn configured_array(&self, entries: &[(CfgKey, Cfg)]) -> PhpValue {
        PhpValue::Array(
            entries
                .iter()
                .map(|(k, v)| {
                    let key = cfg_key_to_php(k);
                    let value = match v {
                        Cfg::Map(inner) => {
                            if let Some(Cfg::Str(instance)) = map_get(inner, "instance") {
                                match map_get(inner, "shared") {
                                    Some(Cfg::Bool(true)) => PhpValue::Array(vec![(
                                        PhpKey::str("_i_"),
                                        PhpValue::str(instance.clone()),
                                    )]),
                                    Some(Cfg::Bool(false)) => PhpValue::Array(vec![(
                                        PhpKey::str("_ins_"),
                                        PhpValue::str(instance.clone()),
                                    )]),
                                    _ => self.instance_pattern(instance),
                                }
                            } else if let Some(argument) = map_get(inner, "argument") {
                                PhpValue::Array(vec![
                                    (PhpKey::str("_a_"), cfg_to_php(argument)),
                                    (PhpKey::str("_d_"), PhpValue::Null),
                                ])
                            } else {
                                self.configured_array(inner)
                            }
                        }
                        other => cfg_to_php(other),
                    };
                    (key, value)
                })
                .collect(),
        )
    }
}

fn vn_pattern() -> PhpValue {
    PhpValue::Array(vec![(PhpKey::str("_vn_"), PhpValue::Bool(true))])
}

/// Any nested map carrying an `instance` or `argument` key (recursive) —
/// ArgumentsResolver::isConfiguredArray.
fn is_configured_array(entries: &[(CfgKey, Cfg)]) -> bool {
    entries.iter().any(|(_, v)| match v {
        Cfg::Map(inner) => {
            map_get(inner, "instance").is_some()
                || map_get(inner, "argument").is_some()
                || is_configured_array(inner)
        }
        _ => false,
    })
}

fn cfg_to_php(value: &Cfg) -> PhpValue {
    match value {
        Cfg::Str(s) => PhpValue::Str(s.clone()),
        Cfg::Int(i) => PhpValue::Int(*i),
        Cfg::Float(f) => PhpValue::Float(*f),
        Cfg::Bool(b) => PhpValue::Bool(*b),
        Cfg::Null => PhpValue::Null,
        Cfg::Map(entries) => PhpValue::Array(
            entries
                .iter()
                .map(|(k, v)| (cfg_key_to_php(k), cfg_to_php(v)))
                .collect(),
        ),
    }
}

fn cfg_key_to_php(key: &CfgKey) -> PhpKey {
    match key {
        CfgKey::Int(i) => PhpKey::Int(*i),
        CfgKey::Str(s) => PhpKey::Str(s.clone()),
    }
}

fn const_to_cfg(value: &ConstValue) -> Cfg {
    match value {
        ConstValue::Null => Cfg::Null,
        ConstValue::Bool(b) => Cfg::Bool(*b),
        ConstValue::Int(i) => Cfg::Int(*i),
        ConstValue::Float(f) => Cfg::Float(*f),
        ConstValue::Str(s) => Cfg::Str(s.clone()),
        ConstValue::Array(items) => Cfg::Map(
            items
                .iter()
                .map(|(k, v)| {
                    let key = match k {
                        magecommand_php::constexpr::ArrayKey::Int(i) => CfgKey::Int(*i),
                        magecommand_php::constexpr::ArrayKey::Str(s) => CfgKey::Str(s.clone()),
                        _ => CfgKey::Str(String::new()),
                    };
                    (key, const_to_cfg(v))
                })
                .collect(),
        ),
        _ => Cfg::Null,
    }
}

// ---- array_replace / array_replace_recursive / SortItems --------------------

fn array_replace(base: &mut Vec<(CfgKey, Cfg)>, over: Vec<(CfgKey, Cfg)>) {
    for (key, value) in over {
        match base.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => base.push((key, value)),
        }
    }
}

fn array_replace_recursive(base: &mut Vec<(CfgKey, Cfg)>, over: Vec<(CfgKey, Cfg)>) {
    for (key, value) in over {
        match base.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => match (&mut slot.1, value) {
                (Cfg::Map(base_inner), Cfg::Map(over_inner)) => {
                    array_replace_recursive(base_inner, over_inner);
                }
                (slot_value, value) => *slot_value = value,
            },
            None => base.push((key, value)),
        }
    }
}

/// ObjectManager\Helper\SortItems: when any argument's ITEMS carry a
/// `sortOrder` key, each argument's items are stably re-ordered by it.
/// (The single-level mode reorders the argument MAP itself, which never
/// reaches the compiled output — lookups are by name.)
fn sort_items(arguments: &mut [(CfgKey, Cfg)]) {
    let multi = arguments.iter().any(|(_, v)| match v {
        Cfg::Map(items) => items
            .iter()
            .any(|(_, item)| matches!(item, Cfg::Map(m) if map_get(m, "sortOrder").is_some())),
        _ => false,
    });
    if !multi {
        return;
    }
    for (_, value) in arguments.iter_mut() {
        if let Cfg::Map(items) = value {
            items.sort_by_key(|(_, item)| match item {
                Cfg::Map(m) => match map_get(m, "sortOrder") {
                    Some(Cfg::Int(i)) => *i,
                    Some(Cfg::Str(s)) => s.trim().parse().unwrap_or(0),
                    _ => 0,
                },
                _ => 0,
            });
        }
    }
}

// ---- const lookup over the scanned corpus ------------------------------------

pub struct DefsLookup<'a> {
    pub defs: &'a Definitions,
}

impl ConstLookup for DefsLookup<'_> {
    fn class_const(&self, class: &str, name: &str) -> Option<ParsedExpr> {
        let record = self.defs.get(class)?;
        let ns = record.meta.fqcn.rsplit_once('\\').map(|(n, _)| n).unwrap_or("");
        if let Some(c) = record.meta.constants.iter().find(|c| c.name == name) {
            return Some(parse_const_expr(&c.value, ns, &record.meta.uses));
        }
        // Interface constants are reachable through the implementing class.
        for iface in self.defs.all_interfaces(class) {
            if let Some(r) = self.defs.get(&iface) {
                if let Some(c) = r.meta.constants.iter().find(|c| c.name == name) {
                    let ins = r.meta.fqcn.rsplit_once('\\').map(|(n, _)| n).unwrap_or("");
                    return Some(parse_const_expr(&c.value, ins, &r.meta.uses));
                }
            }
        }
        None
    }

    fn parent_of(&self, class: &str) -> Option<String> {
        let record = self.defs.get(class)?;
        if record.meta.kind == ClassKind::Interface {
            return None;
        }
        record.meta.extends.first().cloned()
    }
}

/// Build the whole pre-chain `arguments` map for one area: every scanned
/// Magento-concrete class, plus vtypes over their base's constructor, plus
/// NULL rows for concrete preference keys outside the scan set.
pub(crate) fn build_arguments(ctx: &ArgsCtx, magento: &Magento) -> BTreeMap<String, PhpValue> {
    let _ = magento;
    let mut out: BTreeMap<String, PhpValue> = BTreeMap::new();
    for name in ctx.scanned {
        if !ctx.defs.is_concrete(name) {
            continue;
        }
        let value = match ctx.defs.constructor_of(name) {
            Ok(Some(ctor)) => {
                ctx.resolve_instance(name, ctor.definer_fqcn, ctor.definer_uses, ctor.params)
            }
            Ok(None) => PhpValue::Null,
            Err(missing) => {
                ctx.finding(format!("{name}: constructor chain leaves known set at {missing}"));
                PhpValue::Null
            }
        };
        out.insert(name.clone(), value);
    }
    // fillThirdPartyInterfaces: preference keys enter the collection with an
    // empty constructor — concrete ones become NULL rows.
    for key in &ctx.pref_keys {
        if !out.contains_key(key) && !ctx.scanned.contains(key) && ctx.defs.is_concrete(key) {
            out.insert(key.clone(), PhpValue::Null);
        }
    }
    // Virtual types: the base type's constructor, resolved under the vtype's
    // own (inherited + overlaid) configuration.
    for (vtype, base) in &ctx.vtypes {
        let base_real = {
            // chase vtype-on-vtype to the real class
            let mut current = base.as_str();
            let mut seen = HashSet::new();
            while let Some(next) = ctx.vtypes.get(current) {
                if !seen.insert(next.as_str()) {
                    break;
                }
                current = next;
            }
            current.to_owned()
        };
        // Reader: an in-collection base is used as-is (even abstract);
        // the isConcrete guard applies only to the reflect-fallback path.
        let in_collection = ctx.scanned.contains(&base_real);
        let value = if in_collection || ctx.defs.contains(&base_real) {
            if !in_collection && !ctx.defs.is_concrete(&base_real) {
                continue;
            }
            match ctx.defs.constructor_of(&base_real) {
                Ok(Some(ctor)) => {
                    ctx.resolve_instance(vtype, ctor.definer_fqcn, ctor.definer_uses, ctor.params)
                }
                Ok(None) => PhpValue::Null,
                Err(missing) => {
                    ctx.finding(format!(
                        "{vtype}: base {base_real} constructor chain leaves known set at {missing}"
                    ));
                    PhpValue::Null
                }
            }
        } else {
            ctx.finding(format!("{vtype}: base {base_real} not found anywhere"));
            continue;
        };
        out.insert(vtype.clone(), value);
    }
    out
}


// ---- DiCompileCommand::configureObjectManager overrides ---------------------

/// Reproduce the runtime DI reconfiguration the compile command applies to
/// its own object manager before running the operations. The compiled area
/// files bake these — including exclude regexes with absolute paths and the
/// module list in ComponentRegistrar (autoload_files.php) order.
pub(crate) fn setup_overrides(
    magento: &Magento,
    root: &std::path::Path,
) -> Vec<(String, String, Cfg)> {
    let chain = "Magento\\Setup\\Module\\Di\\Compiler\\Config\\Chain";
    let mods = [
        ("BackslashTrim", format!("{chain}\\BackslashTrim")),
        ("PreferencesResolving", format!("{chain}\\PreferencesResolving")),
        ("InterceptorSubstitution", format!("{chain}\\InterceptorSubstitution")),
        ("InterceptionPreferencesResolving", format!("{chain}\\PreferencesResolving")),
        ("NonLazyTypes", format!("{chain}\\NonLazyTypes")),
    ];
    let mut overrides: Vec<(String, String, Cfg)> = vec![
        (
            "Magento\\Setup\\Module\\Di\\Compiler\\Config\\ModificationChain".to_owned(),
            "modificationsList".to_owned(),
            Cfg::Map(
                mods.into_iter()
                    .map(|(k, cls)| {
                        (CfgKey::s(k), Cfg::Map(vec![(CfgKey::s("instance"), Cfg::Str(cls))]))
                    })
                    .collect(),
            ),
        ),
        (
            "Magento\\Setup\\Module\\Di\\Code\\Generator\\PluginList".to_owned(),
            "cache".to_owned(),
            Cfg::Map(vec![(
                CfgKey::s("instance"),
                Cfg::Str("Magento\\Framework\\App\\Interception\\Cache\\CompiledConfig".to_owned()),
            )]),
        ),
    ];

    // Module/library paths in ComponentRegistrar order = the order their
    // registration.php files run = composer's autoload_files.php order.
    let autoload_files = root.join("vendor/composer/autoload_files.php");
    let text = std::fs::read_to_string(&autoload_files).unwrap_or_default();
    let vendor = root.join("vendor");
    let mut registration_dirs: Vec<std::path::PathBuf> = Vec::new();
    for line in text.lines() {
        let Some(idx) = line.find("$vendorDir . '") else { continue };
        let rest = &line[idx + 14..];
        let Some(end) = rest.find('\'') else { continue };
        let rel = rest[..end].trim_start_matches('/');
        if let Some(dir) = std::path::Path::new(rel).parent() {
            registration_dirs.push(vendor.join(dir));
        }
    }

    let module_by_path: HashMap<&std::path::Path, bool> = magento
        .modules()
        .iter()
        .map(|m| (m.path.as_path(), m.enabled))
        .collect();
    let mut seen_modules: HashSet<&std::path::Path> = HashSet::new();
    let module_paths: Vec<&std::path::Path> = registration_dirs
        .iter()
        .map(|d| d.as_path())
        .filter(|d| module_by_path.get(d).copied().unwrap_or(false))
        .filter(|d| seen_modules.insert(*d))
        .collect();
    let library_set: HashSet<&std::path::Path> =
        magento.library_paths().iter().map(|p| p.as_path()).collect();
    let mut seen_libs: HashSet<&std::path::Path> = HashSet::new();
    let library_paths: Vec<&std::path::Path> = registration_dirs
        .iter()
        .map(|d| d.as_path())
        .filter(|d| library_set.contains(*d))
        .filter(|d| seen_libs.insert(*d))
        .collect();

    // getExcludedModulePaths: group by base path, then vendor dir, keeping
    // encounter order; only the BASE PATH is preg_quoted.
    let mut base_order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, (Vec<String>, HashMap<String, Vec<String>>)> = HashMap::new();
    for path in &module_paths {
        let module_dir = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let vendor_path = path.parent().unwrap_or(std::path::Path::new(""));
        let vendor_dir = vendor_path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let base_path = vendor_path.parent().unwrap_or(std::path::Path::new("")).to_string_lossy().into_owned();
        let entry = groups.entry(base_path.clone()).or_insert_with(|| {
            base_order.push(base_path.clone());
            (Vec::new(), HashMap::new())
        });
        let vendors = &mut entry.0;
        let by_vendor = &mut entry.1;
        if !by_vendor.contains_key(&vendor_dir) {
            vendors.push(vendor_dir.clone());
        }
        by_vendor.entry(vendor_dir).or_default().push(module_dir);
    }
    let mut base_regexps: Vec<String> = Vec::new();
    for base in &base_order {
        let (vendors, by_vendor) = &groups[base];
        let vendor_parts: Vec<String> = vendors
            .iter()
            .map(|v| format!("{}/(?:{})", v, by_vendor[v].join("|")))
            .collect();
        base_regexps.push(format!("{}/(?:{})", preg_quote(base), vendor_parts.join("|")));
    }
    let application = vec![
        format!("#^(?:{})/Test#", base_regexps.join("|")),
        format!("#^(?:{})/tests#", base_regexps.join("|")),
    ];
    let libs_quoted: Vec<String> = library_paths.iter().map(|p| preg_quote(&p.to_string_lossy())).collect();
    let framework = vec![
        format!("#^(?:{})/([\\w]+/)?Test#", libs_quoted.join("|")),
        format!("#^(?:{})/([\\w]+/)?tests#", libs_quoted.join("|")),
    ];
    let setup_path = root.join("setup/src");
    let setup = vec![format!(
        "#^(?:{})(/[\\w]+)*/Test#",
        preg_quote(&setup_path.to_string_lossy())
    )];

    let to_list = |items: Vec<String>| {
        Cfg::Map(
            items
                .into_iter()
                .enumerate()
                .map(|(i, s)| (CfgKey::Int(i as i64), Cfg::Str(s)))
                .collect(),
        )
    };
    overrides.push((
        "Magento\\Setup\\Module\\Di\\Code\\Reader\\ClassesScanner".to_owned(),
        "excludePatterns".to_owned(),
        Cfg::Map(vec![
            (CfgKey::s("application"), to_list(application)),
            (CfgKey::s("framework"), to_list(framework)),
            (CfgKey::s("setup"), to_list(setup)),
        ]),
    ));
    overrides
}

/// PHP's preg_quote with '#' delimiter.
fn preg_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(
            ch,
            '.' | '\\' | '+' | '*' | '?' | '[' | '^' | ']' | '$' | '(' | ')' | '{' | '}'
                | '=' | '!' | '<' | '>' | '|' | ':' | '-' | '#' | '/'
        ) && ch != '/'
        {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}
