#!/usr/bin/env bash
#
# Ground-truth the compile fixtures against real Magento.
#
# `cargo test -p magecommand-engine --test compile` pins what magecommand
# emits. It cannot say whether that is CORRECT. This does: it installs every
# fixture module into a throwaway copy of a real install, runs
# `bin/magento setup:di:compile`, and diffs Magento's own output against the
# fixture goldens — metadata entries filtered to each fixture's namespace, and
# every generated class byte for byte.
#
# Not a test: it needs a Magento install, PHP and bougie, so it never runs in
# CI. Run it when adding or changing a fixture. A fixture that has never been
# through this pins behaviour nobody has checked.
#
#   ./verify-fixtures-against-magento.sh /path/to/magento-install
#
# The install is COPIED first and never written to. The copy costs ~1 GB.
#
# Four fixture defects this found the first time it ran, none of which any
# amount of staring at the goldens would have surfaced:
#   - no registration.php, so Magento never loaded the modules at all and
#     silently compiled nothing for them;
#   - di.xml using xsi:type without declaring the xsi namespace, which
#     DOMDocument rejects (quick-xml matches the attribute name literally and
#     does not care);
#   - an <argument> with no xsi:type, which aborts the real compile outright
#     and appears zero times in a 685 MB install;
#   - a reference to a generated kind with no emitter, likewise fatal.
set -euo pipefail

SRC=${1:?usage: verify-fixtures-against-magento.sh <magento-root>}
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
FIX="$HERE/../tests/compile-fixtures"
WORK=${MAGECOMMAND_ORACLE_DIR:-/tmp/magecommand-oracle}
SANDBOX="$WORK/install"

# Fixtures this script cannot ground-truth. Each must say why in its own
# source, so a skip is never a quiet exemption.
#
#   never-return-is-magentos-own-bug  Magento's generator emits invalid PHP for
#     a plugged `never` method, which aborts setup:di:compile outright.
#   extension-attributes  its golden is computed with NO framework present, so
#     `OrderExtension extends \Magento\Framework\Api\AbstractSimpleObject`
#     resolves to nothing and the constructor surface is empty — against a real
#     install the parent IS there and `data` appears. Both outputs are correct
#     for their own input; comparing them is the mistake. Leaving it in made
#     every run report a divergence, which is how a verifier stops being read.
SKIP_INSTALL=("never-return-is-magentos-own-bug")

skip() { local n=$1; for s in "${SKIP_INSTALL[@]}"; do [ "$n" = "$s" ] && return 0; done; return 1; }

mkdir -p "$WORK"
if [ ! -d "$SANDBOX" ]; then
    echo "copying $SRC -> $SANDBOX (once; ~1 GB)"
    cp -a "$SRC" "$SANDBOX"
fi

# ---- install every fixture module -----------------------------------------
rm -rf "${SANDBOX:?}/app/code/Acme"
python3 - "$SANDBOX" <<'PY'
import pathlib, re, sys
p = pathlib.Path(sys.argv[1]) / 'app/etc/config.php'
p.write_text(re.sub(r"\n\s*'Acme_[A-Za-z]+' => [01],", "", p.read_text()))
PY

mods=()
for case in "$FIX"/*/; do
    name=$(basename "$case")
    [ -d "$case/in/app/code" ] || continue
    skip "$name" && continue
    cp -r "$case/in/app/code/"* "$SANDBOX/app/code/"
    while IFS= read -r m; do mods+=("$m"); done < <(
        find "$case/in/app/code" -name module.xml -printf '%h\n' |
            sed 's|.*/app/code/||; s|/etc$||; s|/|_|'
    )
done
printf '%s\n' "${mods[@]}" | sort -u > "$WORK/modules.txt"
python3 - "$SANDBOX" "$WORK/modules.txt" <<'PY'
import pathlib, re, sys
mods = [m.strip() for m in open(sys.argv[2]) if m.strip()]
p = pathlib.Path(sys.argv[1]) / 'app/etc/config.php'
ins = "".join(f"\n        '{m}' => 1," for m in mods)
p.write_text(re.sub(r"('modules'\s*=>\s*\[)", lambda m: m.group(1) + ins, p.read_text(), count=1))
print(f"enabled {len(mods)} fixture modules")
PY

# ---- Magento's own compile -------------------------------------------------
echo "running setup:di:compile …"
( cd "$SANDBOX" && rm -rf generated/code generated/metadata &&
  bougie run php bin/magento setup:di:compile ) | tail -1

