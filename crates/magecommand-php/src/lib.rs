//! Structural PHP parser for magecommand.
//!
//! A DI compiler needs declarations, never method bodies. This crate parses
//! exactly that subset: namespace + use imports, class/interface/trait/enum
//! headers, constructor parameters (incl. promoted properties), method
//! signatures, trait uses + adaptations, class constants, and declared
//! properties (for the objects `new X()` parameter defaults build). Bodies are
//! *skipped*, not parsed — `memchr`-accelerated scanning past string/comment/
//! heredoc interiors, with a scalar recursive-descent over the structural
//! remainder.
//!
//! Design contract (CAS prerequisite): [`parse_file`] is a pure function of
//! the input bytes. No filesystem access, no environment, no PHP execution —
//! ever. A construct the parser cannot classify is a hard [`ParseIssue`],
//! never a silent guess; there is no fallback tier behind it.

mod constants_generated;
pub mod constexpr;
mod model;
mod parse;
mod scan;

pub use model::*;
pub use parse::{parse_file, parse_file_with};

#[cfg(test)]
mod tests {
    use super::*;

    fn one(src: &str) -> ClassMeta {
        let meta = parse_file(src.as_bytes());
        assert!(
            meta.issues.is_empty(),
            "unexpected issues: {:?}",
            meta.issues
        );
        assert_eq!(meta.declarations.len(), 1, "expected one declaration");
        meta.declarations.into_iter().next().unwrap()
    }

    #[test]
    fn alt_syntax_class_exists_guard_keeps_if_branch() {
        // `if (class_exists(X)): class C {…} else: class C {…} endif;` — the
        // alternative-syntax compat shim (TIG PostNL's SourceItemsDataBySkuProxy,
        // G2). The class must be registered (not dropped), at brace depth 0 so it
        // is scanned, keeping the FIRST (`if`-branch) body with its constructor.
        let src = r#"<?php
namespace App;
use Magento\Framework\ObjectManagerInterface;
if (class_exists(\App\Dep\Factory::class)):
    class Proxy {
        public function __construct(ObjectManagerInterface $om, Dep $dep) {}
    }
else:
    class Proxy {
        public function __construct(ObjectManagerInterface $om) {}
    }
endif;
"#;
        // `parse_file` keeps the first declaration, guards unevaluated.
        // Only one declaration survives dedup — the if-branch — and it is NOT
        // conditional (no braces around it), so the scan will include it.
        let c = one(src);
        assert_eq!(c.fqcn, "App\\Proxy");
        assert!(!c.conditional, "alt-syntax class sits at brace depth 0");
        let ctor = c.methods.iter().find(|m| m.name == "__construct").unwrap();
        assert_eq!(ctor.params.len(), 2, "the if-branch constructor is kept");
        assert_eq!(ctor.params[1].name, "dep");
    }

    /// Parse `src` with `loadable` answering every guard, expecting one
    /// surviving declaration.
    fn one_with(src: &str, loadable: impl Fn(&str) -> bool) -> ClassMeta {
        let meta = parse_file_with(src.as_bytes(), loadable);
        assert!(
            meta.issues.is_empty(),
            "unexpected issues: {:?}",
            meta.issues
        );
        assert_eq!(meta.declarations.len(), 1, "expected one declaration");
        meta.declarations.into_iter().next().unwrap()
    }

    const ALT_SHIM: &str = r#"<?php
namespace App;
use Magento\Framework\ObjectManagerInterface;
use App\Dep\Factory as DepFactory;
if (class_exists(DepFactory::class)):
    class Proxy {
        public function __construct(ObjectManagerInterface $om, Dep $dep) {}
    }
else:
    class Proxy {
    }
endif;
"#;

    #[test]
    fn alt_syntax_guard_records_the_resolved_class() {
        let meta = parse_file_with(ALT_SHIM.as_bytes(), |_| true);
        let c = &meta.declarations[0];
        assert_eq!(
            c.guard,
            Some(ClassGuard {
                class: "App\\Dep\\Factory".into(),
                requires: true
            })
        );
    }

