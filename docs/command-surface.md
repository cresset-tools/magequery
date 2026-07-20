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

**The `di` group and the `static` group's LESS + requirejs + bundle + minify pipelines are built today.** Everything
else is the planned surface — documented here so the grammar is fixed before scripts
and muscle memory depend on it, but not yet wired in `main.rs`. New commands MUST slot
into a group under this grammar, never appear as a bare top-level verb.

```
GENERATE   (static, byte-exact reproducible — a real `bin/magento` run is the oracle)
  di       compile | verify | watch | digest      # setup:di:compile              (BUILT)
  static   less | cssdiff | requirejs | bundle | minify   # LESS + JS deploy artifacts (BUILT)
           deploy | verify | watch                # full static-content deploy    (planned)
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
di compile [--dry-run] [--force] [--incremental] [--fused]
    Generate the DI config + generated code. --dry-run previews the work plan;
    --force overwrites an existing generated tree; --incremental skips the whole
    compile when no input changed since the last run (stat-fingerprint short-circuit);
    --fused emits fused interceptors (see below).

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

**`di compile --fused` (built) — the fused-interceptor profile.** An opt-in mode that
emits creatuity-style *fused* interceptors (the plugin chain unrolled into the method
body, no runtime `PluginList` lookup) instead of Magento's stock interceptors. It reuses
the same oracle-validated resolution and changes only interceptor rendering, so the
default stays stock/byte-exact and the archive-verify gate stays meaningful. Validated
byte-exact against a fixed-creatuity `di:compile` across a full codebase (2489 fused
interceptors, every edge case). The `default` switch branch runs the **global** chain —
guarding against creatuity's issue #28 (global plugins silently not firing in
CLI/`primary` scope). Credit: the fused technique is creatuity's prior art
(github.com/creatuity/magento2-interceptors), reimplemented clean-room.

## The `static` group (LESS + requirejs + bundle + minify built)

The pure-Rust half of `setup:static-content:deploy` (no PHP, no node). Global
flags plus:

```
static less --theme <VENDOR/NAME> [--locale <L>] [--entry <NAME>…] [--out <DIR>]
            [--stdout] [--compress] [--skip-broken-modules]
    Assemble and compile a theme's LESS entry points (theme fallback chain,
    //@magento_import expansion, Vendor_Module:: resolution) into
    pub/static/<area>/<theme>/<locale>/css/. --out redirects the writes;
    --stdout prints a single --entry's CSS instead. --skip-broken-modules drops a
    broken module's partial and re-splices instead of failing the entry point.

static less --file <PATH> [--out <DIR>] [--stdout] [--compress]
    Per-file mode — the interface the Magento bridge adapter shells out to
    (Css\PreProcessor\Adapter\Less\Processor parity): compile ONE materialized
    .less file (var/view_preprocessed — //@magento_import and module notation
    already expanded), relative imports resolved from its directory, under the
    Magento production profile (relativeUrls=false, parens-division math).
    Prints to stdout unless --out (written as <stem>.css). Mutually exclusive
    with --theme/--entry. A compile error exits non-zero with the compiler's
    rendering (file, line, column, source excerpt) verbatim on stderr — the PHP
    adapter shows that message as-is.

    --compress (both modes) sets the compress serializer — Less_Parser
    compress=true, what Magento's adapter uses outside developer mode.

