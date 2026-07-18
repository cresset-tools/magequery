# magecommand-less — build notes

Tracks scaffold state and decisions across the phased build in
`magecommand-less-plan.md`. Delete rows as they land.

## Step 1 — SCAFFOLD (done)

The crate compiles, `cargo build --workspace` is green, and the crate is
additive (no existing crate touched except adding the workspace member).

- Public API from plan §9.5 is defined: `parse` / `eval` / `compile`,
  `LessOptions` (+ `MathMode`, `RewriteUrls`, `CompatProfile`, and the
  `::default()`==less-js / `::magento_production()` / `::magento_developer()`
  constructors), the `ImportResolver` trait (with the default no-op
  `magento_import`, §7.1), `Css`, `Warning`, `LessError` + the 7 `ErrorKind`s
  (§F3), and the import boundary types.
- Full §9.1 module tree exists as compiling stubs (lex/parser/eval/functions/
  data/css/minify), each with a doc comment naming the plan section it owns.
- Two foundational pieces are already **real**, not stubs, since everything
  downstream depends on them: `lex::normalize_source` (BOM strip + CRLF→LF, §H2)
  and `lex::LineMap` (byte offset → 1-based line/col via binary search, for exact
  error provenance, §5.5). Both unit-tested.
- `compile()` is a source-normalizing **passthrough**: `parse` normalizes and
  wraps the whole input in `Node::Anonymous`; `eval` emits it verbatim.

### Deliberate API decision (flag for reviewers)

The STEP 1 prose gave `compile(...) -> Result<String, LessError>`, but plan §9.5
specifies `-> Result<Css, LessError>` and the whole architecture (`css.rs`, the
`Css { code, imports, source_map, warnings }` struct, the §5.5 harness reading
`.css`/`.err`) is built around `Css`. **We followed §9.5 (`Css`).** A caller that
only wants the string does `compile(...)?.code`. This is a superset and avoids
rework; the plan §9 is the authoritative source ("Define the public API from §9").

### Dependencies

`cssparser` (CSS-side serialization helpers only — it cannot tokenize LESS, §9),
`smallvec` (unit multiset), `rustc-hash` (scope frames), `thiserror`
(`ImportError`). `lightningcss` (pinned `=1.0.0-alpha.71`) is **optional**, behind
`feature = "minify"`, confined to the downstream `.min.css` bridge (§9.4/§9.7) and
never in the default build or any acceptance gate. Verified: `--features minify`
compiles.

### Build/gate command

```
export CARGO_TARGET_DIR="$HOME/.cache/magecommand-gate-target"
cargo build --workspace                       # the gate — green
cargo test  -p magecommand-less --lib         # 6 lib unit tests passing
cargo test  -p magecommand-less --test fixtures  # prints the pass-rate X/Y
```

## Step 2 — TEST HARNESS + DEFAULT-OPTION FIXTURES (done)

Vendored the less.js **default-option compile fixtures** and wired a
`libtest-mimic` harness. `cargo build --workspace` stays green; the crate remains
additive (only its own `Cargo.toml`/`tests/`/`scripts/` touched).

- **Reference:** cloned `less/less.js` at the pinned tag **`v4.6.7`** (commit
  `8ae2cc3bfa79f0718ad6fe5f263a1d6819fe9d5c`). The `git clone --depth 1 --branch
  v4.6.7` succeeded first try (no fallback needed).
- **Layout surprise (plan §5.1 confirmed):** the old `packages/less/test/less/**`
  → `test/css/**` layout is **gone** at v4.6.7 (the dir exists but holds zero
  fixtures). Real fixtures live in **`packages/test-data/tests-unit/`**, one
  `<suite>/` dir each, top-level `<name>.less` + sibling `<name>.css` = a
  compile+diff entry (subdirs are import helpers). The Step-2 prompt's
  `packages/less/test/less/*.less` path is the pre-4.2.2 layout; vendored from
  the current location instead, per the plan.
- **Vendored** (`tests/fixtures/less-testdata/`, via `scripts/vendor-less-testdata.sh`):
  the whole `tests-unit/` tree (inputs + expected + import helpers) + the binary
  `data/` assets (§C-assets) + the upstream `LICENSE` (Apache-2.0). Provenance in
  `tests/fixtures/VENDOR.txt`. The cosmiconfig `styles.config.cjs` option files are
  deleted from the vendored tree (harness runs default options).
