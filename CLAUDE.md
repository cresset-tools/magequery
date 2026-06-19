# magequery

A fast, Rust-based developer tool for understanding a Magento 2 codebase from the command
line: modules, DI resolution (preferences/plugins/virtual types), events/observers, cron,
routes, config across scopes, and (phase 2) live DB/Redis introspection.

The value prop is **speed and zero-bootstrap**: answer "what's going on in this codebase"
in milliseconds, ideally on a checkout that has *never been set up* — no DB, no DI
compile, no working PHP. `bin/magento`/magerun bootstrap the whole framework (1–3s/call);
magequery does not.

## Locked decisions

- **Pure static engine.** Re-implement Magento's config-merge semantics in Rust by parsing
  the source XML/PHP directly. Do **not** read `var/cache` or `generated/` merged
  artifacts — those only exist after setup/compile, which breaks the fresh-checkout
  promise.
- **Target: Magento 2.4 OSS only** for the MVP. No Adobe Commerce / Mage-OS / OpenMage yet.
- **Depth-first on the flagship `resolve`/`di` command** before breadth. The DI resolver is
  the hard 80%; the other commands are simpler projections of the same index.
- **Library-first.** `magequery-core` computes and returns owned, structured data; it never
  prints, exits, or reads ambient state. The CLI is a thin renderer on top.

## Architecture

Workspace:
- `magequery-core` — parsing, indexing, resolution. Deps: `quick-xml`, `serde` (default
  feature), `thiserror`. **No `clap`, no output, no `anyhow`.**
- `magequery-cli` (not built yet) — `clap` + table/`--json` renderers. May use `anyhow`
  internally to flatten errors for `main`.

### The central engine

Everything routes through one config-merge engine; subcommands are views over it.
1. **Module discovery + load order** from `app/etc/config.php` (`modules` map = enabled +
   authoritative order) and each `etc/module.xml` `<sequence>`. Load order makes merges
   deterministic.
2. **Area-aware merge.** For each config type, merge `global` (base) overlaid by the
   per-area config, in module load order.
3. **Per-node merge rules.** Preferences = last-wins, followed to a fixpoint. Plugins =
   keyed by name, honor `disabled` + `sortOrder`, split before/around/after. Observers =
   keyed by name, honor `disabled`.

### The flagship `resolve(type, area)`

```
concrete = follow_preferences(type, area)   # fixpoint over merged preference map
chain    = ancestors(concrete)              # parents + interfaces
plugins  = plugins on concrete OR any ancestor/interface, merged (global ← area),
           drop disabled, sort by sortOrder, split before/around/after
args     = merged <arguments> (+ virtualType layering + parent-type inheritance)
→ every row tagged with Source { module, file, line, area }
```

### Pure static still needs PHP parsing (not execution)

Plugins declared on an **interface or parent class apply to all implementations/
subclasses** — the case people miss. So `resolve` needs the class hierarchy, which lives
in PHP. Approach (keeps the no-bootstrap promise):
- Use composer PSR-4 autoload maps for `class → file` (pure string math; vendor is too big
  to scan — 716 modules in the test checkout).
- Parse PHP **on demand**, only for classes on the resolution path, extracting just
  `extends`/`implements` from the class header. Cache it. (tree-sitter-php or a focused
  header parser — never execute PHP.)

### Areas

Fixed 2.4 OSS set, hardcoded, never discovered from the filesystem (`etc/` contains
non-area dirs like `postcode_eu/`, `some_config/`, `redis/`):
`global, frontend, adminhtml, crontab, webapi_rest, webapi_soap, graphql`. `global` is the
base; every real area = `global` overlaid by itself.

CLI area model:
- *(default)* collapsed diff — `global` base + per-area deltas only (silence = same
  everywhere = information).
- `--area <name>` — single area.
- `--all-areas` — full per-area expansion. (`--area`/`--all-areas` are mutually exclusive.)
- `routes` defaults to all-areas (frontend vs adminhtml routers are the point).

The collapse lives in **core** as `ByArea::deltas()`, so library users and the CLI render
from the same computation.

## Type design (`magequery-core`)

- **Typed identifiers, never stringly-typed**: `ClassName`, `ModuleName`, `EventName`,
  `ConfigPath`, `Area` (enum).
- **Provenance everywhere**: `Source { module, file, line, area }` on every returned fact;
  `.location()` → clickable `file:42`. This is the whole point — answers jump to source.
