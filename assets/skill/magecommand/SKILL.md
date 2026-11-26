---
name: magecommand
description: >-
  Build a Magento 2 codebase with the fast `magecommand` CLI in place of
  `bin/magento setup:di:compile` and `setup:static-content:deploy`. Use whenever
  you need to generate or refresh `generated/code` and `generated/metadata`,
  deploy or rebuild `pub/static`, compile a theme's LESS, emit requirejs
  artifacts or `js/bundle/bundle<N>.js`, minify a css/js file, keep `generated/`
  warm during an edit loop, produce a CI cache key for `generated/`, or verify
  generated/deployed output against a real Magento run. Also use it in a deploy
  to decide whether `setup:upgrade` or `app:config:import` has work to do (`db
  status`, `config status`, `theme status`, `queue status`). It needs no PHP and
  no node. Apart from those gates magecommand WRITES build artifacts to the
  checkout: use it only to generate them, never to inspect, explain, or debug a
  codebase, and never for any other `bin/magento` command.
---

# Building a Magento 2 codebase with magecommand

`magecommand` reimplements Magento's two build steps in Rust. It parses the source
tree and writes the same artifacts `bin/magento` would, without executing PHP and
without node. Two command groups: `di` (compilation) and `static` (content deploy),
plus the read-only upgrade gates a deploy runs before `setup:upgrade`.

## The rule

`magecommand` only generates artifacts. If the task is to inspect, explain, or
debug a codebase — or to report what is already on disk — this is the wrong tool.
Reach for it only when files must actually be written.

Only these two build steps and the upgrade gates below are covered. Every other
`bin/magento` command
(`setup:upgrade`, `cache:flush`, `indexer:reindex`, …) is out of scope — run those
with `bin/magento`.

## Before you start

- Check it is installed: `magecommand --version`. If missing:
  `curl -LsSf https://bougie.tools/magequery.sh | sh`.
- Run it from the Magento root, or pass `--root /path/to/magento`.
- It writes to the checkout. Preview a compile with `magecommand di compile
  --dry-run` when the effect on `generated/` is not wanted yet, and send a static
  deploy to a scratch tree with `--out <DIR>` instead of `pub/static`.

## DI compilation — `magecommand di`

Writes `generated/code` and `generated/metadata`.

- Compile once → `di compile`. `--force` overwrites existing generated files;
  `--dry-run` reports without writing; `--incremental` skips the whole compile
  when no input or setting changed (stat fingerprint); `--fused` emits fused interceptors
  with inlined plugin chains instead of stock runtime-dispatch ones
  (global-only classes for now). On Mage-OS 4.0+ it writes every area but
  global as its differences from global, as 4.0 does; `--area-config full`
  writes them whole instead. It refuses, writing nothing, what
  `setup:di:compile` refuses: a preference to a class that doesn't exist (or
  isn't compiled) and a missing plugin class, each with its di.xml line. Like
  `setup:di:compile`, it removes
  `var/.regenerate` (left by `composer install`), so the next `bin/magento`
  command does not wipe `generated/`. A class whose constructor it cannot
  resolve (an unreadable parent, or a default it cannot evaluate such as an
  unknown constant) gets no `arguments` entry, so Magento reflects it at
  runtime. That ignores di.xml, so when di.xml configures the class, stderr
  warns and names it with the reason: setting the unevaluable argument in
  di.xml, or using a constant magecommand can read, restores the compiled row.
- Stay compiled while editing → `di watch`. Builds once, keeps the parsed index in
  memory, recompiles on change and writes only the delta. Long-running.
- Cache `generated/` in CI → `di digest`. The compile is a pure function of the
  source tree, so an unchanged digest means the existing output is still valid.
  Pass it the same settings flags as `di compile` (`--fused`, `--target-php`,
  `--target-platform`, `--area-config`), which it folds into the key.
  Use the default content digest as the cache key; `--stat` (mtime+size) is the
  local `--incremental` short-circuit and is NOT portable across a fresh checkout.
