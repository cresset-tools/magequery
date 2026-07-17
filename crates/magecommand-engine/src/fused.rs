//! Fused interceptors — creatuity/magento2-interceptors' technique
//! (github.com/creatuity/magento2-interceptors), reimplemented clean-room.
//!
//! Instead of Magento's runtime `pluginList->getNext` + `___callPlugins`
//! dispatch, the plugin chain is resolved at compile time and unrolled into the
//! method body: plugins come lazily from `$this->____om->get()` accessors, and
//! the before/around/after listeners are inlined. This module renders the
//! GLOBAL-ONLY case (a class with no per-area plugin differences → a flat body,
//! no `switch (getCurrentScope())`). The per-area switch is a later increment.
//!
//! Credit: the fused format is creatuity's prior art. Their #28 (compiled
//! interceptors skipping global plugins in the `default` branch) is the reason
//! the switch's default must carry the global chain — reproduced here by making
//! the global chain the whole body for global-only classes.

use std::collections::HashSet;

use magequery_core::{Area, Magento};

use crate::definitions::Definitions;
use crate::laminas::render_type;
use crate::pluginlist::GlobalChains;
use crate::reflect::{self, RMethod, RParam};

const OM_TYPE: &str = "\\Magento\\Framework\\ObjectManagerInterface";
const SCOPE_TYPE: &str = "\\Magento\\Framework\\Config\\ScopeInterface";

/// Non-global areas whose overlays could add a per-area plugin.
const AREAS: [Area; 6] = [
    Area::Frontend,
    Area::Adminhtml,
    Area::Crontab,
    Area::WebapiRest,
    Area::WebapiSoap,
    Area::Graphql,
];

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

/// A class is "global-only" (renderable without a scope switch) when no
/// non-global area overlay declares a plugin targeting it or any ancestor.
fn is_global_only(magento: &Magento, defs: &Definitions, source: &str) -> bool {
    let mut anc: HashSet<String> = HashSet::new();
    let mut stack = vec![source.trim_start_matches('\\').to_owned()];
    while let Some(c) = stack.pop() {
        if !anc.insert(c.clone()) {
            continue;
        }
        for r in defs.relations_of(&c) {
            stack.push(r.trim_start_matches('\\').to_owned());
        }
        if let Some(rels) = crate::interception::internal_relations(&c) {
            for r in rels {
                stack.push(r.trim_start_matches('\\').to_owned());
            }
        }
    }
    for area in AREAS {
        let overlay = magento.di_export_overlay(area);
        for p in &overlay.plugins {
            if anc.contains(p.target.as_str().trim_start_matches('\\')) {
                return false;
            }
        }
    }
    true
}

/// One resolved listener layer for a method (a `{type}_{method}_{prev}` node).
struct Node {
    before: Vec<PluginRef>,
    around: Option<PluginRef>,
    after: Vec<PluginRef>,
    /// The layer inside `around`'s `$proceed` closure. `None` == "call parent".
    child: Option<Box<Node>>,
}

