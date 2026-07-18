# magecommand roadmap — beyond `di:compile`

`.plans/magecommand.md` is the build plan for the flagship `di:compile` reproducer —
now **shipped** (`di compile`/`verify`/`watch`/`digest`, plus `di compile --fused`,
byte-exact on the oracle). This document is the forward roadmap: growing magecommand
from a single-command reproducer into the full **write-side companion to magequery**.

## Vision & identity

magequery *reads* a Magento entity; magecommand *writes or generates* it. They share a
noun vocabulary — `magequery product <sku>` inspects it, `magecommand product create …`
writes it. The through-line is magequery's: **reproduce/generate what `bin/magento`
produces — fast, no-bootstrap, and byte-exact wherever a real-compiler oracle exists.**

Commands fall into four classes by *how we know they're correct* — this is the axis that
drives sequencing and risk, more than the surface grouping does:

1. **Reproduction** (a byte-exact oracle exists) — `di`, `static`, `i18n`. Gold
   standard: diff our output against a real `bin/magento` run. This is where magecommand
   is strongest and most trustworthy.
2. **Scaffolding** (no fidelity bar) — `make`. Template codegen; "correct" = it compiles
   and follows convention. Lowest risk, high value.
3. **Config mutation** (deterministic, low-risk) — `module`/`mode`/`maintenance`/
   `config`/`cache`. Edit `config.php`/`env.php`/flags/cache backends; no bootstrap.
4. **Entity writes** (NO byte-exact oracle, high risk) — the `DATA` group. Must
   replicate Magento's write logic (EAV, `url_rewrite`, sequences, stock, indexers) with
   no compiler to diff against. The hard frontier; where the no-bootstrap promise strains.

## The grammar (locked)