- **Errors vs diagnostics split** (the key, hard-to-retrofit decision):
  - `Error` (`#[non_exhaustive]`, returns `Err`) = can't produce a meaningful answer at
    all (no Magento root, unreadable `config.php`, unknown class).
  - `Diagnostic` (collected on the index, surfaced via `Magento::diagnostics()`) =
    non-fatal per-file problem (one malformed `di.xml` among hundreds). `open()` succeeds
    on messy codebases; a single broken file never blinds the tool.
- **Owned returns** (clone out of the index) so callers don't thread the `Magento`
  handle's lifetime. Data is small.
- `#[non_exhaustive]` on public enums/structs so the API can grow without major bumps.
- All public types derive `serde::Serialize`; `--json` and library use share one type set.
  (serde is a hard dependency, not feature-gated: `serde_json` — required for parsing
  `installed.json` — pulls serde into the build unconditionally, so gating it bought
  nothing.)
- Core is **sync** (file-bound, fast). Phase-2 DB/Redis go behind `db`/`redis` features and
  may block — core never pulls in an async runtime.

## Output styling (colors) — cross-cutting

Colors are a **CLI-only** concern; `magequery-core` never emits escape codes (library-first).
A central `style` module in `magequery-cli` owns the palette, and **every renderer styles by
semantic role**, so a given kind of entity is the same color in every command. New commands
MUST reuse these helpers, never hardcode colors.

Palette (semantic role → color):
- class/interface (FQCN) → cyan
- module name (`Vendor_Module`) → magenta
- area tag (`base`/`frontend`/…) → yellow
- file path / `file:line` → dim (bright-black)
- declaration name (plugin name, event name) → green
- interception kind (`before`/`around`/`after`) → blue
- target method / `▶` actual implementation → bold
- enabled/`on` → green; disabled/`off`/errors → red; warnings → yellow
- literal syntax (di.xml arg values, PHP-style): string `"…"` → green, number → yellow,
  `true`/`false`/`null` → magenta, object/`\Class` → cyan (class)

Rules:
- Color is enabled only when stdout is a TTY; honor `NO_COLOR` and a global
  `--color <auto|always|never>` flag (default `auto`), decided once at startup via
  `style::init`.