    #[test]
    fn alt_syntax_guard_on_a_missing_class_keeps_the_else_branch() {
        // TIG PostNL's SourceItemsDataBySkuProxy on a store without MSI: PHP
        // defines the constructor-less `else` class, so Magento compiles NULL.
        let c = one_with(ALT_SHIM, |_| false);
        assert!(
            c.methods.is_empty(),
            "the else-branch class has no constructor"
        );
        assert_eq!(
            c.guard,
            Some(ClassGuard {
                class: "App\\Dep\\Factory".into(),
                requires: false
            })
        );
    }

    #[test]
    fn braced_guard_on_a_missing_class_keeps_the_else_branch() {
        let src = r#"<?php
namespace Acme\Shim;
if (\class_exists('\\Vendor\\Real\\Base')) {
    class Shim extends \Vendor\Real\Base {}
} else {
    class Shim {}
}"#;
        let c = one_with(src, |c| {
            assert_eq!(c, "Vendor\\Real\\Base", "the string argument, unescaped");
            false
        });
        assert!(c.extends.is_empty(), "the else-branch stub");
        assert!(c.conditional);
    }

    #[test]
    fn negated_guard_inverts_the_branch() {
        let src = r#"<?php
namespace A;
if (!interface_exists(\B\I::class)):
    class C { public function __construct(int $x) {} }
else:
    class C {}
endif;
"#;
        assert_eq!(
            one_with(src, |_| false).methods.len(),
            1,
            "the `if` branch runs"
        );
        assert!(
            one_with(src, |_| true).methods.is_empty(),
            "the `else` branch runs"
        );
    }

    #[test]
    fn a_compound_condition_or_elseif_is_unguarded_and_keeps_the_first() {
        // Conditions we can't read decide nothing: the first declaration wins,
        // as before guards were evaluated.
        let compound = r#"<?php
namespace A;
if (class_exists(\B\X::class) && PHP_VERSION_ID > 80000) {
    class C { public function m() {} }
} else {
    class C {}
}"#;
        let c = one_with(compound, |_| false);
        assert_eq!(c.methods.len(), 1);
        assert_eq!(c.guard, None);

        let elseif = r#"<?php
namespace A;
if (class_exists(\B\X::class)):
    class C {}
elseif (class_exists(\B\Y::class)):
    class C { public function m() {} }
else:
    class C { public function n() {} }
endif;
"#;
        // X missing: the `if` doesn't hold; the elseif and the else after it
        // are unguarded, so the elseif (the next declaration) is kept.
        let c = one_with(elseif, |_| false);
        assert_eq!(c.methods[0].name, "m");
    }

    #[test]
    fn a_guard_ends_at_endif() {
        let src = r#"<?php
namespace A;
if (class_exists(\B\X::class)):
    class C {}
endif;
class D {}
"#;
        let meta = parse_file_with(src.as_bytes(), |_| true);
        let d = meta.declarations.iter().find(|d| d.fqcn == "A\\D").unwrap();
        assert_eq!(d.guard, None);
    }

    #[test]
    fn minimal_class() {
        let c = one("<?php namespace A\\B; class C {}");
        assert_eq!(c.fqcn, "A\\B\\C");
        assert_eq!(c.kind, ClassKind::Class);
        assert!(c.methods.is_empty());
    }

    #[test]
    fn header_with_use_resolution() {
        let c = one(
            "<?php\nnamespace App;\nuse Magento\\Framework\\App\\ActionInterface as Action;\nuse Vendor\\Base\\{AbstractThing, Other};\n\nfinal class Controller extends AbstractThing implements Action, \\Serializable {}",
        );
        assert!(c.is_final);
        assert_eq!(c.extends, ["Vendor\\Base\\AbstractThing"]);
        assert_eq!(
            c.implements,
            ["Magento\\Framework\\App\\ActionInterface", "Serializable"]
        );
    }

