# magecommand — the write-side companion to magequery

magequery reads; magecommand writes. A new binary in this workspace that replaces
`bin/magento setup:di:compile` with a fast Rust pipeline built on magequery-core's
config-merge engine. Inspired by speedupmate/di-compiler (their `.plans/.features/`
00–39 is the map of the territory and its parity mines), but implemented from
scratch — no code or crate dependency on it.

## Locked decisions

- **New tool, own compiler.** Nothing lands in or depends on speedupmate/di-compiler.
  Their plans/tickets are read as a map (where parity broke, in what order to build);
  their code is not ported. This kills the governance problem (third-party repo) and
  the two-engines-drift risk: magequery-core is the *only* config-merge engine, and
  the compiler is a consumer of it.
- **Oracle first.** Before any generation code exists, capture ground truth
  (`bin/magento setup:di:compile` output archived as `generated/_code` +
  `generated/_metadata`) and build the archive-compare harness. Every milestone lands
  green against it. Mirrors speedupmate's feature 00 — the one part of their process
  to copy exactly.
- **Hand-written SIMD-accelerated PHP parser. No tree-sitter.** Consistent with this
  codebase's philosophy (php.rs, graphql.rs, phparray.rs — focused hand-written
  parsers, no parser crates). Design below.
- **Monorepo.** New crates in the magequery workspace, `magecommand-` prefixed (the
  generic-crate-names lesson from di-compiler). magequery-core stays read-only and
  dependency-light: no writes, no heavy deps, ever.
- **Pure static, single parser, no tiers.** magecommand never executes PHP — not
  even as an opt-in fallback. Constants in di.xml (`xsi:type="const"`,
  `init_parameter`) are resolved by a static const-expression evaluator; signatures
  inherited from PHP built-in classes come from a bundled stub table; anything the
  parser cannot handle is a hard, named diagnostic — the fix is always in the
  parser, never in a crutch. magequery's never-execute-PHP promise holds for both
  binaries, unconditionally.
- **Content-addressable store (CAS).** Derived data (parse results, generated
  artifacts) cached in a blake3-keyed object store shared across checkouts.
  Correctness never depends on it: cold-cache runs are the CI/oracle configuration,
  `--no-cache` is the escape hatch.

## Architecture

```
crates/magequery-core       (existing) the config engine; grows bulk-export APIs
crates/magecommand-php      the PHP structural parser: zero deps beyond memchr;
                            fuzzable + benchable in isolation
crates/magecommand-engine   extraction orchestration, detection, argument compiler,
                            emitters, writer, CAS, archive-compare validator.
                            Never prints (core's discipline).
crates/magecommand          the binary: clap + renderers; style copied/shared from
                            magequery-cli
```

Command grammar: magequery is nouns (inspect an entity), magecommand is verbs (act on
the codebase). `magecommand compile` with magequery's global flags (`--root`,
`--json`, `--color`) plus `--jobs`, `--dry-run`, `--force`, `--no-cache`,
`--interceptor-style magento|compiled`.

Guardrails: writes only under `<root>/generated/`; refuses unexpected existing
content without `--force`; `--dry-run` reports the full work plan.

### magequery-core additions

Bulk export of the merged per-area DI config (all preferences, virtual types, type
arguments, plugin declarations, as owned serde data). Today's API is query-shaped
(`preference(class)`, `plugins(class)`); a compiler iterates the whole config.
Read-only, fits the library-first philosophy, useful to any library user. Core's
heuristic `plugin_methods` stays for interactive queries; compile parity uses the
real flattened method sets from magecommand-php.

## The PHP parser (magecommand-php)

A DI compiler needs declarations, never method bodies. Bodies are ~90% of the bytes,
so the speed lever is skipping them at memory bandwidth and precisely parsing only
the structural remainder — the shape SIMD is actually good at.

- **Two-level design.** Level 1: SIMD-accelerated skipping via `memchr`/`memchr2/3`
  (AVX2/NEON-accelerated on stable Rust) — find the next structurally interesting
  byte (quote, brace, `/`, `#`, `<`), skip string/comment/heredoc interiors, balance
  braces across method bodies. Level 2: a scalar recursive-descent over the ~10%
  structural bytes for precise signature parsing. Files are memmapped (memmap2) and
  parsed in parallel (rayon). `core::arch` intrinsics only if profiling later
  justifies them (`std::simd` is nightly; stay stable).
