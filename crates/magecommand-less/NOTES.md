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
cargo build --workspace          # the gate — green
cargo test  -p magecommand-less  # 6 passing
```

## Not yet started (later steps / phases)

- `README` pinning the §0 success definition verbatim (Gate T0 200/200 + Gate T2
  zero SCD residuals) — plan §0 asks for this; not part of scaffold.
- Phase 0 remainder: vendor `@less/test-data` @ v4.6.7 + `data/` assets, generate
  `manifest.json`, `libtest-mimic` harness with `doReplacements` + the 7-kind
  error renderer + CI ratchet; **the §G2 compatibility-proof blocker task**.
- `error::LessError` `Display` is provisional (scaffold `<Kind>: <message>` form);
  the byte-exact `<Type>Error:` prefix + 3-line source excerpt land in the error
  phase, gated by the 74 error fixtures.
- `ast::Node` covers only the passthrough leaf + root; the remaining ~42 `tree/`
  nodes land as the parser is built (Phase 1+).
- `functions/*`, `data/*` (148 colors, unit tables, mime table), and `css` genCSS
  are stubs.
