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
   magecommand_engine 70 · magecommand_less lib 42 · fixtures 87 (31+56 xfail)
   · magecommand_php 41 · magequery_core 49 · magequery_lsp 17 · e2e 1 · doc-tests
```

The crate stays **additive** — no existing crate/test was touched. Before Step 5
the fixtures binary's `conclusion.exit()` hard-failed (67 red), and cargo's
fail-fast then skipped every *later* crate's tests; the ratchet fixes that.

## Phase 2 — MIXINS + GUARDS + PATTERN-MATCHING + NAMESPACES (done)

Extended the evaluator (plan §2.5/§2.6, §4.4) to the full parametric-mixin +
guard surface. `cargo build --workspace` + `cargo test --workspace` stay green;
the crate is additive.

- **Pass-rate delta: 20/87 → 30/87 → 31/87.** Ten newly-green fixtures (added to
  `EXPECTED_PASS`): `mixins/mixins`, `mixins/mixins-advanced`, `mixins-closure`,
  `mixins-nested`, `mixins-named-args`, `mixins-pattern`, `mixins-important`,
  `css-guards`, `scope`, `mixins-guards-default-func`. Lib tests 31 → 38. The
  follow-up fixes below add `mixins-guards` (lib tests 38 → 42).

- **Closure param capture + `@arguments` variadic + recursive guard grammar +
  guarded namespaces (follow-up).** Four faithfulness fixes (verified against
  less.js v4.6.7):
  - **Bound-param closures:** an inner mixin definition injected into the caller
    by a parametric outer mixin now freezes the outer's evaluation frames (its
    bound params). `collect_injected` wraps each injected `MixinDefinition` in a
    new eval-only `Node::Closure { inner, scope }` whose `scope` indexes a
    captured-frame side table on `Ctx` (`closures: Vec<Vec<Frame>>`); the frames
    can't live on `Node` (it must stay `Send + Sync` for `Arc`-sharing).
    `find_candidates` resolves a `Closure` against its frozen frames. Two
    injections of the same inner name both stay in scope and both emit (matches
    less.js — NOT last-wins).
  - **`@arguments` variadic tail:** `bind_params` grew the `@arguments`
    accumulator past the param count so variadic-overflow args aren't dropped
    (`.m(@a, @rest...)` with `.m(1,2,3)` → `@arguments == 1 2 3`).
  - **Recursive guard grammar:** `eval_guard_str` is now a proper recursive
    boolean parser — `,`/`or` (OR) < `and` < `not`/`( … )`/atomic — so nested
    `not(…)` and arbitrarily deep parens (the `parenthesisNot`/`orderOfEvaluation`
    cases) evaluate correctly. Previously it stripped only one `not` + one paren
    layer.
  - **Guarded namespaces on a call path:** `find_candidates` gates a namespace
    segment by `matchArgs(null)` (`accepts_zero_args`) so a parametric namespace
    is only traversed with zero args, and threads each traversed namespace's guard
    into `Candidate.path_guards`; `calc_def_group` AND-evaluates them with the
    mixin's own guard (less.js `calcDefGroup`).

- **Frame model change:** `Frame` is now `Rc<RefCell<Vec<Node>>>` so a mixin call
  can **inject** its returned variables/mixins/rulesets back into the caller's
  frame (scope-injection). `eval_rules` runs the less.js two-pass shape (plan
  §4.2): pass 1 expands every mixin call — splicing injected scope in — so pass-2
  declarations (even ones *earlier* in source, via lazy resolution) see them.

- **Mixin resolution (`expand_mixin_call`):** candidate lookup by normalized
  `mixinElements` names (`find_candidates`, recursing into namespaces `#ns > .m`,
  `.a.b.c` via `&`-joined nested rulesets); **closure capture** — each candidate
  carries the definition-scope frames (`def_scope = frames[k..]`), so `@var`
  resolves in the mixin's definition scope, the caller's frames appended after
  (the theming lever, §4.3). **matchArgs** (arity + literal-pattern by `toCSS`
  equality), **emit-all-matching** in source order, `No matching definition` /
  `X is undefined` errors. **Parametric binding** (`bind_params`): named →
  positional → defaults (evaluated in def scope), `@rest...` variadic, call-site
  spread `.m(@list...)`, `@arguments`. `.m() !important` propagation
  (`make_important_*`). **On-stack recursion guard** by ruleset span
  (MixinDefinitions exempt).

- **Guards (`eval_guard_str`):** comma-OR of `and`-lists, `not`, nested parens,
  comparisons (`= < > <= >= =<`) via a faithful `compare_values` port of less.js
  `Node.compare` (Dimension unit-unify, Quoted quote-agnostic, Color channels,
  Anonymous/toCSS, element-wise list compare with type-strict space-vs-comma),
  type-check functions (`iscolor` incl. named-color keywords, `isnumber`,
  `isstring`, `iskeyword`, `isurl`, `ispixel`/`isem`/`ispercentage`, `isunit`),
  `isdefined`, and the **`default()` two-subpass** classification with
  `Ambiguous use of default()`. CSS guards on selectors + `& when (…)` bare-`&`
  folding (`is_just_parent`).

- **New functions** (guard/Luma subset): `lightness`/`hue`/`saturation`,
  `red`/`green`/`blue`, `e()`, the `is*` set. `default()` passes through verbatim
  outside a guard.

- **Parser fixes:** `when` guard detected after whitespace (`.m () when (…)`);
  full boolean guard text captured (`and`/`or`/`not`/comma); space-before-parens
  mixin defs/calls (`.m (@a)`) split via `split_mixin_parens`.

### DEFERRED from Phase 2 (needs later-phase subsystems)

- `mixins/maps`, `nesting`, `namespace-targeted` — **property accessors / maps**
  `@p[key]`, `#ns[prop]`, `$@var` property interpolation (plan §2.12, Phase 4).
- `mixins-interpolated` — **interpolated mixin names** (`.@{a1}()` matching a
  ruleset whose selector is `.@{a1}`) — interpolation-as-lookup-key.
- `mixins-guards` — **done** (see the closure/guarded-namespace follow-up above).

## Phase 3 — FUNCTION LIBRARY + STRINGS (done)

The full §2.7 built-in registry plus the string/paren/comment parse semantics the
function fixtures exposed. Every non-obvious semantic was verified against real
less.js 4.6.7 (`node` probes over the vendored `lib/less/functions/*` sources,
never remembered formulas); `data-uri`/`image-size` outputs were byte-diffed
against `less.render` on the vendored `data/` assets.

- **Pass-rate delta: 31/87 → 48/87** (floor raised to 48). Newly green:
  `calc`, `color-functions/{alpha,basic,comprehensive,formats,modern-syntax,rgba}`,
  `comments/comments2`, `css-escapes`, `extract-and-length`, `ie-filters`,
  `merge`, `parser-slashed-combinator`, `property-name-interp`, `strings`,
  `variables/variable-advanced`, `whitespace`.

- **Registry** (`src/functions/`, one module per group, `dispatch` in `mod.rs`):
  - string: `e` (escaped-Quoted result, quote kept for later `replace`/`%`),
    `escape` (JS `encodeURI` + `=:#;()`), `%` (sequential `/%[sda]/i`, uppercase
    → `encodeURIComponent`, `%%`→`%`), `replace` (JS-regex via `fancy-regex` —
    lookaround/backrefs work — with hand-implemented JS replacement-pattern
    semantics, `g`/`i`/`m`/`s` flags, subject quote+escaped preserved; see the
    review-fixes section below).
  - list: `length`/`extract` (1-based, non-list = singleton)/`range`/the `~(…)`
    paren-escape; `each()` — see below.
  - math: `ceil/floor/sqrt/abs` keep the unit; `sin/cos/tan` unify→unitless;
    `asin/acos/atan` unify→`rad`; `round` = JS `toFixed` (ties AWAY from zero,
    computed on the exact decimal expansion, not a float multiply —
    `(0.615).toFixed(2) == "0.61"`).
  - number: `percentage` (unifies first), `convert`, `pi`, `mod` (truncated, JS
    `%`), `pow`, and **`min`/`max` as a faithful `minMax` port** (§4.8): unit
    groups reduce, list args flatten, incompatible/non-dimension args abort to
    the literal-CSS passthrough (`min(1, 4ex, 2pt)`, `min(calc(1 + 1), 1)`).
  - types: all `is*` (incl. `isruleset`, `isunit(v, '')` = unitless), `unit`
    (keyword unit → name, anything else → its `toCSS`, so a quoted unit keeps
    quotes — faithful), `get-unit`.
  - color def: `rgb`/`hsl` (incl. the space-separated `rgb(0 128 255 / 50%)`
    form riding the deferred `/` Operation), `rgba`/`hsla` (color first arg
    reuses channels; alpha NOT clamped — `rgba(0,30,0,238)` → `#001e00`),
    `hsv`/`hsva` (the perm-table port), `argb` (`#AARRGGBB`), `color()` (quoted
    hex keeps its spelling; keyword/color clears the original → hex out).
  - channels: `hue/saturation/lightness`, `hsv*` (new `Color::to_hsv`),
    `red/green/blue/alpha` (unrounded — fround happens at genCSS),
    `luma` (gamma, new `Color::luma`)/`luminance` (linear).
  - ops: `saturate/desaturate/lighten/darken/fadein/fadeout` (+`relative`),
    `fade`, `spin`, `mix` (SASS formula), `tint`/`shade`, `greyscale` (British
    only), `contrast` (dark/light auto-swap by luma, threshold 0.43,
    given nodes returned verbatim). All rebuild through the shared
    `hsla(origColor, …)` so an `rgb`/`hsl`-written input keeps its output form
    and everything else prints as `rgb` (hex when opaque).
  - blending: the compositing-1 port incl. the alpha correction (`softlight`'s
    `(d - cb)` sign verified from source).
  - resource: `data-uri` (mime table in `data/mime.rs`; text vs base64 via the
    `text/*` charset rule; svg never base64; `#fragment` kept; missing file →
    `url(path)` fallback; **no size cap — 4.6.7 dropped the IE-32KB check**,
    verified in the vendored source), `image-size/-width/-height`
    (PNG/GIF/JPEG/SVG header sniffing) — both read real bytes through the new
    `ImportResolver::load_binary` hook (default `None` keeps pure callers pure;
    the harness + `lessc` example resolve fixture-relative), `svg-gradient`
    (encodeURIComponent parity, stop-position/opacity rules).
  - `if`/`boolean` (lazy, condition arg rendered back to text and run through
    the Phase-2 guard grammar — less.js routes them to `parsers.condition`),
    `isdefined` — evaluator-dispatched (`evalArgs: false`).
  - Named-color keywords coerce to `Color` at the function boundary (less.js
    converts at parse time), so `lighten(blue, 10%)` works and `iskeyword(red)`
    is false, both faithful.
