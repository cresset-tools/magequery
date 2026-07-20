//! Compiler options and compat profiles (plan §1, §2.0, §9.5).
//!
//! One engine with less.js-4.x semantics; behavior is driven by named compat
//! profiles and a first-class `math` option rather than by forking the engine.

use std::path::PathBuf;

/// Math evaluation mode (plan §1/§2.0). `strict-legacy` (value 3) was removed in
/// less.js 4.0 and is intentionally absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MathMode {
    /// `always` (=0): eager — every `+ - * /` divides/operates.
    Always,
    /// `parens-division` (=1, default): `/` needs parens; `+ - *` do not.
    ParensDivision,
    /// `parens` (=2, == strict): every op needs parens.
    Parens,
}

impl Default for MathMode {
    fn default() -> Self {
        MathMode::ParensDivision
    }
}

/// `rewriteUrls` mode for generated `url()`/`@import` paths (plan §2.0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteUrls {
    /// `Off` (default): don't rewrite.
    Off,
    /// `All`: rewrite every url.
    All,
    /// `Local`: rewrite only relative (`./`, `../`) urls.
    Local,
}

impl Default for RewriteUrls {
    fn default() -> Self {
        RewriteUrls::Off
    }
}

/// Named compatibility profile (plan §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatProfile {
    /// less.js 4.x default: parens-division, full 4.x function set. The suite
    /// runs under this (per-fixture config overrides `math` etc.).
    LessJs,
    /// Magento 2.4.8 (less.php 5.5): parens-division, `compress=false`,
    /// `relativeUrls=false`, JS-eval OFF, `calc()` preserved.
    Magento248,
    /// Magento 2.4.7 legacy stores (less.php 3.x): `math=always` + `calc()`
    /// interior math ON. The only profile that intentionally diverges from
    /// less.js. Selected by reading the target's `wikimedia/less.php` version.
    Magento247,
}

impl Default for CompatProfile {
    fn default() -> Self {
        CompatProfile::LessJs
    }
}

/// A custom function callable from LESS (plan §2.7): already-evaluated
/// arguments in, `Some(result)` out — or `None` to fall through to the
/// unknown-function passthrough.
pub type CustomFunction = fn(&[crate::ast::Node]) -> Option<crate::ast::Node>;

