#!/usr/bin/env python3
"""Regenerate `src/constants_generated.rs` from JetBrains/phpstorm-stubs.

    git clone --depth 1 https://github.com/JetBrains/phpstorm-stubs.git /tmp/stubs
    python3 crates/magecommand-php/scripts/gen-php-constants.py /tmp/stubs

Why a generated table at all: `setup:di:compile` folds a constructor default
by REFLECTING it, so the oracle holds whatever value the compiling PHP had.
magecommand never executes PHP, so it needs the values from somewhere, and
hand-picking them one incident at a time does not scale.

Why it cannot be a blanket import: the oracle bakes the COMPILING MACHINE's
value, and some constants differ by platform, PHP version or build. The
measured example is `SIGUSR1` — 10 in the stubs (Linux), 30 on macOS. Baking
either one produces a confidently wrong value on the other platform, which is
strictly worse than not folding: an unresolved default degrades to a NULL row
that `di verify` reports honestly, while a wrong value silently ships.

So anything environment-dependent is EXCLUDED here and left to that honest
degradation. The hand-written table in `constexpr.rs` keeps precedence over
this file, because its entries encode deliberate decisions (POSIX
`DIRECTORY_SEPARATOR`, 64-bit `PHP_INT_SIZE`, CLI `PHP_SAPI`).
"""

import os
import re
import sys
import collections

# Environment-dependent, non-scalar, or deliberately owned by the hand table.
EXCLUDE_EXACT = {
    # Decided by the hand-written table (compile-environment assumptions).
    "PHP_SAPI", "PHP_EOL", "DIRECTORY_SEPARATOR", "PATH_SEPARATOR",
    "PHP_INT_MAX", "PHP_INT_MIN", "PHP_INT_SIZE", "E_ALL",
    # Vary by PHP version.
    "PHP_VERSION", "PHP_VERSION_ID", "PHP_MAJOR_VERSION", "PHP_MINOR_VERSION",
    "PHP_RELEASE_VERSION", "PHP_EXTRA_VERSION",
    # Vary by platform / build / install layout.
    "PHP_OS", "PHP_OS_FAMILY", "PHP_DEBUG", "PHP_ZTS", "PHP_MAXPATHLEN",
    "PHP_BINARY", "PHP_BINDIR", "PHP_LIBDIR", "PHP_DATADIR", "PHP_MANDIR",
    "PHP_SYSCONFDIR", "PHP_LOCALSTATEDIR", "PHP_CONFIG_FILE_PATH",
    "PHP_CONFIG_FILE_SCAN_DIR", "PHP_PREFIX", "PHP_SHLIB_SUFFIX",
    "PHP_EXTENSION_DIR", "PHP_FD_SETSIZE",
    # Float layout is build-dependent.
    "PHP_FLOAT_DIG", "PHP_FLOAT_EPSILON", "PHP_FLOAT_MIN", "PHP_FLOAT_MAX",
}
# Signal numbers are platform-specific (SIGUSR1: 10 Linux / 30 macOS), and the
# Windows-only family never applies to a store we compile.
EXCLUDE_PREFIX = ("SIG", "PHP_WINDOWS_")

DEFINE = re.compile(
    r"""^[ \t]*define\s*\(\s*(['"])([A-Za-z_]\w*)\1\s*,\s*(.+?)\s*\)\s*;""", re.M
)
INT = re.compile(r"^-?\d+$")
HEX = re.compile(r"^-?0[xX][0-9a-fA-F]+$")
FLOAT = re.compile(r"^-?(\d+\.\d*|\.\d+|\d+)([eE][+-]?\d+)?$")
SQ = re.compile(r"^'(([^'\\]|\\.)*)'$")
DQ = re.compile(r'^"(([^"\\]|\\.)*)"$')


def php_single_quoted(body):
    return body.replace("\\\\", "\\").replace("\\'", "'")


