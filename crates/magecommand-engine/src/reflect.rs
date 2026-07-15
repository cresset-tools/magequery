//! Reconstruct PHP reflection's `ReflectionClass::getMethods(IS_PUBLIC)` for
//! a class or interface — the input the Proxy and Interceptor generators walk.
//! Each method carries its full signature (parameters with reflection-resolved
//! types and evaluated defaults, return type, by-ref/variadic/static/final
//! flags) and its declaring class (for `self`/`parent` resolution).
//!
//! Ordering mirrors the engine's internal function table: a class's own
//! public methods in declaration order, then trait-imported methods, then the
//! parent chain (nearest definition wins); an interface's own methods, then
//! its extended-interface tables. Validated against the frozen archive.

use magecommand_php::{ClassKind, Visibility as PhpVis};
use magecommand_php::constexpr::{
    eval, parse_const_expr, ArrayKey, BinOp, ClassRef, ConstExpr, ConstValue, EvalCtx,
};
use std::collections::HashSet;

use crate::arguments::DefsLookup;
use crate::definitions::Definitions;
use crate::laminas::{ArrKey, Val};

#[derive(Debug, Clone)]
pub struct RParam {
    pub name: String,
    /// Reflection-resolved type (no leading backslash on classes; the Laminas
    /// TypeGenerator adds it). `None` = untyped.
    pub type_str: Option<String>,
    pub by_ref: bool,
    pub variadic: bool,
    pub default: Option<Val>,
}

#[derive(Debug, Clone)]
pub struct RMethod {
    pub name: String,
    pub params: Vec<RParam>,
    pub return_type: Option<String>,
    pub returns_ref: bool,
    pub is_static: bool,
    pub is_final: bool,
    pub is_abstract: bool,
    pub declaring: String,
}

/// Public methods of `fqcn` in reflection order, signatures resolved.
pub fn public_methods(defs: &Definitions, fqcn: &str) -> Vec<RMethod> {
    let mut out: Vec<RMethod> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let Some(record) = defs.get(fqcn) else {
        return out;
    };

    if record.meta.kind == ClassKind::Interface {
        // Interface: own methods, then each extended interface's table.
        collect_from(defs, fqcn, &mut out, &mut seen);
        for iface in defs.all_interfaces(fqcn) {
            collect_from(defs, &iface, &mut out, &mut seen);
        }
        return out;
    }

    // Class: own + traits, then parent chain (nearest wins).
    let mut current = Some(fqcn.to_owned());
    let mut hops = 0;
    while let Some(name) = current {
        hops += 1;
        if hops > 64 {
            break;
        }
        let Some(rec) = defs.get(&name) else { break };
        collect_from(defs, &name, &mut out, &mut seen);
        // Trait methods, flattened depth-first in declaration order: each trait
        // contributes its own public methods, then recurses into its nested
        // `use`d traits (pre-order), siblings left-to-right — mirroring the
        // engine's trait-flattening into the function table. (A stack/`pop`
        // walk here would reverse sibling traits, misordering the interceptor's
        // methods for any class that `use`s more than one.)
        let mut expanded: HashSet<String> = HashSet::new();
        for t in &rec.meta.traits {
            collect_trait(defs, t, &mut out, &mut seen, &mut expanded);
        }
        current = rec
            .meta
            .extends
            .first()
            .filter(|_| rec.meta.kind != ClassKind::Interface)
            .cloned();
    }
    out
}

/// Append a trait's public methods, then recurse into its nested `use`d traits
/// in declaration order (pre-order DFS) — PHP's trait-flattening order. The
/// `expanded` guard applies each trait once per class hop (a diamond of traits
/// contributes its methods at first encounter; later paths are `seen`-deduped).
fn collect_trait(
    defs: &Definitions,
    trait_name: &str,
    out: &mut Vec<RMethod>,
    seen: &mut HashSet<String>,
    expanded: &mut HashSet<String>,
) {
    if !expanded.insert(trait_name.to_owned()) {
        return;
    }
    collect_from(defs, trait_name, out, seen);
    if let Some(tr) = defs.get(trait_name) {
        for nested in &tr.meta.traits {
            collect_trait(defs, nested, out, seen, expanded);
        }
    }
}

