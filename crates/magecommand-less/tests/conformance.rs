//! §7.4 conformance triad — the Magento-correctness core, exercised on small
//! SYNTHETIC fixtures that mirror the real theme shapes (written fresh for
//! this suite; no Magento sources vendored):
//!
//! - **G-ref**: `(reference)`-import visibility × `:extend(.abs-… all)` —
//!   the `_extends.less` pattern: an `.abs-*` library imported `(reference)`
//!   must emit NOTHING bare; extended selectors are grafted (nested rules and
//!   `& when (@media-common = true)` wrappers included).
//! - **G-resp**: the `.media-width` collector — every module adds guarded
//!   `.media-width(@extremum, @break)` DEFINITIONS; `_responsive.less` calls
//!   the mixin inside literal `@media` blocks at the end of the entry, so all
//!   matching bodies group under ONE `@media` block, with unit-aware guard
//!   equality (`@break = @screen__m` resolving 768px) and the
//!   `@media-common: false` / `@media-target` suppression switches
//!   (redeclared AFTER the imports — last-wins across imports, the X1
//!   whole-scope semantics).
//! - **G-detached**: `@dr()` replay — declaration-scope resolution wins over
//!   a caller-scope shadow, and forward refs within the defining file work.
//!
//! Plus the §7.8 load-bearing pattern: **`.lib-css(@prop, @val)` skips output
//! when `@val` is `false`** (keyword-false guard equality), when it is `''`,
//! and when ANY of the first five list members is `false` (`extract` guards).
//!
//! Every expected output below is **byte-pinned against a live less.js 4.6.7
//! probe** (2026-07, scratchpad `lessprobe/triad`) — the suite gates our
//! compiler against ground truth, ahead of the Tier-2 SCD oracle diff.

use magecommand_less::{
    NoopResolver,
    FileInfo, ImportError, ImportPayload, ImportRequest, ImportResolver, LessOptions,
    ResolvedImport,
};

/// In-memory `path -> content` resolver.
struct MapResolver(Vec<(&'static str, &'static str)>);

impl ImportResolver for MapResolver {
    fn resolve(&self, req: &ImportRequest) -> Result<ResolvedImport, ImportError> {
        let raw = req.path.as_str();
        let key = if raw.ends_with(".less") || raw.ends_with(".css") {
            raw.to_string()
        } else {
            format!("{raw}.less")
        };
        let Some((_, content)) = self.0.iter().find(|(p, _)| *p == key) else {
            return Err(ImportError::NotFound(key));
        };
        Ok(ResolvedImport {
            file: FileInfo {
                filename: key.clone(),
                current_directory: String::new(),
                ..Default::default()
            },
            payload: ImportPayload::Less(std::sync::Arc::from(*content)),
        })
    }
}

fn compile_with(files: &[(&'static str, &'static str)], src: &str) -> String {
    let resolver = MapResolver(files.to_vec());
    magecommand_less::compile(src, &LessOptions::default(), &resolver)
        .expect("conformance fixture must compile")
        .code
}

/// The `_extends.less`-shaped `(reference)` library: a `& when
/// (@media-common = true)` wrapped abs rule with a nested child, a plain abs
/// rule with nested structure, and one that nothing extends.
const EXTENDS: &str = "\
& when (@media-common = true) {
    .abs-reset-list {
        margin: 0;
        padding: 0;
        > li {
            margin: 0;
        }
    }
}
.abs-discount-block {
    .actions-toolbar {
        .action.primary {
            border: 1px dashed;
        }
    }
}
.abs-unused {
    color: hotpink;
}
";

const MODULE_A: &str = "\
& when (@media-common = true) {
    .cart-summary .checkout-methods:extend(.abs-reset-list all) {
        background: white;
    }
}
.media-width(@extremum, @break) when (@extremum = 'min') and (@break = @screen__m) {
    .mod-a-wide { width: 50%; }
}
";

