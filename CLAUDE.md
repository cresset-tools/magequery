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
- file path / `file:line` → dim (bright-black), rendered as `# file:line` (the leading `#`
  makes it a trailing comment so a line copy-pastes cleanly); root-relative via `short_loc`
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

## Command surface (organization — LOCKED)

**The one grammar rule: `magequery <command> [target] [flags]`.** The command names what
you're inspecting (a noun); the argument is a *Magento entity* (a class, event, config path,
table). One level deep. The namespace stays **flat** — `di Foo` must never become `wiring di
Foo`; grep-ability and muscle memory are the whole UX. Grouping is a *help-rendering* concern
(the `Command` enum order + a hand-rendered root help screen), **not** a typing one.

**Nesting earns its keep in exactly one case: a noun with multiple verbs** — `db info`/`db
ping`, `redis info`/`redis ping`. Info-only nouns (`session`/`cache`/`lock`/`queue`) stay flat.

The `Command` enum in `main.rs` is ordered into these seven groups (banner comments mark
them). clap can't natively head-group subcommands without *nesting* them (which would break
flat invocation), and its `help_template` can't color literal text — so `main.rs` renders the
**root** help/no-args screen itself (`print_help` + the `HELP_GROUPS`/`HELP_OPTIONS` tables,
styled through the `style` module, so it's grouped *and* colored, and plain when piped). It's
intercepted by `wants_root_help` *before* clap parses; every `magequery <command> --help`
still uses clap's native per-command help. Keep `HELP_GROUPS` in sync with the enum. New
commands MUST slot into a group, never append to the end ad hoc:

```
WIRING        (object manager — how a class is assembled)
  di <type>            flagship: preference + plugins + args + vtypes
  preference <class>     focused view of di
  plugins <class>        focused view of di  [--chain]
  events [<event>]       observers per event   (NOT `observers` — that name is retired)
  uses <class>           reverse DI: who injects it                         (backlog)

ENTRY POINTS  (how execution starts)
  routes [--area]    actions [<url>]    webapi [<url>]    cron [<group>]
  graphql <type>   (backlog)            commands         (backlog)

DATA          (persistence & model)
  schema [<table>]       extension-attributes <type> (backlog)
  indexers (backlog)     eav [<attr>] (backlog, --db)

CONFIG & ADMIN (where settings & permissions live)
  config <path> [--scope] [--db] [--decrypt]    system-config [<filter>]
  acl (backlog)                                 menu (backlog)

FRONTEND      (presentation)                    -- all backlog
  layout <handle>   widgets   email-templates   translations <str>   view   ui-components

RUNTIME       (env.php config & live connections)
  db info|ping     redis info|ping     url-rewrites [<path>] [--store] [--redirects] [--limit]
  session   cache   lock   queue       (info-only)

PROJECT       (the codebase itself)
  modules [--check] [--enabled|--disabled] [--source app|vendor]
  deps <module> (backlog)   patches [--db] (backlog)   doctor (backlog)   whatis <class> (backlog)
```

### Cross-cutting flag vocabulary (a flag means the same thing everywhere)

- **`--area <name>` / `--all-areas`** — only on area-aware commands (`di`, `preference`,
  `plugins`, `events`, `routes`, `actions`, `webapi`). Default = collapsed diff.
- **`--json`** and **`--color auto|always|never`** + **`--root <path>`** — global, every command.
- **`--db`** — the opt-in switch on every *hybrid static-or-live* command (`config` today;
  future `eav`, `indexers --status`, `patches`). Static by default; DB overlay when asked;
  clean `Error::Db` if unreachable. Pure-live commands (`db`/`redis` `ping`, `url-rewrites`)
  require the `db`/`redis` build feature instead.
- **`--source app|vendor`** — listing commands, "only my code" (today `modules`; generalize).

### Consolidations baked into this lock

- `observers` is folded into **`events [<event>]`** (one command). Already done in code.
- `preference` / `plugins` are **focused views of `di`** — kept flat (high-frequency), but
  documented as such; `di <type>` is the single full entry point.
- reverse-DI ships as **`uses <class>`** (or folded into `whatis`), never a literal
  `reverse-di` command. `doctor` is the home for cross-index lints (the `modules --check`
  philosophy, generalized: preference/vtype cycles, di refs to missing classes, plugins on
  `final`/non-shared, `<sequence>` cycles).

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
- **Per-item provenance**: each array item is an `ArgItem { key, value, source }` — its own
  module/file/line, set at merge time (parser emits a source-free `RawArg` with line numbers;
  `di::to_arg_value` attaches the `Source` from the file being merged; on override the newer
  item's source wins). `di` expands array arguments one item per line with provenance, so e.g.
  `di Magento\Framework\App\RouterList --area frontend` shows which module added each router
  (`blog` → magefan, `prismicio` → elgentos, …).
- **`args_of(name, area)`**: virtual type → inherit base type's args then overlay its own;
  real type → merge parent-type args along the PHP ancestor chain (distant first), then self.
  Cycle-guarded. Result sorted by argument name.
- Limitations: `init_parameter`/`const` values shown verbatim (not evaluated); doesn't model
  rare di.xml `<argument>` removal/`shared` nuances.

## Breadth (step 5, done)

`breadth.rs` holds four thin projection indexes, each parsed from a per-module XML file and
merged in load order. **Built lazily** via `OnceLock` on the `Magento` handle (built on first
query), so they don't slow `modules`/`di`/`plugins`. The per-module file reads+parses are
**parallel** (rayon) via the shared `read_parse` helper (read+parse all modules' `etc/[<area>/]
<file>` concurrently, returning results in module/load order for a deterministic sequential
merge) — events ~108ms→25ms, routes ~100ms→21ms.

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

### `actions` (controller subroutes)

`Magento::actions(area, url_filter)` lists controller actions (the "subroutes" that aren't in
any XML — they're `Controller/<Path>/<Action>.php` classes by convention). For each route's
modules it scans `Controller/` (frontend) or `Controller/Adminhtml/` (admin), maps each file
to `frontName/controller/action`, and keeps only **concrete action classes** —
`resolver.is_action` checks the PHP header (`php.rs` now tracks `is_interface`/`is_abstract`)
and that ancestors include a Magento action base (`ActionInterface` etc.), so abstract bases
and interfaces are excluded. The class name is built from the `Vendor_Module → Vendor\Module`
convention (limitation: a module whose PHP namespace diverges from its name is missed). The
`url_filter` is applied to the URL *before* parsing the PHP, so `actions catalog` is cheap.
CLI: `magequery actions [<url-substr>] [--area]` — greppable `url  class  file` lines
(frontend default). ~95ms for all frontend actions.

**Lazy di (done):** the di.xml index is built lazily via `OnceLock<DiBuilt>` on `Magento`
(`di_index()` builds it + collects its diagnostics on first DI query), so `modules` and the
breadth-only commands no longer pay the ~22ms di parse — `modules` is back to ~16ms (was
~68ms). The resolver stays eager (cheap: PSR-4 maps only; PHP parsing is lazy). `diagnostics()`
now returns an owned `Vec` merging index + di (once built); the CLI prints diagnostics *after*
the command so lazily-built di parse warnings are included.

### `schema` (declarative `db_schema.xml`, static, done)

Tables read from each module's `etc/db_schema.xml` — Magento's **declarative schema** source
of truth since 2.3 — so this is fully static (no DB; fits the no-bootstrap promise). Another
`SchemaIndex` in `breadth.rs`, built lazily (`OnceLock` on `Magento`), parsed in parallel via
the shared `read_parse`, merged in load order.
- `parse::db_schema_xml` → `Vec<RawTable>` with exact line provenance. The subtlety: a
  `<column>` directly under `<table>` is a **definition** (carries `xsi:type`), but a
  `<column>` inside a `<constraint>`/`<index>` is a **reference** (only `name`) — routed by
  tracking the current `in_constraint`/`in_index` context. That context is opened only on a
  `Start` event, never an `Empty` one, so a self-closing foreign `<constraint/>` (no `End`)
  can't capture the following table's columns. `xsi:type` matched namespace-prefix-agnostically.
  Two unit tests lock these two edge cases.
- Merge (`SchemaIndex::build`): tables keyed by `name`; within a table, columns by `name`,
  constraints/indexes by `referenceId` — last-wins, `disabled="true"` removes (a whole table
  too). A module can add columns/constraints/indexes to **another module's** table; each
  merged item keeps the **adding** module's `Source`. Table-level attrs are last-wins; the
  table `Source` keeps the first declaration.
- `Magento::schema(name_substr?)` (list, sorted) + `Magento::table(name)` (one, exact, full).
  CLI `magequery schema [<table>]`: exact name → full DDL-ish view (columns with
  `type(len)`/`unsigned`/`NULL`/`auto_increment`/`default`, constraints incl. FK `→
  refTable(refCol) ON DELETE …`, indexes); otherwise a name-substring **list** (`name  N cols
  # loc`); no arg lists all. Columns added by a **different** module than the table's are
  tagged `← Vendor_Module` — the payoff of cross-module merge. Validated on proforto: 545
  tables; `sales_order` shows 152 columns merged from 8 module files, each third-party
  extension column attributed (`← Magento_Paypal`, `← Billink_Billink`, …); FKs/indexes/types
  exact; ~20ms.

### `system-config` (admin settings map from `adminhtml/system.xml`, static, done)

The inverse of `config`: `config` resolves a path's **value**; `system-config` says **where
that path lives in the admin** (Stores → Configuration → tab → section → group → field) and how
it behaves. A `SystemConfigIndex` in `breadth.rs`, lazy (`OnceLock`), parsed in parallel via
`read_parse` over `etc/adminhtml/system.xml`, merged in load order.
- `parse::system_xml` walks the tab/section/group/field tree. The leaf values (`<label>`,
  `<tab>` ref, `<resource>`, `<source_model>`/`<backend_model>`/`<config_path>`) are **text**,
  not attributes, so a `SysText` target enum routes each text run to the innermost open
  element. Top-level `<tab id=>` (a tab definition) is told apart from a section's `<tab>id
  </tab>` (a reference) by the presence of the `id` attribute. Line provenance per field.
- Merge (`SystemConfigIndex::build`): tabs by id, sections by id, groups by id within a
  section, fields by id within a group — **merge-non-empty** (a later module may only tweak a
  field, e.g. add a scope, without re-stating label/model), so labels/models carry forward.
  Breadcrumb labels are resolved at flatten time (`fields()`), so a field keeps the right
  section/group/tab label even when a *different* module declared those. Config path =
  `section/group/field` unless the field's `<config_path>` overrides. Scopes from
  `showInDefault`/`showInWebsite`/`showInStore`. Each field tagged with the **declaring**
  module's `Source`.
- `Magento::system_config(filter?)` — filter matches the config path **or** the label (so you
  can find a setting by its human name without knowing the path). CLI `magequery system-config
  [<filter>]`: greppable `path  Tab > Section > Group > Field  [scopes]  # loc`; hidden
  config-only fields (no label) fall back to the field id. Validated on proforto: 2656
  settings; `web/unsecure/base_url → General > Web > Base URLs > Base URL [default, website,
  store]`; cross-module section/group labels resolve (e.g. a third-party delivery method under
  `Sales > Delivery Methods`); ~find by label works (`"sort order"`).

## Future query tools (backlog — not yet built)

Ideas surfaced while scoping breadth, in rough priority. All but the GraphQL/DB ones are
static breadth-projections in the same `read_parse` + merge shape.
- **`acl`** — admin ACL resource tree from `acl.xml` (the `<resource>` refs `webapi`/`system-
  config` already cite). "What does `Magento_Sales::actions_view` grant / who declares it."
- **`commands`** — console commands registered via di.xml's `CommandList`/
  `CommandListInterface` argument. "What custom `bin/magento` commands does this codebase add?"
- **`indexers`** — `indexer.xml` + `mview.xml` (indexer definitions, their mview subscriptions
  and tracked tables). Common "why isn't this index updating" surface.
- **queue topology** — `communication.xml` / `queue_topology.xml` / `queue_consumer.xml` /
  `queue_publisher.xml`: topics → consumers → handlers. Complements the `queue` deployment-info
  command (connection) with the wiring.
- **reverse DI** — "who injects / depends on class X" (invert `type_args` + `preferences`); the
  flip side of `di`.
- **`deps <Module>`** — dependency graph from `<sequence>` + composer `require` (forward + who
  depends on it).
- **`graphql <Type>`** — map a `schema.graphqls` type/field to its resolver class via di
  (two-source join; bigger).
- **DB-backed (phase-2 style, opt-in like `url-rewrites`):** `indexer:status` (`indexer_state`),
  `eav`/attributes (`eav_attribute` + `eav_entity_type`), admin users/roles, cron history
  (`cron_schedule`), queue backlog.

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

## Phase 2 (deployment config / DB)

### PHP array parser + `db` commands (done)

- `phparray.rs` — a focused parser for the `<?php return [...];` literals in
  `env.php`/`config.php` (hand-written tokenizer + recursive descent → `PhpValue`
  enum: Array/Str/Int/Float/Bool/Null/Const). Handles `'...'`/`"..."` (PHP escape rules),
  comments, `array(...)` form, and keeps `\Class::CONST` references verbatim as `Const`. We
  never execute PHP. `PhpValue::get`/`as_str`/`as_array`/`scalar_string` for navigation.
- `deploy.rs` — `read_env(root)` parses `app/etc/env.php`; `db_config` extracts the `db`
  section (table prefix + connections). Host parsing splits `host:port`, and a host starting
  with `/` (or an explicit `unix_socket`) is treated as a socket. `Index` now stores `root`.
- `Magento::db_config()` (always available) + `Magento::ping_db(name)` (behind the **`db`
  feature**, which pulls the `mysql` client — `default-features=false, features=["minimal"]`
  to avoid the openssl/TLS build dependency). `db.rs` does a fast `TcpStream::connect_timeout`
  pre-check (5s) so an unreachable host fails fast, then connects + `SELECT VERSION()`.
- CLI (cli enables the `db` feature): `magequery db info` (connections incl. the real
  password — no masking; `(empty)` shown only when the value is genuinely empty) and
  `magequery db ping [<connection>]` (OK/FAIL + server version + ms, non-zero exit on fail).
  Validated on proforto: parsed the socket connection / dbname / empty password correctly;
  ping fails cleanly when the socket isn't reachable.
- `Magento::redis_config()` + `deploy::redis_config` extract every Redis/Valkey usage from
  `env.php`: cache frontends (`cache/frontend/<id>` with a `Redis`/`RemoteSynchronized…`
  backend → `backend_options`/`remote_backend_options`) and session (`session/save == redis`
  → `session/redis`). Handles socket hosts (`/…`) and null ports.
- `Magento::ping_redis()` + `redis.rs` test connectivity over the **raw RESP protocol** (no
  client crate — pure `std::net`, works over TCP and unix sockets): connect → optional
  `AUTH` → `SELECT <db>` → `PING` → `INFO server` for the version. One result per instance.
- CLI: `magequery redis info` / `redis ping` (mirrors `db info`/`db ping`).
  Validated on proforto (Valkey over socket): info shows cache→db3, page_cache→db2,
  session→db1; ping connected to all three (`redis 7.2.4`, ~1ms).

### Deployment-info commands (`session`/`cache`/`lock`/`queue`, done)

Thin `env.php`-parsing projections, same shape as `db`/`redis` (info only — no connectivity
test). Each is a `deploy::<x>_config(env) -> <X>Config` extractor + a `Magento::<x>_config()`
accessor + a CLI renderer; all reuse the shared `redis_endpoint`/`host_port` helpers.
- **session** (`session` section): `save` handler (`files`/`db`/`redis`); for redis the
  server/socket + db, for files the `save_path`. `SessionConfig { handler, location, database }`.
- **cache** (`cache` + `cache_types`): backend per frontend (`default`, `page_cache`) with its
  Redis location/db, plus every cache type's enable flag with an `N/M enabled` summary.
  `CacheConfig { frontends: Vec<CacheFrontend>, types: Vec<CacheType> }`.
- **lock** (`lock` section): `provider` (`db`/`file`/`zookeeper`/`cache`) + provider-specific
  settings (`BTreeMap`, NULL/empty entries dropped). `LockConfig { provider, config }`.
- **queue** (`queue` section): the `amqp` block plus any `queue/connections/<name>`, each with
  host/port/user/password/virtualhost; + the `consumers_wait_for_messages` flag. Passwords
  shown raw (matching `db info`). `QueueConfig { connections, consumers_wait_for_messages }`.
Validated on proforto: session→redis socket db1; cache default→db3/page_cache→db2 + 14/16
types on (layout, full_page off); lock→db; queue→amqp localhost:5672 vhost proforto.

### `url-rewrites` (DB-only, done)

URL rewrites are **runtime data** (generated from products/categories/CMS pages, plus manual
entries) living only in the `url_rewrite` table — there is no static source, so this command
necessarily needs the `db` feature and a reachable DB (no static fallback; clean `Error::Db`
otherwise). `db::fetch_url_rewrites` reads the table, resolving each row's `store_id` to a
store code via `store`. **Filters are pushed into SQL** because the table is often huge:
request/target path substring (`LIKE`, bound via `params!` to avoid injection), `--store`
(resolved to an id first), `--redirects` (`redirect_type <> 0`). Fetches `limit + 1` to detect
truncation and returns `UrlRewrites { rewrites, truncated }`; the CLI flags "showing first N"
on stderr (no silent caps). `Magento::url_rewrites(path, store, redirects_only, limit)`.
- CLI `magequery url-rewrites [<path>] [--store <code>] [--redirects] [--limit 200]`: greppable
  aligned lines — `request_path  →|⇒301  target_path  # entity:id · store=code [manual]`
  (internal rewrites use a dim `→`; redirects a red `⇒<code>`; `manual` marks
  non-autogenerated). Validated on proforto: combined path+store+redirect filtering, manual vs
  autogenerated flagged, store codes resolved, truncation note, clean error on unknown store.

### `config` (system config resolution — static sources, done)

`sysconfig.rs` resolves a config `path` at a `scope` from the **static** sources, merged in
Magento's precedence (lower → higher, higher overrides). The recognized source layers are:
1. `ModularConfigSource` → module `config.xml` `<default>` (parsed in parallel via
   `parse::config_xml_defaults`, which flattens `<config><default|websites|stores>…` into
   `(scope, path, value, line)`),
2. `RuntimeConfigSource` → `core_config_data` (DB, opt-in via `--db`),
3. `InitialConfigSource` → the deployment config: `config.php` `system` then `env.php`
   `system` node (both flattened from the parsed PhpValue).
Then `CONFIG__*` env vars (`CONFIG__<SCOPE>__<PATH>`, path lowercased) override everything.
Stored as `(scope, path) -> ConfigValue { value, source, file, line }`; later sources
overwrite earlier ones.

**Order is derived from di.xml, not hardcoded** (the architecture-faithful refinement, done):
`Magento::system_config_source_order()` reads the `systemConfigSourceAggregated` virtual
type's `sources` argument from the DI index, follows each source's virtual-type indirection
to a concrete class (`classify_config_source`), and sorts by the declared `sortOrder`
(modular 10 → dynamic 100 → initial 1000 by default). A module that re-orders or drops a
source via di.xml is therefore honored; unrecognized custom `ConfigSourceInterface`s are
skipped. Falls back to the default order if the di declaration can't be read. (`CONFIG__*`
env vars are applied last unconditionally — they're the deployment-config overlay, not one of
the aggregated sources.)

- `Magento::config_get(scope, path)` (scope fallback chain mirrors Magento: a **store** falls
  back to its parent **website**, then `default`; a website falls back to `default`). The
  store→website parentage is read **statically** from `config.php`'s `scopes` node
  (`scope_parents`: `scopes/websites/*` gives `website_id → code`, then each `scopes/stores/*`
  `website_id` resolves to its website code) — no DB needed. `config_section(scope, prefix)`,
  `config_scopes(path)`.
- CLI `magequery config <path> [--scope]`: **by default shows the value in every scope that
  sets it** (e.g. each store's `web/secure/base_url`) — for a multi-store install seeing only
  `default` is useless. `--scope <scope>` resolves a single scope (with `(inherited)` note);
  a prefix lists the section, and **omitting the path lists every key** (empty prefix matches
  all — `under("", …)` is true). Each line: `[source]` tag + `# file:line` provenance; `EnvVar`
  shows the reconstructed `$CONFIG__…` name.

### DB config source (`--db`, done)

`Magento::config(include_db) -> ConfigSet` is the public API (replaced the old per-path
methods + the `OnceLock` cache — built fresh per call). With `include_db`, `db::fetch_config`
queries `core_config_data` + `store_website`/`store` (table-prefixed) and resolves each row's
`scope`/`scope_id` to `default`/`websites/<code>`/`stores/<code>`; those rows are applied as
`ConfigSourceKind::Database` **between** config.xml (modular, 10) and the config.php/env.php
`system` overrides (initial, 1000) — so the `system` node correctly wins over the DB, matching
Magento's `sortOrder`. CLI: `magequery config <path> --db` (opt-in; clean `Error::Db` if the
DB is unreachable; static-only otherwise). Validated on proforto: pulled website-scope
base_urls that exist only in the DB (`[db]` source), while `default`/store values from env.php
still won as `[env.php]`. The DB layer's position is **derived** from di.xml's `sortOrder`
(see "Order is derived from di.xml" above), not hardcoded; custom `ConfigSourceInterface`s
aren't read.

### `--decrypt` (done, Magento-faithful)

`decrypt.rs` (`Decryptor`) decrypts Magento-encrypted config values, mirroring
`Magento\Framework\Encryption\Encryptor`.
- **Key loading**: `crypt_keys` splits `env.php` `crypt/key` on **whitespace**
  (`str::split_whitespace`, like Magento's `preg_split('/\s+/', trim($key))`) → the rotated
  keys. The value's `keyVersion` indexes this list; the key is used **directly** (Magento does
  no further derivation).
- **Format parsing** (`parse`): `keyVersion:cipher[:iv]:base64`, with the 1/2/3/4-part
  shorthands Magento accepts (`cipher:data` → keyVersion 0; bare `data` → Blowfish; 4-part →
  Rijndael-256). Picks the right key + the value's own cipher.
- **Ciphers**: 3 = ChaCha20-Poly1305 IETF (modern default, RustCrypto `chacha20poly1305`,
  12-byte nonce + 32-byte key + empty AAD = `SodiumChachaIetf`); 1 = Rijndael-128/ECB
  (= AES-256-ECB via the `aes` crate, zero-padding stripped); 2 = Rijndael-256/CBC (a 256-bit
  *block*, not AES — via the `simple-rijndael` crate, `RijndaelCbc<ZeroPadding>` at block size
  32, mirroring mcrypt's `MCRYPT_RIJNDAEL_256` CBC + 32-byte key/IV; the IV is the 4-part
  form's 3rd field, accepted base64 or raw, zero IV when absent; trailing `\0` stripped). Only
  0 = Blowfish remains **unsupported** (no maintained Rust impl) — flagged distinctly in the
  CLI. Result is trimmed (as `Encryptor::decrypt` does).
- `Magento::decryptor()` + `magequery config <path> --decrypt`. Plaintext untouched; an
  undecryptable encrypted value is flagged `(encrypted — crypt key mismatch?)` (or
  `legacy Blowfish cipher unsupported`). The mismatch case is common: a DB imported from
  another environment whose key isn't in this `env.php`.

Verified by round-trip unit tests (`decrypt::tests`): ChaCha v3 + AES-ECB v1 + Rijndael-256 v2
decrypt, correct key-version selection (wrong version fails), wrong IV ≠ plaintext, Blowfish
returns `None`, `is_encrypted` heuristic. On proforto the DB secrets are v3 from a foreign env,
so they correctly stay flagged.

**Phase 2 is complete** (db info/ping, redis info/ping, config static + DB source, decrypt;
config precedence now derived from di.xml's `systemConfigSourceAggregated` `sortOrder`, not
hardcoded; store→website scope inheritance via `config.php`'s `scopes` node;
`session`/`cache`/`lock`/`queue` deployment-info commands; `url-rewrites`; `schema` from
`db_schema.xml`; decrypt covers every cipher except legacy Blowfish). No open refinements
outstanding.

## Test checkout

`/Users/jelle/www/proforto` — a real Magento 2.4 install (716 modules: 563 vendor + 153
app/code). Validated: `config.php` shape, per-area `di.xml` layout, PSR-4 autoload
(`"Magento\\Catalog\\": ""` = module root), and real plugin-on-interface declarations.
