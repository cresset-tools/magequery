#!/bin/bash
# Differential test: our LESS compiler vs a store's own wikimedia/less.php.
#
#   diff-against-less-php.sh <magento-root> <file.less> [more.less ...]
#
# Point it at TWO stores on different less.php majors to separate "we are wrong"
# from "the versions disagree" — the distinction that matters, because a 2.4.7
# store and a 2.4.8 store legitimately deploy different css from one source.
# That is how the `@document` scoping difference and the mixin-extend one below
# were both found.
set -u
root=$1; shift
here=$(cd "$(dirname "$0")" && pwd)
mc=${MAGECOMMAND:-magecommand}
fail=0
for f in "$@"; do
  php=$(php "$here/lessc-php.php" "$root" "$f" 2>/dev/null | tail -1)
  ours=$("$mc" static less --file "$f" --compress --stdout 2>/dev/null | tail -1)
  if [ "$php" = "$ours" ]; then
    echo "ok    $f"
  else
    fail=1
    echo "DIFFER $f"
    echo "  less.php: $php"
    echo "  ours    : $ours"
  fi
done
exit $fail