#[derive(Clone)]
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
fn emit(node: &Node, method: &str, tabs: usize, out: &mut Vec<String>) {
    let ti = "\t".repeat(tabs);
    let m = ucfirst(method);
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
    let assign = if has_after { "$result = " } else { "return " };
    if let Some(a) = &node.around {
        out.push(format!(
            "{ti}{assign}$this->____plugin_{}()->around{m}($this, function(...$arguments){{",
            a.clean
        ));
        if let Some(child) = &node.child {
            emit(child, method, tabs + 1, out);
        }
        out.push(format!("{ti}}}, ...\\array_values($arguments));"));
    } else {
        out.push(format!("{ti}{assign}parent::{method}(...\\array_values($arguments));"));
    }
    if has_after {
        out.push(ti.clone());
        let n = node.after.len();
        for (i, p) in node.after.iter().enumerate() {
            let kw = if i == n - 1 { "return " } else { "$result = " };
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

/// Render the fused interceptor for `source` if it is global-only; `None`
/// means "not handled here" (multi-area — the caller falls back to stock).
pub fn fused_interceptor_bytes(
    magento: &Magento,
    defs: &Definitions,
    chains: &GlobalChains,
    source: &str,
    intercepted: &HashSet<String>,
) -> Option<String> {
    if !is_global_only(magento, defs, source) {
        return None;
    }
    let record = defs.get(source)?;
    let source_fqcn = record.meta.fqcn.clone();
    let empty = Vec::new();
    let insts = chains.instances.get(&source_fqcn).unwrap_or(&empty);

    // Intercepted methods, in reflection order, with their chain trees.
    let mut method_bodies: Vec<(RMethod, Node)> = Vec::new();
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
        if let Some(node) = build(chains, &source_fqcn, &rm.name, "__self", insts) {
            method_bodies.push((rm, node));
        }
    }

    // Plugins used, in first-appearance order across all method bodies.
    let mut plugins: Vec<PluginRef> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
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
    for (_, node) in &method_bodies {
        collect(node, &mut plugins, &mut seen);
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

    // constructor
    let ctor = defs.constructor_of(source).ok().flatten();
    let mut params = vec![format!("{OM_TYPE} $____om"), format!("{SCOPE_TYPE} $____scope")];
    let mut ctor_body = vec![
        "        $this->____om = $____om;".to_owned(),
        "        $this->____scope = $____scope;".to_owned(),
    ];
    if let Some(info) = ctor {
        let subject_params = reflect::resolve_params(defs, info.definer_fqcn, info.params);
        for p in &subject_params {
            params.push(render_param(p));
        }
        let forwarded: Vec<String> = subject_params
            .iter()
            .map(|p| if p.variadic { format!("... ${}", p.name) } else { format!("${}", p.name) })
            .collect();
        ctor_body.push(format!("        parent::__construct({});", forwarded.join(", ")));
    }
    members.push(format!(
        "{}\n    public function __construct({})\n    {{\n{}\n    }}",
        docblock(&["{@inheritdoc}".into()]),
        params.join(", "),
        ctor_body.join("\n"),
    ));

    // intercepted methods
    for (rm, node) in &method_bodies {
        let mut body_lines = Vec::new();
        emit(node, &rm.name, 0, &mut body_lines);
        members.push(format!(
            "{}\n    public function {}\n    {{\n{}\n    }}",
            docblock(&["{@inheritdoc}".into()]),
            method_signature(rm),
            indent8(&body_lines),
        ));
    }

    // plugin accessors
    for p in &plugins {
        // di.xml name for the docblock `plugin "<name>"`: recover from insts.
        let name = insts.iter().find(|(n, _)| clean(n) == p.clean).map(|(n, _)| n.clone()).unwrap_or_else(|| p.clean.clone());
        let body = format!(
            "        if ($this->____plugin_{c} === null) {{\n        \t$this->____plugin_{c} = $this->____om->get(\\{i}::class);\n        }}\n        return $this->____plugin_{c};",
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
        emit(&root, "process", 0, &mut out);
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

    /// The after-only global case (ProbeGlobalOnly): parent call, then a single
    /// `return`-ed after.
    #[test]
    fn emit_after_only_calls_parent_then_after() {
        let node = Node { before: vec![], around: None, after: vec![pref("g")], child: None };
        let mut out = Vec::new();
        emit(&node, "greet", 0, &mut out);
        assert_eq!(
            out.join("\n"),
            "$arguments = \\func_get_args();\n\
             $result = parent::greet(...\\array_values($arguments));\n\
             \n\
             return $this->____plugin_g()->afterGreet($this, $result, ...\\array_values($arguments));"
        );
    }

    #[test]
    fn clean_and_ucfirst() {
        assert_eq!(clean("magequery_rich-low.x"), "magequery_rich_low_x");
        assert_eq!(ucfirst("getList"), "GetList");
    }
}
