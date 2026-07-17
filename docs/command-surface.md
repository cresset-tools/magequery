# magecommand — command surface (organization — LOCKED)

magecommand is the **write-side companion to magequery**. magequery *reads* a Magento
entity (a noun); magecommand *writes* it — generate artifacts, mutate config, create
data. The two share a noun vocabulary: `magequery product <sku>` reads a product,
`magecommand product create …` writes one.

**The one grammar rule: `magecommand <group> <subcommand> [args] [flags]`.** Always
exactly two tokens — a `group` (a noun/namespace) then a `subcommand` (a verb or a
type). This is deliberately *not* `bin/magento`'s sprawling `namespace:command`
surface; it is a curated, grep-able set. Bare `magecommand` and bare `magecommand
<group>` print that level's help (clap `arg_required_else_help`), never a default
action. Global flags (`--root <path>`, `--json`) apply to every command.

**Only the `di` group is built today.** Everything below `di` is the planned surface —
documented here so the grammar is fixed before scripts and muscle memory depend on it,
but not yet wired in `main.rs`. New commands MUST slot into a group under this grammar,
never appear as a bare top-level verb.

```
GENERATE   (static, byte-exact reproducible — a real `bin/magento` run is the oracle)
  di       compile | verify | watch | digest      # setup:di:compile              (BUILT)
  static   deploy  | verify | watch               # setup:static-content:deploy   (planned)
  i18n     collect                                # i18n:collect-phrases          (planned)

SCAFFOLD   (Laravel make: — template codegen, no Magento bootstrap, no DB)         (planned)
  make     module | controller | block | model | plugin | observer | command
           | cron | graphql-resolver | ui-component | …

PROJECT    (edit config.php / env.php / flags / cache)                             (planned)
  module   enable <name…> | disable <name…>       mode set <developer|production|default>
  maintenance  enable | disable                   config set <path> <value> [--scope]
  cache    clean [type…] | flush                  # no-bootstrap: Redis/file via env.php

DATA       (live DB — write counterpart to magequery's read cards)                (planned)
  product | order | customer | category | …   create | update <id> | delete <id>
```

## The `di` group (built)

Reproduces `setup:di:compile` byte-for-byte (`generated/code` + `generated/metadata`).
Global flags plus:

```
di compile [--dry-run] [--force] [--incremental]
    Generate the DI config + generated code. --dry-run previews the work plan;
    --force overwrites an existing generated tree; --incremental skips the whole
    compile when no input changed since the last run (stat-fingerprint short-circuit).

di verify --archive <DIR> --output <DIR> [--fail-on-diff] [--sample <N>]
          [--no-explain] [--strict-ordering] [--show-residual <FILE>]
    Diff a generated tree against an archived ground truth (`generated/_code`,
    `generated/_metadata`) — the oracle every milestone is accepted against.
    Known/expected divergences are classified and explained unless --no-explain;
    --fail-on-diff exits non-zero on any UNEXPLAINED diff; --strict-ordering counts
    PHP-reflection method-order differences as changed; --show-residual pinpoints why
    one metadata file stays unexplained. (Was `compare` before the group rename.)

di watch [--once]
    Long-running compile server: build once, keep the parsed index in memory, and
    recompile on file change writing only the delta. The fast edit loop.

di digest [--stat]
    Print the content-addressed digest of the compile inputs — a CI cache key for
    `generated/`. Since compile is a pure function of the source tree, an unchanged
    digest means the last output is still valid. --stat fingerprints by mtime+size
    (fast, local-only; not a portable CI key). See docs/incremental-compile.md.
```

**Planned `di compile` profile — `--fused`.** An opt-in mode that emits creatuity-style
*fused* interceptors (the plugin chain unrolled into the method body, no runtime
`PluginList` lookup) instead of Magento's stock interceptors. It reuses the same
oracle-validated resolution and changes only interceptor rendering. The default stays
stock/byte-exact so the archive-verify gate stays meaningful. Credit: the fused
technique is creatuity's (github.com/creatuity/magento2-interceptors); the `default`
switch branch MUST run the global chain (guards against their issue #28, where global
plugins silently don't fire in CLI/primary scope).

## Which groups earn their keep, and in what order

The grammar absorbs everything above, but the groups differ wildly in difficulty — an
implementation-ordering concern, not a naming one:

- **Easy, no fidelity bar:** `make`, `cache`, `module`/`mode`/`maintenance`. Best early
  wins — `make` especially (template codegen; it can't produce "wrong" output the way a
  reproduction command can).
- **Medium:** `static` / `i18n` — a byte-exact oracle exists, but `static` drags in
  LESS/requirejs/minification (a large surface).
- **Hard / risky:** the `DATA` group. Creating a product/order correctly means
  replicating Magento's write logic (EAV across value tables, `url_rewrite`, sequences,
  stock items, indexer invalidation) with **no byte-exact oracle** — a fundamentally
  harder and riskier class than the reproduction commands, and where the no-bootstrap
  promise is most tempting to abandon. Lands last, and behind a clear "raw writes, you
  own the consistency" understanding.

## The global dev watcher (planned)

Bare `magecommand watch` (no group) is the dev-loop orchestrator: it watches the whole
project and, per change, regenerates the affected artifact **and** purges only the cache
that change invalidated — no bootstrap, cache cleared straight through env.php's
Redis/file backend (the config magequery already reads). The per-artifact `di watch` /
`static watch` remain scoped single-artifact watchers; `cache clean` is also usable
standalone. The change→reaction map is what makes it smart rather than a blunt
clean-everything:

| file changed | regenerate | clean cache |
|---|---|---|
| `di.xml`, PHP class ctor/hierarchy | `di compile` (incremental) | `config` |
| `layout` / `*.phtml` / `less` / `ui_component` | `static deploy` (delta) | `layout`, `block_html`, `full_page` |
| `config.xml`, `system.xml` | — | `config` |
| `i18n/*.csv` | `i18n collect` | `translate` |
