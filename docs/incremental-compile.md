# magecommand incremental compile (CAS) — design scope

Status: **`watch` server shipped (v1)** — the real fast-loop answer, see the next
section; **Win 2 shipped** (`compile --incremental` = no-op short-circuit,
`digest`); **Win 1 retired** (partial reconcile — a net loss on APFS, see below);
**CI cache demoted** (mostly moot — see the footnote). Target: cut the re-compile
cost, especially on APFS where the compile is filesystem-bound.

## `watch` — the long-running compile server (the fast-loop answer)

The insight that reframes everything below: the APFS FS-metadata lock only hurts
when you *touch many files*. Every cold-process approach still touches all ~10k on
some axis — the scan re-reads them, the write re-writes them, even
`--incremental` re-stats them to detect the change. A **warm server** touches only
what changed:

- the parsed PHP universe ([`Definitions`]) stays in memory, so an edit that
  doesn't touch PHP (a di.xml tweak — the common case) **skips the scan**;
- the OS file-watcher (`notify`) reports the exact delta, so there's **no re-stat
  walk**;
- the previous output tree stays in memory, so we **diff and write only the
  handful of files that changed** (this is Win 1's goal, finally correct — a warm
  process holds both trees, so the diff is a free map compare and the write is
  just the delta; no rename-aside, no 10k-file hash).

`magecommand di watch`: initial full build (identical to `di compile --force`), then on
each file change re-parse only what's needed, recompute, and write the delta.
**Correctness by construction:** each recompile runs the *same*
`build::compute_outputs` a cold compile runs, over a `Definitions` that is either
freshly re-scanned (a PHP file changed) or unchanged (nothing PHP changed → a
re-scan would be identical). So the on-disk result after an edit is byte-for-byte
a cold `di compile --force` of that edited state.

Verified on the oracle: disabling one plugin recompiled writing **2 files, 4120
unchanged** (vs a cold compile re-writing all 4122), and the tree was
**byte-identical** to a cold compile of the same edited state; a di-only edit
skips the PHP scan (`reopen` only). The write-delta is the whole APFS payoff —
the 2.4.8 store measurement pending. Two fixes the prototype forced: filter watcher
events to real writes (Create/Remove/Modify-data — Access events from the
recompile's own file reads would otherwise feed back into an endless loop), and
exclude `**/_files/**` fixtures the scan already ignores.

v1 scope / next steps: on a PHP change it re-scans the whole universe (v2:
incremental per-file `Definitions` update); it recomputes all 7 areas from the
in-memory parses (v3: per-area invalidation); it doesn't yet update the input
digest manifest (a later cold `--incremental` recomputes, which is safe). The
compute itself (~2s CPU on the 2.4.8 store) is the remaining floor a warm server keeps
paying until v2/v3.

---

The material below predates `watch` and is kept for the CAS/`--incremental`
design and the (now demoted) CI-cache investigation.

**Win 2 (input-digest short-circuit + `digest`) — done, and it is the whole
`--incremental` mechanism.** `compile` records a stat-fingerprint of the whole
compile INPUT set in `generated/.mqcache/manifest.json` (guarded by format
version + tool version + `BP`); `compile --incremental` recomputes it up front
and, on a match, **skips the entire compile** (scan + compute + write) — on the
oracle a no-op recompile is ~90ms (the input walk) vs ~890ms full, on the 2.4.8 store
~1.2s vs ~7.4s. On **any** change it falls through to a plain full compile (clear
+ write all) — no partial reconcile. The digest is computed **once**: the same
walk that detects the change is reused as the new manifest's digest (the inputs
don't change during a compile — it only writes `generated/`, which is not an
input), so the tree is never fingerprinted twice. The input set is
`definitions::compile_input_files` (the `.php` the scan reads + the DI `.xml` +
config/composer files, under the scan's own exclusion rules — sound by
construction: over-covering only recompiles unnecessarily, under-covering would
serve stale). `magecommand di digest` prints the same digest as a **CI cache key**
(content-hashed by default = checkout-independent; `--stat` = the fast local
variant). Verified on the oracle: full compile byte-exact (4106 code + 16
metadata); no-op short-circuits; touching an input triggers a full recompile that
stays byte-exact; live mode (archive hidden) incremental-on-change is
byte-identical to a full compile. Caveat: the short-circuit walks the module +
framework trees to stat them (~90ms oracle, ~1.2–1.7s APFS); still far below a
full compile.