- **Unknown-function passthrough** (§2.7): unregistered names — and registered
  ones whose args don't fit (`saturate(5%)`, `contrast(30%)`) — re-emit
  `name(evaluated-args)`. Math turns ON inside any call's arguments (less.js
  `Call.eval` sets `mathOn = !calc`): `foo(1 + 2)` → `foo(3)`, `foo(10/2)` stays.
- **calc** (§2.4): math suppressed, `inCalc` flagged; variables dereferenced
  inside calc fold their own operations (the `_SELF` wrap); paren output follows
  `Expression.eval` exactly — `Node::Paren` grew an `in_op` flag (`parensInOp`)
  and literal parens survive only for in-operation parens whose math didn't run
  and whose result isn't a folded number, with the `doubleParen` collapse.
- **Strings** (§2.14): iterated interpolation is now regex-faithful
  (`/@\{([\w-]+)\}/` per pass, to a fixpoint — `@{box-@{suffix}}` resolves
  inner-first); interpolation replacements render UNROUNDED (less.js calls
  `toCSS()` with no context — `pi()` interpolates as `3.141592653589793`);
  a `@{a}_checked` glued word parses as one escaped Quoted (the
  `permissiveValue` capture), so no stray space.
- **IE filters** (§2.17): call names extend over `progid:[\w.]+(`; `key=value`
  args are a new `Node::Assignment` (evaluated value, `key=value` genCSS);
  `alpha(opacity=…)` is a PARSER special case (less.js `ieAlpha` via
  `customFuncCall` — see the review-fixes section below).
- **anonymousValue** (the less.js declaration fast path): a value with none of
  ``.#@$+/'"*`(;{}-`` up to `;` is captured VERBATIM (raw whitespace/newlines,
  inline `!important`) — this is what `whitespace` byte-parity needed.
- **Custom properties** (§2.16): values now try less.js's `permissiveValue`
  entity chain first — a fully-entity-parseable value IS structured and
  evaluated (`--x: rgba(0, 30, 0, 238)` → `#001e00`) while anything else keeps
  the raw capture with `@{}` interpolation only.
- **Value comments**: block comments between expression items are kept as
  `Comment` nodes (rendered in values, filtered out of registered-function args
  like `functionCaller`, kept in passthrough re-emits).
- **merge `+:`/`+_:`** (§2.10, pulled forward from Phase 4): a
  `ToCSSVisitor._mergeRules` port at output time (`merge/merge` green).
- **`each()`** (§2.7): statement-level expansion binding `@value`/`@key`/
  `@index` (or anonymous-mixin `.(@v, @k, @i) { }` params — call args now parse
  `{ … }` detached rulesets, anonymous mixins, and less.js's semicolon-group
  argument rule); detached-ruleset lists iterate their declarations by name.
- **Misc fixes that fell out**: `@arguments` now includes unnamed-variadic and
  pattern-param positions; `\9`/`\5FAE` escape tokens in values; `/deep/`
  slashed combinators; `@namespace` no longer hoisted (verified: less.js leaves
  it in source position); prelude comments relocate into the at-rule block and
  comment-only `@media` blocks prune (comment-only `@keyframes` don't);
  `Unit::cancel` adopts a missing `backupUnit` from the first numerator
  (`(1 * 10px) * 14cm` → `140px`); whitespace tolerated around the merge flag
  (`prop + :`); statement-level function calls (`e('…');` at root) evaluate and
  emit verbatim.

### DEFERRED from Phase 3 (needs later-phase subsystems)

- `functions/functions` — needs detached-ruleset CALLS (`@dr()`), `$prop`
  property accessors, `{…}` branches inside `if()` args, AND the less.js
  test-runner's registered custom functions (`_color`/`increment`/`add` — the
  harness would have to mirror `test/index.js`'s registry); revisit with Phase 4.
- `functions-each` — the remaining reds are maps/lookups (`@schemes[@@name]`,
  `@one[@two]`) — §2.12. (Mixin-calls-as-list-args, `each(.set-2(), …)`, landed
  with the review fixes below.)
- `urls` — `data-uri`/`svg-gradient`/`image-size` themselves are done and
  byte-verified, but the fixture needs `@import` inlining + rootpath-relative
  URL rewriting (§2.9, Phase 4).
- `each()` over rulesets with variable declarations skips them (less.js iterates
  them as Declarations); no in-scope fixture observes the difference.

### Phase 3 review fixes (adversarial audit vs less.js 4.6.7)

Four review lenses audited the Phase 3 implementation against a locally
installed `less@4.6.7`; every confirmed in-phase finding below was reproduced,
fixed, and re-verified against the real compiler (pass rate stays 48/87 — the
fixes close probe divergences; the still-red fixtures wait on Phase 4 subsystems).

- **fround is now decimal `toFixed`, never a float multiply** (C13/F1, systemic):
  `Node.fround` is `Number((v + 2e-16).toFixed(8))`; the previous
  `round((v+2e-16)*1e8)/1e8` crossed half boundaries the exact decimal expansion
  sits below (`179.999999995` → `180` instead of `179.99999999`; ~8.5% of random
  9-decimal literals diverged) and corrupted ≥1e21 values. `value::to_fixed`
  (the exact-decimal-expansion rounder, moved from `functions::math`) now backs
  `format_number` AND `Color`'s fround. Verified with a 4000-value random fuzz
  (byte-identical) plus the three color fuzz corpora.
- **JS `String(number)` spellings** (C21/F3/C7): `Infinity`/`-Infinity`, the
  ≥1e21 exponent form (`pow(10, 21)` → `1e+21`), and the <1e-6 exponent form for
  rgba/hsl argument joins (`rgba(0, 0, 0, 1e-7)`) — one `value::js_number_string`
  shared by dimensions and color components. (Dimension genCSS still masks the
  tiny branch with its own `toFixed(20)` guard, as less.js does.)
- **Error parity — the passthrough rule is now exactly less.js's caught set**
  (C9/F2/F4/F5/F16, systemic): `dispatch` arms return `Result<Option<Node>>`;
  an `Err` is wrapped ``Error evaluating function `name`: …`` like `Call.eval`.
  `Ok(None)` (→ passthrough) remains ONLY where less.js itself catches or
  null-returns: `min`/`max`, `rgb[a]`/`hsl[a]` bodies, the `saturate`/`contrast`
  non-color filter carve-outs, out-of-range `extract`. Everything else now
  hard-errors with less.js's messages: MathHelper's `argument must be a number`
  (incl. the very reachable `round(10 / 3)` under default math), `pow`'s
  `arguments must be numbers`, the Dimension constructor's
  `Dimension is not a number.` on NaN (`sqrt(-1)`, `mod(7, 0)`, `(0 / 0)` — also
  guarded in `Operation` eval; Infinity stays legal), `round`'s `toFixed()`
  RangeError outside 0..=100, `unit`'s first-arg throw (+ the
  forgotten-parenthesis hint), `convert`'s TypeErrors, the color functions'
  `Argument cannot be evaluated to a color` / `color functions take numbers as
  parameters` / missing-argument `Cannot read properties of undefined` family,
  `color()`'s keyword/hex Argument throw, `hsv(-60, …)`'s `perm[-1]` TypeError,
  and the string/list functions' missing-arg TypeErrors.