- **Extracted per class:** namespace + use imports, kind (class/interface/trait/
  enum), abstract/final, extends/implements, trait `use` + adaptations
  (insteadof/as), constructor parameters (promoted properties, defaults as constant
  expressions, variadics, by-ref, nullable/union/intersection/DNF types), all public
  method signatures (incl. static/final/by-ref), class constants (for static const
  resolution).
- **Method flattening** along the hierarchy including traits — a Magento-style
  interceptor overrides every public method the class exposes, inherited or not.
- **Known grammar mines** (each gets a fixture test): heredoc/nowdoc (naive brace
  balancing dies here), string interpolation `{$…}`/`${…}`, `#[Attributes]` with
  nested parens/arrays, `?>`…`<?php` gaps, enums with methods, readonly, and
  forward-compat: PHP 8.4 property hooks (bodies inside property declarations) and
  asymmetric visibility.
- **No tiers.** The parser is the product; nothing sits behind it. A file it cannot
  classify confidently is a hard diagnostic naming the file and construct. Two
  static pieces replace what reflection fallbacks usually cover:
  a **const-expression evaluator** (literal consts, transitive `Class::CONST`
  references, concatenation/arithmetic, enum cases, plus a small table of PHP core
  constants) and a **bundled stub table of PHP built-in class signatures** (the
  phpstan/IDE-stubs approach), so method flattening works for classes extending
  `\ArrayObject` and friends. tree-sitter is deliberately absent (no C toolchain in
  CI; pure-Rust workspace, matching the musl/aarch64 dist targets).
- **Correctness harness:** differential testing against PHP reflection via a
  bougie-provided PHP (ground truth) over the full vendor corpus of the reference
  checkouts (tens of thousands of real files) — PHP runs only inside magecommand's
  own test suite, never in the shipped tool (same category as the archive-compare
  oracle itself); cargo-fuzz for panic-freedom; criterion benches with a stated
  target (full mageos-lite corpus extraction well under a second warm).
- **Determinism contract (CAS prerequisite):** extraction is a pure function
  `file bytes → ClassMeta`, versioned (`PARSER_VERSION` in the cache key), no
  ambient state.

The detection phase (which factories/proxies/interceptors to generate) is a
multi-pattern scan over all PHP + di.xml for `…Factory`, `\Proxy`, `::class`, etc. —
`aho-corasick` (Teddy SIMD prefilter) is purpose-built for this.

## CAS (content-addressable store)

`~/.cache/magecommand/cas/` (XDG), objects keyed by blake3:

- **Parse objects:** key = hash(file content) + `PARSER_VERSION` → serialized
  ClassMeta. Vendor trees are largely identical across this machine's many
  checkouts — parse results dedup across all of them.
- **Action cache:** key = hash(ordered input set: target ClassMeta hashes + the
  relevant merged-DI slice + `EMITTER_VERSION`) → generated file content. On hit,
  write/hardlink into `generated/`; on miss, generate and store. Writes are
  temp+rename, so concurrent runs are safe and idempotent.
- **Key discipline is the hard part:** an interceptor's key is its target's flattened
  method set + its resolved plugin config, not "the whole di.xml" — over-broad keys
  mean zero hits after any config change. Per-area metadata files legitimately key on
  the whole merged config and always regenerate; they're few and cheap.
- Second runs and branch switches become near-instant and mtime-independent. CI can
  restore the store as a cache. Oracle/CI parity runs use `--no-cache`.

## Milestones

**M0 — Oracle + scaffold.** Capture the baseline on mageos-lite (PHP via bougie;
`generated/` currently holds only ~151 runtime-generated files — no baseline exists
yet), archive as `_code`/`_metadata`; build the compare harness (missing/extra/
changed reports, normalized "comparable metadata" diffing, `--fail-on-diff`);
scaffold crates, CLI skeleton, writer guardrails; core bulk-export API.
*Acceptance:* `compile --dry-run` walks the config and reports the work plan; the
comparator reports today's (empty) output as 100% missing.

