//! Structural equivalence of two generated classes across code-generator
//! versions — the evidence behind `di verify`'s generator-formatting group.
//!
//! Magento's code generator changed a handful of details between the archive's
//! release and magecommand's 2.4.9 target. Rewriting text to paper over them
//! (the earlier approach) also papered over real changes: deleting every
//! `" = null,"` hid an interceptor that DROPPED a default, which PHP refuses to
//! load. So both files are parsed and compared as structure, and exactly the
//! known generator changes are allowed:
//!
//! - **Formatting** (`foo() : T` vs `foo(): T`, docblock wording, method
//!   order): invisible here by construction — types come back resolved,
//!   comments and whitespace are not tokens, methods are matched by name.
//! - **Explicit nullable default**: a nullable-typed parameter with no default
//!   in the archive and `= null` in the output. Nothing else about a parameter
//!   may differ.
//! - **Proxy laziness hardening**: in a proxy only, `__clone` and
//!   `_resetState` may each be either the old or the new template body, and
//!   the output may add the template's `__debugInfo`.
//!
//! Everything else — a changed type, an added or dropped parameter or default,
//! any other body change, any other method on one side only — is a difference.

use magecommand_php::{ClassMeta, MethodMeta, ParamMeta};

/// Whether `archive` and `output` are the same generated class modulo the
/// generator-version changes listed in the module docs.
pub(crate) fn generator_equivalent(archive: &str, output: &str) -> bool {
    let (Some(a), Some(b)) = (Generated::parse(archive), Generated::parse(output)) else {
        return false;
    };
    if a.shape != b.shape {
        return false;
    }
    let is_proxy = a.shape.implements.iter().any(|i| i == NONINTERCEPTABLE);

    for (name, (meta_a, body_a)) in &a.methods {
        let Some((meta_b, body_b)) = b.methods.get(name) else {
            return false;
        };
        if !signature_equivalent(meta_a, meta_b) {
            return false;
        }
        let bodies_ok = if is_proxy && (name == "__clone" || name == "_resetState") {
            let allowed = proxy_template_bodies(name);
            allowed.iter().any(|t| same(body_a, t)) && allowed.iter().any(|t| same(body_b, t))
        } else {
            body_a == body_b
        };
        if !bodies_ok {
            return false;
        }
    }
    // Output-only methods: the proxy template's `__debugInfo`, nothing else.
    b.methods.iter().all(|(name, (meta, body))| {
        a.methods.contains_key(name)
            || (is_proxy
                && name == "__debugInfo"
                && meta.params.is_empty()
                && same(body, DEBUG_INFO_BODY))
    })
}

fn same(body: &[String], template: &[&str]) -> bool {
    body.len() == template.len() && body.iter().zip(template).all(|(a, b)| a == b)
}

const NONINTERCEPTABLE: &str = "Magento\\Framework\\ObjectManager\\NoninterceptableInterface";

/// `return ['i' => $this->_subject];`, as tokens.
const DEBUG_INFO_BODY: &[&str] =
    &["return", "[", "'i'", "=", ">", "$this", "-", ">", "_subject", "]", ";"];

/// The old (unguarded) and new (null-guarded) proxy template bodies.
fn proxy_template_bodies(method: &str) -> [&'static [&'static str]; 2] {
    const CLONE_OLD: &[&str] =
        &["$this", "-", ">", "_subject", "=", "clone", "$this", "-", ">", "_getSubject", "(", ")", ";"];
    const CLONE_NEW: &[&str] = &[
        "if", "(", "$this", "-", ">", "_subject", ")", "{", "$this", "-", ">", "_subject", "=",
        "clone", "$this", "-", ">", "_getSubject", "(", ")", ";", "}",
    ];
    const RESET_OLD: &[&str] =
        &["$this", "-", ">", "_getSubject", "(", ")", "-", ">", "_resetState", "(", ")", ";"];
    const RESET_NEW: &[&str] = &[
        "if", "(", "$this", "-", ">", "_subject", ")", "{", "$this", "-", ">", "_subject", "-",
        ">", "_resetState", "(", ")", ";", "}",
    ];
    if method == "__clone" {
        [CLONE_OLD, CLONE_NEW]
    } else {
        [RESET_OLD, RESET_NEW]
    }
}

fn signature_equivalent(a: &MethodMeta, b: &MethodMeta) -> bool {
    a.name == b.name
        && a.visibility == b.visibility
        && a.is_static == b.is_static
        && a.is_abstract == b.is_abstract
        && a.is_final == b.is_final
        && a.returns_ref == b.returns_ref
        && a.return_type == b.return_type
        && a.params.len() == b.params.len()
        && a.params.iter().zip(&b.params).all(|(pa, pb)| param_equivalent(pa, pb))
}

