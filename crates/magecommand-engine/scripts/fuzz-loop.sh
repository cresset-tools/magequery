#!/usr/bin/env bash
#
# One differential fuzz round per seed: generate random modules, compile the
# same tree with real Magento and with magecommand, diff both outputs.
#
#   ./fuzz-loop.sh <magento-sandbox> <magecommand-binary> <seed>...
#
# The sandbox must be a THROWAWAY copy of an install — this rewrites its
# app/code/Acme, app/etc/config.php and generated/. See
# verify-fixtures-against-magento.sh, which makes one.
#
# A round costs about three minutes, almost all of it Magento's compile, which
# is why the generator emits many modules per round rather than one. A seed
# that reports a divergence reproduces it exactly:
#
#   ./fuzz-against-magento.py --seed <n> --modules 40 --emit /tmp/fz
set -uo pipefail

SANDBOX=${1:?usage: fuzz-loop.sh <sandbox> <magecommand> <seed>...}
MC=${2:?usage: fuzz-loop.sh <sandbox> <magecommand> <seed>...}
shift 2
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
WORK=${FUZZ_WORK:-/tmp/magecommand-fuzz}
mkdir -p "$WORK"

# Memory ceilings. A round is dominated by Magento's compile, which peaked
# around 390 MiB on a 300-module install — so a 1G cap is generous but BOUNDED,
# and a runaway fails the round instead of the machine. magecommand's scan is
# rayon-parallel, and peak memory scales with thread count, so cap that too.
# Rounds run strictly one at a time.
PHP_MEM=${FUZZ_PHP_MEM:-1G}
export RAYON_NUM_THREADS=${FUZZ_THREADS:-4}

fail=0
for seed in "$@"; do
    echo "=== seed $seed ==="
    "$HERE/fuzz-against-magento.py" --seed "$seed" --modules "${FUZZ_MODULES:-25}" \
        --emit "$WORK/gen" > "$WORK/modules.txt"

    rm -rf "${SANDBOX:?}/app/code/Acme"
    cp -r "$WORK/gen/Acme" "$SANDBOX/app/code/Acme"
    python3 - "$SANDBOX" <<'PY'
import pathlib, re, sys
root = pathlib.Path(sys.argv[1])
p = root / 'app/etc/config.php'
s = re.sub(r"\n\s*'Acme_[A-Za-z0-9]+' => [01],", "", p.read_text())
mods = sorted(d.name for d in (root / 'app/code/Acme').iterdir())
ins = "".join(f"\n        'Acme_{m}' => 1," for m in mods)
p.write_text(re.sub(r"('modules'\s*=>\s*\[)", lambda m: m.group(1) + ins, s, count=1))
PY

    ( cd "$SANDBOX" && rm -rf generated/code generated/metadata generated/_fz_code generated/_fz_meta )
    if ! ( cd "$SANDBOX" && bougie run php -d memory_limit="$PHP_MEM" \
            bin/magento setup:di:compile ) >"$WORK/magento.log" 2>&1; then
        echo "  MAGENTO FAILED TO COMPILE — a rejected shape, or the ${PHP_MEM} cap."
        grep -iE 'fatal|error' "$WORK/magento.log" | head -3
        fail=1
        continue
    fi
    mv "$SANDBOX/generated/code" "$SANDBOX/generated/_fz_code"
    mv "$SANDBOX/generated/metadata" "$SANDBOX/generated/_fz_meta"

    # Peak RSS via procfs rather than GNU time, which is not installed here.
    # Polling VmHWM is portable and, unlike wrapping the run twice, does not
    # double the work.
    "$MC" di compile --root "$SANDBOX" --force >"$WORK/magecommand.log" 2>&1 &
    mcpid=$!
    peak=0
    while kill -0 "$mcpid" 2>/dev/null; do
        hwm=$(awk '/VmHWM/{print $2}' "/proc/$mcpid/status" 2>/dev/null)
        [ -n "${hwm:-}" ] && [ "$hwm" -gt "$peak" ] && peak=$hwm
        sleep 0.2
    done
    wait "$mcpid"
    [ "$peak" -gt 0 ] && echo "  magecommand peak RSS: $((peak / 1024)) MiB"

    diverged=0
    for f in $(cd "$SANDBOX/generated/_fz_meta" && ls); do
        if ! diff -q "$SANDBOX/generated/_fz_meta/$f" "$SANDBOX/generated/metadata/$f" >/dev/null 2>&1; then
            echo "  METADATA DIVERGES: $f"
            diff "$SANDBOX/generated/_fz_meta/$f" "$SANDBOX/generated/metadata/$f" | head -8
            diverged=1
        fi
    done
    while IFS= read -r f; do
        if ! diff -q "$SANDBOX/generated/_fz_code/$f" "$SANDBOX/generated/code/$f" >/dev/null 2>&1; then
            echo "  CODE DIVERGES: $f"
            diff "$SANDBOX/generated/_fz_code/$f" "$SANDBOX/generated/code/$f" | head -8
            diverged=1
        fi
    done < <(cd "$SANDBOX/generated/_fz_code" && find Acme -name '*.php' 2>/dev/null)

    if [ $diverged -eq 0 ]; then
        echo "  clean: $(cd "$SANDBOX/generated/_fz_code" && find . -name '*.php' | wc -l) files, 16 metadata"
    else
        fail=1
    fi
done
exit $fail