- **Never colorize `--json`** — machine output stays clean. (Diagnostics on stderr may use
  color independently of stdout's choice.)
- All escapes go through `style::*` (built on `anstyle`); retheme in one place.
- Width/alignment: pad the *plain* string, then color (escape codes don't count toward
  display width), or color first and pad with computed spaces.

## Command surface

```
magequery modules     [--enabled|--disabled] [--source app|vendor]
magequery preference  <Class>
magequery plugins     <Class>                  # interceptor chain, execution order
magequery observers   [<event>]
magequery di          <Type>                   # FLAGSHIP: preference + plugin chain + args + vtypes
magequery cron        [<group>]
magequery routes      [--area ...]
magequery webapi      [<route>]
magequery config      <path?> [--scope ...] [--show-source] [--decrypt]   # phase 2 (DB)
magequery db                                   # phase 2, opt-in live introspection
magequery redis                                # phase 2, opt-in
magequery schema      [<table>]                # phase 2
```

## Module discovery (step 1, done)

Do **not** brute-walk `vendor/` — on a real install that's ~38k directories to find ~500
modules. Use composer metadata:

- **Vendor**: read `vendor/composer/installed.json` once. For each package, candidate
  module roots = the dir of every `autoload.files` entry (registration.php) **plus** the
  conventional `pkg/etc` and `pkg/src/etc`. The `autoload.files` entries are what catch
  packages that *bundle several modules under `src/`* (e.g. `mirasvit/module-dynamic-category`
  → `src/DynamicCategory`, `src/Merchandiser`). A path is a module root iff `etc/module.xml`
  exists there. Falls back to the recursive scan only if `installed.json` is unreadable.
  This is also *more correct* than scanning: it naturally excludes `dev/tests/.../_files`
  test-fixture modules.
- **app/code**: small and not composer-managed → pruned recursive scan (stops at module
  roots; skips `Test`, `_files`, `var`, `generated`, …).
- `config.php` order is the authoritative, already-sequence-resolved load order; disk
  discovery only supplies path/source/`<sequence>`.

Result on the proforto checkout: 655 modules (matches `config.php` exactly), ~16ms warm
(was ~180ms with the brute walk).

Discovery is parallel (rayon): each package's candidate roots are probed via a pure
`read_module_root` in `par_iter().flat_map_iter(...)`, collected in package order, then
merged sequentially so "first wins" stays deterministic. Phase timing is available behind
`MQ_PROFILE=1`. Warm costs (~13ms wall): parallel module.xml reads ~5ms, app/code scan ~3ms,
installed.json parse ~1.7ms, config.php ~0.1ms. `installed.json` is parsed with a typed
zero-copy (`Cow`) `Deserialize` over only the 3 fields we use — not `serde_json::Value` —
which cut its parse from ~4.6ms to ~1.7ms. Remaining costs are I/O-bound (file reads + the
app/code directory walk); parallel file reading pays off more in step 2's di.xml pass.

### `modules --check` (lint)

Module-set consistency is exposed structurally as `ModuleCheck` (not always-on diagnostic
noise) via `Magento::module_check()`, and surfaced by `magequery modules --check`:
- `on_disk_not_in_config` — registered on disk but absent from `config.php` ⇒ "forgot
  `setup:upgrade`". Reported with a fix hint; CLI exits non-zero.
- `in_config_not_on_disk` — listed in `config.php` but no `module.xml` on disk ⇒ broken.
Only genuine parse/read failures remain as `Diagnostic`s.

## DI index (step 2, done)

`di.rs` builds the merged DI config per area, mirroring Magento: merge every module's
`etc/di.xml` in load order → `global`; then each real area = `global.clone()` overlaid by
that area's `etc/<area>/di.xml` in load order. Files are read+parsed in parallel (rayon),
merged sequentially in load order so last-wins is deterministic.

- `parse::di_xml` extracts preferences, plugins, and virtualTypes with **exact line
  provenance** (`LineMap`: byte offset → 1-based line via binary search over newline
  offsets; offset from `quick-xml`'s `buffer_position()`). `<plugin>` is attributed to its
  enclosing `<type>`/`<virtualType>`.
- Merge rules: preferences & virtualTypes last-wins; **plugins are attribute-level merged
  by name** (a later `<plugin name=.. disabled="true"/>` updates only `disabled`, keeping
  the earlier `type`) — `RawPlugin` fields are `Option` to make this work.
- `Source.area` records where a declaration came from: an entry inherited from global keeps
  `area = Global` even when viewed in adminhtml; an override cites the area file. This drives
  honest collapsed-diff output.
- Exposed so far: `Magento::preference(class, area)` (preference fixpoint with cycle guard;
  no preference ⇒ class is its own concrete). Plugins/virtualTypes are parsed & merged into
  `AreaConfig` but not yet surfaced by a command — that comes with step 4.
- Cost: ~22ms to parse ~900 di.xml files (parallel); ~30ms total index. Now the dominant
  phase, as expected — this is where rayon earns its keep.

Validated on proforto: `CartManagementInterface → QuoteManagement` (di.xml:25);
`IsProductSalableInterface` collapsed-diff correctly shows global + frontend + graphql
overrides while adminhtml/crontab/webapi inherit; line numbers exact.

## Class resolver (step 3, file-resolution half done)

`resolver.rs` maps a class name to its source file via PSR-4, so we can answer "does this
class exist?":
- **Vendor**: PSR-4 maps from `installed.json`'s embedded `autoload.psr-4` (700/789 packages),
  parsed alongside `autoload.files` in `composer.rs` (prefix → absolute dirs).
- **app/code**: not composer-managed → synthesize the convention `Vendor_Module` →
  namespace `Vendor\Module\` rooted at the module dir. (Limitation: a module whose PHP
  namespace diverges from its name would be missed; revisit by reading app/code
  composer.json if it ever bites.)
- `file_for` does PSR-4 longest-prefix-first match, `stat`s candidates, returns the first
  that exists. Built once from the already-parsed `packages` (parsing installed.json once,
  shared with module discovery).

Wired into `preference`: when no preference applies, the class is its own concrete type
**only if it actually exists** — otherwise `Error::ClassNotFound`. The CLI prints a clean
"class not found" message (namespace/spelling hint) and exits non-zero. `class_known` also
treats a name as real if it's a virtualType or a DI-referenced type, to avoid false
negatives when the PSR-4 map is incomplete.

### PHP header parse + ancestor walk (done)

`php.rs` is a focused, non-executing PHP tokenizer that reads a class file's header:
`namespace`, `use` imports (incl. `as` aliases and group `use A\{B, C}`), and the
`extends`/`implements` names — each resolved to an FQCN via PHP name rules (leading `\` =
absolute; first segment matching a `use` alias expands; else relative to namespace).
Handles `//`/`#`/`/* */` comments, strings, and PHP 8 `#[attributes]` (not treated as
comments). Stops at the first type declaration.

`resolver.rs` adds:
- `header(class)` — lazily reads+parses a class's header, cached in a `Mutex<HashMap>`
  (`None` = file missing, e.g. PHP built-ins — that just ends a branch).
- `ancestors(class)` — BFS over extends+implements, transitively, nearest-first. **This is
  what makes plugin resolution correct**: a plugin declared on an interface/parent applies
  to every implementation/subclass.
- `plugin_methods(class)` — parses the plugin file for its **public** `before*`/`around*`/
  `after*` methods (skips private/protected via a modifier look-back) and derives the
  intercepted target method (`beforeSave` → `save`, `afterGetList` → `getList`). Returns
  `Vec<PluginMethod { kind, target, plugin_method }>`. (Heuristic: doesn't follow plugin
  inheritance/traits, but matches Magento's convention.)

`Magento::plugins(class, area)` resolves the preference → concrete, collects plugins from
the concrete **and every ancestor/interface**, dedups by plugin name across the hierarchy
(nearest wins, as Magento merges by name), tags each with `declared_on` and its intercepted
methods, includes-but-flags disabled. **Order = Magento's**: ascending `sort_order`, ties
broken by *declaration order* — `order_key = (area_rank, module load_order, line)` stored on
each plugin in the DI index (global rank 0 before area-overlay rank 1; set at first
declaration, preserved across attribute-merges). NOT alphabetical by name. (Earlier versions
tiebroke by name — coincidentally identical on proforto, but wrong in general.)
`magequery plugins <Class>` renders it with `← declared on <Ancestor>` for inherited
plugins, an `intercepts: before save, after getList` line per plugin, and provenance.
`--area` overlays that area's plugins.

`Magento::plugin_chains(class, area, only)` builds the **execution onion** per intercepted
method: before plugins ascending `sort_order`, around plugins nested (ascending = outer),
the target method, then around unwinding and after plugins **descending**. Disabled plugins
excluded. `magequery plugins <Class> --chain [--method <m>]` renders it with indentation for
around nesting (`around↘`/`▶ target`/`around↖`). Plugins render compactly with an inline **`area=` tag** (`base` for global, else the area
name) on every plugin/step — flat: 2 lines (`sort name [intercepts] ← origin` / `class ·
area=X · file:line`); chain steps: `[Class::method  so=N  area=X]`. `--all-areas` is a
single **merged** view, not per-area sections: `plugins_all_areas`/`plugin_chains_all_areas`
union every area's plugins (deduped by name, base wins a clash) into one ordered list/onion,
each tagged with the **full set** of areas it's declared in (`Plugin.areas`/
`ChainPluginRef.areas`) — e.g. `area=base` or `area=webapi_rest,webapi_soap` — so a base
plugin appears once, not per area. Targets come from the global concrete (preference rarely
differs per area). `--all-areas` is mutually exclusive with `--area`. (Simplification: the standard onion;
Magento's exact segmentation when arounds interleave with other plugins' before/after across
sort orders is not modeled — accurate for the common case.) Verified on proforto: webapi_rest
`save()` shows before ascending (so 0,10,10) and after descending (so 10 then 0).

Validated on proforto: `ProductRepositoryInterface` plugins (all declared on the interface)
correctly attributed to the concrete `ProductRepository`; global set exactly matches ground
truth; `--area webapi_rest` correctly adds the REST-only `product_authorization` + Mirasvit
plugins. ~38ms total (ancestor walk + header parse is negligible).

## Resolution (`di`, step 4, done)

`Magento::resolve(class, area) -> Resolution` is the flagship: concrete type + preference
chain + `instantiates` (for a virtual type, the real class it builds — follows the
`virtualType type=` chain to a non-virtual class, cycle-guarded) + merged constructor
arguments + plugin chain + contributing ancestors, all with provenance. `magequery di <Type>`
renders it (colored), with argument values shown as **PHP-style literals** (strings quoted,
objects as `\FQCN`, arrays as `['k' => v, …]`) via `render_arg`.

- **Arguments**: `parse::di_xml` now parses `<arguments>` via a stack-based recursive parser
  (`parse_arguments`) into a typed `ArgValue` (`Object`/`Scalar{xsi_type,text}`/`Array`/`Null`),
  attributed to the enclosing `<type>`/`<virtualType>`. Stored per type in
  `AreaConfig.type_args` (type → arg name → value).
- **Merge semantics** (`ArgValue::merged_with`): **array arguments merge item-by-item by key
  across modules** (newer overrides same-key, appends new keys, recursing into nested arrays);
  scalars/objects replace. Applied both cross-module (di.rs `merge_file`) and across the type
  hierarchy / virtual layering (`args_of`). Validated: `EntryConverterPool`'s collection
  accumulates `image` + `external-video` from two modules.
- **`args_of(name, area)`**: virtual type → inherit base type's args then overlay its own;
  real type → merge parent-type args along the PHP ancestor chain (distant first), then self.
  Cycle-guarded. Result sorted by argument name.
- Limitations: `init_parameter`/`const` values shown verbatim (not evaluated); doesn't model
  rare di.xml `<argument>` removal/`shared` nuances.

## Breadth (step 5, done)

`breadth.rs` holds four thin projection indexes, each parsed from a per-module XML file and
merged in load order. **Built lazily** via `OnceLock` on the `Magento` handle (built on first
query), so they don't slow `modules`/`di`/`plugins`.

- **events/observers** — `events.xml`, per-area (global + overlay), observers merged by name
  within an event (last-wins, disabled/shared attrs). `Magento::observers(event, area)`,
  `events(area)`. CLI: `events [<event>] [--area]` (list with counts, or one event's observers).
- **cron** — `crontab.xml` (global; the parser fills `<schedule>`/`<config_path>` text into
  the current `<job>`). Merged by (group, name). `cron_jobs(group?)`. CLI: `cron [<group>]`.
- **routes** — `routes.xml`, per-area (frontend/adminhtml/…); routes keyed by (router, id),
  modules accumulated across modules. `routes(area)`. CLI: `routes [--area]` (defaults to
  frontend+adminhtml). (Limitation: module `before`/`after` ordering within a route not
  applied — modules listed in encounter order.)
- **webapi** — `webapi.xml` (global), keyed by (method, url). `webapi(url_filter?)`. CLI:
  `webapi [<url-substr>]`. Shows service class::method + ACL resources.

Validated on proforto: 269 events, cron groups with literal + `config:` schedules, frontend
routes (frontName → modules), 688 REST endpoints — all with provenance.

**Perf note (open todo):** the di index is built *eagerly* in `Index::build` (since step 2),
so `modules` and the breadth-only commands pay ~22ms for di they don't use (~68ms vs the old
~13ms for `modules`). Making the di index + resolver lazy (same `OnceLock` pattern) would fix
this.

## Build order

1. ~~`ModuleIndex` — parse `config.php` + `module.xml` sequence, classify app/vendor →
   `magequery modules`.~~ **Done** (composer-based discovery + `--check` lint).
2. ~~Provenance-tracking `di.xml` indexer (per-area; `quick-xml` + byte→line table).~~
   **Done** — see "DI index" below. `magequery preference <Class>` works.
3. ~~`ClassResolver` (composer autoload → on-demand PHP-header parse + cache).~~ **Done** —
   PSR-4 class→file + PHP-header ancestor walk. `magequery plugins <Class>` works.
4. ~~`magequery di <Type>` — wire it together.~~ **Done** — see "Resolution (`di`)" below.
5. ~~Breadth (`observers`, `cron`, `routes`, `webapi`).~~ **Done** — see "Breadth" below.

Validate early against a real 2.4 checkout (resolve a class with a known
interface-declared plugin) to catch merge-semantics surprises before building breadth.

## Phase 2 notes (not built yet)

- `app/etc/env.php` and `config.php` are **PHP array literals** — write a small `nom`/`pest`
  array-literal parser; do **not** shell out to `php` (breaks the no-PHP promise).
- Config-source precedence for `config --show-source`: `config.xml` defaults → DB
  `core_config_data` (per scope) → `env.php[system]` → `config.php[system]` (locked) →
  `CONFIG__*` env vars. Both `env.php` and `config.php` carry a `system` node; `config.php`
  also has `scopes` (website/store tree) and `themes`.
- `--decrypt`: some `core_config_data` values are AES-256-CBC encrypted with the `crypt`
  key in `env.php`.

## Test checkout

`/Users/jelle/www/proforto` — a real Magento 2.4 install (716 modules: 563 vendor + 153
app/code). Validated: `config.php` shape, per-area `di.xml` layout, PSR-4 autoload
(`"Magento\\Catalog\\": ""` = module root), and real plugin-on-interface declarations.