/// Append `owner`'s directly-declared public methods (not yet seen).
fn collect_from(defs: &Definitions, owner: &str, out: &mut Vec<RMethod>, seen: &mut HashSet<String>) {
    let Some(rec) = defs.get(owner) else { return };
    for m in &rec.meta.methods {
        if m.visibility != PhpVis::Public {
            continue;
        }
        if !seen.insert(m.name.to_ascii_lowercase()) {
            continue;
        }
        let declaring = rec.meta.fqcn.clone();
        let params = m
            .params
            .iter()
            .map(|p| resolve_param(defs, &declaring, p))
            .collect();
        // PHP gives magic methods a tentative return type even when the source
        // omits one: `__toString` reflects as `: string` (Stringable is
        // auto-implemented for any class declaring it).
        let mut return_type = resolve_return_type(defs, &declaring, m.return_type.as_deref());
        if return_type.is_none() && m.name.eq_ignore_ascii_case("__toString") {
            return_type = Some("string".to_owned());
        }
        out.push(RMethod {
            name: m.name.clone(),
            params,
            return_type,
            returns_ref: m.returns_ref,
            is_static: m.is_static,
            is_final: m.is_final,
            is_abstract: m.is_abstract,
            declaring,
        });
    }
}

/// Resolve a parameter list (a constructor's, say) in `declaring`'s context —
/// the same reflection extraction the method walk applies.
pub fn resolve_params(
    defs: &Definitions,
    declaring: &str,
    params: &[magecommand_php::ParamMeta],
) -> Vec<RParam> {
    params.iter().map(|p| resolve_param(defs, declaring, p)).collect()
}

/// Resolve one parameter's type + default the way `EntityAbstract`'s
/// reflection extractors do.
fn resolve_param(defs: &Definitions, declaring: &str, p: &magecommand_php::ParamMeta) -> RParam {
    let default = p.default.as_deref().and_then(|expr| eval_default(defs, declaring, expr));
    let type_str = resolve_param_type(defs, declaring, p.ty.as_deref(), default.as_ref());
    RParam {
        name: p.name.clone(),
        type_str,
        by_ref: p.by_ref,
        variadic: p.variadic,
        default,
    }
}

/// `extractParameterType`: resolve `self`/`parent`, then apply the implicit
/// nullable rule (a `null` default on a non-nullable, non-mixed type adds `?`).
fn resolve_param_type(
    defs: &Definitions,
    declaring: &str,
    ty: Option<&str>,
    default: Option<&Val>,
) -> Option<String> {
    let ty = ty?;
    let resolved = normalize_nullable(&resolve_self_parent(defs, declaring, ty));
    let allows_null = resolved.starts_with('?')
        || contains_null_member(&resolved)
        || matches!(default, Some(Val::Null));
    if allows_null
        && resolved != "mixed"
        && !resolved.starts_with('?')
        && !contains_null_member(&resolved)
    {
        Some(format!("?{resolved}"))
    } else {
        Some(resolved)
    }
}

fn resolve_return_type(defs: &Definitions, declaring: &str, rt: Option<&str>) -> Option<String> {
    let rt = rt?;
    Some(normalize_nullable(&resolve_self_parent(defs, declaring, rt)))
}

/// PHP reflection normalizes a union of exactly one non-null type plus `null`
/// into a nullable named type: `T|null` (or `null|T`) reflects as `?T`
/// (`getType()` returns a `ReflectionNamedType` with `allowsNull()`, not a
/// `ReflectionUnionType`). Genuine multi-type unions keep `null` as a member.
fn normalize_nullable(ty: &str) -> String {
    if ty.starts_with('?') || !ty.contains('|') {
        return ty.to_owned();
    }
    let members: Vec<&str> = ty.split('|').map(str::trim).collect();
    let non_null: Vec<&str> =
        members.iter().copied().filter(|m| !m.eq_ignore_ascii_case("null")).collect();
    let has_null = members.iter().any(|m| m.eq_ignore_ascii_case("null"));
    if has_null && non_null.len() == 1 {
        format!("?{}", non_null[0])
    } else {
        ty.to_owned()
    }
}