const MODULE_B: &str = "\
.block-discount:extend(.abs-discount-block all) {
    display: block;
}
.media-width(@extremum, @break) when (@extremum = 'min') and (@break = @screen__m) {
    .mod-b-wide { float: left; }
}
.media-width(@extremum, @break) when (@extremum = 'max') and (@break = @screen__s) {
    .mod-b-narrow { float: none; }
}
";

/// The collector, mirroring `lib/_responsive.less`'s shape: literal `@media`
/// blocks calling `.media-width(...)`, gated on `@media-target`.
const RESPONSIVE: &str = "\
@media-common: true;
@media-target: 'all';
& when (@media-target = 'mobile'), (@media-target = 'all') {
    @media only screen and (max-width: (@screen__s - 1)) {
        .media-width('max', @screen__s);
    }
    @media all and (min-width: @screen__s) {
        .media-width('min', @screen__s);
    }
}
& when (@media-target = 'desktop'), (@media-target = 'all') {
    @media all and (min-width: @screen__m),
    print {
        .media-width('min', @screen__m);
    }
}
";

const THEME: &str = "\
@screen__s: 640px;
@screen__m: 768px;
";

fn theme_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("_theme.less", THEME),
        ("_extends.less", EXTENDS),
        ("_module-a.less", MODULE_A),
        ("_module-b.less", MODULE_B),
        ("_responsive.less", RESPONSIVE),
    ]
}

const ENTRY: &str = "\
@import '_theme.less';
@import (reference) '_extends.less';
@import '_module-a.less';
@import '_module-b.less';
@import '_responsive.less';
";

/// G-ref + G-resp, the `styles-m` shape: common styles on, mobile target
/// (declared after the imports — last-wins across imports).
///
/// Pinned against less.js 4.6.7. What it locks:
/// - extended `.abs-reset-list` grafts onto the consumer, its `& when` fold,
///   nested `> li`, and the `.abs-discount-block` nested graft;
/// - `.abs-unused` (extended by nothing) and every other bare `.abs-*` rule
///   emit NOTHING;
/// - the mobile `min-width: 640px` collector block has no matching
///   definitions and is pruned entirely;
/// - the `max-width: 639px` block collects the one matching body, with the
///   guard `(@break = @screen__s)` resolving 640px = 640px unit-aware;
/// - the desktop group is suppressed by `@media-target: 'mobile'`.
///
/// ONE deliberate byte deviation (D-fold-join): for the extend-grafted copy
/// of a `& when`-folded hidden rule, less.js joins the folded declarations
/// on one line (`margin: 0;padding: 0;`); we emit them on separate lines.
/// Semantically identical — and the REAL Magento oracle (less.php, checked
/// against the reference install's SCD output) contains ZERO such joins, so
/// our formatting matches the Tier-2 contract better than less.js does here.
#[test]
fn g_ref_and_g_resp_mobile_entry() {
    let src = format!("{ENTRY}@media-target: 'mobile';\n");
    let css = compile_with(&theme_files(), &src);
    assert_eq!(
        css,
        "\
.cart-summary .checkout-methods {
  margin: 0;
  padding: 0;
}
.cart-summary .checkout-methods > li {
  margin: 0;
}
.block-discount .actions-toolbar .action.primary {
  border: 1px dashed;
}
.cart-summary .checkout-methods {
  background: white;
}
.block-discount {
  display: block;
}
@media only screen and (max-width: 639px) {
  .mod-b-narrow {
    float: none;
  }
}
"
    );
    // The G-ref grep assertion, explicit: zero `.abs-` anywhere in output.
    assert!(!css.contains(".abs-"), "bare .abs- leaked:\n{css}");
}

