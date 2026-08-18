---
name: magecommand
description: >-
  Build a Magento 2 codebase with the fast `magecommand` CLI in place of
  `bin/magento setup:di:compile` and `setup:static-content:deploy`. Use whenever
  you need to generate or refresh `generated/code` and `generated/metadata`,
  deploy or rebuild `pub/static`, compile a theme's LESS, emit requirejs
  artifacts or `js/bundle/bundle<N>.js`, minify a css/js file, keep `generated/`
  warm during an edit loop, produce a CI cache key for `generated/`, or verify
  generated/deployed output against a real Magento run. It needs no PHP and no
  node. magecommand WRITES build artifacts to the checkout: use it only to
  generate them, never to inspect, explain, or debug a codebase, and never for any
  other `bin/magento` command.
---

# Building a Magento 2 codebase with magecommand

`magecommand` reimplements Magento's two build steps in Rust. It parses the source
tree and writes the same artifacts `bin/magento` would, without executing PHP and
without node. Two command groups: `di` (compilation) and `static` (content deploy).

## The rule

`magecommand` only generates artifacts. If the task is to inspect, explain, or
debug a codebase — or to report what is already on disk — this is the wrong tool.
Reach for it only when files must actually be written.

Only these two build steps are covered. Every other `bin/magento` command
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
  when no input changed (stat fingerprint); `--fused` emits fused interceptors
  with inlined plugin chains instead of stock runtime-dispatch ones
  (global-only classes for now).
- Stay compiled while editing → `di watch`. Builds once, keeps the parsed index in
  memory, recompiles on change and writes only the delta. Long-running.
- Cache `generated/` in CI → `di digest`. The compile is a pure function of the
  source tree, so an unchanged digest means the existing output is still valid.
  Use the default content digest as the cache key; `--stat` (mtime+size) is the
  local `--incremental` short-circuit and is NOT portable across a fresh checkout.
- Check output against a known-good tree → `di verify --archive <DIR> --output
  <DIR>`. `--fail-on-diff` exits non-zero unless identical. Interceptors differing
  only in method order are reported as `reordered`, not `changed`, unless
  `--strict-ordering`.

## Static content — `magecommand static`

Writes the `pub/static` packages, or `--out <DIR>`.

- Full deploy → `static deploy <LOCALE>...`. At least one locale is required.
  `--theme Vendor/Name[:loc,loc]` (repeatable) selects themes and may override
  their locales; omitting every `--theme` deploys all registered themes. A child
  theme also deploys its parents unless `--no-parent`. `--area frontend,adminhtml`
  restricts areas. Magento's skip flags exist: `--no-less`, `--no-js-bundle`,
  `--no-compress` (developer-mode uncompressed css). `--jobs N` caps the parallel
  fan-out; `--symlink file` symlinks byte-identical assets to their source instead
  of copying.
- One theme's package → `static files --theme Vendor/Name [--area] [--locale]`.
- One artifact only → `static less`, `static requirejs`, `static bundle`.
- Minify one file → `static minify`. Semantic equivalence via lightningcss and
  oxc, deliberately not byte-identical to Magento's cssmin/JShrink.
- Compare against a real deploy → deploy with `--out <DIR>`, then `static verify
  --reference <real pub/static> --output <DIR>` (`--fail-on-diff` for CI; by
  default only the packages the output contains are checked, `--all` requires full
  coverage). `static cssdiff <expected> <actual>` diffs one css file, ignoring
  non-semantic formatting.

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
- Only `setup:di:compile` and `setup:static-content:deploy` are reproduced.
- Static deploy implements Magento's **quick** strategy.
- `--fused` interceptors are limited to global-only classes; multi-area classes
  still get stock interceptors.
- `--no-html-minify` is accepted for parity but is a no-op: `.html` is byte-copied
  and never minified.
