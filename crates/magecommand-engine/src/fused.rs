//! Fused interceptors — creatuity/magento2-interceptors' technique
//! (github.com/creatuity/magento2-interceptors), reimplemented clean-room.
//!
//! Instead of Magento's runtime `pluginList->getNext` + `___callPlugins`
//! dispatch, the plugin chain is resolved at compile time and unrolled into the
//! method body: plugins come lazily from `$this->____om()->get()` accessors (the OM itself from the `ObjectManager::getInstance()` singleton), and
//! the before/around/after listeners are inlined. This module renders the
//! GLOBAL-ONLY case (a class with no per-area plugin differences → a flat body,
//! no `switch (getCurrentScope())`). The per-area switch is a later increment.
//!
//! Credit: the fused format is creatuity's prior art. Their #28 (compiled
//! interceptors skipping global plugins in the `default` branch) is the reason
//! the switch's default must carry the global chain — reproduced here by making
//! the global chain the whole body for global-only classes.

use std::collections::HashSet;

use magequery_core::Magento;

use crate::definitions::Definitions;
use crate::laminas::render_type;
use crate::pluginlist::{GlobalChains, ScopeChains};
use crate::reflect::{self, RMethod, RParam};

/// di.xml plugin name → the property/accessor suffix: `[^A-Za-z0-9_] → _`.
fn clean(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect()
}