# ---- helpers ---------------------------------------------------------------
cat > "$WORK/subset.php" <<'PHP'
<?php
// Entries of a compiled metadata file whose key starts with $prefix, in a
// stable var_export form so two compilers diff exactly.
$data = require $argv[1];
$out = [];
// Generated Extension classes extend \Magento\Framework\Api\AbstractSimpleObject.
// A fixture golden is computed with NO framework present, so that parent
// resolves to nothing and the constructor surface comes out empty; against a
// real install the parent IS there and a `data` argument appears. Both answers
// are right for their own input, so comparing them is the mistake — it is not
// a divergence, and reporting it every run is how a verifier stops being read.
//
// Excluded by ENTRY rather than by fixture, so the Proxy and Factory coverage
// in the same fixtures still counts. The proper fix is to commit a minimal
// AbstractSimpleObject stub into those fixtures' `in/` trees (the technique
// PR #120 used for PluginListGenerator), which would make them comparable and
// let this exclusion go.
$unverifiable = static fn (string $k): bool =>
    str_contains($k, 'Extension') || str_contains($k, 'ExtensionInterface');

foreach (['arguments', 'preferences', 'instanceTypes', 'nonLazyTypes'] as $sec) {
    if (!isset($data[$sec]) || !is_array($data[$sec])) { continue; }
    $sub = [];
    foreach ($data[$sec] as $k => $v) {
        // Match on the NAMESPACE BOUNDARY, not a bare prefix: `Acme\Pref`
        // also prefixes `Acme\Prefix`, so a bare match pulled another
        // fixture's entries into this one's comparison and reported a
        // divergence that was really two fixtures sharing five characters.
        $k = (string) $k;
        if (($k === $argv[2] || str_starts_with($k, $argv[2] . '\\')) && !$unverifiable($k)) {
            $sub[$k] = $v;
        }
    }
    ksort($sub);
    $out[$sec] = $sub;
}
var_export($out);
echo "\n";
PHP

extract() { # golden, "code/Foo.php" -> the file body, without the separator
    awk -v want="===== file: $2 =====" '
        $0 == want { on=1; next } on && /^===== / { exit } on { print }' "$1" |
    awk '{ l[NR]=$0 } END { n=NR; if (l[n]=="") n--; for (i=1;i<=n;i++) print l[i] }'
}

# ---- compare ---------------------------------------------------------------
meta_ok=0; meta_bad=0; code_ok=0; code_bad=0
for case in "$FIX"/*/; do
    name=$(basename "$case")
    [ -f "$case/expected.txt" ] || continue
    skip "$name" && { echo "skipped (documented): $name"; continue; }

    extract "$case/expected.txt" "metadata/global.php" > "$WORK/g.php"
    bad=0
    while IFS= read -r pre; do
        [ -n "$pre" ] || continue
        bougie run php "$WORK/subset.php" "$SANDBOX/generated/metadata/global.php" "$pre" 2>/dev/null | grep -v '^Resolved' > "$WORK/o.txt"
        bougie run php "$WORK/subset.php" "$WORK/g.php" "$pre" 2>/dev/null | grep -v '^Resolved' > "$WORK/m.txt"
        if ! diff -q "$WORK/o.txt" "$WORK/m.txt" >/dev/null; then
            bad=1; echo "=== METADATA DIFFERS: $name [$pre] ==="; diff "$WORK/o.txt" "$WORK/m.txt" | head -25
        fi
    done < <(find "$case/in/app/code" -name module.xml -printf '%h\n' 2>/dev/null |
             sed 's|.*/app/code/||; s|/etc$||; s|/|\\|' | sort -u)
    [ $bad -eq 0 ] && meta_ok=$((meta_ok+1)) || meta_bad=$((meta_bad+1))

    while IFS= read -r f; do
        [ -n "$f" ] || continue
        if [ ! -f "$SANDBOX/generated/code/$f" ]; then
            echo "=== ONLY MAGECOMMAND EMITS: $f ($name) ==="; code_bad=$((code_bad+1)); continue
        fi
        extract "$case/expected.txt" "code/$f" > "$WORK/c.php"
        if diff -q "$SANDBOX/generated/code/$f" "$WORK/c.php" >/dev/null; then
            code_ok=$((code_ok+1))
        else
            code_bad=$((code_bad+1)); echo "=== CODE DIFFERS: $f ($name) ==="
            diff "$SANDBOX/generated/code/$f" "$WORK/c.php" | head -20
        fi
    done < <(grep -o '^===== file: code/[^ ]*' "$case/expected.txt" | sed 's|^===== file: code/||')
done

echo
echo "metadata: $meta_ok verified, $meta_bad divergent"
echo "code:     $code_ok verified, $code_bad divergent"