/// G-resp, the `styles-l` shape: `@media-common: false` suppresses every
/// `& when (@media-common = true)` block (module commons AND the reference
/// library's), while the desktop collector groups BOTH modules' matching
/// `.media-width('min', @screen__m)` bodies under ONE
/// `@media all and (min-width: 768px), print` block, in module splice order.
///
/// Byte-pinned against less.js 4.6.7.
#[test]
fn g_resp_desktop_entry_media_common_false() {
    let src = format!("{ENTRY}@media-common: false;\n@media-target: 'desktop';\n");
    let css = compile_with(&theme_files(), &src);
    assert_eq!(
        css,
        "\
.block-discount .actions-toolbar .action.primary {
  border: 1px dashed;
}
.block-discount {
  display: block;
}
@media all and (min-width: 768px), print {
  .mod-a-wide {
    width: 50%;
  }
  .mod-b-wide {
    float: left;
  }
}
"
    );
    assert!(!css.contains(".abs-"), "bare .abs- leaked:\n{css}");
    // Exactly one @media block: the collector grouped, not duplicated.
    assert_eq!(css.matches("@media").count(), 1, "collector split:\n{css}");
}

/// G-detached: `@dr()` replay resolves in the DECLARATION scope — a
/// caller-scope `@badge-color: red` shadow does NOT win over the visible
/// `blue`, and `@dr-outline` (declared AFTER the ruleset in the defining
/// file) resolves forward. Byte-pinned against less.js 4.6.7.
#[test]
fn g_detached_replay_dual_scope() {
    let files = vec![(
        "_dr-lib.less",
        "@dr-badge: {\n    color: @badge-color;\n    outline: @dr-outline;\n};\n@dr-outline: 1px dotted;\n",
    )];
    let css = compile_with(
        &files,
        "\
@import '_dr-lib.less';
@badge-color: blue;
.a {
    @badge-color: red;
    @dr-badge();
}
.b {
    @dr-badge();
}
",
    );
    assert_eq!(
        css,
        "\
.a {
  color: blue;
  outline: 1px dotted;
}
.b {
  color: blue;
  outline: 1px dotted;
}
"
    );
}

/// §7.8: `.lib-css(@prop, @val)` — the pervasive "unset = false" idiom. The
/// mixin definition mirrors the real library's guard shape (keyword-`false`
/// equality, `''`, and `extract(@_value, 1..5) = false` list checks, plus the
/// `@_prefix: 1` `-webkit-` fan-out with interpolated `@{_property}`).
/// Byte-pinned against less.js 4.6.7: only `margin` and the prefixed
/// `transition` emit; `false`, a list containing `false`, `''`, and a
/// variable holding `false` all skip.
#[test]
fn lib_css_skips_false_values() {
    let css = compile_with(
        &[],
        "\
.lib-css(
    @_property,
    @_value,
    @_prefix: 0
) when (@_prefix = 1)
  and not (@_value = '')
  and not (@_value = false)
  and not (extract(@_value, 1) = false)
  and not (extract(@_value, 2) = false)
  and not (extract(@_value, 3) = false)
  and not (extract(@_value, 4) = false)
  and not (extract(@_value, 5) = false) {
  -webkit-@{_property}: @_value;
}
.lib-css(
    @_property,
    @_value,
    @_prefix: 0
) when not (@_value = '')
  and not (@_value = false)
  and not (extract(@_value, 1) = false)
  and not (extract(@_value, 2) = false)
  and not (extract(@_value, 3) = false)
  and not (extract(@_value, 4) = false)
  and not (extract(@_value, 5) = false) {
    @{_property}: @_value;
}
@no: false;
.card {
    .lib-css(margin, 10px 20px);
    .lib-css(padding, false);
    .lib-css(border, 1px solid false);
    .lib-css(color, '');
    .lib-css(background, @no);
    .lib-css(transition, opacity 1s, 1);
}
",
    );
    assert_eq!(
        css,
        "\
.card {
  margin: 10px 20px;
  -webkit-transition: opacity 1s;
  transition: opacity 1s;
}
"
    );
}

