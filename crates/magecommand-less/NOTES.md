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

## Not yet started (later steps / phases)

- `README` pinning the §0 success definition verbatim (Gate T0 200/200 + Gate T2
  zero SCD residuals) — plan §0 asks for this; not part of scaffold.
- Phase 0 remainder: generate `manifest.json`, extend the harness with the 7-kind
  error renderer + CI ratchet over `tests-config/`/`tests-error/`; **the §G2
  compatibility-proof blocker task**.
- `error::LessError` `Display` is provisional (scaffold `<Kind>: <message>` form);
  the byte-exact `<Type>Error:` prefix + 3-line source excerpt land in the error
  phase, gated by the 74 error fixtures.
- `ast::Node` covers only the passthrough leaf + root; the remaining ~42 `tree/`
  nodes land as the parser is built (Phase 1+).
- `functions/*`, `data/*` (148 colors, unit tables, mime table), and `css` genCSS
  are stubs.