fn ucfirst(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// One resolved listener layer for a method (a `{type}_{method}_{prev}` node).
#[derive(Clone, PartialEq)]
struct Node {
    before: Vec<PluginRef>,
    around: Option<PluginRef>,
    after: Vec<PluginRef>,
    /// The layer inside `around`'s `$proceed` closure. `None` == "call parent".
    child: Option<Box<Node>>,
}

#[derive(Clone, PartialEq)]
struct PluginRef {
    /// The property/accessor suffix (cleaned di.xml name).
    clean: String,
    /// Plugin class FQCN (no leading backslash).
    instance: String,
}

/// Build the listener-chain tree for `(type, method)` starting at `prev`,
/// resolving each plugin name to its instance class. An `around` whose next
/// node is absent yields a synthetic parent-calling leaf.
fn build(chains: &GlobalChains, ty: &str, method: &str, prev: &str, insts: &[(String, String)]) -> Option<Node> {
    let key = format!("{ty}_{method}_{prev}");
    let (before, around, after) = chains.nodes.get(&key)?;
    let resolve = |name: &str| PluginRef {
        clean: clean(name),
        instance: insts.iter().find(|(n, _)| n == name).map(|(_, i)| i.clone()).unwrap_or_default(),
    };
    let child = around.as_ref().map(|a| {
        Box::new(build(chains, ty, method, a, insts).unwrap_or(Node {
            before: Vec::new(),
            around: None,
            after: Vec::new(),
            child: None,
        }))
    });
    Some(Node {
        before: before.iter().map(|n| resolve(n)).collect(),
        around: around.as_ref().map(|a| resolve(a)),
        after: after.iter().map(|n| resolve(n)).collect(),
        child,
    })
}

/// Emit the unrolled body of one layer at `tabs` nesting (0 = the method body,
/// +1 per enclosing `around` closure). Lines carry only the tab indent; the
/// 8-space method indent is applied by the caller.
fn emit(node: &Node, method: &str, tabs: usize, out: &mut Vec<String>, void: bool, top_level: bool) {
    let ti = "\t".repeat(tabs);
    let m = ucfirst(method);
    // A `: void` intercepted method must not `return` a value (PHP fatal). Drop
    // the `return` only at the method's OWN top level, though: the `$proceed`
    // closures are untyped `function(...$arguments)`, so their
    // `return parent::…()` — the void call evaluates to null — stays valid and
    // is how the inner layer hands control back to `around`.
    let ret_kw = if void && top_level { "" } else { "return " };
    out.push(format!("{ti}$arguments = \\func_get_args();"));
    for p in &node.before {
        out.push(format!(
            "{ti}$beforeResult = $this->____plugin_{}()->before{m}($this, ...\\array_values($arguments));",
            p.clean
        ));
        out.push(format!("{ti}if ($beforeResult !== null) $arguments = (array)$beforeResult;"));
    }
    let has_after = !node.after.is_empty();
    if !node.before.is_empty() {
        out.push(ti.clone());
    }
    let assign = if has_after { "$result = " } else { ret_kw };
    if let Some(a) = &node.around {
        out.push(format!(
            "{ti}{assign}$this->____plugin_{}()->around{m}($this, function(...$arguments){{",
            a.clean
        ));
        if let Some(child) = &node.child {
            emit(child, method, tabs + 1, out, void, false);
        }
        out.push(format!("{ti}}}, ...\\array_values($arguments));"));
    } else {
        out.push(format!("{ti}{assign}parent::{method}(...\\array_values($arguments));"));
    }
    if has_after {
        // Each after is its own result-chain piece; creatuity separates pieces
        // with a blank line (getResultChainLines), so emit one before each.
        let n = node.after.len();
        for (i, p) in node.after.iter().enumerate() {
            out.push(ti.clone());
            let kw = if i == n - 1 { ret_kw } else { "$result = " };
            out.push(format!(
                "{ti}{kw}$this->____plugin_{}()->after{m}($this, $result, ...\\array_values($arguments));",
                p.clean
            ));
        }
    }
}

/// 8-space indent every non-empty body line (empty lines stay empty), join.
fn indent8(lines: &[String]) -> String {
    lines
        .iter()
        .map(|l| if l.is_empty() { String::new() } else { format!("        {l}") })
        .collect::<Vec<_>>()
        .join("\n")
}

fn docblock(lines: &[String]) -> String {
    let mut s = String::from("    /**\n");
    for l in lines {
        if l.is_empty() {
            s.push_str("     *\n");
        } else {
            s.push_str("     * ");
            s.push_str(l);
            s.push('\n');
        }
    }
    s.push_str("     */");
    s
}

fn render_param(p: &RParam) -> String {
    let mut s = String::new();
    if let Some(t) = &p.type_str {
        s.push_str(&render_type(t));
        s.push(' ');
    }
    if p.by_ref {
        s.push('&');
    }
    if p.variadic {
        s.push_str("... ");
    }
    s.push('$');
    s.push_str(&p.name);
    if let Some(d) = &p.default {
        s.push_str(" = ");
        s.push_str(&d.render());
    }
    s
}

/// Accumulate a method's plugins in first-appearance order (before → around →
/// nested child → after), deduped by cleaned name — the property/accessor order.
fn collect(node: &Node, plugins: &mut Vec<PluginRef>, seen: &mut HashSet<String>) {
    for p in &node.before {
        if seen.insert(p.clean.clone()) {
            plugins.push(p.clone());
        }
    }
    if let Some(a) = &node.around {
        if seen.insert(a.clean.clone()) {
            plugins.push(a.clone());
        }
    }
    if let Some(c) = &node.child {
        collect(c, plugins, seen);
    }
    for p in &node.after {
        if seen.insert(p.clean.clone()) {
            plugins.push(p.clone());
        }
    }
}

/// Render one method's body: a flat unrolled chain when every scope resolves to
/// the same chain as `global`, else a `switch (getCurrentScope())` whose
/// `default` carries the global chain (the #28 guard) and which emits a `case`
/// only for scopes whose chain differs (equal scopes stack their labels, scopes
/// equal to global collapse into `default`). Matches creatuity's
/// `getScopeCasesFromConfig`. A scope with no chain renders as a parent-direct
/// body.
fn render_method_body(
    scope_names: &[&str],
    per_scope: &[Option<Node>],
    default_idx: usize,
    method: &str,
    void: bool,
) -> Vec<String> {
    let parent = Node { before: Vec::new(), around: None, after: Vec::new(), child: None };
    let node_of = |o: &Option<Node>| o.clone().unwrap_or_else(|| parent.clone());
    let default = &per_scope[default_idx];

    // Non-default scopes grouped by chain, in enumeration order.
    let mut groups: Vec<(Vec<&str>, &Option<Node>)> = Vec::new();
    for (i, name) in scope_names.iter().enumerate() {
        let conf = &per_scope[i];
        if conf == default {
            continue;
        }
        if let Some(g) = groups.iter_mut().find(|(_, c)| *c == conf) {
            g.0.push(name);
        } else {
            groups.push((vec![name], conf));
        }
    }

    let mut lines = Vec::new();
    // The switch cases/default ARE the method's top level (not inside a closure),
    // so every method-body emit passes `top_level = true`; only `emit`'s own
    // recursion into an `around` closure flips it off.
    if groups.is_empty() {
        emit(&node_of(default), method, 0, &mut lines, void, true);
    } else {
        lines.push("switch ($this->____scope()->getCurrentScope()) {".to_owned());
        for (names, conf) in &groups {
            for n in names {
                lines.push(format!("\tcase '{n}':"));
            }
            emit(&node_of(conf), method, 2, &mut lines, void, true);
        }
        lines.push("\tdefault:".to_owned());
        emit(&node_of(default), method, 2, &mut lines, void, true);
        lines.push("}".to_owned());
    }
    lines
}

/// Render the fused interceptor for `source` (global-only → flat bodies,
/// multi-area → per-method `switch`). `None` only when the subject class is
/// unresolvable, in which case the caller falls back to the stock interceptor.
pub fn fused_interceptor_bytes(
    magento: &Magento,
    defs: &Definitions,
    scopes: &ScopeChains,
    source: &str,
    intercepted: &HashSet<String>,
) -> Option<String> {
    let _ = magento;
    let record = defs.get(source)?;
    let source_fqcn = record.meta.fqcn.clone();

    // Ordered scopes, matching creatuity's `['primary','global'] + getAllScopes()`:
    // `primary` and `global` both resolve to the global (base) chains; each real
    // area is global overlaid by that area. The `switch` default is 'global'.
    let mut scope_list: Vec<(&str, &GlobalChains)> =
        vec![("primary", &scopes.global), ("global", &scopes.global)];
    for (name, ch) in &scopes.areas {
        scope_list.push((name, ch));
    }
    let scope_names: Vec<&str> = scope_list.iter().map(|(n, _)| *n).collect();
    let default_idx = 1;

    // name -> instance, unioned across scopes (a plugin class is scope-independent).
    let mut insts: Vec<(String, String)> = Vec::new();
    let mut inst_seen: HashSet<String> = HashSet::new();
    for (_, ch) in &scope_list {
        if let Some(list) = ch.instances.get(&source_fqcn) {
            for (n, i) in list {
                if inst_seen.insert(n.clone()) {
                    insts.push((n.clone(), i.clone()));
                }
            }
        }
    }

    // Intercepted methods, reflection order: per-scope chains → a flat or switched
    // body, plus the plugins each uses (first-appearance order across scopes).
    let mut method_bodies: Vec<(RMethod, Vec<String>)> = Vec::new();
    let mut plugins: Vec<PluginRef> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for rm in reflect::public_methods(defs, &source_fqcn) {
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
        let per_scope: Vec<Option<Node>> = scope_list
            .iter()
            .map(|(_, ch)| build(ch, &source_fqcn, &rm.name, "__self", &insts))
            .collect();
        if per_scope.iter().all(Option::is_none) {
            continue;
        }
        for node in per_scope.iter().flatten() {
            collect(node, &mut plugins, &mut seen);
        }
        let void = rm.return_type.as_deref() == Some("void");
        let body = render_method_body(&scope_names, &per_scope, default_idx, &rm.name, void);
        method_bodies.push((rm, body));
    }

    // ---- assemble ----
    let mut members: Vec<String> = Vec::new();

    // properties: scope, om, then plugins
    members.push(format!("{}\n    private $____scope = null;", docblock(&["@var ScopeInterface".into()])));
    members.push(format!("{}\n    private $____om = null;", docblock(&["@var ObjectManagerInterface".into()])));
    for p in &plugins {
        members.push(format!(
            "{}\n    private $____plugin_{} = null;",
            docblock(&[format!("@var \\{}", p.instance)]),
            p.clean
        ));
    }

    // The ObjectManager and scope are NOT constructor-injected. A subject with
    // no compiled `arguments` entry (e.g. CsrfValidator — no di.xml config) gives
    // Magento's Compiled factory nothing to key the interceptor's ctor args off,
    // so extra ctor deps fatal at runtime ("Argument #1 ($____om) not passed").
    // Instead the interceptor keeps the subject's OWN constructor (a plain
    // forward to parent, or inherited when the subject has none), so the factory
    // builds it exactly like the subject; the OM is pulled from the global
    // singleton lazily (____om()/____scope() below) — creatuity's __wakeup
    // fallback, promoted to the only path.
    if let Some(info) = defs.constructor_of(source).ok().flatten() {
        let subject_params = reflect::resolve_params(defs, info.definer_fqcn, info.params);
        let params: Vec<String> = subject_params.iter().map(render_param).collect();
        let forwarded: Vec<String> = subject_params
            .iter()
            .map(|p| if p.variadic { format!("... ${}", p.name) } else { format!("${}", p.name) })
            .collect();
        members.push(format!(
            "{}\n    public function __construct({})\n    {{\n        parent::__construct({});\n    }}",
            docblock(&["{@inheritdoc}".into()]),
            params.join(", "),
            forwarded.join(", "),
        ));
    }
    members.push(
        "    private function ____om()\n    {\n        if ($this->____om === null) {\n        \t$this->____om = \\Magento\\Framework\\App\\ObjectManager::getInstance();\n        }\n        return $this->____om;\n    }".to_owned(),
    );
    members.push(
        "    private function ____scope()\n    {\n        if ($this->____scope === null) {\n        \t$this->____scope = $this->____om()->get(\\Magento\\Framework\\Config\\ScopeInterface::class);\n        }\n        return $this->____scope;\n    }".to_owned(),
    );

    // intercepted methods
    for (rm, body_lines) in &method_bodies {
        members.push(format!(
            "{}\n    public function {}\n    {{\n{}\n    }}",
            docblock(&["{@inheritdoc}".into()]),
            method_signature(rm),
            indent8(body_lines),
        ));
    }

    // plugin accessors
    for p in &plugins {
        // di.xml name for the docblock `plugin "<name>"`: recover from insts.
        let name = insts.iter().find(|(n, _)| clean(n) == p.clean).map(|(n, _)| n.clone()).unwrap_or_else(|| p.clean.clone());
        let body = format!(
            "        if ($this->____plugin_{c} === null) {{\n        \t$this->____plugin_{c} = $this->____om()->get(\\{i}::class);\n        }}\n        return $this->____plugin_{c};",
            c = p.clean,
            i = p.instance,
        );
        members.push(format!(
            "{}\n    private function ____plugin_{}()\n    {{\n{}\n    }}",
            docblock(&[format!("plugin \"{name}\""), format!("@return \\{}", p.instance)]),
            p.clean,
            body,
        ));
    }

    let header = format!(
        "<?php\nnamespace {ns};\n\nuse Magento\\Framework\\Config\\ScopeInterface;\nuse Magento\\Framework\\ObjectManagerInterface;\n\n/**\n * Interceptor class for @see \\{ns}\n */\nclass Interceptor extends \\{ns}\n{{\n",
        ns = source_fqcn,
    );
    Some(format!("{header}{}\n}}\n", members.join("\n\n")))
}

fn method_signature(rm: &RMethod) -> String {
    let params: Vec<String> = rm.params.iter().map(render_param).collect();
    let mut s = format!("{}({})", rm.name, params.join(", "));
    if let Some(rt) = &rm.return_type {
        s.push_str(": ");
        s.push_str(&render_type(rt));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pref(clean: &str) -> PluginRef {
        PluginRef { clean: clean.into(), instance: "X".into() }
    }

    /// The nested around case (low so=10 outer, high so=20 inner, both
    /// before/around/after) unrolls to the exact creatuity body: before
    /// ascending, arounds nested (parent innermost), after descending, with
    /// `\t` per closure and empty separators between phases.
    #[test]
    fn emit_unrolls_nested_around_chain() {
        let leaf = Node { before: vec![], around: None, after: vec![], child: None };
        let inner = Node {
            before: vec![pref("high")],
            around: Some(pref("high")),
            after: vec![pref("high")],
            child: Some(Box::new(leaf)),
        };
        let root = Node {
            before: vec![pref("low")],
            around: Some(pref("low")),
            after: vec![pref("low")],
            child: Some(Box::new(inner)),
        };
        let mut out = Vec::new();
        emit(&root, "process", 0, &mut out, false, true);
        let expected = "\
$arguments = \\func_get_args();
$beforeResult = $this->____plugin_low()->beforeProcess($this, ...\\array_values($arguments));
if ($beforeResult !== null) $arguments = (array)$beforeResult;

$result = $this->____plugin_low()->aroundProcess($this, function(...$arguments){
\t$arguments = \\func_get_args();
\t$beforeResult = $this->____plugin_high()->beforeProcess($this, ...\\array_values($arguments));
\tif ($beforeResult !== null) $arguments = (array)$beforeResult;
\t
\t$result = $this->____plugin_high()->aroundProcess($this, function(...$arguments){
\t\t$arguments = \\func_get_args();
\t\treturn parent::process(...\\array_values($arguments));
\t}, ...\\array_values($arguments));
\t
\treturn $this->____plugin_high()->afterProcess($this, $result, ...\\array_values($arguments));
}, ...\\array_values($arguments));

return $this->____plugin_low()->afterProcess($this, $result, ...\\array_values($arguments));";
        assert_eq!(out.join("\n"), expected);
    }

    /// A `: void` intercepted method (e.g. CsrfValidator::validate with an
    /// around plugin) must NOT `return` at its own top level — PHP fatals with
    /// "A void method must not return a value" — but the untyped `$proceed`
    /// closure KEEPS `return parent::…()` (the void call evaluates to null).
    #[test]
    fn emit_void_drops_top_level_return_keeps_closure_return() {
        let leaf = Node { before: vec![], around: None, after: vec![], child: None };
        let root = Node {
            before: vec![],
            around: Some(pref("skip")),
            after: vec![],
            child: Some(Box::new(leaf)),
        };
        let mut out = Vec::new();
        emit(&root, "validate", 0, &mut out, true, true);
        let body = out.join("\n");
        // No line at the method's top level starts with `return `.
        assert!(
            !body.lines().any(|l| l.starts_with("return ")),
            "void top level must not return, got:\n{body}"
        );
        // The around dispatch is a bare statement, no `return `/`$result = `.
        assert!(
            body.contains("\n$this->____plugin_skip()->aroundValidate($this, function"),
            "around dispatch not bare, got:\n{body}"
        );
        // The inner closure still hands control back with a `return`.
        assert!(
            body.contains("\treturn parent::validate(...\\array_values($arguments));"),
            "closure must keep its return, got:\n{body}"
        );
    }

    /// The after-only global case (ProbeGlobalOnly): parent call, then a single
    /// `return`-ed after.
    #[test]
    fn emit_after_only_calls_parent_then_after() {
        let node = Node { before: vec![], around: None, after: vec![pref("g")], child: None };
        let mut out = Vec::new();
        emit(&node, "greet", 0, &mut out, false, true);
        assert_eq!(
            out.join("\n"),
            "$arguments = \\func_get_args();\n\
             $result = parent::greet(...\\array_values($arguments));\n\
             \n\
             return $this->____plugin_g()->afterGreet($this, $result, ...\\array_values($arguments));"
        );
    }

    /// Two afters at one layer (e.g. an adminhtml case with global + area after
    /// plugins) get a blank separator before EACH — creatuity's result-chain
    /// pieces are blank-separated.
    #[test]
    fn emit_separates_multiple_afters() {
        let node =
            Node { before: vec![], around: None, after: vec![pref("g"), pref("a")], child: None };
        let mut out = Vec::new();
        emit(&node, "greet", 0, &mut out, false, true);
        assert_eq!(
            out.join("\n"),
            "$arguments = \\func_get_args();\n\
             $result = parent::greet(...\\array_values($arguments));\n\
             \n\
             $result = $this->____plugin_g()->afterGreet($this, $result, ...\\array_values($arguments));\n\
             \n\
             return $this->____plugin_a()->afterGreet($this, $result, ...\\array_values($arguments));"
        );
    }

    /// The multi-area switch: `default` carries the GLOBAL chain (the #28 guard),
    /// a `case` is emitted only for a scope whose chain differs, and a scope equal
    /// to global (here `primary`) collapses into `default` — no `case 'primary'`.
    #[test]
    fn switch_default_is_global_with_case_per_differing_scope() {
        let g = Node { before: vec![], around: None, after: vec![pref("g")], child: None };
        let ga =
            Node { before: vec![], around: None, after: vec![pref("g"), pref("a")], child: None };
        let names = ["primary", "global", "adminhtml"];
        let per_scope = vec![Some(g.clone()), Some(g), Some(ga)];
        let lines = render_method_body(&names, &per_scope, 1, "greet", false);

        assert_eq!(lines[0], "switch ($this->____scope()->getCurrentScope()) {");
        assert_eq!(lines[1], "\tcase 'adminhtml':");
        assert!(lines.iter().any(|l| l == "\tdefault:"), "default branch present");
        assert!(!lines.iter().any(|l| l.contains("'primary'")), "primary collapses into default");
        assert_eq!(lines.last().unwrap(), "}");
        // default carries the global (single-after) chain
        let default_pos = lines.iter().position(|l| l == "\tdefault:").unwrap();
        assert!(lines[default_pos..].iter().any(|l| l.contains("____plugin_g()->afterGreet")));
    }

    /// All scopes equal to global ⇒ a flat, switch-free body.
    #[test]
    fn global_only_renders_flat_no_switch() {
        let g = Node { before: vec![], around: None, after: vec![pref("g")], child: None };
        let names = ["primary", "global", "adminhtml"];
        let per_scope = vec![Some(g.clone()), Some(g.clone()), Some(g)];
        let lines = render_method_body(&names, &per_scope, 1, "greet", false);
        assert!(!lines.iter().any(|l| l.contains("switch (")), "no switch when all == global");
        assert_eq!(lines[0], "$arguments = \\func_get_args();");
    }

    #[test]
    fn clean_and_ucfirst() {
        assert_eq!(clean("magequery_rich-low.x"), "magequery_rich_low_x");
        assert_eq!(ucfirst("getList"), "GetList");
    }
}