- **NaN propagation prints like JS** (C11): `.value` arithmetic coercion
  (`js_arg_num`: Dimension → number, string-ish → `Number()` — so
  `lighten(#800, "20")` WORKS and `banana` is NaN), NaN-preserving clamps (Rust
  `f64::max` swallows NaN; JS `Math.max` doesn't), NaN-poisoning `toHSL`/`toHSV`,
  and `NaN`-literal hex channels → `lighten(#800, banana)`, `mix(…, banana)`,
  `(#000000 / #000000)` all print `#NaNNaNNaN`.
- **Constructor forms** (C1/C2/C12/C19/C10): `rgba(1, 2, 3)`/`hsla(90, 50%, 50%)`
  with missing alpha re-emit the call (no invented `a = 1`, no `hsla`→`hsl`
  rename); `rgb(<color>)` reuses channels; the space form unpacks `val[0..2]`
  with NO length check (a slash-less 4th item silently drops); `hsv`/`hsva`
  errors propagate (missing args, negative hue).
- **`color('<keyword>')` fallback** (C4/F2-residual): quoted keywords go through
  `Color.fromKeyword` with the keyword cleared (`color("plum")` → `#dda0dd`,
  `color('transparent')` → `rgba(0, 0, 0, 0)`); invalid input throws.
- **Quoted `'relative'`** (C6): the adjusters' method check is
  `method.value === 'relative'` — true for Quoted/Anonymous too.
- **`e()`/`escape()` read the raw JS `.value`** (C16/C17/F1-str/F10/F21):
  a Color contributes its internal `rgb`/`hsl`/keyword/hex marker (`e(hsl(…))` →
  `hsl`; an operated color is undefined → `e` prints empty, `escape` prints
  `undefined`); a Dimension its bare full-precision number (unit dropped;
  `e(0px)` is empty via the Quoted ctor's `content || ''`); Expression/URL args
  JS-stringify to `[object Object]`.
- **`replace()` is a real JS replace** (F2/F3/F15/F20): `fancy-regex` compiles
  lookahead/lookbehind/backreferences; replacement patterns follow ES
  `GetSubstitution` (`$$` `$&` ``$` `` `$'` `$N`/`$NN` with out-of-range staying
  LITERAL, `$<name>` only when the regex has named groups); flags validate
  against `[dgimsuvy]` (invalid → error); numeric patterns coerce, numeric
  subjects error (`result.replace is not a function`).
- **fround application boundary** (F8): `unit()`'s unit-arg, `%()` args and
  `replace()` replacements render via context-less `toCSS()` — FULL float
  digits, no `numPrecision` (`unit(5, 1.234567891px)` → `51.234567891px`).
- **ieAlpha is a parser special case** (C14/C15/F9-math, §2.17): `alpha(` tries
  `/^opacity=/i` — a hit commits to `<digits>` or `@var` (→ `@{var}`
  interpolation) + `)` and emits the lowercase-normalized escaped literal
  (`alpha(Opacity=87)` → `alpha(opacity=87)`); `opacity=87.5` / `opacity=` /
  `opacity=@{v}` are parse errors (the old path emitted corrupted output for
  `alpha(opacity=)`); a miss falls back to normal args (`alpha(opacity = 87)`
  then errors in the color function, like less.js).
- **`url()` follows the less.js grammar** (F6-url/F7-residual/F7/F8-url): the
  content is a quoted string, a bare `@variable` (**resolved at eval** —
  `url(@a)` with `@a: 'Trebuchet'` → `url('Trebuchet')`; `url(@{a})` stays
  verbatim in both), or the raw run `(?:\\[()'"]|[^()'"])+` — whitespace inside
  is kept (`url(spaced.png  )`, leading trimmed only) and `url(fn(x))` is the
  same `expected ')' got '('` parse error as less.js.
- **Interpolation quote boundary** (F18): selector and property-NAME
  interpolation genCSS the value (quoted variables keep quotes: `."sel" {`,
  `"color": red`); string/permissive interpolation still strips them.
- **Duplicate-declaration removal** (C22/F1-residual — unspecced in the plan,
  found in less.js `toCSSVisitor._removeDuplicateRules`): per ruleset, walking
  backwards, an earlier declaration whose name AND rendered CSS match a later
  one is dropped; `!important` variants differ and both stay.
- **Root-level guards** (F11/F16): declarations at the stylesheet root error
  (`Properties must be inside selector blocks. They cannot be in the root` —
  also covers `each()` bodies at root), and a detached ruleset evaluated on a
  real property errors (`Rulesets cannot be evaluated on a property.`).
- **Permissive values** (F5/F6-residual): a backslash escapes the next byte in
  the raw capture (`--v: ( x; // i\'m serious; )` no longer swallows braces to
  EOF), and block comments between permissive entities are kept
  (`--value: a/* { ; } */` renders `a /* { ; } */`).
- **`each()` over a mixin call** (F3-residual): `each(.set-2(), …)` parses the
  arg as a mixin call (the less.js `mixinLookup` route) and iterates its emitted
  declarations.

Known deviations kept (deliberate): error MESSAGE kinds render with the
provisional `Kind: ` prefix (§F3 renderer is a later phase) and a few TypeError
texts differ in wording only (`color(5)`, fancy-regex's invalid-pattern detail);
both sides still fail compilation identically. Out-of-phase findings collected
for Phase 4/parser follow-ups: scientific-notation lexing (`1e-7` must lex as
`1e` minus `7` — C8/F6/F12), invalid hex literals accepted (C18), `//` in a
same-line declaration value swallowing the rest of the line (F9-str), quoted
`+` operations tolerated (F13), comments inside mixin-DEFINITION parameter
lists (F4-residual), `@var` permissive brace values evaluated as rulesets
(F9-residual), `@media` feature fround (F11-math), parse tolerance for
`b: img.png` / `unknown(1, , 2)` / parenthesized args like
`lighten(#800, 20%, ('relative'))` (F10-info), detached-ruleset calls (F17 —
**resolved in Phase 4A**), and the harness's missing less.js test-runner custom
functions (`_color`/`add`/`increment`, F8-residual — moot until Phase 4 makes
`functions/functions` otherwise reachable).

### Phase 4A — detached rulesets, maps/lookups, at-rule bubbling (§2.11–§2.13)

64/87 (floor 48→64; 16 fixtures newly green: `mixins/maps`, `nesting`,
`namespace-targeted`, `parser-property-interp`, `property-accessors`,
`property-targeted`, `functions-each`, `detached-rulesets`, `media`,
`container`, `layer`, `at-rules`, `at-rules-targeted`,
`at-rules-keyword-comments`, `directives-bubbling`, `starting-style`). Every
non-obvious semantic below was probed against a local `less@4.6.7` (several
multi-construct batches diffed byte-identical against `lessc`).

- **Detached rulesets (§2.11).** `@dr: { … }` values capture the frames at
  their DECLARATION site (less.js evaluates every declaration during
  `Ruleset.eval`, so the DR's `frames` snapshot is the defining scope — a
  `@a: inner` in the *calling* ruleset must NOT win). Implemented as a pass-0
  in `eval_rules`: DR-literal variable values in the just-pushed frame are
  wrapped into `Node::Closure` entries indexing the captured-frame side table;
  `eval_value` on a bare DR literal (mixin args, injected values) captures at
  evaluation site — which is exactly less.js's `Variable.eval` fallback for
  never-evaluated frames (namespace bodies). `@dr();` call = `callEval`:
  captured frames ++ caller frames, body evaluated at the call position;
  **variables never splice back** ("do not pollute the scope at all") but
  mixin definitions/rulesets do (the "unlocking mixins" case). A value-position
  `@dr()` without a `[lookup]` is less.js's `Missing '[...]' lookup in variable
  call` PARSE error, `@dr() !important` its `Unrecognised input`; a call on a
  non-DR value errors `Could not evaluate variable call @x`; an unknown name is
  the plain `variable @x is undefined`.
- **Mixin/DR/import bodies get a FRESH media context** (the ordering subtlety
  that broke `detached-rulesets`): less.js's `contexts.Eval` copies neither
  `mediaPath` nor `mediaBlocks` into a call's context, so `@media` blocks
  inside a called body materialize standalone and are RE-merged (the "spliced
  rules are re-evaluated" pass) against the ambient media path at the call's
  source position. Mirrored by isolating `media_path`/`media_blocks` around
  pass-1 expansion and re-merging each produced `@media`/`@container` block in
  pass 2 (`absorb_expansion_outs`) — sibling media stay in source order.
- **Maps/lookups (§2.12).** `Node::Lookup` (less.js `NamespaceValue`) over a
  `VariableCall` or value-position `MixinCall` target: property keys (`[k]` ≡
  `[$k]`, merged via `+:`/`+_:` groups, LAST wins), variable keys (`[@k]`),
  dynamic keys (`[@@ref]`, `[$@ref]`), the unnamed `[]` (last declaration —
  property OR variable), chained lookups descending into nested DR values, and
  less.js's exact `property "x" not found` / `variable @x not found` errors.
  Mixin-as-map (`@p: .mk-map();` and `.m()[key]` / `#ns.mx(4)[r]`) evaluates
  every surviving candidate's body in the mixin's own scope, in source order,
  variables included. A looked-up declaration's raw-capture `!important` splits
  OUT of the value (the `parseValue` re-parse) and is dropped.
- **`$prop` accessors + `${prop}` interpolation (§2.12).** `Property.eval`
  semantics: the nearest frame with the property wins; ALL its same-name
  declarations merge; the last one's value evaluates in the ACCESSING context;
  its `!important` propagates to the reading declaration. Mixin-emitted
  declarations are now SPLICED into the caller's frame at the call position
  (pass 1), so `$color` last-wins interleaves correctly with call order — and
  the parseValue mutation quirk is reproduced: a *read* declaration that was
  already emitted re-renders its important as `red!important` (no space),
  while later same-name declarations re-normalize (order-faithful via a
  per-frame trim journal drained after each pass-2 rule).
- **At-rule bubbling (§2.13).** Nestable `@media`/`@container` merge nested
  same-kind features with `and` (comma lists cross-multiply, FIRST list varying
  fastest) and surface merged blocks as siblings of the outermost block in
  depth-first ENTRY order; mixed-kind nesting stays in place unmerged (the
  shared-path type check). Every other at-rule is a media boundary (backs up
  the path/collector). `isRooted=false` kinds (`@supports`/`@document`/
  `@starting-style`/`@layer`) bubble out of rulesets wrapping bare declarations
  in the parent's joined selector; rooted kinds (`@font-face`/`@keyframes`/
  `@page`/unknown) bubble bare. A value-less non-rooted at-rule whose evaluated
  body is pure declarations (`@starting-style`, bare `@layer` — incl. bodies
  produced by `each()`/merge) renders NESTED inside the enclosing rule
  (`simpleBlock`). Nested `@layer` blocks do NOT merge names. Media feature
  normalization: `(orientation:portrait)` → `(orientation: portrait)` with the
  value evaluated (vars/escaped strings resolve), `( width<500px )` →
  `(width < 500px)`, `and(x)` → `and (x)` but `style(…)`/`layer(…)` stay glued;
  `@media @smartphone` resolves. Empty `@media` blocks prune; empty
  `@container` blocks KEEP their shell (less.js prunes only `Media`).
  `@charset`: hoisted AND deduped to the first occurrence. Prelude comments
  (Gate T0 review C1/C1c, all probed vs 4.6.7): a comment directly before a
  `,`/prelude-end relocates into the block; in `@media`/`@container` a
  LEADING comment additionally lands in the block TWICE (less.js's feature
  scan passes it twice — a faithful quirk) and one right after a query `,`
  once; EVERY `@import` feature comment becomes a root-level sibling after
  the import (the commentStore flush). Still dropped (known divergence C1b):
  a comment between `and` and a feature paren — less.js embeds it INSIDE the
  paren (`and (/*! k */ color)`), a render-inside-feature-value case our
  string-shaped prelude model doesn't carry.
- **Stopgap `.less` `@import` inline** (full §2.9 two-stage machinery is 4B):
  resolved through the `ImportResolver`, parsed, evaluated at the import's
  position with the same fresh-media-context + re-merge treatment as mixin
  calls, top-level variables/mixins/rulesets spliced into the importing scope
  (subject to the caller-wins variable filter); `(optional)` misses are silent;
  `(inline)` splices the payload verbatim. `once`/`reference`/media-feature
  wrapping/path rewriting remain 4B (`import/*` fixtures stay xfail).
- **`!important` interplay (§2.15)** probed: `@v: red !important` propagates
  through DR bodies, and merge groups OR their flags — byte-identical to
  less.js. (The 4A claim that `.m() !important` forces importance into at-rule
  bodies was WRONG — less.js implements `makeImportant` only on
  Declaration/Ruleset/MixinDefinition; corrected in the Phase 4 review fixes
  below: Media/AtRule bodies pass through untouched.)
- **Selector join fix:** `os.replace('&', parent)` now trims only the LEADING
  whitespace — a trailing `&` at root keeps its descendant space
  (`.outOfMedia &` → `.outOfMedia  {`, two spaces, as less.js renders).

Known 4A deviations (deliberate, unfixtured): mid-feature prelude comments are
dropped rather than embedded in the feature value; `$@ref` outside `[]` parses
as a `$` accessor runtime error instead of less.js's ParseError; the `.less`
import stopgap above.

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

Each is gated by its own fixture slice. **Phase 2 (mixins + guards +
pattern-matching + namespaces) is DONE** — see the Phase-2 section above; the
residual mixin fixtures it couldn't green (`maps`, `namespace-targeted`,
`mixins-interpolated`, the 14 residual `mixins-guards` lines) are documented there
and fold into Phase 4's map/property-accessor work.

- **Phase 3 — full function library** (plan §2.7, Phase 3): **done** — see the
  Phase 3 section above (48/87). Residue: `functions-each` greened in Phase 4A
  (maps/lookups + mixin-as-list); `functions/functions` now waits only on the
  harness's less.js test-runner custom functions (`_color`/`add`/`increment`);
  `urls` waits on Phase 4B import inlining + URL rewriting (the resource
  functions themselves are byte-verified).
- **Phase 4 — imports, extend, detached rulesets, merge, at-rule bubbling, maps**
  (plan §2.8–§2.13, Phase 4):
  - **`@import` two-stage + all options** (§2.9): `once`/`reference`/`optional`/
    `inline`/`less`/`css`/**`layer`**, `strictImports`, interpolated paths, CSS
    re-emit with path rewrite. Unlocks `import/*`.
  - **`:extend` full** (§2.8): finder → chaining fixpoint → replace, the `all`
    keyword, `@media` scoping incl. `(reference)` media stacks. Unlocks `extend*`.
  - **detached rulesets** (§2.11), **merge `+:`/`+_:`** (§2.10), **maps/lookups**
    `@p[key]`/`#ns[k]` (§2.12): **done in Phase 4A** — see the Phase 4A section
    above (unlocked `detached-rulesets`, `mixins/maps`, `property-accessors`,
    `property-targeted`, `namespace-targeted`, `functions-each`, `nesting`,
    `parser-property-interp`; `merge` was already green).
  - **all-rule bubbling & output ordering** (§2.13): **done in Phase 4A** —
    `@media`/`@supports`/`@container`/`@layer`/`@namespace`/`@property`/`@page`/
    `@charset` (unlocked `media`, `container`, `layer`, `directives-bubbling`,
    `at-rules`, `at-rules-targeted`, `at-rules-keyword-comments`,
    `starting-style`).
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

### Phase 4B — `:extend`, full `@import`, URL rewriting, tests-config corpus

122/127 (floor 70→122 across the phase; the corpus grew 87→127 with the
option-driven `tests-config/` suites). The subsystems:

- **`:extend` full (§2.8)** — a faithful port of less.js 4.6.7's
  `ExtendFinderVisitor`/`ProcessExtendsVisitor` re-targeted at the evaluator's
  flattened `Out` tree (`eval/extend.rs`): selector-attached + body `&:extend`,
  exact vs `all` fragment matching at simple-selector granularity (rendered
  paths are re-tokenized into less.js `Element`-level runs; combinator-
  sensitive; attribute quote style normalized; non-overlapping matches),
  `doExtendChaining` with `parent_ids` circular cuts and the 100-iteration
  `extend circular reference detected` error, per-at-rule scope stacks (outer
  extends propagate in, never out — validated by the post-bubbling sibling
  structure matching less.js's post-eval tree), the `extendOnEveryPath`/
  `extendList` matching skips, and `(reference)` visibility: per-selector
  `visible` flags, hidden subtrees marked dark, a visible extend's graft
  re-enabling exactly the matched rule, pruned after the pass. The
  `has no matches` warning mirrors `checkExtendsForNonMatched` (root-scope
  originals only). Multi-`&` selectors now cross-multiply over parent paths
  (`parents^N`, leftmost slowest — extend-nest's amp-test).
- **`@import` follow-ups on the 4B-checkpoint machinery (§2.9)** — the
  ImportResolved node carries only the DIRECT `(reference)` flag (inherited
  hiding is the enclosing import's visibility blanket at eval, so mixins
  replayed from visible call sites emit); inline imports splice into the
  enclosing declaration block; mixin lookup descends into un-inlined import
  roots inside namespaces; `strictImports` gates ruleset-level imports;
  `processImports=false` drops unfetched LESS imports (the upstream
  `[#.&?]css([?;].*)?$` path test decides what still re-emits).
- **URL handling (§2.18)** — `URL.eval` ported: `pathRequiresRewrite` (`local`
  = explicit `./`-relative; `all` AND `off` = plain-relative — upstream's
  `off` differs only in skipping the per-file rootpath accumulation in
  `ImportManager.push`), rootpath prepend with unquoted-path escaping
  (`folder (1)/` → `folder\ \(1\)/`), `normalizePath`, and `urlArgs` (after
  `?`/before `#`, never on `data:`). Import path rewrite shares the rule.
- **Option wiring (§2.0)** — `banner`/`globalVars`/`modifyVars` as source-text
  augmentation (`serializeVars`), `strictUnits` output (a fully-cancelled unit
  renders empty, never the backupUnit guess), the `font`/math=always
  shorthand bypass (`Declaration.eval` drops to parens-division), the legacy
  `./` forced-division operator, and `LessOptions::custom_functions` — the
  minimal `functionRegistry.add` surface the harness uses for the runner's
  `add`/`increment`/`_color`.
- **Parser debt (the Phase-3 catalog, each probed vs 4.6.7 first)** — no
  scientific notation (`1e-2` = `1e` minus `2` → `-1e`); invalid hex =
  ParseError; comments between property name and `:` drop while post-comma
  value comments render; a stranded `+`/`*` after a quoted literal errors
  (quoted strings are not less.js operands — and `operand()`'s real set is
  enforced in multiplication, so `small/20px` stays an entity list rendering
  spaced); comments stripped from mixin param/arg lists; the sticky
  `expand` args-parser quirk (once an arg spreads, every later list-valued
  arg spreads too) reproduced faithfully.
- **Residual fixture work** — namespacing (bare lookup guards, each()'s
  source-map frame, operable re-parsed lookup values, mixin-call map
  contributions, declaration-shaped parens kept verbatim, media-prelude
  lookups incl. namespaced mixin-call targets), selectors (pseudo-class heads
  are never mixin splits; comment-aware balanced scans), parse-interpolation
  (interpolated selectors re-parse as selector groups; glued bracket runs),
  permissive-parse (variable declarations fall back to the brace-balanced
  permissive capture — glued-colon heads only — and raw captures resolve bare
  `@refs`), mixins-interpolated (`interp_name` resolves `@{}` in mixin/
  namespace names during candidate lookup; injected rulesets freeze frames as
  closures so `.@{param}` names resolve later).

Remaining red (1 after the Gate T0 compress stretch below): the three
compress fixtures greened with the §C4 serializer (see "Gate T0 compress
serializer" at the end of this file); `config/3rd-party/bootstrap4` — needs
JS `@plugin` execution (out of scope, §8; the plan's classification keeps it
in-scope, so it stays a permanent xfail). `import/import` (the other
JS-plugin fixture) is reclassified OUT per plan §5.2 — see the Gate T0
error-half section.

Known deviations kept (noted while porting): ~~URL rewriting uses the current
eval file's rootpath~~ (FIXED in the Phase 4 review — urls and resource calls
are now stamped with their declaration-site file info, see below); the extend
carrier inside a bubbling
`@media` under a selector-attached extend does not re-collect in the media
scope (less.js's derived media ruleset shares the selector objects); strict
units error only through `Dimension::operate`'s existing checks (the genCSS
multi-unit throw is not modeled).

### Phase 4 review fixes (adversarial audit vs less.js 4.6.7, post-4B)

Four review lenses (imports+URLs, :extend, detached rulesets/maps, at-rule
bubbling/residuals) audited a0fd6c6 against real lessc 4.6.7. All confirmed
in-phase findings folded in; every fix re-verified against lessc (probe
drivers in the session scratchpad) plus ~350 reviewer probes re-run as a
regression smoke. Corpus stays 122/127 (floor 122, 5 xfail), 138 unit tests.

**Imports (§2.9):**
- **BFS once-slots (F13)** — stage 1 now fetches breadth-first by file visit
  (the less.js `ImportSequencer` registration order): all of a file's imports
  claim their once-slots before any import discovered inside a fetched
  subtree, so a root-level import beats a nested one (output ORDER and, under
  `(reference)`, VISIBILITY both flip vs the old depth-first walk).
- **`layer(...)` imports (F1)** — a LESS import whose feature list is a single
  expression opening `layer(...)` re-emits the literal `@import` (less.js
  `layerCss`); the file is still fetched (once-slots consumed), its rules
  discarded. Bare `layer` and comma lists still @media-wrap.
- **Saved path-eval errors (F2/F4)** — a variable-path import whose path can't
  interpolate at import time stores the NameError on the node; at eval the
  re-evaluated path decides: css-shaped → literal re-emit, else the saved
  error RETHROWS (so a mixin-param-interpolated `.less` path fails even though
  the path resolves at eval). The root frame used for var-import interpolation
  is SNAPSHOT at the first variable import (modeling less.js's stale
  `Ruleset.variables()` memo) — chained variable-path imports now fail
  exactly like less.js.
- **`(reference)` css imports emit nothing (F6)**; **`(optional)` swallows
  parse errors too (F10)**; **inline payloads strip a UTF-8 BOM (F14)**;
  **unknown import options are a parse error (F9)**; **root-level
  "Properties must be inside selector blocks" fires through reference-hidden
  imports (F15)**; **strictImports + a real ruleset-level import fails the
  compile** (a duplicate/skip one silently drops, matching the fixture golden)
  (F5); **the stage-1 walk has a depth cap** (`max_eval_depth`, default 64) so
  a `(multiple)` self-cycle errors cleanly instead of overflowing (F7).
- **Harness FsResolver `.less`-append (F12)** — now mirrors less.js's
  `/(\.[a-z]*$)|([?;].*)$/`: lowercase-only extensions and `?`/`;` suffixes
  exempt; `up.CSS` gets `.less` appended (the raw path is never tried). This
  is the documented contract for orchestration resolvers.

**URL rewriting (§2.18):**
- **Declaration-site fileInfo (F3/F8)** — the import pass stamps `url(...)`
  values and resource-function calls (`data-uri`/`image-size`/`-width`/
  `-height`) with a `FileTag` (rootpath + directory of the file the token was
  WRITTEN in, per import statement); eval unwraps `Node::WithFile` through a
  `decl_file` stack. Urls in variables/mixins defined in imported files now
  rewrite against the DECLARING file in both directions; `data-uri` resolves
  against the declaring dir when rewriteUrls is on (entry dir when off) and
  its missing-file fallback url is rewritten too.
- **Slashless rootpath (F11)** — a non-empty `rootpath` without a trailing
  `/` gets one appended (core less.js parse-setup normalization).

**:extend (§2.8):**
- `!all` accepted as a synonym of `all` (E1); attribute case-flags
  (`[a="v" i]`) ignored in the structural compare and ops inside quoted
  values no longer corrupt `parse_attr` (E2/E8); interpolated extend TARGETS
  never match — less.js needles keep non-string element values (hay-side
  interpolation still matches; E5); grafted selectors keep the leading space
  of an explicit first combinator (E7); the no-match warning renders less.js's
  leading space (`extend ' .zzz' …`) and dedups by (source span, selector) so
  two extends at different positions warn twice (E3/E6); the parse-time
  "Targeting complex selectors…" warning is emitted (approximated at eval,
  deduped by span, message-only — no file/line excerpt) (E4); the circular
  error text matches less.js's spacing (kind label still differs, see below)
  (E10).

**Detached rulesets / maps (§2.11–§2.12):**
- Lookup recursion is depth-guarded (self-referential maps error cleanly;
  P4DR-1) and `mixin_call_map` deduplicates frames like statement calls (a
  recursive mixin-as-map lookup errored after minutes of exponential frame
  growth; P4DR-2). Named lookups take the LAST merge-flagged declaration RAW
  (no `merge_rules` — `$prop` still merges; P4DR-3). Whitespace inside lookup
  brackets is skipped (`[ key ]`; P4DR-6); combinator namespace paths parse
  in value position (`#ns > .m()[k]`, `#ns .m()[k]`; P4DR-7); dynamic keys
  (`[@@k]`/`[$@k]`) use the raw node value (a Dimension keys by NUMBER, unit
  dropped; P4DR-8); `@dr()[k]` and quoted/interpolated keys are parse errors
  (P4DR-9/10); the undefined-namespaced-mixin error renders the path glued
  (`#a.b.m is undefined`; P4DR-12).

**At-rules (§2.13):**
- `.m() !important` no longer forces importance into at-rule bodies
  (@media/@supports/@font-face, incl. through DR calls) — `make_important_out`
  stops at `Out::At`/`Out::Nested` (F4/P4DR-5).
- Bare declarations at a rootish level keep SOURCE ORDER relative to nested
  blocks: they now flow through the child stream as `Out::Decls` runs instead
  of being hoisted (`@page { @top-center{…} margin: 1cm; … }` and root
  @media/@supports bodies interleave like less.js) (F9).
- Merged nested @media/@container blocks DROP bare declaration runs (at every
  merged depth, incl. mixin/import re-merges) and prune when reduced to
  comments; root `@container` drops bare decls AND comments, keeping the
  empty shell (F6/F11/F12/F10).
- Escaped-string media features keep embedded commas as ONE entry through
  merges (`~"tv, projection"`), while a parsed comma-list variable
  (`@pair: screen, print`) still cross-multiplies — the prelude is split
  BEFORE variable resolution and whole-part variables are evaluated as nodes
  (F3). Comma tokens glue to the preceding feature in query normalization.
- Media prelude validation: preludeless `@media {`/`@container {` →
  "Value requires an array argument"; empty feature parens → "badly formed
  media feature definition"; `@{var}` inside parenthesized features →
  ParseError "Missing closing ')'"; uppercase at-rule names (`@MEDIA`) →
  the less.js variable-call ParseError (F13/F5/F8).

**Deliberately NOT modeled (documented skips):**
- **P4DR-4 — less.js's order-dependent DR closure-capture flip** (a stale
  `Ruleset._variables` memo makes a DR declared after any sibling variable
  lookup capture at the CALL site instead of the declaration site, flipping
  def-vs-caller precedence). This models a less.js caching accident whose
  trigger is the position of unrelated sibling rules; the crate's pass-0
  closure capture always uses the declaration site (the common orderings
  match; the flip cases diverge). Revisit only if a real theme hits it.
- **Mid-query prelude comments** are still dropped rather than relocated into
  the next feature's parens (the 4A deviation; cosmetic).
- **Upstream less.js crash parity (F7-info):** `media > ruleset > @import` of
  a media-containing file crashes less.js 4.6.7 ("Maximum call stack size
  exceeded"); the crate compiles the sane re-merge. Matching a stack overflow
  is not meaningful — flagged for corpus awareness.
- **Error KIND labels** still render the crate's 7-kind names (`Runtime:` vs
  less.js `SyntaxError:` etc.) — the error-format phase owns byte-exact
  rendering; messages/positions match.

**Out-of-phase findings catalogued for later phases:**
- **E11 (parser):** `a:hover #main { }` (type+pseudo followed by a
  whitespace-separated non-hex `#id`) is a hard parse error — the
  declaration/ruleset backtracking commits to the `ident:` declaration path
  and dies on the `#id` value token. Also `p:before #i1`,
  `span:nth-child(2) #i1`, `span:hover>#i1`.
- **E9 (serializer):** `[ a = v ]` attribute selectors are not re-rendered
  normalized (`[a=v]`) — less.js rebuilds Attribute nodes structurally; the
  crate keeps bracket text verbatim (extend MATCHING is already structural).
- **P4DR-11 (selector join):** `&`-only selectors at root (`& + & {}`)
  emit garbage instead of being pruned (less.js drops rules whose joined
  selector is empty) — milestone-1 JoinSelector/genCSS empty-path pruning.

## Gate T0 error half — the 7-kind error renderer + the 74-fixture error corpus

**200-fixture Gate T0 state: compile 122/126 · error 74/74 (floor 196; 4
xfail).** The error corpus is FULLY green. Every semantic below was read from
less.js 4.6.7 SOURCE (`less-error.js`, `parser.js`, `parser-input.js`, the
tree nodes) and, where goldens looked surprising, probed against a locally
installed `less@4.6.7`.

### Corpus classification (plan §5.2 — the meta-test)

- In-scope = **126 compile + 74 error = 200**; OUT = exactly the classified
  **36**: compile 17 (plugin/`@plugin` x8 — now including `import/import`,
  which loads plugin-simple and calls the plugin-defined `pi-anon()` —,
  sourcemap x5, debug-linenumbers x3, inline-JS x1) + error 19 (`@plugin`-error
  x15 = `functions-*` + `root-func-undefined-2`, plugin-config x3 =
  `plugin-1/2/3`, inline-JS x1 = `javascript-undefined-var`, whose golden is
  interpolation INSIDE an executed backtick under `javascriptEnabled: true`).
- `CLASSIFIED_OUT` in tests/fixtures.rs pins all 36 names; the
  `meta/corpus-classification` trial asserts the list is exactly 36 distinct
  names, that the on-disk runnable set splits into exactly 126+74 in-scope
  entries, and that the floor lists only name in-scope fixtures — a tag bump
  adding a fixture fails loudly. The 19 OUT error fixtures are excluded at
  VENDOR time (scripts/vendor-less-testdata.sh; VENDOR.txt documents it).
- The disabled-JS error surface (§C-jserr) is IN-scope by unit test: backticks
  parse to a `Node::JavaScript` and eval raises the byte-exact `Inline
  JavaScript is not enabled. Is it set in your options?` (locked in
  `eval::tests`; the `tests-config/no-js-errors` file itself is not vendored —
  including it would break the plan's 200/36 arithmetic). With
  `javascriptEnabled: true` the `@{…}` interpolation still runs first (its
  NameErrors surface like less.js's); actual execution reports unsupported.

### The renderer (`error.rs`, ported from `less-error.js` `toString()`)

- Label = `${type}Error:` — the crate's `ErrorKind::Import` carries less.js's
  `'File'` type, so it renders `FileError:` (import-missing).
- ` in <file>` iff filename; ` on line L, column C:` iff located (column
  stored 1-based = less.js's 0-based + 1).
- Excerpt = `lines[line-2..=line]` of `split('\n')`: missing neighbours are
  SKIPPED, but an empty-string line renders `N ` with a trailing space; the
  gutter is the plain number (NOT right-aligned); the string ends `\n`.
- No filename ⇒ `Kind: msg\n\n` — less.js's `undefined !== null` quirk
  (namespace-*-not-found goldens).
- The harness compares byte-exact up to TRAILING newlines: two vendored
  goldens (property-undefined, recursive-property) carry a stale extra `\n`
  that real lessc 4.6.7 verifiably does not emit (probed) — the plan's ground
  truth is the binary.

### Provenance plumbing (§5.5)

- `LessError` carries `(kind, message, filename, index)` + located
  `line/column/extract`; `locate()` fills from a normalized source.
  `wrapped` ports less.js `Call.eval`'s `hasOwnProperty('line')` wrap guard.
- Parser errors locate at construction. Eval errors locate via the FILE-SCOPE
  stack: `FileScope`/`ImportResolved` now carry the file's normalized source
  (`Arc<str>`); `compile()` threads the entry source (`eval_with_source`).
- The less.js error-anchoring chain is ported: **Call.eval** re-anchors any
  un-wrapped error at the call's index with the ``Error evaluating function
  `name`: …`` wrap (arg-eval errors included; `Node::Call` gained a `span`);
  **MixinCall.eval** re-anchors SELECTIVELY (Gate T0 review F2): candidate
  selection — arg eval, arity/pattern match, GUARD evaluation — runs outside
  less.js's try/catch and keeps its own index (an undefined guard variable
  cites the guard), while BODY errors rethrow `{...e, index: call, filename}`
  (nested calls surface at the outermost call) and the terminal
  `is undefined`/`No matching definition`/ambiguous-`default()` errors carry
  the call index directly; **Declaration.eval** anchors index-less errors
  (operations' plain throws) at the declaration. Variable/Property/
  NamespaceValue errors carry their own index (plain `@var` included since
  the Gate T0 review fixes — `eval_variable` uses its span, sanity-checked
  against the current file's source so re-parsed guard/prelude text can't
  excerpt garbage, with a `find_token_in_source` fallback; `$prop` at the
  accessor, lookup `[k]` at the bracket — the value-position `@dr[k]` span
  starts at `[`, matching less.js's post-name `i`); the mixin-call lookup
  form (`#ns[k]`) has NO fileInfo upstream and stays honestly location-less.
  String-interpolation failures anchor at the OPENING QUOTE (the Quoted
  node's index in less.js), selector/media interpolation at the `@{` token.
- The root-properties error blames the DECLARATION's own site/file (recorded
  during eval where the file scope is live — mixin-emitted and imported
  declarations included), not the call/import position.
- Variable declarations now EAGERLY evaluate their value in pass 2 (discarded)
  — less.js's `Ruleset.eval` runs `Declaration.eval` on every rule, which is
  how `@a: darken(@a, 30%)` errors while never referenced.

### Error-site fidelity fixes (each verified against source/probe)

- `Incompatible units` / the new strict-units `Multiple units in dimension`
  check (post-eval walk) are plain `new Error` throws upstream ⇒ **Syntax**,
  anchored at the declaration. `Could not evaluate variable call` ⇒ Syntax.
- `Invalid % without number`: a bare `%` parses as a KEYWORD and errors only
  when it would RENDER (post-eval walk incl. re-emitted call args) — so
  `unit(100, %)` stays legal.
- svg-gradient ports less.js's exact error ladder (shape check before
  direction check; the 2-arg non-list defers to the direction error —
  svg-gradient3/6).
- The import machinery: resolver-miss ⇒ `FileError: '<path>' wasn't found.
  Tried - <list>` at the `@import` in the importing file (the harness resolver
  formats its candidate list npm-style to match the golden); `@import <word>`
  ⇒ `malformed import statement`; a missing `;` ⇒ `missing semi-colon or
  unrecognised media features on import` — both Syntax, reported at the
  `@import` (less.js resets `i = index` before erroring).
- Parser furthest-failure machinery (minimal port): value parens are `'('
  addition ')'` parsed STRICT-FIRST (less.js `sub`) — a stop short of `)`
  records a soft `Expected ')'` candidate and backtracks; if the whole
  statement fails the candidate is reported (parens-error-1/2/3 positions
  exact), if another alternative parses (`a:hover when (2 = true)`) it is
  cleared. Root `}` / EOF-in-block / unmatched input surface as less.js's
  `Unrecognised input` + `Possibly missing …` hints from the failure char.
- `$parseUntil` pairing in permissive captures AND the paren verbatim
  fallback: mismatched closers ⇒ `Expected ']'` at the offender; unclosed
  same-line quotes ⇒ `Expected '"'` at the opener
  (custom-property-unmatched-block-1/2/3).
- Selector-grammar errors: guards on multiple selectors (both less.js check
  sites, incl. the comma-lookahead `,\s*(not\s*)?\(` that keeps comma-OR
  guards working), extend-on-its-own, extend-not-at-end (position after
  whitespace), `@media`/`@container` `media definitions require block
  statements after any features` (missing AND unclosed blocks), `expected
  condition` after `when`, `expected ')' got ''` for `url(` in at-rule
  preludes, the mixed `;`/`,` mixin-arg delimiter state machine (both error
  positions exact), `@@x:` ⇒ `Unrecognised input` at the statement start, a
  lone `*` is not a property name, a bare spaced `:` ends a selector element,
  and invalid hex literals fail SOFT (the statement then dies at its furthest
  point — invalid-color-with-comment's post-comment `;`).
- CSS guards may not call `default()`: less.js primes `defaultFunc.error`
  during selector eval — reproduced with the Call-wrapped Syntax message,
  located by source search (guards are raw text without spans).
- At-rule preludes resolve bare `@var`s STRICTLY (undefined ⇒ NameError at the
  variable, located by token search; quoted strings in preludes are skipped —
  `@impor "…@import.less"` stays literal). Lenient mode is kept for raw
  permissive captures.

### Known deviations (deliberate)

- Two stale goldens tolerated via trailing-newline normalization (above).
- The guard/prelude token-search locators are heuristics (first standalone
  occurrence) — exact for the corpus, approximate in pathological inputs.
- `javascript-undefined-var`'s exact golden needs interpolation inside an
  EXECUTED backtick context; it is OUT (inline-JS x1) — our enabled-JS path
  interpolates then reports execution unsupported.

## Gate T0 compress serializer (§C4) + compile residuals

`LessOptions::compress` now drives a byte-exact compress mode of the final
renderer (never delegated to lightningcss, §9.4). Greened
`config/at-rules-compressed`, `config/at-rules-compressed-evaluation`,
`config/compression` → compile 125/126, ratchet floor 199 (125+74). Every
rule below was verified against live `lessc 4.6.7 --compress` probes, and the
whole default-options `tests-unit` corpus (78 runnable fixtures + the
resolvable `import/*` ones) byte-matches the live binary under `--compress`.

The rule set (each mirrors a `context.compress` branch in less.js's tree/*):
- **Structure** (`Ruleset.genCSS`/`AtRule.outputRuleset`): no indentation or
  newlines, selectors join `,`, `{`/`}` unspaced, root parts concatenate and
  the whole output is trimmed (`parse-tree`'s leading/trailing strip — no
  trailing newline).
- **`lastRule` semicolons**: the physically-LAST declaration of a rule block
  or of a root at-rule body drops its `;` — decided AFTER the silent-comment
  strip (toCSSVisitor order), so `a { x: 1; /* tail */ }` → `a{x:1}` while a
  kept bang comment preserves the `;` before it. The value-less non-rooted
  `simpleBlock` at-rules (bare `@layer`/`@starting-style`, nested or root)
  keep EVERY `;` — their compressed `outputRuleset` has no lastRule handling
  (probed: `@starting-style{opacity:0;}` vs `@page{margin:2cm;size:A4}`).
- **Comments** (§2.3): non-bang block comments are SILENT — stripped from
  rule bodies, at-rule bodies, and declaration VALUE trees
  (`css::strip_value_comments`, recursing Value/Expression/Paren/Call — the
  visitor analog), so `grey, /* blue */ orange` → `grey,orange`. `/*!` bang
  comments survive everywhere. An interpolated property NAME keeps its
  comments (flattened to a string at eval time, before the visitor — matches
  less.js).
- **Selectors** (`Combinator.genCSS`): spaces dropped around every
  non-descendant combinator (`a>b`, `c+d~.e`, `a^b`, `x^^y`, `a/deep/b`) —
  applied as a render-time transform (`compress_selector`) over the joined
  paths so extend/dedup identities never shift; quoted strings and
  paren/bracket groups untouched (`[title="x > y"]`, `:nth-child(2n + 1)` —
  less.js keeps both, they are element values not combinators).
- **Values**: `Value` comma lists join `,` (the ONE value join that
  compresses — `Call` args keep `, `, probed); `Dimension` drops the leading
  zero (`.5px`) and the unit of a frounded ZERO with a length unit (`0px` →
  `0` — but only for PARSED Dimensions: the `anonymousValue` fast path
  captures simple `0px;` runs verbatim, exactly like less.js, which is why
  `compression.css` keeps `0em`/`0px`); computed colors hex-shorten
  (`#aabbcc` → `#abc`) while written literals/named colors stay verbatim
  (`Color.value` short-circuit), and `rgba()`/`hsla()` args join `,` with the
  alpha's leading zero KEPT (dimension-only rule).
- **Media/at-rule headers**: feature lists join `,`; a paren feature's
  `key: value` colon compresses (it is a Declaration in less.js) and its
  value renders compressed; range-syntax dimension operands compress
  (`inline-size >= 0px` → `>= 0`); `and` keeps its spaces. Text from an
  ESCAPED string or a permissively captured raw value (`@tablet:
  (min-width: @size)`) stays verbatim — Quoted/Anonymous nodes render
  as-written even under compress, so their `: ` survives (the media.less
  `@smartphone` case). `@supports`/generic preludes are Anonymous raw text
  and stay verbatim (golden keeps `(display: grid)`).
- **Interpolation & substitution render with the eval context** (which
  carries compress in less.js): `@{list}` inside a string joins `,`
  (strings.less `~"Univers, @{test}"` → `Univers, Arial,Verdana,San-Serif` —
  the written `, ` stays, the interpolated list compresses), custom-property
  structured values compress (+ comment strip), raw-capture/prelude `@ref`
  substitution compresses, and the `min()`/`max()` literal passthrough joins
  args with `,` (`functions/number.js` `minMax`). Internal identity uses
  (guard compares, mixin matching, dedup keys) stay on the expanded form.
- A statement-level function call's result (`e('/* anything to unquote */')`)
  is emitted as a VERBATIM node, not a Comment — compress must keep it even
  though the text looks like a comment (css-escapes).

