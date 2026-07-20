#!/usr/bin/env bash
# Vendor the less.js test-data fixtures at a pinned tag (plan §5.3).
#
# SCOPE: the DEFAULT-OPTION compile fixtures — the `tests-unit/` tree (every
# `<name>.less` with a sibling `<name>.css`, plus its import-helper subtrees),
# the binary `data/` assets (for data-uri/image-size, plan §C-assets) — and the
# `tests-error/` error-message corpus (every `<name>.less` with a sibling
# `<name>.txt`), minus the 19 OUT error fixtures (plan §5.2: @plugin-error x15,
# plugin-config x3, inline-JS x1 — the exclusion list below; the harness
# meta-test in tests/fixtures.rs pins the same names). The option-driven
# `tests-config/` sub-suites were vendored BY HAND in Phase 4B (selective dirs
# + the bootstrap-less-port node_modules package), joined in the Gate T0
# review pass (R4) by the two tests-config ERROR suites — no-js-errors
# (IN-SCOPE, error denominator 75) and js-type-errors (CLASSIFIED_OUT) —
# this script does NOT refresh them; re-vendor those dirs manually on a tag
# bump (see VENDOR.txt).
#
# Re-run to refresh; it is idempotent (wipes and repopulates the target dir).
# A tag bump is a Node-free operation for this script, but regenerating the
# option manifest (later phases) needs a Node toolchain (plan §G-node).
set -euo pipefail

TAG="v4.6.7"
# The commit the tag resolved to when first vendored — recorded in VENDOR.txt.
EXPECT_SHA="8ae2cc3bfa79f0718ad6fe5f263a1d6819fe9d5c"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(dirname "$SCRIPT_DIR")"
DEST="$CRATE_DIR/tests/fixtures/less-testdata"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Fetching less.js $TAG ..."
# Extract only the test-data package + the repo LICENSE from the release tarball.
curl -sL "https://codeload.github.com/less/less.js/tar.gz/refs/tags/$TAG" \
  | tar xz -C "$TMP" \
      --wildcards \
      'less.js-*/packages/test-data/tests-unit' \
      'less.js-*/packages/test-data/tests-error' \
      'less.js-*/packages/test-data/data' \
      'less.js-*/LICENSE'

SRC="$(echo "$TMP"/less.js-*)"

# NOTE: tests-config/ + node_modules/ are hand-vendored (Phase 4B) and kept.
rm -rf "$DEST/tests-unit" "$DEST/tests-error" "$DEST/data" "$DEST/LICENSE"
mkdir -p "$DEST"
cp -R "$SRC/packages/test-data/tests-unit"  "$DEST/tests-unit"
cp -R "$SRC/packages/test-data/tests-error" "$DEST/tests-error"
cp -R "$SRC/packages/test-data/data"        "$DEST/data"
cp    "$SRC/LICENSE"                         "$DEST/LICENSE"

# Drop the cosmiconfig option files — the harness transcribes the per-suite
# options itself (tests-error runs strictMath:true/strictUnits:true/
# javascriptEnabled:true); their presence would only mislead. (Recorded in
# VENDOR.txt.)
find "$DEST/tests-unit" "$DEST/tests-error" -name 'styles.config.cjs' -delete

# The 19 OUT error fixtures (plan §5.2: @plugin-error x15, plugin-config x3,
# inline-JS x1) — JS-plugin execution surface, never reproducible by a pure
# headless compiler. Keep in sync with CLASSIFIED_OUT in tests/fixtures.rs
# (the §5.6 meta-test surfaces any drift loudly).
for f in functions-1 functions-3-assignment functions-4-call functions-5-color \
         functions-6-condition functions-7-dimension functions-8-element \
         functions-9-expression functions-10-keyword functions-11-operation \
         functions-12-quoted functions-13-selector functions-14-url \
         functions-15-value root-func-undefined-2 \
         plugin-1 plugin-2 plugin-3 javascript-undefined-var; do
  rm -f "$DEST/tests-error/eval/$f.less" "$DEST/tests-error/eval/$f.txt"
done
rm -rf "$DEST/tests-error/eval/plugin"

count=$(find "$DEST/tests-unit" -name '*.less' -print0 \
        | xargs -0 -I{} sh -c '[ -f "${0%.less}.css" ] && echo x' {} 2>/dev/null | wc -l | tr -d ' ')
errcount=$(find "$DEST/tests-error" -name '*.less' -print0 \
        | xargs -0 -I{} sh -c '[ -f "${0%.less}.txt" ] && echo x' {} 2>/dev/null | wc -l | tr -d ' ')
echo "Vendored $DEST"
echo "  tests-unit .less-with-sibling-.css entries: $count"
echo "  tests-error .less-with-sibling-.txt entries: $errcount (expect 74)"
echo "Expected tag $TAG @ $EXPECT_SHA (record in VENDOR.txt)."