/// §3 profile question, probed 2026-07 against BOTH ground truths: a mixin
/// DEFINED in a visible (non-reference) file but CALLED at the top level of a
/// `(reference)`-imported file.
///
/// - less.js 4.6.7: output hidden — visibility follows the CALL context.
/// - wikimedia/less.php 5.5.1 (the Magento oracle): output VISIBLE —
///   visibility follows the DEFINITION's file (`Mixin_Call::compile` only
///   skips `markReferenced` when the call site itself is in a reference
///   file, so rules parsed in a visible file keep their visibility).
///
/// Luma-real: `email.less` reference-imports `_email-base.less`, which calls
/// the visibly-defined `.lib-typography-all()` at top level — the real SCD
/// email.css contains that typography output (52 rules). Gated by
/// `LessOptions::php_reference_visibility` (on in Magento profiles only).
/// The fourth quadrant (defined AND called inside reference files) stays
/// hidden in both engines, also probed.
#[test]
fn php_reference_visibility_definition_based() {
    let files: Vec<(&'static str, &'static str)> = vec![
        ("vis.less", ".tools() {\n  .t { color: red; }\n}\n"),
        (
            "reffile.less",
            ".tools();\n.selfmix() {\n  .s { color: blue; }\n}\n.selfmix();\n.hidden { color: black; }\n",
        ),
    ];
    let src = "@import 'vis.less';\n@import (reference) 'reffile.less';\n";

    // less.js semantics (default profile): everything in the reference file
    // stays hidden.
    let js = {
        let resolver = MapResolver(files.clone());
        magecommand_less::compile(src, &LessOptions::default(), &resolver)
            .expect("must compile")
            .code
    };
    assert_eq!(js, "");

    // less.php semantics (Magento profiles): the visibly-defined mixin's
    // output emits; the reference-defined mixin's output and the file's
    // direct rules stay hidden.
    let php = {
        let resolver = MapResolver(files);
        let opts = LessOptions {
            php_reference_visibility: true,
            ..LessOptions::default()
        };
        magecommand_less::compile(src, &opts, &resolver)
            .expect("must compile")
            .code
    };
    assert_eq!(php, ".t {\n  color: red;\n}\n");
}

/// §3 profile question #2 (probed 2026-07, `ext_ref` pair): extends DECLARED
/// inside a `(reference)` file.
///
/// - less.js 4.6.7: the graft inherits the extend's (hidden) visibility and
///   chaining through it yields nothing — `.consumer:extend(.abs-b all)`
///   where `.abs-b:extend(.abs-a all)` lives in the reference file emits
///   NOTHING for `.abs-a`'s rule.
/// - less.php 5.5.1: extend-added selectors are ALWAYS visible (less.php has
///   no per-selector visibility on grafts) and chaining passes through:
///   the same input emits `.abs-b, .consumer { … }` — `.abs-b` (the
///   reference-file extender) renders, the original `.abs-a` stays hidden.
///
/// Luma-real: `_extends.less`'s abs-on-abs extends
/// (`.abs-action-addto-product:extend(.abs-action-link-button all)`) — the
/// real SCD styles-m contains the `.abs-*`-prefixed grafted selector lists.
#[test]
fn php_reference_extend_grafts_are_visible() {
    let files: Vec<(&'static str, &'static str)> = vec![
        (
            "ext_ref.less",
            ".abs-a { color: red; }\n.abs-b { &:extend(.abs-a all); }\n",
        ),
    ];
    let src = "@import (reference) 'ext_ref.less';\n.consumer { &:extend(.abs-b all); }\n.consumer { margin: 0; }\n";

    let js = {
        let resolver = MapResolver(files.clone());
        magecommand_less::compile(src, &LessOptions::default(), &resolver)
            .expect("must compile")
            .code
    };
    assert_eq!(js, ".consumer {\n  margin: 0;\n}\n");

    let php = {
        let resolver = MapResolver(files);
        let opts = LessOptions {
            php_reference_visibility: true,
            ..LessOptions::default()
        };
        magecommand_less::compile(src, &opts, &resolver)
            .expect("must compile")
            .code
    };
    assert_eq!(
        php,
        ".abs-b,\n.consumer {\n  color: red;\n}\n.consumer {\n  margin: 0;\n}\n"
    );
}