Pinned by `eval::tests::compress_serializer_matches_less_js` (unit) plus the
three fixture goldens. Harness gained `MQ_DIFF_DETAIL=1` (prints each red
compile fixture's first divergence, the `MQ_ERR_DETAIL` twin).

### bootstrap4 (the 1 remaining xfail) — diagnosis

First divergence chased to its root: `_variables.less:139` fails with
`darken(@link-color, 15%)` because `@link-color: theme-color(primary)` and
`theme-color` is a **JS plugin function** (bootstrap-less-port registers
`plugins/theme-color.js` et al. via `@plugin`). Without JS execution the call
stays unevaluated and every downstream color function on it fails. That is
the out-of-scope subsystem (plan §8) already named in the fixture-harness
header — not an engine bug; the fixture stays a permanent xfail inside the
126.

## Gate T0 review fixes (four adversarial audits vs less.js 4.6.7)

**Gate state after the review pass: compile 125/126 · error 75/75 (ratchet
floor 125+75=200; 1 permanent xfail = bootstrap4).** The corpus grew by
`config/no-js-errors` (see the classification note below), so the universe is
now 201 fixtures / 126+75 in-scope / 37 CLASSIFIED_OUT.

### Fixed (each probed against real less.js 4.6.7 before and after)

- **Undefined-variable anchoring (F1 family)** — `eval_variable` now raises at
  its span (the `@` token), with a source sanity check + token-search fallback
  (details in "Provenance plumbing" above). Covers value position, multi-line
  values, recursive definitions (RHS), string/url interpolation (the opening
  quote), selector interpolation, `@media` preludes/features, detached-ruleset
  calls, guards, and globalVars/modifyVars-injected declarations.
- **Guard errors keep their own index (F2)** — mixin-call re-anchoring is now
  selective, exactly less.js's try/catch extent.
- **UTF-16 columns (F4/R2)** — `LineMap::line_col_utf16`; every rendered
  column now counts UTF-16 code units like less.js's `utils.getLocation`.
- **End-of-parse messages (F5/F6)** — the crate-invented "expected a
  selector" is gone; `Unrecognised input` + the `'{'`/`'('`/reached-end
  suffix rules, with true furthest-position anchoring (message-less
  `soft_fail_pos`, unterminated strings anchor at the quote). `parse_quoted`
  now REJECTS unterminated strings like less.js's `$quoted` (close quote
  required before any bare newline/EOF).