**Win 1 (partial output reconcile) — built, then RETIRED as an APFS net loss.**
The idea was to write only the output files whose content changed and reuse the
rest. It ran headlong into two walls:

- **Compute isolation.** A full compile computes with `generated/code` *absent*
  (it clears first); a stale tree leaks generated factories/interceptors into
  BOTH the scan universe and the class resolver, changing what gets emitted (saw
  4104 or 2489 files vs a full compile's 4103 in live mode — the oracle masked it
  by always scanning the frozen `_code` archive). So "reuse in place" is
  incorrect: the old tree MUST be moved aside before the compute, then reconciled
  back.
- **APFS renames ≈ writes.** Reconciling back means renaming the unchanged
  majority (10k+ files) from the moved-aside tree. On APFS a rename is as
  metadata-lock-bound as a write, so the reconcile measured **~3.6–3.9s** —
  *slower* than the ~1.8s plain full write it was trying to avoid. Combined with
  a second input-digest walk at manifest-save time (~1.3–1.6s) and a multi-MB
  per-file-hash manifest, `--incremental` on a one-plugin change hit **~15–16s vs
  the ~7.4s a plain full compile takes** — a regression the user caught on
  the 2.4.8 store.

The fix was to delete the reconcile entirely: `--incremental` short-circuits on a
no-op (Win 2) and does a plain full compile on any change. The manifest shrank
from a 4106-entry hash map (~MB) to the 170-byte input digest, the double walk
became a single one, and the changed case is back to full-compile cost plus the
one detection walk. **General rule this nailed down (matches the write/delete
findings): on APFS the compile saturates one serialized FS-metadata lock, so no
FS op is cheaper than another — the only lever is doing FEWER FS ops. Skipping a
write by renaming saves nothing; only Win 2's skip-everything and Win 3's
read-fewer-files help.**

## The core observation

`magecommand di compile` is a **pure, deterministic function of the source tree**:

```
generated/{code,metadata} = compile(inputs, BP)
```

There is no DB, no runtime state, no PHP execution — the whole no-bootstrap
premise. That makes the output perfectly content-addressable: hash the inputs,
and you know the output without recomputing it. Nothing else in the Magento
tooling ecosystem has this property (`bin/magento setup:di:compile` needs a
bootstrapped app), and it's the lever the entire incremental + CI-cache design
hangs on.

**One caveat up front:** the output is *not* path-independent. `BP` (the absolute
Magento root) is baked verbatim into some generated arguments (the dev/test
path-exclusion regexes) and the area metadata. So `BP` is part of the input
digest, and a cache built at `/Users/jelle/www/store` will not byte-match one
built at `/home/runner/work/...`. Within one project's CI (stable workspace path)
this is a non-issue; across machines it just means the cache is path-scoped.

## Where the time goes (measured, the 2.4.8 store = 761 modules / 10 643 output files)

| phase | macOS/APFS | Linux oracle | nature |
|---|---|---|---|
| scan php universe | ~2.5 s | ~0.16 s | **read** ~thousands of PHP files + parse headers |
| write code files | ~1.8 s | ~0.02 s | **create** 10 643 files (APFS file-creation cost) |
| generate_code (CPU) | ~1.2 s | ~0.14 s | interceptor plan + collect + bytes |
| build+render areas (CPU) | ~0.7 s | ~0.19 s | DI merge + arg resolution |
| clear generated (delete) | ~1.6 s | ~0.04 s | unlink old 10 643 files |

Thread parallelism is exhausted; on APFS the FS-metadata lock is the wall
(`sys ≫ usr`), and no reordering of FS ops helps — only doing **fewer** FS ops
does. That is exactly what incremental buys: skip the read of unchanged inputs
and the write of unchanged outputs.

## The inputs (what the digest must cover)

Enumerated from the actual read sites, not guessed — the design must derive these
from the same code paths the compile uses, never a hardcoded list:

1. `app/etc/config.php` — module set + load order + `scopes`.
2. `vendor/composer/installed.json` + root & app/code `composer.json` — discovery,
   PSR-4/PSR-0/classmap autoload maps.
3. each enabled module's `etc/module.xml` — `<sequence>`.
4. the primary di glob `app/etc/{*di.xml,*/*di.xml}`.
5. each enabled module's `etc/di.xml` + `etc/<area>/di.xml` (all 7 areas + custom).
6. each module's `etc/extension_attributes.xml` (ExtConfig).
7. **PHP class headers** of every scanned class (`Definitions::scan` roots:
   enabled module dirs, `library_paths()`, `setup/src`, existing `generated/code`)
   and every resolution-path/plugin class.