`magecommand <group> <subcommand> [args]` — always two tokens, curated (not
`bin/magento`'s sprawling `namespace:command`). Bare `<group>` = that group's help
(clap default); bare `magecommand` = grouped root help. Full surface + conventions:
**`docs/command-surface.md`** (the locked reference — keep it in sync).

## Status

| group | command | state |
|---|---|---|
| GENERATE | `di compile` / `verify` / `watch` / `digest` | **shipped** |
| GENERATE | `di compile --fused` | **shipped** (byte-exact, opt-in; runtime validation pending) |
| GENERATE | `static deploy`, `i18n collect` | planned |
| SCAFFOLD | `make …` | planned |
| PROJECT | `module`/`mode`/`maintenance`/`config`/`cache` | planned |
| DATA | `product`/`order`/`customer`/`category` … | planned (hard) |
| DEV | `watch` (global, smart cache) | planned |

## The plan, by group

### GENERATE — reproduction commands (byte-exact oracle)

- **`di`** — done. The template for everything else: resolution in `magequery-core`,
  rendering in `magecommand-engine`, validated byte-exact against `bin/magento
  setup:di:compile` (`~/oracle-gate.sh`, 8246/8246). `--fused` proved the "second
  renderer over a validated plan" pattern (`~/oracle-gate-fused.sh`).
- **`static deploy`** — reproduce `setup:static-content:deploy` → `pub/static`. Pure
  file generation, no DB. **Strategy — Hyvä-first, delegate Luma** (validated by
  [elgentos/magento2-static-deploy](https://github.com/elgentos/magento2-static-deploy),
  a Go tool doing exactly this at ~230–380× native speed, ~40k files/s):
  - **Hyvä themes ship pre-built** (Tailwind via npm), so their deploy is *pure
    fallback-resolution + parallel file copy* — no LESS/RequireJS/minify. This is the
    perfect no-bootstrap fit and where the huge win is. **Primary target.**
  - **Luma needs Magento's LESS/RequireJS/minify pipeline** — the "largest surface"
    concern. **Do NOT reimplement it; delegate to `bin/magento setup:static-content:deploy`**
    (elgentos does exactly this; only email CSS uses a `wikimedia/less.php` shim). This
    **resolves the old blocking decision**: no LESS/JS reimplementation.
  - **Where we go beyond elgentos:** it does raw copy with *no* fallback resolution;
    magecommand must do the real theme→parent→module + locale fallback so the Hyvä output
    is **byte-exact** against a real SCD run (the reproduction-command bar).
  - Oracle = a real **Hyvä** SCD run archived like `_code`; byte-exact scoped to Hyvä
    (Luma's LESS output isn't reproducible). Borrow: theme-type auto-detect
    (`Hyva/default`|`reset` inheritance or a `web/tailwind/` dir), `-j/--jobs`,
    `--symlink=file|locale` (disk footprint), `--content-version` reuse, graceful
    skip/delegate. Credit elgentos (prior art).
- **`i18n collect`** — reproduce `i18n:collect-phrases` → the phrase CSV. Scan source
  (`__('…')`, `.phtml`, `.xml`, JS `$t`) for translatable strings. Pure static, medium
  surface (reuses the PHP/XML scanning already in the tree). Oracle = a real collect run.

### SCAFFOLD — `make` (Laravel `make:` idiom, shifted to a space)

`make module | controller | block | model | plugin | observer | command | cron |
graphql-resolver | ui-component | …`. Template-driven codegen with **no fidelity bar** —
it can't produce "wrong" output the way a reproduction command can, which makes it the
**best early win**. Design: a registry of scaffold types, each a set of file templates +
the minimal wiring (registration.php, module.xml, di.xml stanza, etc.). Flag-driven
(`make plugin Vendor_Module --on Some\Class --before save`). Value: removes the most
tedious Magento boilerplate; pairs naturally with magequery's introspection.

### PROJECT — config mutation

`module enable/disable`, `mode set <mode>`, `maintenance enable/disable`, `config set
<path> <value> [--scope]`, `cache clean [type…] | flush`. All edit `config.php`/
`env.php`/flags, or (for `cache`) purge Redis/file backends straight through `env.php`
(no bootstrap — the config magequery already reads). Individually easy; the trivial
toggles add little over `bin/magento` on their own, **but `cache clean` is the
building block the dev watcher needs**, so it lands early.

### DATA — entity writes (the hard frontier)

`product`/`order`/`customer`/`category` … `create | update | delete`. The write
counterpart to magequery's read cards. **This is the one class with no byte-exact
oracle**: creating a product correctly means replicating EAV writes across value tables,
`url_rewrite` generation, `sales_sequence`, stock items, and indexer invalidation — the
logic Magento's business layer owns. Risks: silent data inconsistency; the no-bootstrap
promise is most tempting to abandon here. **Approach:** land last; spike on the *safest*
entity first; define a validation strategy up front (candidates: diff resulting DB rows
against a `bin/magento`/API-created twin; or accept a *minimal* bootstrap for writes
only, scoped to DATA). Ship behind an explicit "raw writes — you own the consistency"
understanding.

### DEV — the global watcher

Bare **`magecommand watch`** = watch the whole project and, per change, regenerate the
affected artifact **and purge only the cache that change invalidated** — no bootstrap,
cache cleared through `env.php`'s backend. The smart part is the change→reaction map:

| file changed | regenerate | clean cache |
|---|---|---|
| `di.xml`, PHP ctor/hierarchy | `di compile` (incremental) | `config` |
| `layout`/`*.phtml`/`less`/`ui_component` | `static deploy` (delta) | `layout`, `block_html`, `full_page` |
| `config.xml`, `system.xml` | — | `config` |
| `i18n/*.csv` | `i18n collect` | `translate` |

Per-artifact `di watch` / `static watch` stay as scoped watchers; `cache clean` is also
standalone. Depends on `cache clean` + the generation commands it orchestrates.

## Cross-cutting concerns

- **Validation discipline (the core value).** Byte-exact oracle wherever a real compiler
  exists: `mg-install-310` (stock) + `mg-install-310-fused` (fused), gated by
  `~/oracle-gate.sh` / `~/oracle-gate-fused.sh` (both default to `origin/main`; pass
  `MG_BRANCH` for a feature branch). The repeatable technique: *install a real
  module/tool into the oracle, run the real command, diff.* **Runtime/behavioral
  validation is a SEPARATE, weaker-available bar** — it needs a live DB-backed store the
  compile-only oracle can't provide (see the `--fused` caveat, `di` two-pass note).
- **The no-bootstrap promise** is the product. It holds cleanly for GENERATE/SCAFFOLD/
  PROJECT; it *strains* for DATA — treat any bootstrap there as a deliberate, scoped
  exception, never a default.
- **Attribution/credit** — the creatuity precedent (fused): when we build on prior art,
  contribute the fix upstream *and* credit clearly (a source comment + the docs). Repeat
  that norm for anything derived (`static`'s deploy logic, `make`'s skeletons, etc.).
- **magequery-core stays the single engine.** magecommand is a consumer; new write
  primitives that belong in core (like `di_export`) land there, read-only-safe, and the
  compiler never forks the config-merge logic.

## Proposed sequencing (priorities open — reprioritize freely)

- **Phase 1 — quick wins, no fidelity bar:** `make` (scaffolding) · `cache clean/flush`
  · `module`/`mode`/`maintenance`. Fast user-visible value; `cache` unblocks the watcher.
- **Phase 2 — the daily driver:** the global `watch` (watch-everything + smart cache
  clean). Highest leverage for the dev loop; builds on Phase 1's `cache` + `di watch`.
- **Phase 3 — reproduction, medium→large:** `i18n collect` (smaller) then `static
  deploy` (the big one — likely its own multi-phase effort gated on the LESS/JS
  decision).
- **Phase 4 — config:** `config set` · `generate-whitelist` (setup:db-declaration).
- **Phase 5 — the hard frontier:** `DATA` writes. Spike + validation-strategy decision
  first; expand entity by entity, cautiously, behind the "raw writes" understanding.
- **Continuous:** `--fused` runtime validation once a live store is available; the `di`
  two-pass refinement (`main.rs`) if it ever bites; keep `docs/command-surface.md` and
  this file in sync as groups land.

## Open decisions (need a call before their phase)

1. **`static`: reimplement vs shell out** — **largely RESOLVED** by elgentos's approach
   (see the `static deploy` entry): go **Hyvä-first** (pure fallback-resolution + parallel
   copy, no-bootstrap, byte-exact) and **delegate Luma** to `bin/magento` — don't
   reimplement LESS/RequireJS/minify. Remaining sub-question: whether to support Luma at
   all eventually (an email-CSS `wikimedia/less.php` shim is the one small exception
   elgentos makes) or stay Hyvä-only.
2. **`DATA` validation** with no byte-exact oracle — diff-against-a-twin vs a scoped
   minimal bootstrap. Determines whether DATA is even in-scope for the no-bootstrap tool.
3. **`make` fidelity** — how closely to mirror Magento's own module skeletons; flag-only
   vs interactive prompts.
4. **`watch` cache map accuracy** — the change→cache-type table must be exact (over-clean
   is slow, under-clean serves stale); needs a source-of-truth pass over Magento's cache
   tags.