/// §2.8 element granularity through `&`-concatenation (probed 2026-07,
/// `amp_ref2` triple): `.abs-tax-total { &-expanded { .mixsym(); } }` joins
/// to elements `.abs-tax-total` + `-expanded` — NOT one element
/// `.abs-tax-total-expanded`.
///
/// - Both engines: `:extend(.abs-tax-total-expanded all)` does NOT match the
///   fused path (only the literally-declared `.abs-tax-total-expanded` rule
///   grafts). This was an engine bug (string-rendered selectors lost the
///   element boundary); the `\u{2}` fusion marker restores it.
/// - Divergence on the PREFIX: `:extend(.abs-tax-total all)` DOES match the
///   fused path's first element in less.js (graft `.consumer-expanded`);
///   less.php never matches into a fused element (no graft) — php-profile
///   matching rejects fragments whose edge cuts a fusion.
///
/// Luma-real: the `.abs-tax-total`/`-expanded` pair in `_extends.less` — the
/// real SCD styles-m has the fused rule UNGRAFTED and the literal rule
/// grafted.
#[test]
fn amp_fusion_extend_element_granularity() {
    let files: Vec<(&'static str, &'static str)> = vec![
        ("lib.less", ".mixsym() { &:after { content: 'x'; } }\n"),
        (
            "amp_ref.less",
            ".abs-tax-total { &-expanded { .mixsym(); } }\n.abs-tax-total-expanded { .mixsym(); }\n",
        ),
    ];

    // less.js: extend of the fused spelling matches only the literal rule.
    let js = {
        let resolver = MapResolver(files.clone());
        let src = "@import 'lib.less';\n@import (reference) 'amp_ref.less';\n\
                   .consumer { &:extend(.abs-tax-total-expanded all); }\n";
        magecommand_less::compile(src, &LessOptions::default(), &resolver)
            .expect("must compile")
            .code
    };
    assert_eq!(js, ".consumer:after {\n  content: 'x';\n}\n");

    // less.js: the PREFIX extend matches the fused path element-wise.
    let js_prefix = {
        let resolver = MapResolver(files.clone());
        let src = "@import 'lib.less';\n@import (reference) 'amp_ref.less';\n\
                   .consumer { &:extend(.abs-tax-total all); }\n";
        magecommand_less::compile(src, &LessOptions::default(), &resolver)
            .expect("must compile")
            .code
    };
    assert_eq!(js_prefix, ".consumer-expanded:after {\n  content: 'x';\n}\n");

    // less.php profile: fused rule visible (visible-defined mixin) but never
    // grafted — neither by the fused spelling nor by the prefix.
    let php = {
        let resolver = MapResolver(files);
        let opts = LessOptions {
            php_reference_visibility: true,
            ..LessOptions::default()
        };
        let src = "@import 'lib.less';\n@import (reference) 'amp_ref.less';\n\
                   .consumer { &:extend(.abs-tax-total-expanded all); }\n";
        magecommand_less::compile(src, &opts, &resolver)
            .expect("must compile")
            .code
    };
    assert_eq!(
        php,
        ".abs-tax-total-expanded:after {\n  content: 'x';\n}\n\
         .abs-tax-total-expanded:after,\n.consumer:after {\n  content: 'x';\n}\n"
    );
}