# ---- phase 2: the other plugin-list baseline -------------------------------
#
# Some behaviour depends on which FRAMEWORK the store ships, not on its config,
# and a run against a single unmodified install can only ever see one side of
# it. The live case is the global-plugin baseline for plugin lists: Magento
# snapshots it on the `global` pass, which never runs because `primary` already
# loaded `global`, so every area after frontend starts from an empty base.
# Mage-OS also snapshots on `primary`, and magecommand branches on whichever
# guard the store's own generator has.
#
# A verifier that never alters the install cannot reach the other branch — and
# that gap is exactly how the branch came to be wrong for five areas
# (adminhtml, crontab, webapi_rest, webapi_soap, graphql; crontab's list was
# 3 KB against a native 334 KB). So: flip the guard in the SANDBOX's vendor
# tree, recompile both sides, and diff the whole generated tree rather than
# the fixture namespaces — the fixtures have no vendor/, so they cannot
# express this at all.
#
# Set MAGECOMMAND_SKIP_GUARD_FLIP=1 to stop after phase 1.
if [ "${MAGECOMMAND_SKIP_GUARD_FLIP:-0}" = "1" ]; then
    echo
    echo "guard flip: skipped (MAGECOMMAND_SKIP_GUARD_FLIP=1)"
    [ $meta_bad -eq 0 ] && [ $code_bad -eq 0 ]
    exit $?
fi

GEN=$(ls "$SANDBOX"/vendor/*/framework/Interception/PluginListGenerator.php 2>/dev/null | head -1)
[ -n "$GEN" ] || GEN=$(ls "$SANDBOX"/lib/internal/Magento/Framework/Interception/PluginListGenerator.php 2>/dev/null | head -1)

guard_bad=0
if [ -z "${GEN:-}" ]; then
    echo
    echo "guard flip: no PluginListGenerator.php found — skipped"
else
    echo
    cp "$GEN" "$WORK/generator.orig"
    restore_generator() { cp "$WORK/generator.orig" "$GEN"; }
    trap restore_generator EXIT

    if grep -q "scope === 'primary'" "$GEN"; then
        echo "guard flip: store has the Mage-OS guard; flipping to Adobe's"
        python3 - "$GEN" <<'PY'
import pathlib, sys
p = pathlib.Path(sys.argv[1])
s = p.read_text()
p.write_text(s.replace("$scope === 'global' || $scope === 'primary'", "$scope === 'global'", 1))
PY
    else
        echo "guard flip: store has the Adobe guard; flipping to Mage-OS's"
        python3 - "$GEN" <<'PY'
import pathlib, sys
p = pathlib.Path(sys.argv[1])
s = p.read_text()
p.write_text(s.replace("if ($scope === 'global') {",
                       "if ($scope === 'global' || $scope === 'primary') {", 1))
PY
    fi

    ( cd "$SANDBOX" && rm -rf generated/code generated/metadata generated/_guard_code generated/_guard_meta )
    if ! ( cd "$SANDBOX" && bougie run php bin/magento setup:di:compile ) >"$WORK/guard-magento.log" 2>&1; then
        echo "  flipped compile FAILED:"; grep -iE 'fatal|error' "$WORK/guard-magento.log" | head -3
        guard_bad=1
    else
        mv "$SANDBOX/generated/code" "$SANDBOX/generated/_guard_code"
        mv "$SANDBOX/generated/metadata" "$SANDBOX/generated/_guard_meta"
        MC_BIN=${MAGECOMMAND_BIN:-magecommand}
        "$MC_BIN" di compile --root "$SANDBOX" --force >"$WORK/guard-mc.log" 2>&1 || true
        gm=0
        for f in $(cd "$SANDBOX/generated/_guard_meta" && ls); do
            if ! diff -q "$SANDBOX/generated/_guard_meta/$f" "$SANDBOX/generated/metadata/$f" >/dev/null 2>&1; then
                gm=$((gm+1)); echo "  METADATA DIVERGES: $f"
                diff "$SANDBOX/generated/_guard_meta/$f" "$SANDBOX/generated/metadata/$f" | head -6
            fi
        done
        gc=0
        while IFS= read -r f; do
            diff -q "$SANDBOX/generated/_guard_code/$f" "$SANDBOX/generated/code/$f" >/dev/null 2>&1 || {
                gc=$((gc+1)); [ $gc -le 3 ] && echo "  CODE DIVERGES: $f"; }
        done < <(cd "$SANDBOX/generated/_guard_code" && find . -name '*.php')
        echo "  flipped guard: $gm metadata divergent, $gc code divergent"
        [ $gm -eq 0 ] && [ $gc -eq 0 ] || guard_bad=1
    fi
    restore_generator
    trap - EXIT
fi

[ $meta_bad -eq 0 ] && [ $code_bad -eq 0 ] && [ $guard_bad -eq 0 ]