static requirejs --theme <VENDOR/NAME> [--locale <L>] [--out <DIR>] [--stdout]
    Emit a theme's requirejs deploy artifacts into
    pub/static/<area>/<theme>/<locale>/ at their deployed relative paths, one
    report line each:
    - requirejs-config.js — a TEXTUAL concatenation, not a semantic JS merge
      (Framework\RequireJs\Config::getConfig): each collected
      requirejs-config.js is wrapped in an IIFE and the whole thing in one
      outer IIFE. Collector order: lib/web → module contexts (view/base then
      view/<area>, config.php load order) → theme layers ancestor-first (each
      theme's <Vendor_Module>/ contexts in load order, then its own file).
    - requirejs-min-resolver.js — Config::getMinResolverCode: the fixed IIFE
      template whose only variable part is the exclude condition —
      url.indexOf(baseUrl)===0 plus one !url.match(/<regex>/) per
      dev/js/minify_exclude value (Minification::getExcludes('js'); module
      config.xml <default> merge in load order; only / is escaped, as \/),
      joined with &&. Unminified — the default-mode artifact.
    - mage/requirejs/mixins.js — a verbatim byte copy of
      lib/web/mage/requirejs/mixins.js.
    --out redirects the writes (same relative paths under <DIR>); --stdout
    prints ONLY requirejs-config.js and writes nothing — the sibling artifacts
    are not emitted (backward compatible with the original config-only mode).
    The global --json claims stdout for the ordered source list (file, module,
    theme, origin) — the "which module contributed what, in what order" view —
    plus the min-resolver's excludes and the mixins source, so it replaces the
    raw JS: with --json the files are still written unless --stdout is also
    given, in which case nothing is written at all.

static bundle --theme <VENDOR/NAME>... [--locale <L>] [--out <DIR>]
              [--order probe|sorted] [--probe-dir <DIR>]
    Generate a theme's js/bundle/bundle<N>.js files (SCD JS bundling —
    Deploy\Service\Bundle + Package\Bundle\RequireJs) from the SOURCE tree.
    The real deploy bundles the DEPLOYED package, so the command first
    resolves that view of the world: the js/html subset of static-deploy
    file resolution (lib/web at the package root minus css/docs; per enabled
    module view/base + view/<area> web files plus i18n/<locale> overlays;
    theme chain ancestor-first — theme web/ at the root, <Vendor_Module>/web/
    contexts, i18n overlays — later layers win), plus the two generated
    requirejs artifacts (both are bundled on real deploys). Then Magento's
    exact pipeline: recursive *.* glob order (files of a dir before its
    subdirs), js→jsbuild / html→text pools, the order-dependent .min-sibling
    drop rule, theme etc/view.xml <exclude> items (Lib:: → package root,
    raw-prefix directory matches), Js_Bundle/bundle_size splitting (strict >,
    fractional KB of CHARACTER count), continuous bundle numbering across
    pools, PHP json_encode(JSON_UNESCAPED_SLASHES) maps, and the RequireJS
    init snippet on the last file. Byte-exact against a real bundled deploy
    (goldens gate: blank + luma, 7 bundles each).
    --theme is repeatable and ordered: one invocation = one deploy run — the
    .min-sibling cache is SHARED across the run's themes (upstream keeps it
    on the service object), so bundling blank then luma drops luma's plain
    variants whose .min siblings blank iterated first, exactly like Magento.
    --order probe (default) reproduces the output filesystem's readdir order
    (what PHP's GLOB_NOSORT glob yields; a pure function of each directory's
    name set on ext4-htree-style filesystems) by probing scratch dirs —
    --probe-dir must be on the deploy target's filesystem (default: the
    output base). --order sorted is the portable lexicographic fallback
    (deterministic everywhere, but not what a PHP deploy produced on a
    hash-ordered filesystem). --out writes
    <DIR>/<Vendor>/<name>/js/bundle/bundle<N>.js instead of pub/static;
    existing bundle dirs are cleared first (the deploy's clear()). --json
    prints per-theme bundle stats (pool, entries, bytes) instead of the
    human summary.
    Not modeled (documented limits): minified-mode bundling (dev/js/minify
    .min naming), the compact-strategy result_map.json input branch, and
    non-frontend areas.

static minify (--css <FILE> | --js <FILE>) [--out <FILE>] [--stdout]
    Minify ONE CSS or JS file — the .min.* building block of the future
    `static deploy`, and the ONE pipeline piece that deliberately does NOT
    chase byte-parity with Magento (which uses tubalmartin/cssmin v4.1.1 and
    JShrink): a .min artifact's only contract is semantic equivalence, gated
    semantically (see static_deploy/minify.rs — the gates and the pinned
    tool versions live there).
    - CSS: lightningcss, serialization-only — StyleSheet::minify (the
      rule-merging/downleveling optimizer) is never called, targets stay
      empty (no prefix add/strip), error_recovery is ON with every recovery
      surfaced as a warning on stderr, output iterated to the printer's own
      fixpoint (re-minifying a .min.css is a no-op). Leading /*! license
      comments are kept.
    - JS: the oxc suite (parse → compress+mangle → codegen) pinned to an ES5
      output floor (no `??`/`?.`/template-literal injection into the AMD
      corpus), require/exports/module reserved from mangling (RequireJS
      CJS-sugar scans factory.toString() for literal require("…") calls),
      /*! and @license/@preserve comments kept, `debugger;` kept, and a
      span-precise output fixup that (a) rewrites codegen's backtick string
      quoting back to plain quotes and (b) re-expands oxc's
      `"a.b".split(".")` array substitution inside define(/require( calls so
      dependency arrays stay literally scannable.
    Default output: the input's .min.* sibling (Minification::addMinifiedSign
    naming — already-.min names are refused); --out writes an exact path;
    --stdout prints only the minified content. Parse errors exit non-zero
    with the file name on stderr.

static cssdiff <expected.css> <actual.css> [--limit <N>]
    Semantic CSS diff (order-preserving; normalizes only non-semantic formatting:
    whitespace, hex case/shorthand, leading zeros, comments). Exit 0 when
    semantically identical — every remaining finding is a real residual.
```

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