- **Parse-error kind/message/anchor picks (F7/F11/F12/F14, E1/E2/E3, F4-fca)**
  — `@import (bogus)` = SyntaxError at the offending option char; unterminated
  import path = `malformed import statement`; `expectChar` at EOF = Syntax
  `got ''`; empty `:extend()` = `Missing target selector for :extend().`;
  `@keyframes @{n}` = `expected @keyframes identifier` at the `@`; `@CHARSET`
  column after the `$re` whitespace skip; interp-in-feature-paren
  `Missing closing ')'` stalls at the `@`; `@media @{x}` (defined or not) =
  the block-statement Syntax error at the `@` (mediaFeature has no
  variableCurly alternative).
- **OperationError (R1)** — `Operation on an invalid type` when math is on
  and an operand is not Dimension/Color, with less.js's parens-division
  deferred-`/`-left-operand exception; all 7 error kinds now reachable (§F3).
- **Compress C2** — an escaped-string media fragment joined with `and` stays
  verbatim (`substitute_query_words` wraps it; `normalize_media_query` emits
  the wrapped run untouched).
- **Media prelude comments (C1/C1c/C1i)** — see the amended §C4 rules above.
- **Classification (R4)** — `tests-config/no-js-errors` vendored and IN
  (upstream runs it through `testErrors`; plan §5.2's prose counts disabled-JS
  error fixtures in-scope even though its table tallied tests-error/ only);
  `tests-config/js-type-errors` vendored and CLASSIFIED_OUT (V8 TypeError
  golden of an executed backtick). Error denominator 74 → 75, OUT set 36 → 37
  — the reconciliation is documented in tests/fixtures.rs + VENDOR.txt.