- Check output against a known-good tree → `di verify --archive <DIR> --output
  <DIR>`. `--fail-on-diff` exits non-zero only on UNEXPLAINED differences, which
  the report lists first (DI config vs generated code, each changed file with its
  first diverging line); known/expected ones link to
  docs/di-verify-differences.md. A class whose `arguments` entry is NULL in the
  output but not the archive is always unexplained (Magento builds it with
  `new X()`), and leads the report. A class the output leaves OUT of
  `arguments` (what `di compile` does when it can't read a constructor, so
  Magento reflects it at runtime) is a WARNING when di.xml configures nothing
  for it, and unexplained when di.xml does (runtime reflection ignores di.xml,
  and a required non-object argument set only there then throws `Missing
  required argument`; `di compile` names those on stderr). `--strict-fallback` makes every
  such omission unexplained, configured or not. On macOS, paths differing only in
  letter case are a WARNING, not a failure; elsewhere they fail. Run verify with
  `--root` at the checkout the archive was built from: several rules (e.g. the
  pre-2.4.8 setup/src area config) read it as evidence. `--show-residual <FILE>` gives more context for
  one metadata file. Interceptors differing only in method order are reported as
  `reordered`, not `changed`, unless `--strict-ordering`.

## Static content — `magecommand static`

Writes the `pub/static` packages, or `--out <DIR>`.

- Full deploy → `static deploy <LOCALE>...`. At least one locale is required.
  `--theme Vendor/Name[:loc,loc]` (repeatable) selects themes and may override
  their locales; omitting every `--theme` deploys all registered themes. A child
  theme also deploys its parents unless `--no-parent`. `--area frontend,adminhtml`
  restricts areas. Magento's skip flags exist: `--no-less`, `--no-js-bundle`,
  `--no-compress` (developer-mode uncompressed css). `--jobs N` caps the parallel
  fan-out; `--symlink file` symlinks byte-identical assets to their source instead
  of copying; `--symlink dir` (experimental) writes each repeated directory once
  and links the identical ones to it inside `pub/static` (the web server must
  follow symlinks).
- One theme's package → `static files --theme Vendor/Name [--area] [--locale]`.
- One artifact only → `static less`, `static requirejs`, `static bundle`.
- Minify one file → `static minify`. Semantic equivalence via lightningcss and
  oxc, deliberately not byte-identical to Magento's cssmin/JShrink.
- Compare against a real deploy → deploy with `--out <DIR>`, then `static verify
  --reference <real pub/static> --output <DIR>` (`--fail-on-diff` for CI; by
  default only the packages the output contains are checked, `--all` requires full
  coverage). `static cssdiff <expected> <actual>` diffs one css file, ignoring
  non-semantic formatting.

## Upgrade gates — `db`, `config`, `theme`, `queue status`

Read-only checks a deploy runs to decide whether the slow steps have work, with
`bin/magento`'s exit codes: 0 = nothing to do, 2 = work to do, 1 = error.

- `magecommand db status` → `setup:db:status` (schema, patches, module versions;
  says whether the upgrade needs maintenance mode).
- `magecommand config status` → `app:config:status`. Exit 1 also means "can't hash
  this config exactly" — fall back to `bin/magento app:config:status`.
- `magecommand theme status` / `queue status` → Mage-OS's
  `theme:registration:status` / `queue:config:status` (database queues; prints the
  AMQP topology fingerprint for RabbitMQ).

## Development `setup:upgrade` — `magecommand dev upgrade`

Use it instead of `bin/magento setup:upgrade` in development. When nothing needs
upgrading it finishes in well under a second without PHP: it writes
`app/etc/config.php` (new or removed modules), cleans the caches, empties
`generated/` and `pub/static` (`--keep-generated` to skip), and does the core
recurring setup work natively. When a schema change, patch, config import, theme,
non-core `Setup/Recurring.php` or anything else needs PHP, it says why and runs
`php bin/magento setup:upgrade` itself (`--php "<cmd>"` for another PHP command,
`--full` to always run it). It refuses production mode.

## Conventions

- Every command accepts `--root <path>` and `--json` (machine-readable, stable).
- `--order probe|sorted` on the bundling commands: `probe` (default) reproduces
  the output filesystem's readdir order and is byte-faithful. `sorted` is a
  determinism aid only — it changes which files are bundled, so it matches no real
  deploy; use it only to compare two magecommand runs.
- `--deployed-version <V>` writes `deployed_version.txt`. Omitted, no file is
  written, because the real value is a per-run timestamp.
- Discover the surface with `magecommand --help` and any command's flags with
  `magecommand <group> <command> --help`.

## Scope and limits

- Target is Magento 2.4 Open Source.
- Only `setup:di:compile`, `setup:static-content:deploy`, the status gates and a
  development `setup:upgrade` with nothing to do are reproduced.
- Static deploy implements Magento's **quick** strategy.
- `--fused` interceptors are limited to global-only classes; multi-area classes
  still get stock interceptors.
- `--no-html-minify` is accepted for parity but is a no-op: `.html` is byte-copied
  and never minified.