- **Denominator: 87 in-scope default-option compile fixtures** — every `.less`
  with a sibling `.css` under `tests-unit/`, minus the 4 in the deferred
  `javascript`/`plugin`/`plugin-module`/`plugin-preeval` sub-suites (91 total −
  4). The `plugi` typo dir + `parser-property-interp` stay IN (false-positive-JS,
  plan §5.2). *(This is a subset of the plan's ultimate 126 compile fixtures — the
  option-driven `tests-config/` compile fixtures are DEFERRED, below.)*
- **Harness** (`tests/fixtures.rs`, `harness = false`): one `libtest-mimic` Trial
  per fixture. Each compiles with `LessOptions::default()` + an `FsResolver`
  (filesystem `ImportResolver` rooted at the fixture dir — wired now, unused by
  the passthrough `compile()` yet), applies the `doReplacements`/`globalReplacements`
  placeholder substitution (`{path}`/`{pathimport}`/`{pathesc}`/… + `\r\n`→`\n`,
  ported from less-test.js:404) and trailing-newline normalization to the expected
  `.css`, then byte-diffs. The runner prints a pass-rate line + libtest-mimic's own
  `N passed; M failed` summary.
- **Current pass-rate: 2/87** (passthrough — `empty/empty` and `tailwind/tailwind`
  happen to be identity transforms). Mostly-failing is EXPECTED before the Phase-1
  evaluator. The harness exits non-zero while any fixture fails (honest pre-ratchet
  state); `cargo build --workspace` is unaffected. Lib unit tests: 6 passing.

### DEFERRED to later phases (NOT vendored / NOT yet in the harness)

Per plan §5.2/§5.4 and the Step-2 skip list. Each becomes a fixture slice for the
phase that implements it:
- **`tests-config/`** — every option-driven sub-suite: `math/*` +
  `math-{always,parens-division,strict}` (incl. the split-math input-vs-expected
  quirk, §5.2), `units/{strict,no-strict}` (strict-units), `globalVars`/`modifyVars`,
  `url-args`, `rootpath-*`/`static-urls`, `rewrite-urls-*`, `namespacing`,
  `strict-imports`, `include-path*`, `process-imports`, `compression`/`compress`/
  `at-rules-compressed*`, `3rd-party`, and the plugin/`postProcessor`/`preProcessor`/
  `visitorPlugin`/`filemanagerPlugin`/`sourcemaps*`/`debug`/`root-registry`/
  `import-redirect`/`js-type-errors`/`no-js-errors` suites (JS/sourcemap/plugin — OUT).
- **`tests-error/`** — the 74 byte-exact error-message fixtures (incl. the in-scope
  disabled-JS *error* fixtures, §5.2). Needs the 7-kind error renderer + 3-line
  source excerpt (plan §5.5) — the error phase.
- **`tests-unit/{javascript,plugin,plugin-module,plugin-preeval}`** — JS/plugin,
  OUT for a headless pure compiler (§8).
- **The generated `manifest.json`** (plan §5.4) — a Node-toolchain step that folds
  in per-suite cosmiconfig options; when it lands, the harness reads options from it
  rather than assuming defaults, and the `tests-config/` compile fixtures + the 74
  error fixtures join the denominator toward the plan's 200/200. Until then the
  harness discovers fixtures by filesystem walk (sibling `.css`) under default options.
- **The §5.6 CI ratchet** (`min_pass_rate` gate + `xfail` allowlist + report JSON +
  the "out-set == the classified 36" meta-test) — arrives with the manifest.
- **The §G2 compatibility-proof blocker task** — Phase 0 written finding.

## Step 3 — TOKENIZER + PARSER + AST + plain-CSS genCSS (done)

Hand-written tokenizer, recursive-descent parser producing the **complete
parse-side AST** for the core subset, and a genCSS serializer wired so **plain
CSS round-trips**. `cargo build --workspace` stays green; the crate is additive.

- **Pass-rate delta: 2/87 → 9/87** default-option compile fixtures. The newly
  passing genuinely-flat-CSS fixtures are `at-rules-declarations`,
  `at-rules-empty`, `at-rules-empty-block` (plus the pre-existing `empty`,
  `tailwind`, `no-output`, `impor`, `plugi`, `color-functions/modern` identity
  cases). Everything else in the corpus needs **evaluation** — nesting/`&` join,
  variable/mixin/operation resolution, `@import` inlining — which is the next
  step, so those fixtures stay red by design.

