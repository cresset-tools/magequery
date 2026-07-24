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
    /// less.php number printing (`php_number_format`): float-multiply `fround`
    /// plus PHP `%.16G` (`Less_Parser` sets `precision=16`) instead of less.js
    /// `toFixed` + shortest-round-trip. A literal `66.6%` prints as
    /// `66.59999999999999%` under it — real, deployed bytes.
    pub php_number_format: bool,
    /// Evaluate math INSIDE `calc(…)`. less.js only special-cased `calc` in
    /// 3.0 (preserving its interior); before that — less.js 2.5.3 / less.php
    /// 3.x — `calc` is an ordinary function whose arguments evaluate under the
    /// active math mode, so `calc(100% - 40px + 10px)` folds to
    /// `calc(100% - 50px)`. `false` for the modern profiles; `true` for
    /// [`magento_247`](Self::magento_247).
    pub php_calc_interior_math: bool,
    /// Hoist `@charset` to the top of the output (less.js 3.x `visitAtRule`
    /// behavior, less.php 5.x). less.php 3.x / less.js 2.5.3 does NOT — a
    /// `@charset` inside an imported file stays where it appears. `true` for
    /// the modern profiles; `false` for [`magento_247`](Self::magento_247).
    pub hoist_charset: bool,
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
    /// less.php compress zero-length units (§C4, source-read v5.5 + probed):
    /// `Dimension::genCSS` guards the "zero drops its unit" branch with a
    /// STRICT `$value === 0`, but the constructor stores `floatval($value)` —
    /// the float `0.0` never matches the int `0`, so less.php keeps the unit
    /// under compress (`margin-top:0rem`, `top:0px`) where less.js emits `0`.
    /// Blank/Luma-real: compressed styles-m carries `0rem`/`0px` from
    /// `.lib-font-size-value(0)` and literal `0px`. On in Magento profiles.
    pub php_zero_units: bool,
    /// less.php synchronous import sequencing (source-read v5.5.1,
    /// `Less_ImportVisitor::processImportNode`): a fetched file's subtree is
    /// visited IMMEDIATELY (`$this->visitObj($root)`) — depth-first — so a
    /// NESTED duplicate import claims the once-slot before a later import in
    /// the outer file. less.js's async ImportSequencer instead registers all
    /// of a file's own imports before any fetched subtree is visited (BFS),
    /// handing the OUTER import the slot. Backend-real:
    /// `components/_calendar-temp.less` is imported both from
    /// `source/_components.less` (nested) and directly by `styles.less`; the
    /// real SCD css emits the block at the NESTED (earlier) position — and
    /// the datepicker `:extend` selectors graft in that earlier order too.
    /// On in Magento profiles.
    pub php_import_order: bool,
    /// less.php interpolated-selector semantics (source-read v5.5.1): only
    /// declaration VALUES are ever re-parsed (`Ruleset::transformDeclaration`)
    /// — an interpolated selector stays ONE selector even when the value
    /// carries commas, printing as a single `a, b, c { … }` line. less.js
    /// 3.13 re-parses evaluated selectors (`Ruleset.prototype.eval` →
    /// `parseNode(…, ["selectors"])`), splitting the comma list into a real
    /// selector GROUP (one per line). Backend-real: `_grid-framework.less`
    /// builds `~'.col-xs-1, .col-m-1, …'` lists and opens `@{list} { … }`.
    /// On in Magento profiles.
    pub php_selector_interpolation: bool,
    /// less.php compresses combinators INSIDE a functional-selector pseudo —
    /// `:not()`/`:is()`/`:where()`/`:has()` — because it parses the argument as
    /// a selector list, so ` > ` → `>` there just like at the top level. less.js
    /// 3.13 captures the pseudo's parenthetical as a raw element value and
    /// leaves its spaces intact under compression. Only compressed output
    /// differs; `:nth-child(2n + 1)` and attribute values stay literal under
    /// both (not selector lists). Backend-real: `styles.less` emits
    /// `…-link > a.option-title` inside a `:not(…)`, which the SCD css
    /// compresses to `…-link>a.option-title`.
    /// On in Magento profiles.
    pub php_selector_paren_combinators: bool,
    /// less.php widens the `@import` CSS-passthrough test (source-read v5.5.1,
    /// `Less_Tree_Import::__construct`): a path is left as a literal `@import`
    /// (never fetched) when EITHER `/[#.&?\/]css([?;].*)?$/` matches — note the
    /// **forward slash** added to less.js's `/[#.&?]css([?;].*)?$/` class, so a
    /// URL segment like `…/css?family=…` counts — OR the path is a remote
    /// resource, `/^(https?:)?\/\//i` (`http://`, `https://`, or protocol-
    /// relative `//host`), regardless of extension. less.js has neither rule:
    /// it would try to FETCH `@import url('https://fonts.googleapis.com/css?…')`
    /// as a `.less` file and fail. Real Magento themes (e.g. Yotpo_Reviews'
    /// `_module.less`) import Google-Fonts URLs this way, expecting them to
    /// survive into the compiled CSS. On in Magento profiles.
    pub php_css_url_passthrough: bool,
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
            php_number_format: false,
            filename: None,
            magento_mode: false,
            php_float_shim: false,
            php_encoding_shim: false,
            php_interp_rounding: false,
            php_reference_visibility: false,
            php_zero_units: false,
            php_import_order: false,
            php_selector_interpolation: false,
            php_selector_paren_combinators: false,
            php_css_url_passthrough: false,
            php_calc_interior_math: false,
            hoist_charset: true,
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
            php_zero_units: true,
            php_number_format: true,
            php_import_order: true,
            php_selector_interpolation: true,
            php_selector_paren_combinators: true,
            php_css_url_passthrough: true,
            ..LessOptions::default()
        }
    }

    /// Magento on **`wikimedia/less.php` 3.x** (a port of less.js 2.5.3), used
    /// by 2.4.7 and earlier stores. The one behavioral divergence from
    /// [`magento_production`](Self::magento_production) (less.php 5.x /
    /// less.js 3.13) is the math mode: 2.5.3 is `math=always`, so `*` and `/`
    /// evaluate without parentheses. That is exactly what makes Magento's
    /// `_typography.less` — `unit(@root__font-size * 16/100)`, unparenthesized
    /// — compile at all; under parens-division it throws, and the theme's whole
    /// `styles-m.css` silently fails to deploy. Every other formatting rule
    /// Also gated: less.js 2.5.3 has no `@charset`-hoisting visitor (a
    /// `@charset` stays where it appears) and no `calc()` special-casing (its
    /// interior evaluates as plain arithmetic that ignores unit compatibility,
    /// `calc(100% - 40px + 10px)` -> `calc(70%)`).
    ///
    /// KNOWN residuals (not yet modeled — cosmetic, visually identical, so a
    /// deploy is ~99.9% byte-identical with these confined to third-party
    /// module CSS): under 2.5.3 COMPRESS a named color and an `hsl()`/`rgb()`
    /// value print as computed hex (`white` -> `#fff`), and a source dimension
    /// keeps its original spelling (`-.5em` is NOT normalized to `-0.5em`).
    /// These are distinct output paths (Keyword, Color, Dimension) whose modern
    /// behavior is byte-load-bearing for the 2.4.8 profile, so they are left
    /// for a separate, carefully-gated change.
    pub fn magento_247() -> Self {
        LessOptions {
            profile: CompatProfile::Magento247,
            math: MathMode::Always,
            // less.js 2.5.3 has no `@charset` hoisting visitor — it stays where
            // it appears (verified against less.php 3.2.1).
            hoist_charset: false,
            // less.js 2.5.3 has no calc special-casing — interior math folds.
            php_calc_interior_math: true,
            ..LessOptions::magento_production()
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
            php_zero_units: true,
            php_number_format: true,
            php_import_order: true,
            php_selector_interpolation: true,
            php_selector_paren_combinators: true,
            php_css_url_passthrough: true,
            ..LessOptions::default()
        }
    }
}