/// Equal, or the one allowed change: the output spells out `= null` on a
/// nullable parameter the archive left without a default.
fn param_equivalent(a: &ParamMeta, b: &ParamMeta) -> bool {
    if a == b {
        return true;
    }
    let nullable = a.ty.as_deref().is_some_and(|t| {
        t.starts_with('?') || t.split('|').any(|p| p.trim().eq_ignore_ascii_case("null"))
    });
    nullable
        && a.default.is_none()
        && b.default.as_deref().is_some_and(|d| d.eq_ignore_ascii_case("null"))
        && a.name == b.name
        && a.ty == b.ty
        && a.by_ref == b.by_ref
        && a.variadic == b.variadic
        && a.promoted == b.promoted
        && a.readonly == b.readonly
}

/// One parsed generated class: its shape (everything but the methods) and its
/// methods by name, each with its body as tokens.
struct Generated {
    shape: ClassMeta,
    methods: std::collections::BTreeMap<String, (MethodMeta, Vec<String>)>,
}

impl Generated {
    fn parse(src: &str) -> Option<Generated> {
        let meta = magecommand_php::parse_file(src.as_bytes());
        if !meta.issues.is_empty() || meta.declarations.len() != 1 {
            return None;
        }
        let mut shape = meta.declarations.into_iter().next()?;
        let mut methods = std::collections::BTreeMap::new();
        for mut m in std::mem::take(&mut shape.methods) {
            let body = body_tokens(src, m.offset)?;
            m.offset = 0;
            if methods.insert(m.name.clone(), (m, body)).is_some() {
                return None;
            }
        }
        Some(Generated { shape, methods })
    }
}

/// The tokens of the method body that follows the `function` keyword at
/// `offset`: from the first `{` after the parameter list to its matching `}`,
/// exclusive. `None` for a bodiless (abstract) method or an unbalanced file —
/// generated classes have neither, and the caller then refuses the match.
fn body_tokens(src: &str, offset: usize) -> Option<Vec<String>> {
    let tokens = tokenize(src.get(offset..)?);
    let mut paren = 0i32;
    let mut seen_params = false;
    let mut start = None;
    for (i, t) in tokens.iter().enumerate() {
        match *t {
            "(" => {
                paren += 1;
                seen_params = true;
            }
            ")" => paren -= 1,
            ";" if paren == 0 && seen_params => return None,
            "{" if paren == 0 && seen_params => {
                start = Some(i + 1);
                break;
            }
            _ => {}
        }
    }
    let start = start?;
    let mut depth = 1i32;
    for (i, t) in tokens.iter().enumerate().skip(start) {
        match *t {
            "{" => depth += 1,
            "}" => {
                depth -= 1;
                if depth == 0 {
                    return Some(tokens[start..i].iter().map(|t| (*t).to_owned()).collect());
                }
            }
            _ => {}
        }
    }
    None
}

