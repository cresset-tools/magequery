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
  `ConfigPath`, `Area` (enum). `ClassName::new` **strips a leading backslash** (`\Foo\Bar`
  ≡ `Foo\Bar`) — the invariant is enforced at construction, not at call sites, mirroring
  Magento's `ltrim($type, '\\')` at every config read. Both spellings occur in real di.xml
  (core module-elasticsearch writes `type="\Magento\…"`) and must merge/compare as one
  name; before this was enforced, `uses` missed backslash-declared virtualTypes and their
  arg inheritance silently failed to merge.
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
ping`, `redis info`/`redis ping`, `queue info`/`queue topology` (bare `queue` = `queue info`,
kept for back-compat via an optional clap subcommand). Info-only nouns
(`session`/`cache`/`lock`) stay flat.

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
  uses <class>           reverse DI: who injects it

ENTRY POINTS  (how execution starts)
  routes [--area]    actions [<url>]    webapi [<url>]    cron [<group>]
  commands [<filter>]                   graphql [<type>|<Type.field>]

DATA          (persistence & model)
  schema [<table>] [--db]       indexers [<id>]
  extension-attributes [<type>]    catalog-attributes [<group>|<attr>]    eav [<attr>] (backlog, --db)

CONFIG & ADMIN (where settings & permissions live)
  config <path> [--scope] [--db] [--decrypt]    system-config [<filter>]
  acl [<resource>]                              menu [<item>]

FRONTEND      (presentation)
  layout [<handle>] [--area]    widgets [<id>]    email-templates [<id>]
  translations <str> [--locale] [--db]   ui-components (backlog)

RUNTIME       (env.php config & live connections)
  db info|ping     redis info|ping     url-rewrites [<path>] [--store] [--redirects] [--limit]
  queue [info]|topology [<topic>]      session   cache   lock   (info-only)

PROJECT       (the codebase itself)
  info      mode   maintenance   base-url [--secure]   admin-url   (single-fact views of info)
  modules [--check] [--enabled|--disabled] [--source app|vendor]
  deps <module>             doctor [--source]          whatis <class>   patches [--db|--pending]
```

### Cross-cutting flag vocabulary (a flag means the same thing everywhere)

- **`--area <name>` / `--all-areas`** — only on area-aware commands (`di`, `preference`,
  `plugins`, `events`, `routes`, `actions`, `webapi`, `uses`). Default = collapsed diff.
  (`uses` has `--area` but no `--all-areas`: its default is already the merged union.)
- **`--json`** and **`--color auto|always|never`** + **`--root <path>`** — global, every command.
- **`--db`** — the opt-in switch on every *hybrid static-or-live* command (`config`,
  `schema`, `patches`;
  future `eav`, `indexers --status`, `patches`). Static by default; DB overlay when asked;
  clean `Error::Db` if unreachable. Pure-live commands (`db`/`redis` `ping`, `url-rewrites`)
  require the `db`/`redis` build feature instead.
- **`--source app|vendor`** — listing commands, "only my code" (today `modules`; generalize).

### Consolidations baked into this lock

- `observers` is folded into **`events [<event>]`** (one command). Already done in code.
- `preference` / `plugins` are **focused views of `di`** — kept flat (high-frequency), but
  documented as such; `di <type>` is the single full entry point.