/// Map `self`/`parent` to concrete FQCNs (reflection reports them resolved);
/// `static` and everything else pass through. Applies per union member.
fn resolve_self_parent(defs: &Definitions, declaring: &str, ty: &str) -> String {
    let (prefix, body) = match ty.strip_prefix('?') {
        Some(rest) => ("?", rest),
        None => ("", ty),
    };
    let sep = if body.contains('|') {
        Some('|')
    } else if body.contains('&') {
        Some('&')
    } else {
        None
    };
    let mapped = match sep {
        Some(s) => body
            .split(s)
            .map(|m| map_self_parent(defs, declaring, m.trim()))
            .collect::<Vec<_>>()
            .join(&s.to_string()),
        None => map_self_parent(defs, declaring, body),
    };
    format!("{prefix}{mapped}")
}

fn map_self_parent(defs: &Definitions, declaring: &str, name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "self" => declaring.to_owned(),
        "parent" => defs
            .get(declaring)
            .and_then(|r| r.meta.extends.first().cloned())
            .unwrap_or_else(|| name.to_owned()),
        _ => canonicalize_class_case(defs, name),
    }
}

/// PHP class names are case-insensitive, but reflection reports a class type in
/// its DECLARED case, not the case written at the type-hint site. So a param
/// hinted `\…\Resourcemodel\Quote` whose class declares `ResourceModel`
/// reflects (and the real interceptor renders) as `ResourceModel`. Canonicalize
/// any name resolving to a known class to its `meta.fqcn`; scalars, built-ins,
/// and names outside the class universe pass through unchanged.
///
/// A legacy `Zend\…` name is a further wrinkle: the `laminas-zendframework-bridge`
/// aliases it to a real `Laminas\…` class, so reflection reports the canonical
/// Laminas name (a hint `\Zend\Uri\Uri` renders `\Laminas\Uri\Uri`). We reproduce
/// that only when the legacy name is not itself a known class and the Laminas
/// target *is* — matching the bridge, which aliases solely when the target loads.
fn canonicalize_class_case(defs: &Definitions, name: &str) -> String {
    let bare = name.trim_start_matches('\\');
    if let Some(rec) = defs.get(bare) {
        return declared_case(rec, name, bare);
    }
    // Not a known class: try the legacy-alias rewrite (Zend\… -> Laminas\…),
    // gated on the canonical target actually existing in the class universe.
    if let Some(canonical) = magequery_core::laminas_alias::canonical(name) {
        let canonical_bare = canonical.trim_start_matches('\\');
        if let Some(rec) = defs.get(canonical_bare) {
            return declared_case(rec, &canonical, canonical_bare);
        }
    }
    name.to_owned()
}

/// The declared-case FQCN of `rec`, preserving the leading-backslash convention
/// of `name`. `bare` is `name` without its leading backslash.
fn declared_case(rec: &crate::definitions::ClassRecord, name: &str, bare: &str) -> String {
    if rec.meta.fqcn == bare {
        return name.to_owned();
    }
    if name.starts_with('\\') {
        format!("\\{}", rec.meta.fqcn)
    } else {
        rec.meta.fqcn.clone()
    }
}

fn contains_null_member(ty: &str) -> bool {
    ty.split(['|', '&']).any(|m| m.trim().eq_ignore_ascii_case("null"))
}

