//! Structural PHP parser for magecommand.
//!
//! A DI compiler needs declarations, never method bodies. This crate parses
//! exactly that subset: namespace + use imports, class/interface/trait/enum
//! headers, constructor parameters (incl. promoted properties), method
//! signatures, trait uses + adaptations, and class constants. Bodies are
//! *skipped*, not parsed — `memchr`-accelerated scanning past string/comment/
//! heredoc interiors, with a scalar recursive-descent over the structural
//! remainder.
//!
//! Design contract (CAS prerequisite): [`parse_file`] is a pure function of
//! the input bytes. No filesystem access, no environment, no PHP execution —
//! ever. A construct the parser cannot classify is a hard [`ParseIssue`],
//! never a silent guess; there is no fallback tier behind it.

pub mod constexpr;
mod model;
mod parse;
mod scan;

pub use model::*;
pub use parse::parse_file;

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
    fn properties_are_skipped_including_hooks() {
        let c = one(
            "<?php class C {\n  public int $a = 5;\n  protected array $b = ['x' => '}'], $c = [];\n  public string $hooked { get => strtoupper($this->raw); set { $this->raw = $value; } }\n  public function real() {}\n}",
        );
        assert_eq!(c.methods.len(), 1);
        assert_eq!(c.methods[0].name, "real");
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

    #[test]
    fn unknown_construct_is_an_issue_not_a_guess() {
        let meta = parse_file(b"<?php class C { @@@ public function f() {} }");
        assert!(!meta.issues.is_empty());
        // Recovery still finds the method.
        assert_eq!(meta.declarations[0].methods[0].name, "f");
    }
}