    #[test]
    fn conditional_class_declaration_keeps_if_branch() {
        // A class declared inside an `if (class_exists(…)) { … } else { … }`
        // guard — the Hyva compat-shim / PHP-polyfill pattern. The real
        // implementation lives in the `if` branch; the empty fallback in
        // `else`. We descend into both and keep the FIRST (the real one), with
        // its methods, so it enters the class universe for interception.
        let c = one(
            r#"<?php
namespace Acme\Shim;
use Vendor\Real\Base;
if (class_exists(\Vendor\Real\Base::class)) {
    class Shim extends Base {
        public function afterIsVirtual($subject, $result) { return $result; }
    }
} else {
    class Shim {
    }
}"#,
        );
        assert_eq!(c.fqcn, "Acme\\Shim\\Shim");
        assert_eq!(c.extends, ["Vendor\\Real\\Base"], "the if-branch class, not the stub");
        assert_eq!(c.methods.len(), 1);
        assert_eq!(c.methods[0].name, "afterIsVirtual");
    }

    #[test]
    fn constructor_with_promotion_defaults_and_types() {
        let c = one(
            r#"<?php
namespace A;
use B\Dep;
class C {
    public function __construct(
        private readonly Dep $dep,
        ?\Psr\Log\LoggerInterface $logger = null,
        int|string $mixed = PHP_INT_MAX,
        array $data = ['a' => 1, 'b' => [2, ',)']],
        Dep&\Countable ...$rest
    ) {}
}"#,
        );
        let ctor = &c.methods[0];
        assert_eq!(ctor.name, "__construct");
        let p = &ctor.params;
        assert_eq!(p[0].ty.as_deref(), Some("B\\Dep"));
        assert_eq!(p[0].promoted, Some(Visibility::Private));
        assert!(p[0].readonly);
        assert_eq!(p[1].ty.as_deref(), Some("?Psr\\Log\\LoggerInterface"));
        assert_eq!(p[1].default.as_deref(), Some("null"));
        assert_eq!(p[2].ty.as_deref(), Some("int|string"));
        assert_eq!(p[2].default.as_deref(), Some("PHP_INT_MAX"));
        assert_eq!(p[3].default.as_deref(), Some("['a' => 1, 'b' => [2, ',)']]"));
        assert_eq!(p[4].ty.as_deref(), Some("B\\Dep&Countable"));
        assert!(p[4].variadic);
    }

    #[test]
    fn methods_with_bodies_full_of_mines() {
        let c = one(
            "<?php class C {\n  public function a() { $s = \"{$x[\"k\"]}\"; }\n  protected function b(): string { $h = <<<EOT\n}} fake\nEOT;\n    return '}'; }\n  public static function &c(): static { /* } */ return self::$i; }\n}",
        );
        let names: Vec<_> = c.methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(c.methods[1].visibility, Visibility::Protected);
        assert_eq!(c.methods[1].return_type.as_deref(), Some("string"));
        assert!(c.methods[2].is_static);
        assert!(c.methods[2].returns_ref);
        assert_eq!(c.methods[2].return_type.as_deref(), Some("static"));
    }

    #[test]
    fn interface_and_abstract() {
        let meta = parse_file(
            b"<?php namespace N; interface I extends A, B { public function f(); }\nabstract class K { abstract protected function g(): void; }",
        );
        assert_eq!(meta.declarations.len(), 2);
        let i = &meta.declarations[0];
        assert_eq!(i.kind, ClassKind::Interface);
        assert_eq!(i.extends, ["N\\A", "N\\B"]);
        assert_eq!(i.methods[0].name, "f");
        let k = &meta.declarations[1];
        assert!(k.is_abstract);
        assert!(k.methods[0].is_abstract);
        assert_eq!(k.methods[0].visibility, Visibility::Protected);
    }

    #[test]
    fn traits_with_adaptations() {
        let c = one(
            "<?php namespace N; class C { use A, B { A::hello insteadof B; B::hello as protected howdy; } }",
        );
        assert_eq!(c.traits, ["N\\A", "N\\B"]);
        assert_eq!(c.trait_insteadof[0].trait_fqcn, "N\\A");
        assert_eq!(c.trait_insteadof[0].method, "hello");
        assert_eq!(c.trait_insteadof[0].excluded, ["N\\B"]);
        let alias = &c.trait_aliases[0];
        assert_eq!(alias.trait_fqcn.as_deref(), Some("N\\B"));
        assert_eq!(alias.alias.as_deref(), Some("howdy"));
        assert_eq!(alias.visibility, Some(Visibility::Protected));
    }