- **Tokenizer (`lex/mod.rs`):** a `Cursor` scanning layer (the primitives the
  parser drives — `scan_ident`/`scan_number`/`scan_string`/`scan_comment`,
  peek/bump, whitespace/trivia skipping, char-boundary-safe) plus a coarse
  `tokenize()` → `Vec<Token>` for the standalone tokenizer deliverable/tests. LESS
  is context-sensitive (the same `-` is a sign, subtraction, or part of an ident,
  §2.4), so the parser uses the cursor directly, exactly as less.js's own
  `parser-input.js` does. `normalize_source` + `LineMap` unchanged.

- **AST (`ast.rs`):** the full node set — `Ruleset`/`Selector`/`Element`/
  `Combinator`, `Declaration` (`!important`, merge `+`/`+_`, custom props),
  `AtRule` (+ `Import` node), `Comment`, `VariableDecl`, `DetachedRuleset`,
  `MixinDefinition`/`MixinParam`, `MixinCall`/`MixinArg`, `MagentoImport` (§7.1),
  and value leaves `Value`/`Expression`/`Anonymous`/`Dimension`/`Color`/`Quoted`/
  `Keyword`/`Call`/`Url`/`Paren`/`Operation`/`Negative`/`Variable`/
  `VariableVariable`/`Interpolation`/`PropertyAccessor`. Each carries a `Span`.
  `is_output_visible()` prunes non-output nodes + empty blocks like less.js's
  `ToCSSVisitor`.