/// §3 profile question #4 (D-interp, probed 2026-07, Phase-5 review DS-2/TG-2/
/// RT-1): `@{}` string-interpolated dimension print precision.
///
/// - less.js 4.6.7: `Quoted.eval` renders the looked-up value with `toCSS()`
///   and NO context — no `fround`, the literal's full digits print
///   (`~"@{v}"` of `1.428571429` → `1.428571429`).
/// - less.php 5.5.1 (bougie probe, oracle copy): `Quoted` compile renders via
///   `toCSS($env)` — the env carries `numPrecision = 8`, so the interpolated
///   dimension prints ROUNDED (`1.42857143`).
///
/// Blank/Luma-real: `lib/_forms.less` interpolates
/// `~"@{@{_type}__line-height}"` (base `@line-height__base: 1.428571429`) —
/// the real SCD styles-m prints `line-height: 1.42857143` (4 spots/theme).
/// DIRECT declaration output is rounded to numPrecision in BOTH engines
/// (`Dimension.genCSS` frounds with the eval context), also asserted.
#[test]
fn php_interp_rounding_quadrants() {
    let src = "@v: 1.428571429;\n.a {\n  line-height: ~\"@{v}\";\n  top: @v;\n}\n";
    let resolver = MapResolver(Vec::new());

    // less.js semantics (default profile): interpolation keeps full digits,
    // the direct declaration rounds.
    let js = magecommand_less::compile(src, &LessOptions::default(), &resolver)
        .expect("must compile")
        .code;
    assert_eq!(js, ".a {\n  line-height: 1.428571429;\n  top: 1.42857143;\n}\n");

    // less.php semantics (`php_interp_rounding`, on in Magento profiles):
    // both round to numPrecision 8.
    let opts = LessOptions {
        php_interp_rounding: true,
        ..LessOptions::default()
    };
    let php = magecommand_less::compile(src, &opts, &resolver)
        .expect("must compile")
        .code;
    assert_eq!(php, ".a {\n  line-height: 1.42857143;\n  top: 1.42857143;\n}\n");

    // The Magento profile carries the flag.
    assert!(LessOptions::magento_production().php_interp_rounding);
}

/// Probed less.js-vs-less.php compress divergence (Gate T2 follow-up RT-4):
/// zero-length units under `compress`.
///
/// - less.js 4.x (npm probe v4.6.7): `Dimension.genCSS` compress branch drops
///   the unit of an EVALUATED zero length (`unit((0 / 10), rem)` → `0`,
///   `@z: 0px; left: @z` → `0`). A literal `top: 0px;` is untouched — the
///   declaration parser's anonymous-value fast path stores it as `Anonymous`
///   text that never reaches `Dimension.genCSS` (our engine ports that path).
/// - less.php 5.5 (source-read + bougie probe, oracle copy): the same genCSS
///   branch guards with a STRICT `$value === 0`, but the constructor stores
///   `floatval($value)` — the float `0.0` never matches the int `0`, so the
///   unit is ALWAYS kept (`0rem`, `0px`, `0em`), evaluated or not.
///
/// Blank/Luma-real: compressed styles-m carries `margin-top:0rem` (from
/// `.lib-font-size-value(0)`'s `unit((0/10), rem)`) and `top:0px` — byte
/// parity with the default-mode SCD output needs the less.php flavor.
/// Non-length zeros (`0%`, plain `0`) keep their spelling in both engines.
#[test]
fn php_zero_units_compress_quadrants() {
    let src = "@v: 0;\n@z: 0px;\n.a {\n  margin-top: unit((@v / 10), rem);\n  left: @z;\n  top: 0px;\n  width: 0%;\n  line-height: 0;\n}\n";
    let resolver = MapResolver(Vec::new());

    // less.js semantics (default profile): evaluated zero lengths drop their
    // unit; the literal `top: 0px` rides the anonymous-value fast path.
    let js_opts = LessOptions {
        compress: true,
        ..LessOptions::default()
    };
    let js = magecommand_less::compile(src, &js_opts, &resolver)
        .expect("must compile")
        .code;
    assert_eq!(js, ".a{margin-top:0;left:0;top:0px;width:0%;line-height:0}");

    // less.php semantics (`php_zero_units`, on in Magento profiles): the
    // strict-comparison accident keeps every unit.
    let php_opts = LessOptions {
        compress: true,
        php_zero_units: true,
        ..LessOptions::default()
    };
    let php = magecommand_less::compile(src, &php_opts, &resolver)
        .expect("must compile")
        .code;
    assert_eq!(php, ".a{margin-top:0rem;left:0px;top:0px;width:0%;line-height:0}");

    // Both Magento profiles carry the flag.
    assert!(LessOptions::magento_production().php_zero_units);
    assert!(LessOptions::magento_developer().php_zero_units);
}