**M1 — magecommand-php.** The parser as designed above, with the differential
harness, fuzz targets, and benches. *Acceptance:* zero mismatches vs PHP reflection
and zero unparseable files across both reference checkouts' full corpora (no
fallback exists to hide behind); bench target met.

**M2 — Metadata parity.** Per-area compiled metadata (arguments, preferences,
instanceTypes, shared) in Magento's exact serialized shape, from core's merged
config + M1 constructors; plugin-list interception metadata from real flattened
method sets. Known mines (speedupmate features 35–37): interface argument
inheritance, merge-order/null surface, argument-surface closure. *Acceptance:*
metadata section of archive-compare clean on mageos-lite.

**M3 — Code parity.** Emitters: Factory, Proxy (incl. deferred), Interceptor
(Magento style), ExtensionAttributes interface + class (core already parses
extension_attributes.xml), Repository/SearchResults, app action list (core's
`actions` scan is the seed), area config. Detection rides on the aho-corasick scan.
*Acceptance:* code section clean on mageos-lite, then commerce-store.

**M4 — CAS.** The store as designed above, layered under extraction and emission.
*Acceptance:* warm re-run touches no stale content and is dominated by the di.xml
merge; cold-cache output byte-identical to warm-cache output on both checkouts.

**M5 — Compiled interceptor style.** `InterceptorStyle::{Magento, Compiled}`:
static plugin chains baked into the interceptor (creatuity-inspired), per-class
fallback to standard style + denylist for interception-framework classes. The
creatuity module is dormant (last release Nov 2020) — spike whether it runs on the
reference Magento before trusting it as a baseline; otherwise correctness rests on
behavioral tests (same request/unit surface, both styles, diff). Writes a
content-hash manifest of di.xml inputs alongside the output.

**M6 — Synergy back into magequery.** `doctor` lint: compiled output stale vs
current di.xml — designed generically (works for `bin/magento`-compiled installs
too; the M5 manifest upgrades precision when present). This touches magequery's
locked "never read generated/" decision: the lint inspects the artifact rather than
trusting it as a config source; write that carve-out into CLAUDE.md explicitly.

## Ops chores

- `dist-workspace.toml` pins `[dist.binaries] "*" = ["magequery"]` — leave the pin
  until magecommand passes the full oracle, then add it deliberately (it builds in
  CI but never ships until then, which is exactly right).
- Land after the lsp branch merges (same workspace `members` list; 0.5.0 is gated
  on it).
- release-please versions the workspace as one package; magecommand shares
  magequery's version stream unless the config is split later.
- magecommand gets its own agent skill (separate binary, separate audience), not a
  bolt-on to magequery's SKILL.md.

## Risks

| Risk | Mitigation |
|---|---|
| Scale: speedupmate needed 39 features + a long parity-closure tail | Oracle-first; their .plans map as the route; core covers the config third already |
| Hand-rolled parser correctness (the tree-sitter trade) | Differential harness vs PHP reflection over real corpora; fuzzing; fixture tests per grammar mine; hard diagnostics instead of silent guesses |
| Const resolution without executing PHP | Static const-expression evaluator (transitive refs, concat/arithmetic, enum cases) + PHP core-constant table; unevaluable expression = hard diagnostic |
| Stub table drift (PHP built-in signatures change across PHP versions) | Method *names* dominate flattening and are stable; regenerate the table per supported PHP series |
| CAS staleness bugs (wrong keys → stale output) | Versioned keys; cold-cache oracle runs in CI; `--no-cache`; cold-vs-warm byte-identity acceptance in M4 |
| Parity is a moving target across Magento versions | Baselines pinned per reference checkout; regenerated deliberately |
| creatuity oracle may not run on modern Magento | Feasibility spike before M5; behavioral tests as the fallback oracle |

## Sizing (honest)

M0 days; M1 is a serious parser project (the differential harness is what makes it
tractable); M2–M3 are the parity war — months of part-time work, informed but not
shortened much by the map; M4 medium; M5–M6 small once M2's model exists.

## Sequencing

M0 → M1 → M2 → M3 (ship behind the dist pin) → M4 → M5 → M6. Real usability starts
at M3 (full compile parity); M4 makes it fast enough to brag about; M5 makes it
faster than Magento's own output at runtime, not just at compile time.