- **Parser (`parser/mod.rs`):** recursive descent with backtracking (cheap cursor
  index save/restore) mirroring less.js's `primary`/`declaration`/`ruleset`/
  `atrule`/`element`/`combinator`/`multiplication`/`addition`/`operand` order.
  Declarations are tried before selector statements but bail on a `.`/`#`/`&`/`:`
  head (so rulesets/mixin calls aren't swallowed); `@x:` is tried as a variable
  decl with backtrack so `@page :left { … }` still parses as an at-rule.
  Whitespace/sign ambiguity (§2.4) handled: `+`/`-` are operators only with
  whitespace on **both** sides, else a leading `-` is a signed number (a separate
  list item). The `parser/{selector,expr,entities,atrule}.rs` files keep the plan's
  documented ownership doc-comments; the impl is co-located for shared cursor state.

- **genCSS (`css.rs`):** `render_root` + `gen` reproduce less.js's expanded-output
  spacing **exactly** — ruleset/at-rule 2-space indentation (`tabRule`/`tabSet`,
  §4.7), selector paths joined `,\n<tabSet>`, combinator spacing (`>`/`+`/`~`
  spaced, descendant/`|` not), `prop: value;` declarations, `Value` `, `-join,
  `Expression` ` `-join (skipping the pre-`,`-Anonymous space), and
  `Dimension.fround` number formatting (add `2e-16`, `numPrecision=8`, tiny-value
  `toFixed(20)` guard, `-0`→`0`). Verified against the less.js `tree/*.js` sources
  in `/tmp/lessjs`. `eval` now does structural lowering (parse→serialize); true
  evaluation is next.

### KNOWN SCOPE LIMITS (deferred to the eval step, by design)

- No nesting / `&` join, no variable/mixin/operation *evaluation*, no `:extend`,
  no `@import` inlining, no `@media` bubbling/merging, no `@charset`/`@import`
  reordering in genCSS (source order kept). Colors/dimensions round-trip
  verbatim (no color math). Custom-property values and `@{}` interpolation are
  kept raw. These are exactly why the non-flat fixtures remain red.
- Mixin/guard/detached-ruleset internals are parsed structurally (params/args
  split on `;`/`,`; guards + literal params retained as raw `Anonymous`) — enough
  to build the AST and not error, refined when eval needs them.

## Step 4 — EVALUATOR + genCSS (milestone 1) (done)

The tree-rewriting evaluator (plan §4) and the flat expanded serializer. `cargo
build --workspace` stays green; the crate is additive.

- **Pass-rate delta: 9/87 → 20/87** default-option compile fixtures, plus **31
  lib unit tests** (was 6): `variables`, `variables-in-at-rules`, `lazy-eval`,
  `operations`, `operations-advanced`, `color-functions/{operations,modern}`,
  `css-3`, `css-grid`, `rulesets`, `charsets`, `at-rules-{declarations,empty,
  empty-block}`, `tailwind`, `mixin-noparens`, `empty`, `no-output`, `impor`,
  `plugi`.
- **Rich value leaves (plan §9.2/§9.3):** `Node::Dimension`/`Node::Color` now
  carry the runtime `value::Dimension` (open-multiset `unit::Unit` +
  `backupUnit`) and `color::Color` (RGBA `f64` + original literal). Unit
  conversion tables + the 148 named colors are generated-from-source
  (`data/colors.rs`, `value.rs`). `1px / 1px → 1px` (backup-unit persistence)
  and the per-channel color math are locked by tests.
- **Evaluator (`eval/mod.rs`):** one `Ctx` (innermost-first frame stack, math
  state + parens stack, `importantScope`). Implements:
  - **lazy variable resolution** — last-declaration-wins per frame scanned from
    the unevaluated rules (forward refs + child-scope override, §4.1/§4.3), `@@`
    variable-variables, `!important` bubbling via `importantScope`, recursion
    guard;
  - **`@{}` interpolation** (selectors, property names, quoted strings, at-rule
    preludes) + bare-`@var` prelude resolution;
  - **operations** — dimension `+ - * /` with unit coercion/conversion, per-
    channel color math + dim↔color coercion, unary negation, the `isMathOn` gate
    (all 3 math modes; parens-division default), deferred ops emitted literally
    with source spacing; `calc()` suppresses interior math;
  - **nested-rule flattening + `&`** (string-level JoinSelector: descendant
    join, `&` substitution, leading-combinator handling);
  - **at-rules** — `@media`/`@supports`/… container bodies, `@font-face`/`@page`
    declaration bodies (decls interleaved with nested at-rules), empty-block
    pruning, inline no-block directives (`@apply`), and `@charset`/`@import`/
    `@namespace` output hoisting (§2.13);
  - a **minimal function set** the gates need (`rgb`/`rgba`/`hsl`/`hsla`,
    `unit`, `floor`/`ceil`/`round`/`abs`/`sqrt`/`percentage`/`min`/`max`) + the
    unknown-function passthrough (§2.7); **basic mixin calls** (no
    guards/patterns/overloading — a `when` guard is evaluated best-effort).
- **Parser fixes** feeding the evaluator: variable-decl `!important`; the
  less.js `addition` operator/sign rule (`@a -1` list vs `@a - 1` subtraction vs
  `2px+6px` add, §2.4); `[…]` value tokens; `U+…` unicode-range; `//` inside
  `url(…)` no longer eaten as a line comment in at-rule preludes;
  `parse_value_fragment` for re-parsing mixin args/guards.

### DEFERRED (later phases, by design — NOT milestone 1)

Mixins-with-guards/patterns/overloading/`default()` (Phase 2); the full function
library incl. string/list/each/color-op/data-uri (Phase 3); `@import` resolution
& inlining, `:extend`, detached-ruleset replay, maps/lookups (`@p[key]`), merge
`+:`, full at-rule bubbling/`@media` merging (Phase 4); `$prop` property
accessors; the 7-kind error renderer + `tests-error`/`tests-config` denominator.
These are why `nesting` (needs maps), `scope` (mixin scope-injection), `extend*`,
`import/*`, `media`/`container` (bubbling), and the color/string function
fixtures stay red.

## Step 5 — CONSOLIDATE + ratchet gate (done, this is milestone 1)

Froze the milestone-1 state, made the whole workspace test suite green, and
recorded the honest pass-rate + the deferred roadmap. No engine features added;
this step is about the gate and the bookkeeping.

### Honest pass-rate: **20/87** default-option compile fixtures

The denominator is the vendored less.js **`v4.6.7`** `tests-unit/` default-option
compile corpus (every `.less` with a sibling `.css`, minus the deferred
`javascript`/`plugin*` sub-suites — plan §5.2). The `tests-config/` option-driven
compile fixtures and the 74 `tests-error/` fixtures are **not yet in the
denominator** (they need the `manifest.json` + 7-kind error renderer, DEFERRED
below), so this is a subset of the plan's ultimate 200/200 (Gate T0).

Per-category breakdown (▲ = fully green, △ = partial, ○ = all red / deferred):

```
▲ variables            variables, variables-in-at-rules, lazy-eval            (variable-advanced ○)
▲ operations/math      operations, operations-advanced, css-grid              (calc ○)
▲ at-rules (subset)    at-rules-declarations, at-rules-empty, -empty-block, charsets
▲ css passthrough      css-3, tailwind, empty, no-output
△ color-functions      modern, operations green; alpha/basic/comprehensive/formats/
                       modern-syntax/rgba red (full color fn library = Phase 3)
▲ basic mixin/ruleset  mixin-noparens, rulesets
▲ parse-only           impor, plugi  (false-positive-JS, stay in per §5.2)

○ mixins (Phase 2)     mixins, mixins-advanced, maps, mixins-closure, -guards,
                       -guards-default-func, -important, -interpolated, -named-args,
                       -nested, -pattern, namespace-targeted, css-guards
○ functions (Phase 3)  functions, functions-each, extract-and-length, ie-filters,
                       strings, css-escapes  (+ the 6 red color-functions above)
○ import (Phase 4)     import, import-inline, -interpolation, -module, -once,
                       -reference, -reference-issues, -remote
○ extend (Phase 4)     extend, extend-clearfix(×2), extend-chaining, -exact, -media,
                       -nest, -selector
○ dr/merge/maps (P4)   detached-rulesets, merge, mixins/maps, property-accessors
○ at-rule bubbling(P4) media, container, layer, directives-bubbling, at-rules,
                       at-rules-targeted, at-rules-keyword-comments
○ nesting/sel edges    nesting, selectors, scope, parser-slashed-combinator,
                       whitespace, permissive-parse, starting-style, urls, comments,
                       comments2, property-targeted
○ interpolation edges  parse-interpolation, parser-property-interp, property-name-interp
```

### The ratchet gate (`tests/fixtures.rs`, plan §5.6 — milestone-1 form)

The harness now enforces a **checked-in ratchet floor** (`EXPECTED_PASS`, the 20
byte-identical fixtures). Every off-floor in-scope fixture is a labelled `[xfail]`
that is reported red but does **not** fail the suite, so `cargo test --workspace`
is **green** while the engine is incomplete. Two conditions DO fail the suite,
keeping it honest:
- **regression** — a floor fixture stops matching (`RATCHET REGRESSION: …`);
- **improvement** — an `xfail` starts matching (`RATCHET IMPROVEMENT: … add it to
  EXPECTED_PASS`) — so new coverage can never land unrecorded.

Both paths were verified (temporarily promoting `calc/calc` onto the floor trips
`RATCHET REGRESSION`). The reporter still prints `20/87 passing (ratchet floor 20;
67 xfail)`. **When a phase lands, add its newly-green fixtures to `EXPECTED_PASS`.**

### Gate status (both green)

```
export CARGO_TARGET_DIR="$HOME/.cache/magecommand-gate-target"
cargo build --workspace          # GREEN (1 pre-existing warning in magequery-core)
cargo test  --workspace          # GREEN — every crate binary runs:
   magecommand_engine 70 · magecommand_less lib 31 · fixtures 87 (20+67 xfail)
   · magecommand_php 41 · magequery_core 49 · magequery_lsp 17 · e2e 1 · doc-tests
```

The crate stays **additive** — no existing crate/test was touched. Before Step 5
the fixtures binary's `conclusion.exit()` hard-failed (67 red), and cargo's
fail-fast then skipped every *later* crate's tests; the ratchet fixes that.

## What milestone 1 implemented (Steps 1–5 consolidated)

- **Public API (§9.5):** `parse`/`eval`/`compile`; `LessOptions` (+ `MathMode`,
  `RewriteUrls`, `CompatProfile`, and the `::default()`==less-js /
  `::magento_production()` / `::magento_developer()` constructors); the
  `ImportResolver` trait (+ default no-op `magento_import`, §7.1); `Css`,
  `Warning`, `LessError` + the 7 `ErrorKind`s; import-boundary types.
- **Lexer (`lex`):** `normalize_source` (BOM strip + CRLF→LF, §H2), `LineMap`
  (byte→1-based line/col, §5.5), a char-boundary-safe `Cursor` scanner the parser
  drives directly, plus a coarse `tokenize()`.
- **Parser (`parser`):** recursive descent with cheap backtracking mirroring
  less.js's `primary`/`declaration`/`ruleset`/`atrule`/`element`/`addition`/
  `operand` order; the **whitespace/sign ambiguity** (`@a -1` list vs `@a - 1`
  subtraction vs `@a-1` ident, §2.4); variable-decl `!important`; `[…]`/`U+…`
  value tokens; `//`-in-`url()`.
- **Evaluator (`eval`, §4):** one `Ctx` (innermost-first frames + math/parens
  state + `importantScope`); **lazy variable resolution** (last-wins, forward
  refs, child-scope override, `@@`, `!important` bubbling, recursion guard);
  **`@{}` interpolation** (selectors, property names, quoted strings, at-rule
  preludes); **operations** (dimension `+ - * /` with unit coercion/conversion,
  per-channel color math + dim↔color coercion, unary negation, the `isMathOn`
  gate in all 3 modes, deferred ops emitted literally, `calc()` interior-math
  suppression); **nested-rule flattening + `&`** (string-level JoinSelector);
  **at-rules** (`@media`/`@supports` containers, `@font-face`/`@page` decl bodies,
  empty-block pruning, `@charset`/`@import`/`@namespace` output hoisting); a
  **minimal function set** (`rgb`/`rgba`/`hsl`/`hsla`, `unit`, `floor`/`ceil`/
  `round`/`abs`/`sqrt`/`percentage`/`min`/`max`) + the **unknown-function
  passthrough** (§2.7); **basic mixin calls** (no guards/patterns/overloading).
- **Value leaves (§9.2/§9.3):** `Dimension` (open-multiset `Unit` + `backupUnit`,
  f64 + `numPrecision=8` `fround`), `Color` (RGBA f64 + original literal), unit
  conversion tables + **148 named colors** generated-from-source.
- **genCSS (`css`):** less.js expanded-output spacing exactly (2-space indent,
  combinator spacing §4.7, `Value`/`Expression` joins, `fround` formatting).
- **Harness (`tests/fixtures.rs`):** `libtest-mimic`, `doReplacements` (§5.5),
  `FsResolver` (the FS `ImportResolver`), and the Step-5 ratchet gate above.

## DEFERRED roadmap (what's left, cross-referenced to the plan)

Nothing below is started; each is gated by its own fixture slice.

- **Phase 2 — mixins + guards + pattern-matching** (plan §2.5/§2.6, Phase 2):
  parametric mixins (defaults, `;`-arg-separator, named args, `@arguments`, `...`
  variadic), overloading, **pattern matching** (literal/keyword params by
  evaluated `toCSS` equality, `@_`, "No matching definition"), **guards** (`and`/
  comma-OR/`not`, comparisons `> >= = =< <`, the type-check fns `is*`/`isunit`/
  `isdefined`, **`default()` two-subpass + `Ambiguous use of default()`**), CSS
  guards + `& when`, **namespaces** (`#ns.m()`, `#ns > .m()`), `!important`
  propagation via `.m() !important`, recursion + depth cap, scope-injection.
  Unlocks: `mixins*`, `css-guards`, `namespace-targeted`.
- **Phase 3 — full function library** (plan §2.7, Phase 3): the complete registry
  — string (`e`/`escape`/`%`/`replace`), list (`length`/`extract`/`range`/
  `each`), math (`sin`…`atan`), number (`convert`/`pi`/`mod`/`pow` + **min/max
  dual behavior**, §4.8), all `is*`/`get-unit`, color def (`argb`/`hsv`/
  `color()`), color channels, color ops (`saturate`/`lighten`/`mix`/`contrast`/…),
  color blending (`multiply`/`screen`/…), `data-uri` (mime table + IE-32KB
  threshold) + `image-size` **real file reads** (§C-assets), `svg-gradient`
  (URL-encoding parity §3-G); `~"…"`/`e()`/`%()` escaping; **IE-filter/`progid`
  output** (§2.17). Unlocks: `functions*`, `extract-and-length`, the 6 red
  `color-functions/*`, `ie-filters`, `strings`, `css-escapes`.
- **Phase 4 — imports, extend, detached rulesets, merge, at-rule bubbling, maps**
  (plan §2.8–§2.13, Phase 4):
  - **`@import` two-stage + all options** (§2.9): `once`/`reference`/`optional`/
    `inline`/`less`/`css`/**`layer`**, `strictImports`, interpolated paths, CSS
    re-emit with path rewrite. Unlocks `import/*`.
  - **`:extend` full** (§2.8): finder → chaining fixpoint → replace, the `all`
    keyword, `@media` scoping incl. `(reference)` media stacks. Unlocks `extend*`.
  - **detached rulesets** (§2.11), **merge `+:`/`+_:`** (§2.10), **maps/lookups**
    `@p[key]`/`#ns[k]` (§2.12). Unlocks `detached-rulesets`, `merge`, `mixins/maps`,
    `property-accessors`.
  - **all-rule bubbling & output ordering** (§2.13): `@media`/`@supports`/
    `@container`/`@layer`/`@namespace`/`@property`/`@page`/`@charset`. Unlocks
    `media`, `container`, `layer`, `directives-bubbling`, `at-rules*`.
  - the **byte-exact `compress` serializer** (§C4/§9.4 — distinct from the
    expanded genCSS, never delegated to lightningcss).
  - the residual **nesting/selector/interpolation edge cases** (`nesting`,
    `selectors`, `scope`, `whitespace`, `permissive-parse`, `parse-interpolation`,
    the `*-interp`/`*-targeted` fixtures, `comments*`, `urls`, `calc`,
    `variable-advanced`, `starting-style`, `parser-slashed-combinator`).
  - **This slice is the plan's 200/200 (Gate T0).**
- **Deferred fixture sub-suites + the `manifest.json`** (plan §5.2/§5.4/§5.6):
  the Node-toolchain `manifest.json` generation step (folds in per-suite
  cosmiconfig options), after which the **`tests-config/*`** option-driven compile
  fixtures (`math*`, `units/*`, `globalVars`/`modifyVars`, `url-args`, `rootpath-*`,
  `rewrite-urls-*`, `namespacing`, `strict-imports`, `include-path*`,
  `process-imports`, `compression`/`compress`/`at-rules-compressed*`, `3rd-party`)
  and the **74 `tests-error/*`** byte-exact error fixtures (incl. the in-scope
  disabled-JS *error* fixtures, §5.2) join the denominator toward 200/200. These
  need the **7-kind error renderer + 3-line source excerpt** (§5.5;
  `LessError::Display` is provisional today) and the **full §5.6 ratchet**
  (`min_pass_rate` + per-fixture tracking issue + `less-compat-report.json` + the
  "out-set == the classified 36" meta-test). The Step-5 `EXPECTED_PASS` floor is
  this ratchet's milestone-1 form.
- **The §G2 Phase-0 compatibility-proof blocker** (plan §6/§11): the written
  finding that no in-scope 4.6.7 fixture depends on a less.js-4.0 breaking change
  absent from less.php-5.5's 3.13.1 feature level. Not yet produced.
- **less.php compat profiles** (plan §1/§3): the `magento-2.4.8` profile
  (parens-division, `compress=false`, `relativeUrls=false`, JS off, `calc()`
  preserved) and the legacy `magento-2.4.7` profile (`math=always` + `calc()`
  interior math), selected from the target's `composer.lock`; plus the two
  **diagnostic-only** shims — the PHP-float shim (§3-C, number last-digit +
  `round()` half-away-from-zero) and the PHP-encoding shim (§3-G, `rawurlencode`
  set for `svg-gradient`/`data-uri`/`escape`/`%`). `CompatProfile` exists in the
  API; the behavior switches are not wired.
- **Minify wiring** (plan §9.7): the `lightningcss` `.min.css` bridge
  (`feature = "minify"`) compiles but is a stub — not wired into any output path,
  and never part of a diff gate. The sibling `oxc_minifier` JS crate is future.
- **Phase 5 — Magento corpus + wire-in** (plan §7/§10): the `//@magento_import`
  directive in `magento_mode` (§7.1), `Vendor_Module::path` resolution (§7.2),
  load-order splice (§7.3), the `(reference)`×`extend-all`×detached-ruleset triad
  with the **G-ref/G-resp/G-detached** gates (§7.4), the Tier-1 less.php
  differential (§7.6, diagnostic) + the **Tier-2 SCD semantic diff** (§7.7, the
  contract / Gate T2), and integration into `magecommand-engine static deploy`.
- **README** pinning the §0 success definition verbatim (Gate T0 200/200 + Gate
  T2 zero SCD residuals) — plan §0 asks for this; not yet written.
- **AST completeness:** the parse-side node set is in place; a handful of the 44
  `tree/` nodes are still parsed structurally rather than fully evaluated (they
  land with the phase that needs them, above).