/// `isSpaced` is `isWhitespace(-1)` — the whitespace BEFORE the operator only,
/// never the one after. Magento's adminhtml ships `calc((100%)* 0.5 - 24px)`,
/// which both less.js and less.php print glued (`100%*.5`); treating the
/// trailing space as "spaced" produced a real deployed-byte divergence.
#[test]
fn operation_is_spaced_looks_only_before_the_operator() {
    let cases = [
        ("calc((100%)* 0.5 - 24px)", "calc(100%*.5 - 24px)"),
        ("calc((100%) * 0.5 - 24px)", "calc(100% * .5 - 24px)"),
        ("calc(100%* 0.5 - 24px)", "calc(100%*.5 - 24px)"),
        ("calc(100%/2 - 3px)", "calc(100%/2 - 3px)"),
        ("calc(100% / 2 - 3px)", "calc(100% / 2 - 3px)"),
    ];
    for (input, expected) in cases {
        let src = format!(".a {{ w: {input}; }}");
        let mut opts = LessOptions::magento_production();
        opts.compress = true;
        let css = magecommand_less::compile(&src, &opts, &NoopResolver).unwrap().code;
        assert_eq!(css, format!(".a{{w:{expected}}}"), "input {input:?}");
    }
}

/// less.php 3.x (a less.js 2.5.3 port, Magento 2.4.7 and older) is `math=always`
/// AND has neither the `@charset`-hoisting visitor nor the `calc()`
/// special-casing that less.js 3.0 added. Every expectation here is pinned
/// against a live `wikimedia/less.php` v3.2.1 probe.
#[test]
fn magento_247_matches_less_php_3x() {
    let opts = LessOptions::magento_247();

    // math=always: `*` and `/` evaluate without parentheses — this is what
    // makes Magento's unparenthesized `unit(@x * 16/100)` compile at all.
    let src = "@base: 62.5%;\n.a { r: unit(@base * 16/100); }\n";
    let css = magecommand_less::compile(src, &opts, &NoopResolver).unwrap().code;
    assert_eq!(css, ".a {\n  r: 10;\n}\n");

    // calc() interior math folds — 2.5.3 has no calc special-casing, so the
    // interior evaluates as plain arithmetic that ignores unit compatibility
    // and keeps the first unit (probed: less.php 3.2.1 gives `calc(70%)`).
    let mut c = opts.clone();
    c.compress = true;
    assert_eq!(
        magecommand_less::compile(".a { w: calc(100% - 40px + 10px); }\n", &c, &NoopResolver)
            .unwrap()
            .code,
        ".a{w:calc(70%)}"
    );
    assert_eq!(
        magecommand_less::compile(".a { w: calc(100% - 40px); }\n", &c, &NoopResolver)
            .unwrap()
            .code,
        ".a{w:calc(60%)}"
    );

    // @charset stays where it appears — no hoisting to the top.
    let src = ".a { b: 1; }\n@charset \"UTF-8\";\n.c { d: 2; }\n";
    assert_eq!(
        magecommand_less::compile(src, &c, &NoopResolver).unwrap().code,
        ".a{b:1}@charset \"UTF-8\";.c{d:2}"
    );
}

/// The modern profile (less.php 5.x) must keep the opposite behavior — the
/// same inputs throw on unparenthesized division, preserve calc interiors, and
/// hoist @charset. Guards the 2.4.8 byte-parity from the 247 flags.
#[test]
fn magento_248_keeps_parens_division_and_hoisting() {
    let mut opts = LessOptions::magento_production();
    opts.compress = true;

    // calc interior preserved.
    assert_eq!(
        magecommand_less::compile(".a { w: calc(100% - 40px + 10px); }\n", &opts, &NoopResolver)
            .unwrap()
            .code,
        ".a{w:calc(100% - 40px + 10px)}"
    );
    // @charset hoisted to the top.
    let src = ".a { b: 1; }\n@charset \"UTF-8\";\n.c { d: 2; }\n";
    assert_eq!(
        magecommand_less::compile(src, &opts, &NoopResolver).unwrap().code,
        "@charset \"UTF-8\";.a{b:1}.c{d:2}"
    );
}