- reverse-DI shipped as **`uses <class>`** (never a literal `reverse-di` command); if
  `whatis` is ever built, `uses` becomes one of its focused views. `doctor` is the home for cross-index lints (the `modules --check`
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
zero-copy (`Cow`) `Deserialize` over only the fields we use (name, install-path, autoload,
require) — not `serde_json::Value` —
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

`di.rs` builds the merged DI config per area, mirroring Magento: merge Magento's **primary
config first** (it's where framework-level preferences live — `CommandListInterface →
CommandList`, `ScopeConfigInterface → App\Config`, ~230 preferences on a real install;
`Source.module` is the synthetic `(primary)`, module load orders shift by 1), then every
module's `etc/di.xml` in load order → `global`; then each real area = `global.clone()`
overlaid by that area's `etc/<area>/di.xml` in load order. The primary file set is
Magento's exact bootstrap glob (`App\Arguments\FileResolver\Primary`): `{*di.xml,
*/*di.xml}` under `app/etc` — any file *ending in* `di.xml`, top level + one subdirectory
level, in glob order (so a project's `app/etc/zz_di.xml` overrides `app/etc/di.xml`) —
matching Magento's real sequence `extend(primary)` → `configure(global)` →
`configure(<area>)`, verified against `ObjectManagerFactory`/`Environment\Developer`
source. Files are read+parsed in parallel (rayon), merged sequentially in load order so
last-wins is deterministic.

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

### `extension-attributes` (extension_attributes.xml, static, done)

Who bolts what onto which API data interface — the mechanism behind the generated
`…Extension` classes. An `ExtAttrIndex` in `breadth.rs` (lazy, parallel `read_parse` over
`etc/extension_attributes.xml`). `parse::extension_attributes_xml` captures per attribute:
`code`, declared `type` (class or `[]`-suffixed scalar), the gating ACL `<resources>`, and
the `<join>` spec (reference table/fields — the auto-join repositories perform). Merge:
keyed `(for, code)`, last declaration wins wholesale, each attribute keeps the **adding**
module's `Source` — the point: `ProductInterface` is extended by inventory, bundle,
downloadable, configurable, sales-rule, … . `Magento::extension_attributes(filter?)` +
`extended_type(name)`. CLI `magequery extension-attributes [<type>]`: exact type → the
full set (`code  type  ← Vendor_Module  # loc`, plus dim `acl:`/`join:` lines); substring
→ matching types with counts; no arg → all. Validated on commerce-store: 43 extended
types; ProductInterface = 9 attributes from 6 modules, stock_item's ACL gate shown; the
magento_bulk join renders.

### `schema --db` (schema drift vs the live database, done)

The schema half of "is this environment in sync with the code" (`patches --pending` is the
other half): `magequery schema --db` compares the merged declarative schema **and the
`db_schema_whitelist.json` union** against `information_schema`. Four sections, by
severity:
- **declared but missing live** (red) — what `setup:upgrade` would create;
- **whitelisted but no longer declared** (red) — the declarative system owns these, so
  `setup:upgrade` would **DROP** them: the pending-destructive-change detector;
- **declared but not in any whitelist** (yellow) — `generate-whitelist` wasn't run, so a
  future removal would be inert. Real upstream findings: mage-os's
  `email_template.is_legacy` and the tfa_* columns ship unwhitelisted;
- **live but unmanaged** (yellow) — legacy install scripts / non-declarative modules;
  declarative schema won't touch these.

Runtime-managed tables are excluded and counted (`is_runtime_table`): mview `*_cl`
changelogs, `*_replica`, flats, `*_index_store*` dimension tables, `sequence_*`, and the
framework's bookkeeping (`setup_module`, `patch_list`, `cache`, `cache_tag`, `flag`,
`session`) — checked **before** the whitelist, mirroring Magento's diff ignore-list (MSI's
whitelists infamously include `patch_list`; it still never gets dropped). Presence-level
only by design — type/nullability comparison is where the false positives live.
`Magento::schema_drift() -> SchemaDrift`; `schema <table> --db` appends per-table drift
markers ("live schema matches" / missing / unmanaged columns) under the DDL view. ~45ms.

### `catalog-attributes` (etc/catalog_attributes.xml, static, done)

Which attributes Magento loads in each context group (`quote_item`, `wishlist_item`,
`catalog_product`/`catalog_category` collections, `unassignable`, …) — the "why isn't my
attribute available on the quote item" surface. A `CatalogAttrIndex` (lazy, `read_parse`),
attributes unioned per group with the **adding** module's `Source` (Sales adds
`special_price` to `quote_item`, extensions add their own). CLI
`magequery catalog-attributes [<group>|<attribute>]`: no arg → groups with counts; an
exact group → its attributes with `← module` provenance; anything else is an **attribute**
search showing every group containing it (`catalog-attributes special_price` → 2
occurrences with who added each).

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

### `menu` (admin menu tree from `adminhtml/menu.xml`, static, done)

The admin sidebar as data: *where does an admin page live, what route does it open, which
ACL resource guards it.* A `MenuIndex` in `breadth.rs` (lazy, parallel `read_parse` over
`etc/adminhtml/menu.xml`), shaped like `AclIndex` with one structural difference: parents
come from the **`parent` attribute**, not nesting, and declarations are **ops** —
`parse::menu_xml` yields `Upsert` (`<add>`/`<update>` merge identically for us:
attribute-level upsert, the title-giver owns `Source`) and `Remove` (deletes the id;
validated live: CurrencySymbol removes `Magento_Backend::system_currency` and replaces
it). An item whose parent doesn't exist is treated as a root so nothing silently vanishes.
Children by (`sortOrder`, id); pre-order DFS for the tree. `Magento::menu(filter?)`,
`menu_item(id)`, `menu_ancestors(id)`, `menu_children(id)`.

CLI `magequery menu [<item>]`: no arg → the whole tree indented by depth (`id  Title
action  # loc`); substring → flat list (matches id or title); exact id → detail with the
breadcrumb (`Catalog → Inventory → Products`), action, the guarding ACL resource with a
`→ magequery acl <id>` cross-link (the loop with `acl`/`system-config`), dependsOn
module/config, and children. Validated on mageos-lite: 76 items, tree exact, the remove
case, filter by title.

### `acl` (admin permission tree from `acl.xml`, static, done)

The inverse lookup for the `<resource>` ids that `webapi` and `system-config` already print:
*where does `Magento_Sales::actions_view` sit in the admin permission tree, what does it grant,
who declares it.* Another lazy `AclIndex` (`OnceLock`) in `breadth.rs`, parsed in parallel via
`read_parse` over `etc/acl.xml` (a **global** file — `read_parse(Area::Global, …)` — though
each resource's `Source.area` is tagged `adminhtml`, its domain), merged in load order.
- `parse::acl_xml` walks the nested `<resource>` tree with a **stack of enclosing ids**, so each
  resource records its `parent` (from nesting), `title`/`sortOrder`/`disabled` (attributes, not
  text — simpler than `system.xml`), and line. Only a non-self-closing `Start` pushes the stack,
  so a leaf `<resource/>` can't capture following siblings. Two unit tests lock nesting + the
  anchor-restatement case.
- Merge (`AclIndex::build`): resources keyed by id, **merge-non-empty** — a later file re-states
  ancestors as bare path anchors (no title) only to attach a child under another module's
  resource, so title/sortOrder carry forward and the module that gives the **title** owns the
  `Source`. After merge, children lists are built from the parent pointers (sorted by
  `sortOrder` then id) and the whole forest is flattened to a **pre-order DFS** `order` for
  stable tree rendering. Cycle-guarded (malformed parent loops can't hang the DFS/breadcrumb).
- `Magento::acl(filter?)` (tree pre-order, or id/title substring matches), `acl_resource(id)`,
  `acl_ancestors(id)` (breadcrumb), `acl_children(id)`. CLI `magequery acl [<resource>]`: exact
  id → detail (the resource + its `Magento Admin → … →` breadcrumb + the sub-resources it
  grants, all with provenance); substring → flat aligned list; no arg → the whole tree, indented
  by depth. Validated on mageos-lite: the Sales permission tree renders nested with exact lines;
  `acl Magento_Catalog::products` resolves the very resource `webapi /V1/products` cites
  (`→ Catalog → Inventory`, grants `update_attributes` + …) — the loop the command closes.

### `commands` (console commands from di.xml, static, done)

"What custom `bin/magento` commands does this codebase add?" — a projection over the DI
index, not a new parser. Modules register commands as items of the `commands` array argument
on **either** `Magento\Framework\Console\CommandListInterface` (most) **or** the concrete
`CommandList` (e.g. Magento_EncryptionKey); Magento unions them because argument config
merges along the class hierarchy (`Relations` = parents + interfaces). `console_commands()`
mirrors that: resolve the preference (declared in `app/etc/di.xml` — why the primary-config
merge above was a prerequisite) → `args_of(concrete)` (whose ancestor walk pulls in the
interface's args) → expand the array items, each with per-item provenance.

The **actual CLI name** (`cache:clean` — the item key is just a merge identity) is extracted
from the command class by `php.rs::command_info`: a token scan for `setName(…)`/
`setDescription(…)` (incl. the `__('…')` translation wrapper), Symfony's `$defaultName`/
`$defaultDescription`, and `parent::__construct('…')`; values may be literals, `self::CONST`,
or `$this->prop` — consts/property-defaults are collected from the file and, when the
reference isn't local, resolved via the **ancestor files** (`resolver::command_info`). A
`\Proxy` class suffix (generated lazy wrapper, absent on a fresh checkout) is stripped to
read the real class. Only whole-argument values count (a concatenation like
`$this->prefix . 'x'` stays unknown → the CLI falls back to the dimmed `(item_key)`).
Round-trip unit tests in `php::command_tests`. Limitation: commands registered via
`cli_commands.php`/`CommandLocator` (a handful of framework ones: `maintenance:*`,
setup's) have no di.xml declaration and aren't listed.

CLI `magequery commands [<filter>]` (filter matches name/class/item key, case-insensitive):
2 lines per command — `name  description` / `class  # di.xml:line`. Validated on
mageos-lite: 64 commands, every name resolved (incl. `$this->commandName` property
indirection in Indexer's dimension commands and the four MessageQueue `\Proxy` entries);
`encryption:*` proves the interface+concrete union. ~20ms (di parse dominates).

### `indexers` (indexer.xml + mview.xml, static, done)

The "why isn't this index updating" surface: indexer definitions joined with the tables
whose changes feed them. An `IndexerIndex` in `breadth.rs` (lazy `OnceLock`, parallel
`read_parse`, both files global):
- `parse::indexer_xml` — `<indexer id= view_id= class= shared_index=>` with `<title>`/
  `<description>` text children and `<dependencies>`. The subtlety (same shape as
  db_schema's column-reference case): an `<indexer>` inside `<dependencies>` is a
  *reference* to another indexer, not a definition — routed by an `in_dependencies` context
  opened only on a `Start` event. Unit tests lock it.
- `parse::mview_xml` — `<view id=><subscriptions><table name= entity_column=/></…>`. Only
  the id (join key) + subscriptions are read.
- Merge: indexers keyed by id, merge-non-empty (a later module may re-state one to override
  its class or **add dependencies** — e.g. Elasticsearch adds three deps to
  `catalogsearch_fulltext`); dependencies union; `source` keeps the first declaration.
  View subscriptions merge by table name, each keeping the **adding** module's `Source`.
  Join `indexer.view_id → view` at build time.
- `Magento::indexers(filter?)` (id/title substring) + `Magento::indexer(id)`.

CLI `magequery indexers [<id>]`: exact id → detail (title, description, class, view,
`shared` + the other indexers sharing that physical index, `depends on`, and the
subscription list with cross-module `← Vendor_Module` tags); substring → filtered list;
no arg → all (`id  Title  N tables  # loc`). Validated on mageos-lite: 12 indexers;
`catalog_product_price` shows `catalogrule_product_price ← Magento_CatalogRule`; the
`category_product` shared-index pair cross-references; deps exact. ~3ms.

### `queue topology` (message-queue wiring, static, done)

The static half of the queue story (`queue info` = the env.php connections): *when code
publishes topic X, which queue does it land in and who processes it* — joined from four
global files in an `MqIndex` (lazy `OnceLock`, parallel `read_parse`):
- `communication.xml` → topics + handlers (`parse::communication_xml`); topics merge by
  name, handlers by name **attribute-level** (like plugins — `disabled` is `Option<bool>`
  so a later `<handler name=… disabled="true"/>` keeps the class).
- `queue_consumer.xml` → consumers by name, merge-non-empty (`connection` is optional —
  Magento defaults amqp with db fallback at runtime; reported, not resolved).
- `queue_topology.xml` → exchanges keyed **(connection, name)** (same exchange name on
  amqp and db = two exchanges; connection absent ⇒ XSD default `amqp`), bindings by id.
- `queue_publisher.xml` → publishers by topic; `<connection>` children merged by name,
  flattened to the one enabled connection. Also parses the **direct `queue=` shorthand**
  (`<publisher topic=… queue=…/>`, in this codebase's publisher.xsd) — most core modules
  use it.

`Magento::queue_topics(filter?)` (list) + `Magento::queue_topic(name)` → `MqTopicRoute`:
topic + publisher + routes, where each route = one **queue**, every `via` leading to it (the
publisher's direct `queue=` and/or each enabled binding whose **AMQP pattern matches** —
`topic_matches` implements `.`-word semantics, `*` = one word, `#` = zero+, unit-tested),
and the queue's consumers (joined by queue name). A topic declared only in
queue_publisher.xml still routes (stub topic, empty handlers). CLI: `queue topology`
(list: `topic  → queue(s)  (N handler, M consumer)  # loc`), exact topic → detail
(request/response/schema, handlers, `publishes to exchange`, per-queue `via …` lines +
consumers, red flag when **no consumer reads a queue** or no route exists). Validated on
mageos-lite: sales_rule.codegenerator routes to queue `codegenerator` via both the direct
publisher and its binding on exchange magento (amqp), consumer joined; ~3ms.

### `info` + fact commands (the everyday facts, done)

`magequery info` — one screen for "what am I looking at": Magento **version** (from the
product package in `installed.json` — `*/product-enterprise-edition` →
`*/product-community-edition` → `*/magento2-base`, first hit; the package name also tells
the distribution apart), **deploy mode** (`env.php` `MAGE_MODE`; absent = "default"),
**maintenance** (`var/.maintenance.flag` + exempt IPs from `var/.maintenance.ip`),
**base URLs**, **admin URL**, the **frontend stack**, and — the sys:info parity set — **search engine**
(`catalog/search/engine`), **db** (dbname @ host/socket + table prefix, credentials
deliberately omitted from this paste-into-a-ticket view), **session** and **cache**
one-liners (reusing the env.php extractors; an empty backend class renders as the implicit
`file` default), the **store hierarchy** (websites → stores/groups → store views, counted from the live
DB when reachable — `db::fetch_scope_counts` — else from `config.php`'s `scopes` node when
the config is dumped; the synthetic admin scopes are excluded either way; unknown levels
are skipped, never guessed), the **checkout stack** (a curated package map — Hyvä Checkout, Loki
Checkout (`loki-checkout/magento2-core` matched before the vendor prefix so the version is
core's, not an add-on's), Firecheckout, Mageplaza OSC, OneStepCheckout, Bold — then a
generic "any non-core package named *checkout*" fallback reported verbatim; nothing found
renders as "default (Luma)". Hyvä Checkout additionally exposes *which* checkout is
selected — `hyva_themes_checkout/general/checkout`, read through the same ConfigSet:
`default` = the Magento/Luma original is still active, rendered "installed, not selected";
any other value is the chosen namespace, shown verbatim as "(active: …)" — installed ≠
selected. Theme values may be stored in the full-path form (`frontend/Hyva/default`, as
found live on commerce-store); the leading area segment is normalized away before
classification/ancestry matching),
**module counts split vendor / app/code**, the **composer package count**, the
**install date** (`env.php` `install/date`), **locale · currency · timezone**, the
**search host** (`catalog/search/<engine>_server_hostname`), the **FPC application**
(built-in vs varnish, on the cache line), the **queue endpoint**, and **cron health** —
seconds since the last successful `cron_schedule` run, computed with the DB server's own
clock (`TIMESTAMPDIFF`, no client-side time): green under 15 minutes, red "STALE" beyond,
red "no successful runs recorded" when the table has none; the line is skipped when the DB
is unreachable (unknown ≠ alarming).

Rendering: rows go through `info_row` (labels padded *plain* then dimmed, so escape codes
don't break alignment; values carry the color), grouped into blank-line-separated blocks —
identity / web / stack (frontend, checkout, search) / infra (db, session, cache, queue,
cron) / content (stores, modules, packages).

Frontend detection (`theme`/`frontend`/`frontend_version`): the active theme =
`design/theme/theme_id` (default scope; a numeric id is resolved — and its ancestry
walked — via the DB `theme` table through `db::fetch_themes`; a path string works without
it), falling back to the DI default (`Magento\Theme\Model\View\Design`'s
`themes['frontend']` argument — what Magento itself uses when nothing is configured). The
chain is classified: any `Hyva/*` ancestor → Hyvä (version from the
`hyva-themes/magento2-default-theme` package), `*breeze*` → Breeze (swissup packages),
`Magento/luma`/`Magento/blank` → Luma/Blank; an unclassified path renders as "(custom
theme)". Two honesty rules: when only packages identify the stack (active theme
unresolvable) the CLI says "(installed; active theme unknown)", and the DI default is NOT
trusted when the DB is unreachable while Hyvä/Breeze packages are installed — the real
theme row is invisible and "Luma" would be a confident wrong answer on a Hyvä shop. Unlike the `--db` commands, `info`
**always tries the database** (base URLs usually live only in `core_config_data`) and
degrades to the static sources when unreachable — `InstanceInfo.db_error` records why and
the CLI prints a stderr note; the fail-fast TCP pre-check keeps the down-DB case at ~50ms.
Admin URL mirrors Magento: base = `admin/url/custom` when `use_custom`, else the first
*concrete* base URL (secure preferred, never a `{{base_url}}` placeholder = auto-detect);
path = `custom_path` when `use_custom_path`, else `env.php` `backend/frontName`.
`installed.json` parsing also extracts each package's `version` (kept on `PackageMeta`).
Every env-derived field degrades to `None` on a fresh checkout with no `env.php`.

**Fact commands** — script-friendly single values, all views of `info()`: `mode` (prints
`developer`/`production`/`default`), `maintenance` (`on`/`off`), `base-url [--secure]`,
`admin-url`. Bare value on stdout; when the value isn't concrete (placeholder base URL, no
frontName) they exit non-zero with the reason — incl. the DB error when that's why — so
scripts can branch. Validated on mageos-lite against its live MariaDB (full URLs resolve
with no flags) and on a synthetic root with an unreachable DB (static fallback + note,
`admin-url` exits 1, maintenance flag + IPs read).

### `graphql` (schema types → resolvers, static, done)

`magequery graphql [<Type>|<Type.field>]` — the GraphQL schema as Magento actually
assembles it, with every field mapped to its `@resolver` class. `graphql.rs` is a focused
hand-written SDL parser (own module, like `php.rs` — no parser crate): a lexer (commas =
whitespace per spec, `#` comments, `"…"`/`"""…"""` strings unescaped — the `\\Magento…`
FQCNs in directive strings become real backslashes, then `ClassName::new` normalizes the
leading one) + tolerant recursive descent over type/interface/input/enum/union/scalar
definitions, field args with defaults, and directives. `extend type X` is treated as a
plain re-declaration (the merge unions it — also how Magento's stitching reader treats
re-declared types); `schema {}` and `directive @x on …` *definitions* are skipped. The
subtle bugs are greedy name lists: `union A = B | C` members and `implements A & B` names
are only consumed behind an explicit `|`/`&`, else a bodiless definition would swallow the
next definition's keyword. Unit test locks all of it.

`GqlIndex` (lazy, parallel `read_parse` over `etc/schema.graphqls`, `Source.area` tagged
Graphql): types merge by name — implements/enum-values/union-members union, `@typeResolver`
and `@doc` overwrite-when-present, **fields union by name** (a re-declaration replaces,
last module wins, and keeps the declaring module's provenance — the point: `Query` is
assembled from dozens of modules). Extracted per field: `@resolver(class:)`, `@doc`,
`@deprecated(reason:)`, `@cache(cacheable:)`. `Magento::graphql_types(filter?)` +
`graphql_type(name)`.

CLI: list = `Name  kind  N fields  # loc`; exact type → detail (description, implements,
`@typeResolver`, then fields as `name(arg, names): Type` with the `@resolver` line,
cross-module `← Vendor_Module` tags, red `[deprecated: reason]`, dim `[not cacheable]`);
`Type.field` → one field with fully-typed args. The di join: each shown resolver is run
through `preference()` and a redirect renders as `→ preference Concrete` — what you'd miss
reading the schema alone. Validated on mageos-additive-boot (45 schema files, 415 types):
`Query` = 36 fields from ~15 modules each correctly attributed; `ProductInterface` = 53
fields incl. Inventory/GiftMessage additions; the `extend type ShippingCartAddress` case
merges; enum/union/field views exact. List ~10ms; type detail ~23ms (builds the DI index
for the preference join).

### `doctor` (cross-index lints, done)

`magequery doctor [--source app|vendor]` — everything the merged config references that
doesn't exist, structural breakage, and probably-forgotten wiring. Pure projection over the
existing indexes (~90–140ms); exits 1 on **errors** only, so warnings can't fail CI.
`doctor.rs` in core, `Magento::doctor(source?) -> DoctorReport` (typed `DoctorLint` +
`Severity` per finding, provenance where there is one).

**Errors** (break at runtime): preference targets / virtualType bases / plugin classes /
di argument objects / observer instances / cron instances / webapi services / console
commands / mq handlers+consumers / GraphQL resolvers referencing **missing classes**;
webapi `<resource>` ids no acl.xml declares; preference/virtualType/`<sequence>` cycles;
`in_config_not_on_disk` modules. **Warnings**: `on_disk_not_in_config` modules
(setup:upgrade drift), queues no consumer reads, and the **unregistered-code** lints —
classes under `Console/`/`Observer/`/`Plugin/` that are concrete, match their base type
(Symfony Command / ObserverInterface / has `before*`/`around*`/`after*` methods) and are
wired **nowhere**. `--source` restricts only these scans; candidates are verified by
resolving the conventional class name back through PSR-4 to the same file (namespace-
diverging modules are skipped, never misreported).

The false-positive war is the design (doctor must not cry wolf) — `class_known` accepts:
global-namespace names (PHP built-ins like `DateTime`), virtual types, generated code
(`\Proxy`/`\Interceptor`/`…Factory` verified against their base; `…Extension[Interface]`
as-is), and **namespaces no autoload prefix covers** (classmap packages are unverifiable
from installed.json). "Registered" sets are widened by virtual-type bases (Sales registers
grid observers as virtualTypes), ancestors of registered classes, and **any class
referenced anywhere in DI** (preference targets — how Elgentos swaps in its
GenerateVclCommand — and argument objects). Building doctor also drove two resolver fixes:
**PSR-0 support** (`Cm\RedisSession` — session save handler classes) and the **root
composer.json autoload** (`Magento\Setup\`, and the whole framework on git checkouts).

Validated on mageos-lite (down to 2 errors + 2 warnings, all four *genuine upstream
Magento bugs/dead code*: the dangling `Magento\Indexer\Model\Handler\DefaultHandler` di
argument, the `ProductRenderSearchResultsInterface` preference to a nonexistent class,
`MaxHeapTableSizeProcessorOnFullReindex`, `CouponUsagesDecrement`) and commerce-store
(caught a real mage-os bug: crontab.xml references `Cron\UpdateRemoteTemplates`, the class
on disk is `UpdateRemoteTemplateList`; plus genuine Hyvä-modules-not-in-config.php drift).
A synthetic broken module exercises every lint in one run.

### `patches` (setup patches, static + `--db`, done)

`magequery patches [<filter>] [--db|--pending]` — every `Setup/Patch/Data|Schema` class of
the enabled modules (what `setup:upgrade` runs). The scan reuses doctor's walker +
PSR-4-verified class derivation, keeps only concrete classes whose ancestors include
`DataPatchInterface`/`SchemaPatchInterface` (the dirs also hold helpers), and sorts by
(module, class). `--db` marks each **applied/PENDING** per the `patch_list` table
(`patch_name`, leading backslashes normalized; clean `Error::Db` when unreachable) and
reports **orphaned** applied entries no on-disk class explains (patches of removed modules
— never silently dropped, stderr note). `--pending` shows only unapplied ones (implies
`--db`) — the pre-deploy question. `Magento::patches(filter?, include_db)` → `Patches`.
Validated: 133 patches on mageos-lite / 196 on commerce-store, all applied on both (fully
upgraded stores), filter and pending modes exact.

### `whatis` (everything about one class, done)

`magequery whatis <Class>` — the aggregate view for "what IS this thing": pure composition
(`whatis.rs`) of existing queries plus the doctor-style cross-index sweep scoped to one
class. Sections (empty ones omitted): **identity** — file, kind (class/abstract/interface
from the header; "virtual type" flagged), owning module (longest module-path prefix of the
file) + composer package/version (root-ancestor walk over `PackageMeta`), direct
extends/implements; **DI summary** — `resolves to` (preference redirect), `instantiates`
(vtype base), plugin/argument counts with a `→ magequery di X` pointer, and the inlined
`Uses` counts with `→ magequery uses X` (whatis stays scannable; the focused commands are
the drill-downs); **the sweep** — events it observes, cron jobs, webapi routes, the
`bin/magento` command name, GraphQL `@resolver`/`@typeResolver` fields, mq topic handlers
+ consumers, and controller URLs (the directory scan only runs when the name contains
`\Controller\`). Works on virtual types. A real file with **zero references** prints the
interesting negative: "(no configuration references this class — candidate dead code, or
wired only in PHP)". Unknown name + no references = `ClassNotFound`. ~30ms warm on lite.
Validated across every role: mq handler class, preference target, console command,
observer, controller (URL resolved), virtual type, GraphQL resolver, dead code
(`CouponUsagesDecrement`).

### `deps` (module dependency graph, done)

`magequery deps <Module>` — both directions, from the two static sources:
- **`<sequence>`** (module.xml, load-order deps) — already on `Module.sequence`; reverse =
  every module whose sequence names the target.
- **composer `require`** — `installed.json` now also yields `name` + `require` per package
  (`ComposerPackage` grew two fields; `Index` retains a slim `PackageMeta` list). A module
  finds its owning package by walking its path's ancestors to a package root; each required
  package maps back to the module(s) it provides. app/code modules aren't in installed.json,
  so their own `composer.json` is read instead (`read_app_composer`).

Edges dedup by module with `via_sequence`/`via_composer` OR-ed (source = the declaring
file: module.xml wins when both). Composer edges have composer's granularity (a required
package bundling several modules ⇒ one edge per module; same-package siblings are not
edges). Each edge carries `installed`/`enabled` — a `<sequence>` entry naming an absent
optional module is common and flagged `(not installed)`, never hidden; requires that no
installed module provides go to `other_requires` (framework, libs, `php`/`ext-*`).
`Error::ModuleNotFound` (new variant) for an unknown name; the CLI hints at
`modules | grep -i`. CLI line: `Module  sequence, composer  [(not installed)]  # loc`.
Validated on mageos-lite (`Magento_SalesRule`: 21 forward — 4 via both sources — 3
reverse) and a synthetic app/code module (composer.json read, missing sequence target
flagged). ~4ms; `modules` unaffected.

### `uses` (reverse DI, done)

The flip side of `di`: `di Foo` = "when Magento builds Foo, what does it get"; `uses Foo` =
"who receives Foo" — the impact-analysis question. A pure inversion of the in-memory DI
index (`Magento::uses(class, area?)` → `Uses`), no new parsing:
- **`preferred_for`** — preference entries whose *target* is the class (which interfaces
  resolve to it), directly (one hop, not transitive).
- **`virtual_types`** — virtualTypes built on it (`type=` the class).
- **`injections`** — every constructor argument wiring it in, walking argument trees
  recursively: `Object` values matching the class **or its generated `\Proxy`** (lazy
  wrappers count as injections, flagged `via \Proxy`), and `xsi:type="string"` values
  spelling its FQCN (factory/pool-style registration, flagged `as string` — this is how
  `RouterList` entries reference router classes, so it matters). Each hit carries the
  consumer (flagged when itself a virtual type), the argument name + **array-key path**
  (`$routerList['cms']['class']`), and the item-level `Source`. Whole-value matches only.

Area model: default = the merged union — scan global fully, then each area keeping only
declarations made **in that area's own files** (`source.area == area`), so global-inherited
facts aren't repeated per area; each hit's `source.area` is the honest tag. `--area <name>`
= that area's fully merged config instead. No `--all-areas` (the default is already the
union). The target may itself be a virtual type (pools inject vtypes; works naturally, and
`class_known` already treats vtypes as real). Zero hits on an unknown name →
`Error::ClassNotFound`; on a real class → an honest "(nothing in di.xml references it)"
note that autowired constructor type-hints have no di.xml declaration (the known scope
limit: full constructor scanning would break the on-demand-PHP philosophy).

Validated on mageos-lite: `Cms\Controller\Router` → `$routerList['cms']['class']` as
string, area=frontend; `Session\Storage` → 1 reverse preference + 8 vtypes (incl. per-area
declarations, each with its own source); a vtype target (`Backend\Model\Session\Storage`)
resolves its injector. ~19ms (di parse dominates).

### `layout` (layout handles, static, done — first of the FRONTEND group)

`magequery layout [<handle>] [--area]` — which files contribute to a layout handle and
what they do to the page: the "where does this block come from" question. A `LayoutIndex`
in `breadth.rs` over **two layers**: every enabled module's `view/{base,frontend,
adminhtml}/layout/*.xml` (base applies to both areas; merged in load order — Magento's
real base merge) and every **theme**'s `<theme>/<Vendor_Module>/layout/*.xml`. Themes are
discovered statically (`discover_themes`): composer packages whose root holds `theme.xml`
(id read from `registration.php`'s `'frontend/Vendor/name'` literal) plus
`app/design/<area>/<Vendor>/<theme>`. Theme files are listed after modules tagged
`theme <id>` — theme *application* order depends on the active theme's ancestry (runtime
state), so it's reported, not resolved. Handle = file stem; all files parsed in parallel.

`parse::layout_xml` flattens each file into **ops** with an enclosing-element stack (only
`Start` pushes, so self-closing references can't corrupt nesting — unit-tested): `+ block`
(class, template, `(in parent)`), `+ container`, `~ referenceBlock/Container`,
`✕ remove` (`remove="true"`), `← update <handle>`, `→ move X to Y`. The index also builds
the **handle-inclusion graph**: each view lists `includes:` (its `<update>`s) and
`included by:` (who pulls it in). CLI: no arg → handle list with file counts (102 frontend
handles on lite); `<handle>` → per-file op stream with per-op `#line`. Known limitation:
theme `layout/override/` replacement semantics not modeled. Validated: Luma's
catalog_product_view `<move>`s render under the theme layer; commerce-store's `default`
handle = 53 files, 12ms.

### `widgets` (widget types from `etc/widget.xml`, static, done)

What the admin's "Insert Widget" dropdown offers, as data: id, label, the **block class**
that renders it, and the full parameter set. A `WidgetIndex` (lazy, `read_parse` over
`etc/widget.xml`, widgets merged by id / parameters by name). `parse::widget_xml` handles
the two traps: a `<parameter>` inside `<depends>` is a *reference* (the db_schema
column-reference pattern — never a definition), and `<label>` occurs at widget, parameter,
AND option level — routed to the innermost open context, option labels ignored. Captured
per parameter: name, `xsi:type`, `required`, label, `source_model`, `<value>` default;
plus the widget's `<container>` placements. CLI `magequery widgets [<id>]`: list (`id
Label  class  # loc`), exact id or single-match substring → detail with the aligned
parameter table (`name[*] type  Label  source_model  default=`). Validated on mageos-lite
(9 widgets; products_list = 7 params with requireds, defaults, and Yesno source model
exact). Unit test locks the depends/options traps.

### `email-templates` (etc/email_templates.xml, static, done)

Transactional templates as data: id (= the value config stores when a template is
selected), label, type, area, and — the payoff — the **resolved file** and **theme
overrides**. The declared `file` lives in the *referenced* module's `view/<area>/email/`
(the `module` attribute may differ from the declaring module; last declaration per id
wins, since modules re-register each other's templates). Every discovered theme
(`discover_themes`, shared with `layout`) is probed for `<theme>/<Module>/email/<file>`;
matches are listed as overrides — which one applies depends on the active theme, so
reported, not resolved. A declared-but-missing file renders a red `[file missing]`.
CLI `magequery email-templates [<id>]`: list (`id  Label  file  # loc`, `themed×N` tag),
exact/single-match → detail with the resolved path and per-theme override files.
Validated on mageos-lite: 32 templates; `customer_create_account_email_template` resolves
its module file and Luma's override.

### `translations` (dictionary rows in precedence order, done)

`magequery translations <str> [--locale] [--db]` — every dictionary row for a phrase, in
Magento's precedence order, **verified from `Framework\Translate` source** before
building: (1) module `i18n/<locale>.csv` in load order, where at runtime the *current
request's controller module* additionally loads last and wins the layer — request-scoped,
not phrase-scoped, so it can't be resolved statically and the CLI prints the caveat when
module rows conflict; (2) language packs (root `language.xml` probed on composer packages
+ `app/i18n`, filtered by locale code, ordered by `sort_order`; `<use>` inheritance not
modeled); (3) theme `i18n/<locale>.csv` (parents load first, child wins — which chain
applies is active-theme state, caveat printed); (4) the `translation` DB table via `--db`
(store_id shown). **The `_addData` twist is modeled**: an identity row (`key == value`)
*deletes* earlier translations — rendered red "(identity row — deletes earlier
translations)", and the effective-value fold honors it, ending in "(untranslated — the
phrase renders as-is)" when a reset lands last. `parse::i18n_csv` is a real CSV state
machine (quotes, `""` escapes, multiline values, legacy extra columns ignored;
unit-tested). Locale defaults to the configured `general/locale/code`. Exact phrase (or a
single substring hit) → the layered view with `← effective`; multiple hits → key list.
Validated synthetically: module load-order layering, theme override winning, and an
identity row deleting an earlier module's translation.

## Future query tools (backlog — not yet built)

Ideas surfaced while scoping breadth, in rough priority. All but the DB ones are
static breadth-projections in the same `read_parse` + merge shape.
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
