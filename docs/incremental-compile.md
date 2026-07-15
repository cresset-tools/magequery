# magecommand incremental compile (CAS) — design scope

Status: **Win 1 shipped** (`compile --incremental`); Wins 2–3 + CI recipe scoped
below. Target: cut the re-compile cost, especially on APFS where the compile is
filesystem-bound (see the perf notes below), and enable CI to treat
`setup:di:compile` as a **cache restore**.

**Win 1 (output manifest) — done.** `compile` writes
`generated/.mqcache/manifest.json` (blake3 of every output file, guarded by
format version + tool version + `BP`). `compile --incremental` reconciles
`generated/code` in place: it re-runs the compute, hashes the fresh in-memory
output, and writes only the files whose hash changed, deletes the ones that
disappeared, and skips the rest — no clear, no full rewrite. Falls back to a full
compile when no valid manifest exists (or under `--force`). Verified on the
oracle: a no-op re-compile writes 0/4106 files and stays byte-exact; changed and
deleted paths reconcile byte-exact; a missing manifest falls back to full. The
write+clear phases (the ~3.4s APFS cost) collapse to the changed subset. Still
paid every run: scan + compute (that's what Win 2's short-circuit removes).

## The core observation

`magecommand compile` is a **pure, deterministic function of the source tree**:

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
digest, and a cache built at `/Users/jelle/www/proforto` will not byte-match one
built at `/home/runner/work/...`. Within one project's CI (stable workspace path)
this is a non-issue; across machines it just means the cache is path-scoped.

## Where the time goes (measured, proforto = 761 modules / 10 643 output files)

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

### Win 1 — output manifest: skip unchanged **writes** (highest value / lowest risk)

`generate_code` already produces the full output in memory as `Vec<(path,
content)>`. Today we `clear` then write all 10 643. Instead:

- Keep a manifest `generated/.mqcache/manifest` = `{ tool_version, bp,
  inputs_digest, files: { rel_path -> blake3(content) } }`.
- On recompile, after generating content in memory, hash each file and **diff
  against the manifest**:
  - hash unchanged → **skip the write**,
  - hash changed or new path → write,
  - path in manifest but not in new set → delete (the stale-extra case),
  - then rewrite the manifest.
- No need to read the existing output files — the manifest *is* the record of
  on-disk state. Reconcile replaces the `clear + write-all`.

**Effect:** a no-op recompile writes **0 files** (APFS write phase ~1.8 s → ~0)
and deletes 0; a one-module change writes a handful. The `clear` phase also
disappears — we reconcile in place instead of wiping first.

**Byte-exactness:** trivially preserved — we only skip writing bytes that are
already identical. Verify on the oracle: `compile; compile; compare` → identical,
0 writes reported.

**Self-heal / escape hatch:** if the manifest is missing, malformed, its
`tool_version`/`bp` differ, or `--force`/`--no-cache` is passed → full write
(and, to be safe against hand-edited output, a `--verify` mode that re-hashes
disk instead of trusting the manifest).

### Win 2 — input digest: skip the **whole compile** on a no-op

- Add a fast `magecommand digest` subcommand: enumerate + fingerprint the inputs
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

## CI cache integration (the investigated question: **yes, and it fits unusually well**)

Because the output is a pure function of the source, CI can cache it keyed on the
source — the same pattern CI already uses for `vendor/` keyed on `composer.lock`.
Two levels:

### Level A — cache the output, keyed on the input digest (ship first)

```yaml
# GitHub Actions
- id: di
  run: echo "key=$(magecommand digest)" >> "$GITHUB_OUTPUT"
- uses: actions/cache@v4
  with:
    path: |
      generated/code
      generated/metadata
      generated/.mqcache
    key: magento-di-${{ runner.os }}-${{ steps.di.outputs.key }}
- run: magecommand compile --force        # Win 2 short-circuits on an exact hit
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
`.mqcache` (manifest + parse cache), so `compile` runs **incrementally** — Win 1
writes only the delta, Win 3 reuses unchanged parses — instead of cold. Needs
Wins 1+3.

**Feasibility verdict:** viable and a strong fit. The cache key is purely the
source tree (which CI already hashes), the manifest is small and portable (paths
+ 32-byte hashes), and `generated/` is smaller than the `vendor/` trees CI caches
routinely. Caveats to honor: key must include `runner.os` + tool version + `BP`
scope; `generated/` is 10 k files so archive it (CI cache tars automatically);
and keep `magecommand compare` in the pipeline as the byte-exact backstop so a
cache bug can never ship a wrong compile silently.

## Manifest & store layout

```
generated/.mqcache/
  manifest.json         # { version, tool_version, bp, inputs_digest,
                        #   files: { "Magento/Foo/Interceptor.php": "<blake3>", ... } }
  parse/                # (Win 3) content-hash -> serialized ClassMeta
```

- Use **blake3** (task #21) — fast + collision-resistant; better than the present
  non-crypto `twox-hash` for a cache that gates correctness. Small new dep.
- `version` + `tool_version` invalidate the cache when the manifest format or the
  compiler logic changes (an output-format change must not be served from cache).

## Correctness guarantees (non-negotiable)

1. Incremental output is **byte-identical** to a full compile — Win 1 only skips
   writing already-identical bytes; the in-memory `generate_code` output stays the
   single source of truth.
2. The input-digest short-circuit is **sound**: cover the complete input set
   (derive it from the compile's own discovery code, over-cover on doubt).
3. `--force` bypasses **all** caching (full clear + full write) — the always-safe
   fallback and the current default in CI examples until trust is established.
4. `magecommand compare` remains the CI gate: a cache defect fails the build, it
   never ships.

## Phasing

1. **Win 1 — output manifest.** Self-contained in the write path + manifest r/w.
   Kills the APFS write phase on recompile. Oracle-verifiable (0 writes on no-op,
   byte-exact). *Do this first.*
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