    #[test]
    fn constants_typed_and_grouped() {
        let c = one(
            "<?php class C { const A = 'x'; public const int B = 1, D = 2; private const E = [self::A => 1]; }",
        );
        let names: Vec<_> = c.constants.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, ["A", "B", "D", "E"]);
        assert_eq!(c.constants[1].ty.as_deref(), Some("int"));
        assert_eq!(c.constants[1].value, "1");
        assert_eq!(c.constants[3].visibility, Visibility::Private);
        assert_eq!(c.constants[3].value, "[self::A => 1]");
    }

    #[test]
    fn enums() {
        let c = one(
            "<?php namespace N; enum Suit: string implements HasLabel { case Hearts = 'H'; case Spades = 'S'; public function label(): string { return '}'; } }",
        );
        assert_eq!(c.kind, ClassKind::Enum);
        assert_eq!(c.enum_backing.as_deref(), Some("string"));
        assert_eq!(c.cases, ["Hearts", "Spades"]);
        assert_eq!(c.implements, ["N\\HasLabel"]);
        assert_eq!(c.methods[0].name, "label");
    }

    #[test]
    fn properties_are_consumed_including_hooks() {
        let c = one(
            "<?php class C {\n  public int $a = 5;\n  protected array $b = ['x' => '}'], $c = [];\n  public string $hooked { get => strtoupper($this->raw); set { $this->raw = $value; } }\n  public function real() {}\n}",
        );
        assert_eq!(c.methods.len(), 1);
        assert_eq!(c.methods[0].name, "real");
        let props: Vec<_> =
            c.properties.iter().map(|p| (p.name.as_str(), p.default.as_deref(), p.hooked)).collect();
        assert_eq!(
            props,
            [
                ("a", Some("5"), false),
                ("b", Some("['x' => '}']"), false),
                ("c", Some("[]"), false),
                ("hooked", None, true),
            ]
        );
    }

    #[test]
    fn attributes_everywhere() {
        let c = one(
            "<?php\n#[Attr('](', [1,2])]\nclass C {\n  #[Route('/x')]\n  public function f(#[Sensitive] string $p) {}\n}",
        );
        assert_eq!(c.methods[0].name, "f");
        assert_eq!(c.methods[0].params[0].name, "p");
    }

    #[test]
    fn registration_php_yields_nothing() {
        let meta = parse_file(
            b"<?php\nuse Magento\\Framework\\Component\\ComponentRegistrar;\nComponentRegistrar::register(ComponentRegistrar::MODULE, 'Acme_Test', __DIR__);\n",
        );
        assert!(meta.declarations.is_empty());
        assert!(meta.issues.is_empty(), "{:?}", meta.issues);
    }

    #[test]
    fn no_open_tag_is_empty() {
        let meta = parse_file(b"just text, no php");
        assert!(meta.declarations.is_empty());
    }

    #[test]
    fn html_gaps_between_declarations() {
        let meta = parse_file(b"<?php class A {} ?><h1>x</h1><?php class B {}");
        let names: Vec<_> = meta.declarations.iter().map(|d| d.fqcn.as_str()).collect();
        assert_eq!(names, ["A", "B"]);
    }

    #[test]
    fn readonly_class_and_dnf_types() {
        let c = one(
            "<?php namespace N; readonly class C { public function f((A&B)|null $x): (A&B)|C {} }",
        );
        assert!(c.is_readonly);
        assert_eq!(c.methods[0].params[0].ty.as_deref(), Some("(N\\A&N\\B)|null"));
        assert_eq!(c.methods[0].return_type.as_deref(), Some("(N\\A&N\\B)|N\\C"));
    }

    #[test]
    fn bare_readonly_param_promotes_public() {
        let c = one(
            "<?php class C { public function __construct(readonly \\WeakReference $r, readonly int $sort) {} }",
        );
        let p = &c.methods[0].params;
        assert_eq!(p[0].promoted, Some(Visibility::Public));
        assert!(p[0].readonly);
        assert_eq!(p[1].promoted, Some(Visibility::Public));
    }

    #[test]
    fn keyword_named_methods() {
        let c = one("<?php class C { public function list() {} public function use() {} }");
        let names: Vec<_> = c.methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["list", "use"]);
    }

    /// What `var_export` of a `new X()` default needs: every declared property
    /// in source order with its modifiers, type and raw default. Typed vs
    /// untyped is the load-bearing distinction — without a default, an untyped
    /// property is `null` and a typed one is uninitialized.
    #[test]
    fn properties_in_source_order() {
        let c = one(
            r#"<?php
namespace N;
use Other\Thing;
class C {
    public $plain;
    protected ?Thing $typed = null, $second = 2;
    private static array $shared = ['a' => 1, 'b' => [2, 3]];
    var $legacy = 'x';
    public readonly int $ro;
    const K = 1;
    public function __construct() {}
    public int $after = self::K;
}"#,
        );
        let p = &c.properties;
        let names: Vec<_> = p.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["plain", "typed", "second", "shared", "legacy", "ro", "after"]);
        assert_eq!((p[0].visibility, p[0].ty.as_deref(), p[0].default.as_deref()), (Visibility::Public, None, None));
        assert_eq!(p[1].ty.as_deref(), Some("?Other\\Thing"));
        assert_eq!(p[1].default.as_deref(), Some("null"));
        // A grouped declaration shares its modifiers and type.
        assert_eq!((p[2].visibility, p[2].ty.as_deref(), p[2].default.as_deref()),
                   (Visibility::Protected, Some("?Other\\Thing"), Some("2")));
        assert!(p[3].is_static);
        assert_eq!(p[3].default.as_deref(), Some("['a' => 1, 'b' => [2, 3]]"));
        assert_eq!((p[4].visibility, p[4].default.as_deref()), (Visibility::Public, Some("'x'")));
        assert!(p[5].is_readonly && p[5].default.is_none());
        assert_eq!(p[6].default.as_deref(), Some("self::K"));
        assert!(!p.iter().any(|p| p.hooked));
        // Members around the properties are unaffected.
        assert_eq!(c.constants.len(), 1);
        assert_eq!(c.methods.len(), 1);
    }

    /// Promoted constructor params stay on the constructor; they are not
    /// duplicated as declared properties.
    #[test]
    fn promoted_params_are_not_properties() {
        let c = one("<?php class C { public $a; public function __construct(private int $b) {} }");
        assert_eq!(c.properties.len(), 1);
        assert_eq!(c.properties[0].name, "a");
    }

    #[test]
    fn readonly_class_makes_properties_readonly() {
        let c = one("<?php readonly class C { public int $a; }");
        assert!(c.properties[0].is_readonly);
    }

    /// 8.4 hooks are skipped like a body; the property is flagged, its default
    /// kept, and the members after it still parse.
    #[test]
    fn hooked_properties_are_flagged() {
        let c = one(
            r#"<?php class C {
    public string $v { get => strtoupper($this->b); }
    public string $backed = 'x' { set => trim($value); }
    public private(set) int $asym = 1;
    public function f() {}
}"#,
        );
        let p = &c.properties;
        assert_eq!(p.len(), 3);
        assert!(p[0].hooked && p[0].default.is_none());
        assert!(p[1].hooked);
        assert_eq!(p[1].default.as_deref(), Some("'x'"));
        assert!(!p[2].hooked);
        assert_eq!(p[2].default.as_deref(), Some("1"));
        assert_eq!(c.methods[0].name, "f");
    }

    #[test]
    fn unknown_construct_is_an_issue_not_a_guess() {
        let meta = parse_file(b"<?php class C { @@@ public function f() {} }");
        assert!(!meta.issues.is_empty());
        // Recovery still finds the method.
        assert_eq!(meta.declarations[0].methods[0].name, "f");
    }
}