/// Evaluate a parameter default expression to a rendered value. `None` when
/// the evaluator can't fold it (rare — logged by the caller via a diff).
fn eval_default(defs: &Definitions, declaring: &str, expr: &str) -> Option<Val> {
    let ns = declaring.rsplit_once('\\').map(|(n, _)| n).unwrap_or("");
    let record = defs.get(declaring);
    let uses: &[(String, String)] = record.map(|r| r.meta.uses.as_slice()).unwrap_or(&[]);
    let parsed = parse_const_expr(expr, ns, uses);
    let lookup = DefsLookup { defs };
    let ctx = EvalCtx::new(&lookup, Some(declaring));
    match eval(&parsed, &ctx) {
        Ok(value) => Some(const_to_val(&value)),
        // The evaluator can't fold it — typically a `\Class::CONST` whose
        // defining class isn't in the parsed universe (e.g. Adobe's
        // `\Zend_Cache::CLEANING_MODE_ALL`). Dropping the default would turn an
        // inherited-optional parameter into a required one — an illegal
        // override that fatals at class load. Preserve a fully-qualified,
        // always-valid reference instead. Byte-exactness is retained wherever
        // eval succeeds (the oracle never reaches this arm — it stays
        // 4106/4106); this only turns the unresolvable case from broken code
        // into valid code.
        Err(_) => verbatim_expr(&parsed.expr, &parsed.classes).map(Val::Raw),
    }
}

/// Render a const expression back to valid PHP source, fully qualifying every
/// class reference so it is valid regardless of the generated file's `use`
/// context. `None` for shapes not worth reproducing (they don't hit the fatal
/// case, so the prior drop behaviour is retained for them).
fn verbatim_expr(expr: &ConstExpr, classes: &[String]) -> Option<String> {
    Some(match expr {
        ConstExpr::Null => "null".to_owned(),
        ConstExpr::Bool(b) => Val::Bool(*b).render(),
        ConstExpr::Int(n) => Val::Int(*n).render(),
        ConstExpr::Float(f) => Val::Float(*f).render(),
        ConstExpr::Str(s) => Val::Str(s.clone()).render(),
        ConstExpr::ClassNameOf(fqcn) => format!("\\{fqcn}::class"),
        ConstExpr::GlobalConst(name) => name.clone(),
        ConstExpr::ClassConst { class, name } => {
            let cls = match class {
                ClassRef::Fqcn(i) => format!("\\{}", classes.get(*i)?),
                ClassRef::SelfRef => "self".to_owned(),
                ClassRef::ParentRef => "parent".to_owned(),
                ClassRef::StaticRef => "static".to_owned(),
                _ => return None,
            };
            format!("{cls}::{name}")
        }
        ConstExpr::Neg(inner) => format!("-{}", verbatim_expr(inner, classes)?),
        ConstExpr::BinOp { op, left, right } => {
            let sym = match op {
                BinOp::Concat => ".",
                BinOp::Add => "+",
                BinOp::Sub => "-",
                BinOp::Mul => "*",
                BinOp::Div => "/",
                BinOp::Mod => "%",
                BinOp::Shl => "<<",
                BinOp::Shr => ">>",
                BinOp::BitOr => "|",
                BinOp::BitAnd => "&",
                BinOp::BitXor => "^",
                _ => return None,
            };
            format!(
                "{} {sym} {}",
                verbatim_expr(left, classes)?,
                verbatim_expr(right, classes)?
            )
        }
        _ => return None,
    })
}