/// A minimal PHP tokenizer for generated code: whitespace and comments are
/// dropped, strings stay whole (quotes included), identifiers and `$variables`
/// are one token, and every other byte is its own token. `#[` opens an
/// attribute (tokens), not a comment.
fn tokenize(src: &str) -> Vec<&str> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'\\' || c >= 0x80;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i = src[i + 2..].find("*/").map_or(bytes.len(), |e| i + 2 + e + 2);
        } else if (c == b'/' && bytes.get(i + 1) == Some(&b'/'))
            || (c == b'#' && bytes.get(i + 1) != Some(&b'['))
        {
            i = src[i..].find('\n').map_or(bytes.len(), |e| i + e);
        } else if c == b'\'' || c == b'"' {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != c {
                i += if bytes[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(bytes.len());
            out.push(&src[start..i]);
        } else if c == b'$' || ident(c) {
            let start = i;
            i += 1;
            while i < bytes.len() && ident(bytes[i]) {
                i += 1;
            }
            out.push(&src[start..i]);
        } else {
            let len = src[i..].chars().next().map_or(1, char::len_utf8);
            out.push(&src[i..i + len]);
            i += len;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROXY_OLD: &str = r#"<?php
namespace Acme\Mod\Model\Thing;

/**
 * Proxy class for @see \Acme\Mod\Model\Thing
 */
class Proxy extends \Acme\Mod\Model\Thing implements \Magento\Framework\ObjectManager\NoninterceptableInterface
{
    protected $_subject = null;

    public function __clone()
    {
        $this->_subject = clone $this->_getSubject();
    }

    /**
     * {@inheritdoc}
     */
    public function _resetState() : void
    {
        $this->_getSubject()->_resetState();
    }

    /**
     * {@inheritdoc}
     */
    public function load(?int $id, $field) : self
    {
        return $this->_getSubject()->load($id, $field);
    }

    /**
     * {@inheritdoc}
     */
    public function getName()
    {
        return $this->_getSubject()->getName();
    }
}
"#;

    const PROXY_NEW: &str = r#"<?php
namespace Acme\Mod\Model\Thing;

/**
 * Proxy class for @see \Acme\Mod\Model\Thing
 */
class Proxy extends \Acme\Mod\Model\Thing implements \Magento\Framework\ObjectManager\NoninterceptableInterface
{
    protected $_subject = null;

    public function __clone()
    {
        if ($this->_subject) {
            $this->_subject = clone $this->_getSubject();
        }
    }

    /**
     * Debug proxied instance
     */
    public function __debugInfo()
    {
        return ['i' => $this->_subject];
    }

    /**
     * Reset state of proxied instance
     */
    public function _resetState(): void
    {
        if ($this->_subject) {
            $this->_subject->_resetState();
        }
    }

    /**
     * {@inheritdoc}
     */
    public function getName()
    {
        return $this->_getSubject()->getName();
    }

    /**
     * {@inheritdoc}
     */
    public function load(?int $id = null, $field): self
    {
        return $this->_getSubject()->load($id, $field);
    }
}
"#;

    #[test]
    fn every_known_generator_change_is_equivalent() {
        assert!(generator_equivalent(PROXY_OLD, PROXY_NEW));
        assert!(generator_equivalent(PROXY_NEW, PROXY_NEW));
    }

    #[test]
    fn a_dropped_default_is_not_equivalent() {
        // The direction the old text rewrite could not see: the output LOSES a
        // default, which PHP rejects as an incompatible override.
        assert!(!generator_equivalent(PROXY_NEW, PROXY_OLD));
        let lost = PROXY_NEW.replace("$field): self", "$field = 1): self");
        assert!(!generator_equivalent(&lost, PROXY_NEW));
    }

    #[test]
    fn real_changes_are_not_equivalent() {
        let cases = [
            // A changed type.
            PROXY_NEW.replace("?int $id", "?string $id"),
            // A body that calls something else.
            PROXY_NEW.replace("->getName();", "->getLabel();"),
            // A changed argument order in a delegation.
            PROXY_NEW.replace("load($id, $field)", "load($field, $id)"),
            // An extra method.
            PROXY_NEW.replace(
                "    public function getName()",
                "    public function extra()\n    {\n    }\n\n    public function getName()",
            ),
            // A `__debugInfo` that leaks something else.
            PROXY_NEW.replace("['i' => $this->_subject]", "['i' => $this->_getSubject()]"),
            // A third `__clone` body.
            PROXY_NEW.replace(
                "if ($this->_subject) {\n            $this->_subject = clone $this->_getSubject();\n        }",
                "$this->_subject = null;",
            ),
            // A changed parent.
            PROXY_NEW.replace("extends \\Acme\\Mod\\Model\\Thing", "extends \\Acme\\Mod\\Model\\Other"),
        ];
        for (i, output) in cases.iter().enumerate() {
            assert_ne!(output, PROXY_NEW, "case {i} did not apply");
            assert!(!generator_equivalent(PROXY_OLD, output), "case {i} passed as equivalent");
        }
    }

    #[test]
    fn proxy_allowances_do_not_apply_to_interceptors() {
        let interceptor = |body: &str, extra: &str| {
            format!(
                "<?php\nnamespace Acme\\Mod\\Model\\Thing;\n\nclass Interceptor extends \\Acme\\Mod\\Model\\Thing implements \\Magento\\Framework\\Interception\\InterceptorInterface\n{{\n    public function __clone()\n    {{\n        {body}\n    }}\n{extra}}}\n"
            )
        };
        let old = interceptor("$this->_subject = clone $this->_getSubject();", "");
        let guarded = interceptor(
            "if ($this->_subject) { $this->_subject = clone $this->_getSubject(); }",
            "",
        );
        let debug = interceptor(
            "$this->_subject = clone $this->_getSubject();",
            "\n    public function __debugInfo()\n    {\n        return ['i' => $this->_subject];\n    }\n",
        );
        assert!(!generator_equivalent(&old, &guarded));
        assert!(!generator_equivalent(&old, &debug));
    }
}
