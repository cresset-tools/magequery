//! What a DI compiler needs to know about a PHP file: declarations and
//! signatures. Method bodies never appear here — they are skipped, not parsed.

/// Kind of a PHP type declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClassKind {
    Class,
    Interface,
    Trait,
    Enum,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Visibility {
    Public,
    Protected,
    Private,
}

/// Everything extracted from one PHP source file.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct FileMeta {
    /// Type declarations, in source order. Usually one; PHP allows several.
    pub declarations: Vec<ClassMeta>,
    /// Constructs the parser could not confidently classify. There is no
    /// fallback tier: the caller decides whether an issue is fatal.
    pub issues: Vec<ParseIssue>,
}

/// One `class` / `interface` / `trait` / `enum` declaration.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ClassMeta {
    pub kind: ClassKind,
    /// Fully qualified name, no leading backslash.
    pub fqcn: String,
    pub is_abstract: bool,
    pub is_final: bool,
    pub is_readonly: bool,
    /// FQCNs. A class has at most one; an interface may extend several.
    pub extends: Vec<String>,
    pub implements: Vec<String>,
    /// FQCNs of used traits, in source order.
    pub traits: Vec<String>,
    /// `A::m insteadof B[, C]` adaptations.
    pub trait_insteadof: Vec<TraitInsteadof>,
    /// `A::m as [visibility] [alias]` adaptations.
    pub trait_aliases: Vec<TraitAlias>,
    pub constants: Vec<ConstMeta>,
    /// Enum case names (empty for non-enums).
    pub cases: Vec<String>,
    /// The enum backing type (`enum X: string`), when declared.
    pub enum_backing: Option<String>,
    pub methods: Vec<MethodMeta>,
    /// Class-level `#[Attribute]` names, resolved to FQCNs.
    pub attributes: Vec<String>,
    /// Byte offset of the declaration keyword (for diagnostics).
    pub offset: usize,
    /// The file's `use` imports in effect for this declaration:
    /// `(lowercased alias, FQCN)`. Needed to resolve names inside raw
    /// constant expressions (defaults, const values) after parsing.
    pub uses: Vec<(String, String)>,
}

/// `use A, B { A::m insteadof B; }` — A's `m` wins; B (and any further
/// excluded traits) lose theirs.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct TraitInsteadof {
    /// FQCN of the winning trait.
    pub trait_fqcn: String,
    pub method: String,
    /// FQCNs of the traits whose `method` is excluded.
    pub excluded: Vec<String>,
}

/// `use T { [T::]m as [visibility] [alias]; }`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct TraitAlias {
    /// FQCN when the source trait is named explicitly; `None` when the
    /// method reference is unqualified (`m as n`).
    pub trait_fqcn: Option<String>,
    pub method: String,
    /// `None` for pure visibility changes (`m as protected;`).
    pub alias: Option<String>,
    pub visibility: Option<Visibility>,
}

/// A class constant. The value is the raw constant-expression text as
/// written (whitespace collapsed) — evaluation is the const-expression
/// evaluator's job, not the parser's.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ConstMeta {
    pub name: String,
    pub visibility: Visibility,
    /// Declared type for typed constants (8.3+), resolved.
    pub ty: Option<String>,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct MethodMeta {
    pub name: String,
    pub visibility: Visibility,
    pub is_static: bool,
    pub is_abstract: bool,
    pub is_final: bool,
    /// `function &foo()` — returns by reference.
    pub returns_ref: bool,
    pub params: Vec<ParamMeta>,
    /// Resolved return type expression (`?A\B`, `X|Y`, `static`, …).
    pub return_type: Option<String>,
    /// Byte offset of the `function` keyword.
    pub offset: usize,
}

impl ParamMeta {
    /// A synthetic parameter — for stub tables of internal PHP constructors
    /// (reflection knows them; no source file exists to parse).
    pub fn synthetic(name: &str, ty: Option<&str>, default: Option<&str>) -> ParamMeta {
        ParamMeta {
            name: name.to_owned(),
            ty: ty.map(str::to_owned),
            default: default.map(str::to_owned),
            by_ref: false,
            variadic: false,
            promoted: None,
            readonly: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ParamMeta {
    /// Without the `$`.
    pub name: String,
    /// Resolved type expression; `None` for untyped params.
    pub ty: Option<String>,
    /// Raw default-value expression text, whitespace collapsed.
    pub default: Option<String>,
    pub by_ref: bool,
    pub variadic: bool,
    /// Constructor property promotion: the promoted visibility.
    pub promoted: Option<Visibility>,
    pub readonly: bool,
}

/// A construct the parser could not confidently classify — a hard
/// diagnostic naming the position, never a silent guess.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ParseIssue {
    /// Byte offset into the source.
    pub offset: usize,
    pub message: String,
}