- **S1** — `wrong number of arguments for X (N for M)` suffix mirrored from
  mixin-definition.js:177 (believed unreachable; matchArgs rejects first).

### Known divergences left OPEN (out of Gate T0's phase, catalogued)

Parser-acceptance surface (we accept what less.js rejects — invisible to the
error corpus, which can only catch reject-what-should-compile):
- `("x" - 1)` / `(#fff - bob)` / `(1 + true)` in source: less.js's strict
  `sub` grammar raises ParseError (`Expected ')'` / `Unrecognised input.
  Possibly missing opening '('`); we parse them — since R1 they now raise
  OperationError at EVAL (right rejection, wrong kind/position). (F8/F13/R3)
- Non-ASCII property names (`colöör:`): less.js's ASCII-only property regex
  makes this a ParseError; we parse the declaration and any error surfaces
  downstream. (F9)
- Valueless custom at-rules (`@bare-last;`, `@layer;`): less.js errors
  (`Missing '[...]' lookup in variable call` / an internal crash); we accept
  and emit. (E4)
- Attribute-selector interpolation in the NAME position
  (`[data-@{a}="x"]`): less.js raises `expected ']' got '@'`; we compile.
  The full-token form `[@{a}=…]` matches in both. (F5-fca)
- **C1b**: a comment between `and` and a media feature paren is dropped;
  less.js embeds it inside the paren.