8. `BP` (absolute root) and the magecommand **tool version**.

The output is a pure function of exactly this set. Over-cover rather than
under-cover: a missed input means a stale output (a correctness bug); an
over-covered input means an unnecessary recompile (merely slower).

## Three independent wins (different mechanisms, ship separately)

### Win 1 — output manifest: skip unchanged **writes** — RETIRED (APFS net loss)

The original plan (kept here for the record): keep a per-file blake3 manifest and,
on recompile, write only the files whose content changed, reuse the rest. It was
built and reverted — see the retirement note at the top. Two reasons it can't
work on APFS:

- **Reuse requires the old tree moved aside** (a stale `generated/code` pollutes
  the scan + resolver, so it can't stay in place during the compute), and
- **an APFS rename is as expensive as a write**, so renaming the unchanged
  majority back costs *more* than just rewriting every file (~3.9s vs ~1.8s
  measured on the 2.4.8 store).

So skipping a write buys nothing on APFS — the FS-metadata lock is the wall,
and a rename hits it just as hard as a write. The manifest now stores only the
`inputs_digest` (Win 2); there is no per-file hash map and no reconcile.

### Win 2 — input digest: skip the **whole compile** on a no-op

- Add a fast `magecommand di digest` subcommand: enumerate + fingerprint the inputs
  above, print a single `inputs_digest`, do **no** compile.
- At the top of `compile`: if `inputs_digest` matches the manifest's stored value
  (and the manifest matches disk, trusted or `--verify`), the output is already
  correct → **exit in ms**. No scan, no generate, no write.

**Fingerprint strategy — the one real tradeoff:**
- *content hash* (blake3 of each input's bytes): correct, but costs a read of
  every input ≈ the scan cost, so it only helps the write/generate phases.
- *stat fingerprint* (mtime + size + inode): cheap (no read), but mtime is
  unreliable after a fresh `git checkout` (all files get "now"), which is the
  common CI case.
- **Recommendation:** stat-fingerprint by default for the interactive dev loop
  (fast, and edits bump mtime), with a `--content-digest` mode (and it's the
  default under `digest` for CI, where the key must be checkout-independent).

### Win 3 — parse cache: shrink the **scan** when *some* inputs changed (later)

- Cache parsed `ClassMeta` per PHP file keyed by content hash (or path + stat),
  in `generated/.mqcache/parse/`. On recompile, reuse the cached parse for any
  input whose fingerprint is unchanged; only read+parse the changed subset.
- **Effect:** the 2.5 s scan shrinks toward the changed-file subset.
- **Limits:** (a) fingerprinting still stats ~10 k files (FS-bound on APFS,
  though cheaper than read); (b) the DI **merge is global** — one changed di.xml
  forces re-derivation of the whole DI index and the area files regardless of the
  parse cache, so Win 3 helps the PHP-header half of the scan, not the DI
  recompute. Medium value, more moving parts → **defer** until Win 1/2 are in and
  the scan is measured to still dominate.

## CI cache integration — DEMOTED (mostly moot; kept for the record)

Revised verdict after review: for the common Linux-CI case this is **not worth
it**, for three compounding reasons. (1) The input digest is whole-file, coarser
than the true DI dependency (headers + di.xml), so a method-*body* edit busts the
cache even though the output is identical — and active branches touch scanned PHP
on nearly every push, so exact hits are rare. (2) With Win 1 retired a miss is a
full compile, no partial reuse. (3) The clincher: on a Linux runner the compile is
already ~0.9s, and restoring + extracting a ~10k-file cache is probably *slower*
than recomputing — the whole cache premise was "the compile is slow" (APFS), and
CI is fast-FS. magecommand's speed makes caching its own output moot. What
survives: **matrix fan-out on one commit** (compile once, restore across
PHP/DB legs — the output is PHP-version-independent since magecommand never runs
PHP), macOS runners, and same-commit re-runs. The `digest` command stays useful as
a cache-*key* primitive even when you don't cache `generated/`. The recipes below
are retained for those niches.

Because the output is a pure function of the source, CI can cache it keyed on the
source — the same pattern CI already uses for `vendor/` keyed on `composer.lock`.
Two levels:

### Level A — cache the output, keyed on the input digest (ship first)

```yaml
# GitHub Actions
- id: di
  run: echo "key=$(magecommand di digest)" >> "$GITHUB_OUTPUT"
- uses: actions/cache@v4
  with:
    path: |
      generated/code
      generated/metadata
      generated/.mqcache
    key: magento-di-${{ runner.os }}-${{ steps.di.outputs.key }}
- run: magecommand di compile --force        # Win 2 short-circuits on an exact hit
```

- **Exact cache hit** → `generated/` is restored; `compile` sees a matching
  `inputs_digest` and no-ops in ms (or gate the step with
  `if: steps.cache.outputs.cache-hit != 'true'` and skip it entirely).
- **Miss** → compile runs, `generated/` + manifest are saved under the new key.

This turns CI DI-compile into a **cache restore on unchanged source**. It needs
only the `digest` command; Win 2 makes the on-hit run instant but even without it
the `if:`-gate works. `runner.os` in the key encodes the `BP`/path scoping.

### Level B — cache the CAS store with a prefix restore-key (warm incremental)

```yaml
    key:          magento-di-${{ runner.os }}-${{ steps.di.outputs.key }}
    restore-keys: magento-di-${{ runner.os }}-
```

On a near-miss (source changed) the prefix restore-key restores the **previous**
`.mqcache`, so `compile` reuses its cached parses instead of re-reading every PHP
file. With Win 1 retired the *output write* is always full on a change (the
reconcile was a net loss), so Level B's payoff is entirely Win 3's parse-cache
reuse on the scan half. Needs Win 3.

**Feasibility verdict:** viable and a strong fit. The cache key is purely the
source tree (which CI already hashes), the manifest is small and portable (paths
+ 32-byte hashes), and `generated/` is smaller than the `vendor/` trees CI caches
routinely. Caveats to honor: key must include `runner.os` + tool version + `BP`
scope; `generated/` is 10 k files so archive it (CI cache tars automatically);
and keep `magecommand di verify` in the pipeline as the byte-exact backstop so a
cache bug can never ship a wrong compile silently.

## Manifest & store layout

```
generated/.mqcache/
  manifest.json         # { version, tool_version, bp, inputs_digest }
  parse/                # (Win 3) content-hash -> serialized ClassMeta
```

- Use **blake3** (task #21) — fast + collision-resistant; better than the present
  non-crypto `twox-hash` for a cache that gates correctness. Small new dep.
- `version` + `tool_version` invalidate the cache when the manifest format or the
  compiler logic changes (an output-format change must not be served from cache).

## Correctness guarantees (non-negotiable)

1. Incremental output is **byte-identical** to a full compile — `--incremental`
   either skips the whole compile (no input changed) or runs the *identical*
   clear→compute→write a full compile runs. No partial reconcile, so there is no
   divergence surface. Verified live-mode byte-identical on the oracle.
2. The input-digest short-circuit is **sound**: cover the complete input set
   (derive it from the compile's own discovery code, over-cover on doubt).
3. `--force` bypasses the short-circuit (full clear + full write) — the
   always-safe fallback and the current default in CI examples until trust is
   established.
4. `magecommand di verify` remains the CI gate: a cache defect fails the build, it
   never ships.

## Phasing

1. **~~Win 1 — output manifest.~~ RETIRED** — a partial reconcile is a net loss on
   APFS (rename ≈ write, and the compute needs the old tree absent). See the
   retirement note up top.
2. **Win 2 — `digest` command + short-circuit.** Enables the no-op fast path and
   the CI Level A key. Small.
3. **CI Level A recipe** — docs + the `digest` command from Win 2. No new engine
   code.
4. **Win 3 — parse cache** + **CI Level B**. Bigger; defer until the scan is
   measured to still dominate after 1–2.

## Open questions

- **Input enumeration completeness** — must be generated from the same file
  discovery the compile uses (di file set + `Definitions::scan` roots), not a
  parallel hardcoded list that can drift. Needs a small `compile_inputs()` API in
  core/engine that both `compile` and `digest` share.
- **stat vs content fingerprint** default per context (dev loop vs CI checkout).
- **Manifest trust vs `--verify`** — how paranoid by default about hand-edited
  `generated/`.
- **Where the manifest lives** — `generated/.mqcache` (co-located, cached with the
  output) vs `var/` (out of the way). Co-located wins for CI (one path to cache).