fn const_to_val(value: &ConstValue) -> Val {
    match value {
        ConstValue::Null => Val::Null,
        ConstValue::Bool(b) => Val::Bool(*b),
        ConstValue::Int(n) => Val::Int(*n),
        ConstValue::Float(f) => Val::Float(*f),
        ConstValue::Str(s) => Val::Str(s.clone()),
        ConstValue::Array(items) => Val::Array(
            items
                .iter()
                .map(|(k, v)| {
                    let key = match k {
                        ArrayKey::Int(n) => Some(ArrKey::Int(*n)),
                        ArrayKey::Str(s) => Some(ArrKey::Str(s.clone())),
                        _ => None,
                    };
                    (key, const_to_val(v))
                })
                .collect(),
        ),
        _ => Val::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_nullable;

    #[test]
    fn single_class_plus_null_collapses_to_question_mark() {
        // PHP reflection reports `T|null` (one class + null) as the nullable
        // named type `?T` — the AttributeSet Grid Collection interceptor case.
        assert_eq!(
            normalize_nullable("Magento\\Framework\\Model\\ResourceModel\\Db\\AbstractDb|null"),
            "?Magento\\Framework\\Model\\ResourceModel\\Db\\AbstractDb"
        );
        // `null` first works too, and casing is PHP-insensitive.
        assert_eq!(normalize_nullable("null|Foo\\Bar"), "?Foo\\Bar");
        assert_eq!(normalize_nullable("Foo|NULL"), "?Foo");
    }

    #[test]
    fn multi_type_union_keeps_null_member() {
        // Two or more non-null members stay a genuine union.
        assert_eq!(normalize_nullable("Foo|Bar|null"), "Foo|Bar|null");
        assert_eq!(normalize_nullable("int|string"), "int|string");
    }

    #[test]
    fn non_union_and_already_nullable_pass_through() {
        assert_eq!(normalize_nullable("Foo\\Bar"), "Foo\\Bar");
        assert_eq!(normalize_nullable("?Foo\\Bar"), "?Foo\\Bar");
        assert_eq!(normalize_nullable("int"), "int");
    }

    #[test]
    fn verbatim_default_fully_qualifies_class_const() {
        use super::verbatim_expr;
        use magecommand_php::constexpr::{ClassRef, ConstExpr};
        // The Zend_Cache case: an unresolvable `\Zend_Cache::CLEANING_MODE_ALL`
        // renders to a valid, fully-qualified reference — never dropped.
        let expr = ConstExpr::ClassConst {
            class: ClassRef::Fqcn(0),
            name: "CLEANING_MODE_ALL".to_owned(),
        };
        assert_eq!(
            verbatim_expr(&expr, &["Zend_Cache".to_owned()]).as_deref(),
            Some("\\Zend_Cache::CLEANING_MODE_ALL")
        );
        // self::/parent:: stay valid in the generated subclass (inherited const).
        assert_eq!(
            verbatim_expr(
                &ConstExpr::ClassConst { class: ClassRef::SelfRef, name: "X".to_owned() },
                &[]
            )
            .as_deref(),
            Some("self::X")
        );
        // A concat of a const and a string keeps both, recursively.
        let concat = ConstExpr::BinOp {
            op: magecommand_php::constexpr::BinOp::Concat,
            left: Box::new(ConstExpr::ClassConst {
                class: ClassRef::Fqcn(0),
                name: "PREFIX".to_owned(),
            }),
            right: Box::new(ConstExpr::Str("x".to_owned())),
        };
        assert_eq!(
            verbatim_expr(&concat, &["Acme\\C".to_owned()]).as_deref(),
            Some("\\Acme\\C::PREFIX . 'x'")
        );
    }

    #[test]
    fn class_type_canonicalizes_to_declared_case() {
        use super::canonicalize_class_case;
        use crate::definitions::{ClassRecord, Definitions};
        use std::path::PathBuf;

        let meta = magecommand_php::parse_file(
            b"<?php namespace Acme\\Foo\\ResourceModel; class Thing {}",
        )
        .declarations
        .into_iter()
        .next()
        .unwrap();
        let fqcn = meta.fqcn.clone();
        let defs =
            Definitions::from_records([(fqcn, ClassRecord { meta, file: PathBuf::new() })]);

        // A hint written in the wrong case (`Resourcemodel`) resolves to the
        // DECLARED case (`ResourceModel`) — what PHP reflection reports.
        assert_eq!(
            canonicalize_class_case(&defs, "\\Acme\\Foo\\Resourcemodel\\Thing"),
            "\\Acme\\Foo\\ResourceModel\\Thing"
        );
        // The leading-backslash spelling is preserved as written.
        assert_eq!(
            canonicalize_class_case(&defs, "Acme\\Foo\\Resourcemodel\\Thing"),
            "Acme\\Foo\\ResourceModel\\Thing"
        );
        // Scalars and classes outside the universe pass through untouched.
        assert_eq!(canonicalize_class_case(&defs, "int"), "int");
        assert_eq!(canonicalize_class_case(&defs, "\\Other\\Cls"), "\\Other\\Cls");
    }
}