- Cross-file mixin GUARD errors: a guard defined in an imported file but
  failing during a call from another file degrades to the location-less →
  call-site re-anchor fallback (the span sanity check rejects the foreign
  span) instead of citing the definition file like less.js. MixinDefinition
  carries no file tag yet; same-file guards (the overwhelmingly common case)
  are exact.
- **X1 — RESOLVED (Phase 5)**: cross-import forward mixin refs now work.
  The fix is the real §2.9 stage-2 machinery: `eval_rules_inner` gained
  **pass A (`flatten_imports`, mirroring less.js `Ruleset.evalImports`)** —
  every featureless, non-inline, non-`layer(...)` resolved LESS import's
  rules are spliced FLAT into the containing rule list at the import's
  position, bracketed by eval-only `Node::FileEnter`/`Node::FileExit`
  context markers (error provenance + `(reference)` visibility), recursing
  into nested imports; the current frame is re-synced so pass-1 splice
  arithmetic stays parallel. The body is then ONE scope: forward mixin refs
  across imports (incl. mixins produced by mixin CALLS inside later
  imports), variable last-wins across all imports, and guards seeing
  forward variables all fall out — each pinned by an `x1_*` unit test
  probed against less.js 4.6.7. Two behavior corrections came free, both
  probed: the eval-time once slot-closure is claimed in evalImports order
  (a root import beats a mixin-body import of the same file — the file
  emits at the ROOT position), and a feature-carrying import's variables no
  longer leak into the importing scope (the old `frame_variable` peek is
  gated to featureless imports; `@import "x" screen` + `@fv` use is now the
  4.6.7 NameError). Feature/inline/`layer(...)`/strict-imports cases keep
  the pre-flatten pass-1 expand path unchanged.

