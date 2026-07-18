#!/usr/bin/env bash
# Vendor the less.js test-data fixtures at a pinned tag (plan §5.3).
#
# STEP 2 SCOPE: only the DEFAULT-OPTION compile fixtures are vendored — the
# `tests-unit/` tree (every `<name>.less` with a sibling `<name>.css`, plus its
# import-helper subtrees) and the binary `data/` assets (for data-uri/image-size,
# plan §C-assets). The option-driven sub-suites (`tests-config/`, `tests-error/`)
# are DEFERRED to later phases and are NOT vendored here (see NOTES.md).
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
      'less.js-*/packages/test-data/data' \
      'less.js-*/LICENSE'

SRC="$(echo "$TMP"/less.js-*)"

rm -rf "$DEST"
mkdir -p "$DEST"
cp -R "$SRC/packages/test-data/tests-unit" "$DEST/tests-unit"
cp -R "$SRC/packages/test-data/data"       "$DEST/data"
cp    "$SRC/LICENSE"                        "$DEST/LICENSE"

# Drop the cosmiconfig option files — the Rust harness runs default options only;
# their presence would only mislead. (They are recorded in VENDOR.txt.)
find "$DEST/tests-unit" -name 'styles.config.cjs' -delete

count=$(find "$DEST/tests-unit" -name '*.less' -print0 \
        | xargs -0 -I{} sh -c '[ -f "${0%.less}.css" ] && echo x' {} 2>/dev/null | wc -l | tr -d ' ')
echo "Vendored $DEST"
echo "  tests-unit .less-with-sibling-.css entries: $count"
echo "Expected tag $TAG @ $EXPECT_SHA (record in VENDOR.txt)."
