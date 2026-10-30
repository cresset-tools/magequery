# magecommand-php corpus — decision notes

Decisions behind the corpus gate and any xfail.txt entries. The manifest
(`manifest.tsv`) is the regression net; this file explains the deliberate
exceptions and format choices.

## php84 property hooks

`coverage/php84-hooks.php` pins the parser's behavior on PHP 8.4 property
hooks and asymmetric visibility:

- **Plain property hooks** (`public string $x { get => …; set { … } }`) and
  **asymmetric visibility** (`public private(set) string $x`) parse clean:
  they are consumed by the property path (`parse_property`, which skips hook
  bodies like any body and flags the property `hooked`). Zero issues.
- **Hooks on promoted constructor parameters**
  (`public function __construct(public string $x { get => …; })`) CANNOT be
  consumed under the current parameter grammar (a parameter may carry a
  hook body where the grammar expects `,` or `)`) → one hard ParseIssue per
  the no-guess contract: never a silent guess. This is the founding
  `xfail.txt` entry.

When (if) a downstream consumer ever needs the hook surface, the fix is in
the parser (consume a trailing `{ … }` after a promoted parameter's
default), not a fallback tier. Until then the manifest pins the issue
exactly: removing it from the parser's output fails the gate as a
fingerprint drift.

## Fingerprint (v3)

`fingerprint(meta)` renders one line per fact: declaration headers (kind,
fqcn, flags, extends/implements/traits/attributes, enum backing/cases),
trait adaptations (`insteadof` winners and excluded traits; `as` aliases
with their visibility), then per-declaration constants (name, visibility,
type, raw value), properties (name, visibility, static/readonly/hooked
flags, type, raw default) and method signatures (visibility,
static/abstract/final/returns-ref flags, return type, params with
promotion/readonly/by-ref/variadic, types and raw default expressions).
Hashed with sha256 into `manifest.tsv`'s `fp_sha256` column; `file_sha256`
separately pins the corpus file bytes so accidental edits and parser drift
are distinguishable failures.

Deliberately excluded: byte offsets (churn on any edit), the `uses` import
map (already reflected in the resolved names it produces), and anything
derived from `ClassMeta`'s `Debug` (churns with every model edit — the
reason `{:?}` is banned here).

`FINGERPRINT_VERSION` in `tests/corpus.rs` must be bumped when the
rendering changes, and this is **enforced**, not documented: the version is
written into `manifest.tsv` as a `# fingerprint-version:` data line and
asserted on read, so a manifest blessed under one rendering can never be
checked against another. Bless mode reads any version deliberately — it has
to, to show the diff — and says so before the rows.

### v1 → v2

- **Added** trait adaptations. `coverage/trait-adaptations.php` is the
  corpus's most elaborate file and, under v1, none of what it demonstrates
  reached the digest: a parser that dropped all six adaptations produced a
  byte-identical fingerprint. (`src/lib.rs`'s `traits_with_adaptations`
  unit test did cover it, so the crate was not blind — but the corpus
  file was inert.)
- **Removed** a seventh param column, `readonly && promoted.is_some()`,
  fully implied by the `r` flag in column 3 and the promotion visibility in
  column 2. It could never disagree with them, and was baked into every
  digest.

### v2 → v3

- **Added** declared properties (`F` lines). The parser started modeling
  them so `di compile` can export the object a `new X()` parameter default
  builds: `var_export` lists that object's initialized properties, so
  typed-vs-untyped and default-or-not decide what appears. Only the coverage
  files that declare properties changed digest; every other row re-blessed
  byte-identical.

## Contents

`coverage/` is the whole corpus: hand-written PHP, one file per construct
family, plus three `stress-*.php` files that carry shapes rather than
constructs. Each file's header comment states what it pins and why.

## Why the corpus is synthetic

A second tier of sampled real code was built and then removed. It held 655
files and 79,422 lines to pin **504 declarations** — 151 of its files
declared nothing at all — and a construct-by-construct comparison against
`coverage/` found **nothing in it that `coverage/` lacks**: no enums, no
DNF or intersection types, no trait-adaptation blocks, no grouped consts,
no `static` returns, no property hooks. Magento's test trees are PHP 7-era
code; the hand-written tier is strictly richer on the modern surface. It
also produced zero xfail entries — the one pinned exception has always
been in a hand-written file.

What it did carry was *bulk*: files up to 46 KB, brace nesting to depth 11,
heredocs and attributes in quantity. That is a body-skipper stress, not a
declaration-parser one, and it is what `stress-bodies.php`,
`stress-docblocks.php` and `stress-attributes.php` now cover deliberately —
each with a declaration placed *after* the noise, so a skipper that loses
its place shows up as a changed declaration count.

Two further reasons not to vendor real code here:

- **Provenance is a standing cost.** The tier needed a sampler script, a
  pool digest, a 655-row `SOURCES.txt` hash ledger and a `VENDOR.txt`
  count check to stay honest — machinery whose only job was to keep the
  copy trustworthy.
- **A copy of someone's tree collects things you did not intend.** That
  tier arrived carrying 33 machine-local files, four of them absolute
  symlinks into a developer's `$HOME` — the exact shape the monorepo
  importer rejects outright.

Breadth against real code has not gone away; it lives where it belongs, in
the two harnesses that run against a real install rather than in the repo:
`parse_entire_corpus` (the 685 MB smoke, `MAGECOMMAND_CORPUS`) and
`examples/differential.rs` (43,731 classes diffed against PHP reflection,
0 mismatches). When either turns up a construct this corpus cannot express,
the fix is a new synthetic file here, written on purpose.

Note that the differential harness cannot be pointed at this corpus: these
files are written to be *parsed*, not loaded. They reference parents and
traits that do not exist — `class-exists-guard.php` extends a missing
dependency on purpose — and PHP fatals rather than reflecting a class whose
parent is undefined.