def php_double_quoted(body):
    if re.search(r"\\[nrtvfxu0-7$]|\$\{|\$[A-Za-z_]", body):
        return None  # escapes/interpolation we will not reimplement
    return body.replace('\\\\', '\\').replace('\\"', '"')


def literal(raw):
    """(rust_variant, rust_payload) for a PHP literal, or None."""
    raw = raw.strip()
    if INT.match(raw):
        v = int(raw)
        return ("Int", str(v)) if -(2**63) <= v < 2**63 else None
    if HEX.match(raw):
        v = int(raw, 16)
        return ("Int", str(v)) if -(2**63) <= v < 2**63 else None
    if raw in ("true", "TRUE"):
        return ("Bool", "true")
    if raw in ("false", "FALSE"):
        return ("Bool", "false")
    if FLOAT.match(raw) and ("." in raw or "e" in raw.lower()):
        return ("Float", repr(float(raw)))
    m = SQ.match(raw)
    if m:
        s = php_single_quoted(m.group(1))
    else:
        m = DQ.match(raw)
        if not m:
            return None
        s = php_double_quoted(m.group(1))
        if s is None:
            return None
    return ("Str", '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"')


def main(stub_root):
    seen = {}
    conflicts = collections.defaultdict(set)
    for dirpath, dirnames, filenames in os.walk(stub_root):
        dirnames[:] = [d for d in dirnames if d != ".git"]
        for fn in sorted(filenames):
            if not fn.endswith(".php"):
                continue
            path = os.path.join(dirpath, fn)
            with open(path, encoding="utf8", errors="replace") as fh:
                text = fh.read()
            for m in DEFINE.finditer(text):
                name, raw = m.group(2), m.group(3)
                lit = literal(raw)
                if lit is None:
                    continue
                if name in seen and seen[name] != lit:
                    conflicts[name].add(seen[name])
                    conflicts[name].add(lit)
                seen.setdefault(name, lit)

    # A name the stubs give two different values for is not a fact we can
    # state; drop it rather than pick arbitrarily.
    for name in conflicts:
        seen.pop(name, None)

    kept = {
        n: v
        for n, v in seen.items()
        if n not in EXCLUDE_EXACT and not n.startswith(EXCLUDE_PREFIX)
    }

    out = [
        "//! PHP core and extension constants, generated from JetBrains/phpstorm-stubs.",
        "//!",
        "//! DO NOT EDIT BY HAND — regenerate with",
        "//! `crates/magecommand-php/scripts/gen-php-constants.py <stub checkout>`,",
        "//! which also documents why environment-dependent constants are absent.",
        "//!",
        "//! The hand-written table in `constexpr.rs` takes precedence over this one.",
        "",
        "// These are PHP's own values for M_PI and friends, not approximations of",
        "// Rust's constants: reproducing what the oracle folds is the whole point.",
        "#![allow(clippy::approx_constant)]",
        "",
        "/// A PHP literal, in a shape that can be built in a `const` context.",
        "#[derive(Debug, Clone, Copy)]",
        "pub(crate) enum Lit {",
        "    Int(i64),",
        "    Float(f64),",
        "    Str(&'static str),",
        "    Bool(bool),",
        "}",
        "",
        "/// Sorted by name — looked up with a binary search.",
        "pub(crate) static PHP_CONSTANTS: &[(&str, Lit)] = &[",
    ]
    for name in sorted(kept):
        variant, payload = kept[name]
        out.append(f'    ("{name}", Lit::{variant}({payload})),')
    out.append("];")
    out.append("")

    dest = os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "src", "constants_generated.rs"
    )
    with open(os.path.normpath(dest), "w", encoding="utf8") as fh:
        fh.write("\n".join(out))
    print(f"wrote {len(kept)} constants to {os.path.normpath(dest)}")
    print(f"  dropped {len(conflicts)} conflicting, "
          f"{len(seen) - len(kept)} excluded as environment-dependent")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "/tmp/stubs")