## Phase 5 — `//@magento_import` eval wiring (§7.1, done)

The parser half existed (directive node only under `magento_mode`, plain
stripped comment otherwise — both quote forms, optional `(reference)`).
Phase 5 wired the EVAL half: stage 1 (`resolve_imports`) expands each
`MagentoImport` node via `ImportResolver::magento_import(path, reference,
from)` into a synthetic featureless `ImportResolved` container holding one
plain `@import '<entry>';` per returned `MagentoImportEntry` (its
`(reference)` flag mapped to the `(reference)` option), queued so each
entry fetches like a hand-written import. The container "is" the declaring
file (`full_path`/`source` = declaring file, `multiple: true` so the
synthetic path never once-dedups) — entry-fetch errors anchor at the
directive's span in the declaring source, and the X1 flatten then splices
the whole expansion flat at the directive's position (position-preserving,
§7.3; whole-scope visibility across all entries). Unit tests: in-place
in-order splice + cross-entry variable resolution, `(reference)` entries
contributing scope but no bare output, the unset-mode plain-comment
contract (resolver asserts the callback never fires), and a missing entry
surfacing as a located Import error naming the path (§7.5).

## Phase 5 — §7.4 conformance triad suite (done)

`tests/conformance.rs` — 4 tests on synthetic fixtures mirroring the real
theme shapes (written fresh; no Magento sources vendored), each expected
output pinned against a live less.js 4.6.7 probe:
- **G-ref** (`(reference)` × extend-all): the `_extends.less` pattern —
  zero `.abs-` in output (grep-asserted), extended selectors grafted incl.
  nested rules and the `& when (@media-common = true)` fold.
- **G-resp** (the `.media-width` collector): matching module bodies group
  under ONE `@media` block in splice order; unit-aware guard equality
  (`@break = @screen__s` → 640px = 640px); an empty collector block prunes;
  `@media-common: false` and `@media-target` redeclared AFTER the imports
  suppress correctly (X1 last-wins across imports is what makes the
  `styles-l` switch work at all).
- **G-detached**: `@dr()` replay resolves in the DECLARATION scope (a
  caller-scope shadow does not win) with forward refs inside the defining
  file.
- **§7.8 `.lib-css`**: keyword-`false` skip, `''` skip, `extract(@v, 1..5)
  = false` list skips, and the `@_prefix: 1` `-webkit-` fan-out with
  `@{_property}` interpolation — the guard shape mirrors the real library.

**Known cosmetic deviation D-fold-join**: for the extend-grafted copy of a
`& when`-folded hidden rule, less.js joins the folded declarations on one
line (`margin: 0;padding: 0;`); we emit separate lines. The REAL Magento
oracle (less.php SCD output on the reference install) contains zero such
joins — our formatting agrees with the Tier-2 contract, so this is pinned
as-is, not chased.

## Phase 5 — orchestration smoke against the reference install

`magecommand static less --theme Magento/luma` (the new orchestration in
`crates/magecommand/src/static_deploy/less.rs`) compiles **all six** luma
entry points against the untouched reference source tree in <1s total;
styles-m.css comes out 358 KB vs the SCD oracle's 378 KB with matching
overall structure. Semantic diffing (Tier 2, §7.7) is the NEXT stage's
gate; the //@magento_import expansion order was pinned by a live
ObjectManager probe of `Css\PreProcessor\File\Collector\Aggregated`
(library file → module-own view files in config.php load order → theme
module contexts root-ancestor-first alphabetical, with remove-and-append
collation).

## Phase 5 — Tier-2 burn-down: GATE T2 REACHED (zero semantic residuals)

`magecommand static cssdiff` (the §7.7 semantic differ, §G3-validated against
the known-good SCD pair: self-diff 0, one-mutation pair exactly 1) reports
**zero findings on every entry point of both themes** (blank 6/6, luma 6/6
LESS entries; luma's 7th entry `critical.css` is a verbatim pre-minified copy,
byte-identical). Getting there surfaced three probed less.php-vs-less.js
divergences — all REAL on the Luma path, all modeled behind
`LessOptions::php_reference_visibility` (on in the Magento profiles only, so
Gate T0 keeps pure less.js semantics) — plus one genuine engine bug:

- **D-refvis (mixin-call visibility, §3)**: less.php outputs a mixin call
  inside a `(reference)` file when the mixin is DEFINED in a visible file
  (visibility follows the definition); less.js hides it (visibility follows
  the call context). Luma-real: `_email-base.less` (reference) calls the
  visibly-defined `.lib-typography-all()` — 52 rules of the real email.css.
  Implemented via `Candidate::def_in_reference` (FileEnter/FileExit region
  scan in `find_candidates`) + a visibility-block lift in `emit_candidate`,
  with an `Out::Visible` shield the extend pass's darkening stops at.
- **D-refext (extend visibility)**: less.php extend-added selectors are
  ALWAYS visible and chaining passes through reference-declared extends
  (`.abs-b:extend(.abs-a all)` in the reference library renders `.abs-b, …`
  on `.abs-a`'s rule; less.js emits nothing). Luma-real: the `_extends.less`
  abs-on-abs extends — the 276 `.abs-`-prefixed lines of the real luma
  styles-m (now matched exactly).
- **Engine bug (both ground truths agreed)**: extend matching over-matched
  `&`-concatenated selectors — `.abs-tax-total { &-expanded {…} }` joins to
  TWO elements, so `:extend(.abs-tax-total-expanded all)` must not match the
  fused path. String-rendered selectors had lost the element boundary; a
  `\u{2}` fusion marker (inserted by `splice_parent`, honored by the extend
  tokenizer, stripped at render) restores less.js Element granularity.
- **D-ampext (fused-prefix extend)**: on the fused path's PREFIX
  (`:extend(.abs-tax-total all)`), less.js grafts element-wise
  (`.consumer-expanded`), less.php never matches into a fused element —
  php-profile matching rejects fragments whose edge cuts a fusion
  (`SelToken::fused` + `cuts_fusion`).

Also orchestration-level: Magento's `VariableNotation` asset pre-processor
(`{{base_url_path}}` → `{{base_url_path}}<area>/<theme>/{{locale}}`) is
applied post-compile, and the differ canonicalizes number PRINT (parse +
round-8 + shortest print) because less.php leaks PHP float artifacts
(`71.42857143000001%`, `1.0E-6px`) for the same pinned 8-decimal value.

Probes: scratchpad `refprobe/` (less.js 4.6.7 via lessprobe; less.php 5.5.1
via bougie on the oracle copy, scripts run1–run7). Conformance tests pin all
four behaviors in both profiles.