/// The full option set the Rust API accepts (plan §5.4/§9.5). Field names and
/// defaults mirror less.js `default-options.js`.
#[derive(Debug, Clone)]
pub struct LessOptions {
    /// Named compat profile this option set was built from.
    pub profile: CompatProfile,
    /// Math mode (plan §1/§2.0).
    pub math: MathMode,
    /// `strictUnits` (default false): true throws on dimensionally-invalid ops.
    pub strict_units: bool,
    /// `strictImports` (default false).
    pub strict_imports: bool,
    /// `compress` — the crate's own compress serializer. Magento profiles keep
    /// this `false`; `.min.css` is a separate downstream step (plan §1/§9.7).
    pub compress: bool,
    /// `ieCompat` (default true).
    pub ie_compat: bool,
    /// `javascriptEnabled` (default false). Inline JS is parse-and-reject (§8).
    pub javascript_enabled: bool,
    /// `insecure` (default false).
    pub insecure: bool,
    /// `rewriteUrls` (default Off).
    pub rewrite_urls: RewriteUrls,
    /// `relativeUrls` (default false here; Magento sets false ⇒ don't rewrite).
    pub relative_urls: bool,
    /// `rootpath` prepended to generated `@import`/`url()`.
    pub rootpath: Option<String>,
    /// `urlArgs` appended to every rewritten url.
    pub url_args: Option<String>,
    /// `paths` — import search roots.
    pub paths: Vec<PathBuf>,
    /// `globalVars` — prepended ruleset `(name, value)`.
    pub global_vars: Vec<(String, String)>,
    /// `modifyVars` — appended ruleset `(name, value)`.
    pub modify_vars: Vec<(String, String)>,
    /// `banner` prepended to output.
    pub banner: Option<String>,
    /// `dumpLineNumbers` — OUT (§8); parse-affecting, folded into the cache key.
    pub dump_line_numbers: bool,
    /// `sourceMap` — OUT (§8); scoped out for a headless compiler.
    pub source_map: bool,
    /// Whether `@import` resolution runs (default true).
    pub process_imports: bool,
    /// `numPrecision` (default 8).
    pub num_precision: u8,
    /// Entry-point filename (for provenance/error rendering).
    pub filename: Option<String>,
    /// Magento mode: the lexer recognizes `//@magento_import` as a directive
    /// node rather than a stripped comment (plan §7.1).
    pub magento_mode: bool,
    /// Profile-gated PHP-float shim (off by default) — diagnostic only (§3-C).
    pub php_float_shim: bool,
    /// less.php `(reference)` visibility semantics (§3, probed v5.5.1):
    /// mixin-call output is visible when the mixin is DEFINED in a
    /// non-reference file, even when the call site sits inside a
    /// `(reference)` import (less.js hides it — visibility follows the call
    /// context). Luma-real: `_email-base.less` is reference-imported and
    /// calls the visibly-defined `.lib-typography-all()` at top level; the
    /// real SCD email.css contains that output. On in Magento profiles.
    pub php_reference_visibility: bool,
    /// Profile-gated PHP-encoding shim (off by default) — diagnostic only (§3-G).
    pub php_encoding_shim: bool,
    /// less.php `@{}` interpolation rounding (§3, probed v5.5.1): less.php's
    /// `Quoted` compile renders an interpolated value with `toCSS($env)` — the
    /// env carries `numPrecision`, so an interpolated dimension prints rounded
    /// to 8 decimals (`~"@{v}"` of `1.428571429` → `1.42857143`). less.js's
    /// `Quoted.eval` calls `toCSS()` with NO context, so no fround happens and
    /// the full digits print. Blank/Luma-real: `_forms.less` interpolates
    /// `@{@{_type}__line-height}` (base `1.428571429`) — the real SCD output
    /// prints `1.42857143`. On in Magento profiles. The quoted-string case is
    /// the probed one; selector/property interpolation follows the same
    /// `toCSS($env)` mechanism and is rounded under the flag too (no corpus
    /// construct exercises it).
    pub php_interp_rounding: bool,
    /// Registered custom functions (the less.js `functionRegistry.add`
    /// surface, minimal form): `(lowercased name, fn)` pairs consulted before
    /// the built-in registry. `None` from the fn = not handled → the unknown-
    /// function passthrough re-emits the call (plan §2.7). Test harnesses use
    /// this for less.js's runner-registered `add`/`increment`/`_color`.
    pub custom_functions: Vec<(String, CustomFunction)>,
    /// Eval-depth cap for mixin/detached-ruleset/import recursion (plan §2.5)
    /// — the runaway-input guard. `None` = the engine default (128). less.js
    /// has no explicit cap (a runaway dies on the JS call stack); we error
    /// cleanly ("mixin recursion limit exceeded") instead of recursing until
    /// the native stack or memory is exhausted.
    pub max_eval_depth: Option<usize>,
}

impl Default for LessOptions {
    /// The `less-js` profile — matches less.js 4.x defaults (plan §9.5).
    fn default() -> Self {
        LessOptions {
            profile: CompatProfile::LessJs,
            math: MathMode::ParensDivision,
            strict_units: false,
            strict_imports: false,
            compress: false,
            ie_compat: true,
            javascript_enabled: false,
            insecure: false,
            rewrite_urls: RewriteUrls::Off,
            relative_urls: false,
            rootpath: None,
            url_args: None,
            paths: Vec::new(),
            global_vars: Vec::new(),
            modify_vars: Vec::new(),
            banner: None,
            custom_functions: Vec::new(),
            dump_line_numbers: false,
            source_map: false,
            process_imports: true,
            num_precision: 8,
            filename: None,
            magento_mode: false,
            php_float_shim: false,
            php_encoding_shim: false,
            php_interp_rounding: false,
            php_reference_visibility: false,
            max_eval_depth: None,
        }
    }
}

impl LessOptions {
    /// The `less-js` profile (== [`LessOptions::default`]).
    pub fn less_js() -> Self {
        LessOptions::default()
    }

    /// Magento production deploy: the `magento-2.4.8` profile (plan §1). Emits
    /// the non-compressed `.css` the LESS pass produces; `.min.css` is downstream.
    pub fn magento_production() -> Self {
        LessOptions {
            profile: CompatProfile::Magento248,
            math: MathMode::ParensDivision,
            compress: false,
            relative_urls: false,
            rewrite_urls: RewriteUrls::Off,
            javascript_enabled: false,
            magento_mode: true,
            php_interp_rounding: true,
            php_reference_visibility: true,
            ..LessOptions::default()
        }
    }

    /// Magento developer deploy. Source maps are an explicit non-goal (§8/§E5),
    /// so this is the production config with `magento_mode`; developer-mode
    /// source-map output is not byte-supported.
    pub fn magento_developer() -> Self {
        LessOptions {
            profile: CompatProfile::Magento248,
            magento_mode: true,
            php_interp_rounding: true,
            php_reference_visibility: true,
            ..LessOptions::default()
        }
    }
}
