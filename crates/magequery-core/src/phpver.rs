//! PHP's `version_compare()`, ported from `ext/standard/versioning.c`.
//!
//! Magento decides whether a module or patch needs upgrading with `version_compare`, so
//! a status check has to order versions exactly as PHP does — including its quirks:
//! `1.0 < 1.0.0`, `1.0rc1 < 1.0`, `1.0 < 1.0pl1`, and any version `>` the empty string.

use std::cmp::Ordering;

/// PHP `version_compare($a, $b)` (the two-argument form).
pub fn version_compare(a: &str, b: &str) -> Ordering {
    php_version_compare(a.as_bytes(), b.as_bytes())
}

/// `php_version_compare`: empty strings first, then canonicalize (a `#`-prefixed operand —
/// only ever the internal `#N#` — is taken verbatim) and walk the parts.
fn php_version_compare(a: &[u8], b: &[u8]) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    let prep = |v: &[u8]| if v[0] == b'#' { v.to_vec() } else { canonicalize(v) };
    compare_canonical(&prep(a), &prep(b))
}

fn is_dig(c: u8) -> bool {
    c.is_ascii_digit() && c != b'.'
}

fn is_ndig(c: u8) -> bool {
    !c.is_ascii_digit() && c != b'.'
}

/// `php_canonicalize_version`: `-`/`_`/`+` and other non-alphanumerics become `.`, and a
/// `.` is inserted at every digit/non-digit boundary (`1.0rc1` → `1.0.rc.1`).
fn canonicalize(v: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 2);
    let Some((&first, rest)) = v.split_first() else { return out };
    out.push(first);
    let mut lp = first;
    for &c in rest {
        let last = *out.last().expect("non-empty");
        if matches!(c, b'-' | b'_' | b'+') {
            if last != b'.' {
                out.push(b'.');
            }
        } else if (is_ndig(lp) && is_dig(c)) || (is_dig(lp) && is_ndig(c)) {
            if last != b'.' {
                out.push(b'.');
            }
            out.push(c);
        } else if !c.is_ascii_alphanumeric() {
            if last != b'.' {
                out.push(b'.');
            }
        } else {
            out.push(c);
        }
        lp = c;
    }
    // A trailing `.` means an empty last component, which PHP drops.
    if out.last() == Some(&b'.') {
        out.pop();
    }
    out
}

/// `compare_special_version_forms`: `dev < alpha = a < beta = b < RC = rc < # < pl = p`;
/// anything unrecognised sorts below `dev`. Matching is by prefix (`strncmp(form, name,
/// strlen(name))`), so `alpha1`-style leftovers can't occur after canonicalization but
/// `p` still matches `pl…`.
fn special_form(s: &[u8]) -> i32 {
    const FORMS: [(&[u8], i32); 10] = [
        (b"dev", 0),
        (b"alpha", 1),
        (b"a", 1),
        (b"beta", 2),
        (b"b", 2),
        (b"RC", 3),
        (b"rc", 3),
        (b"#", 4),
        (b"pl", 5),
        (b"p", 5),
    ];
    for (name, order) in FORMS {
        if s.starts_with(name) {
            return order;
        }
    }
    -6
}

fn compare_special(a: &[u8], b: &[u8]) -> Ordering {
    special_form(a).cmp(&special_form(b))
}

/// `strtol(p, NULL, 10)` on a part that starts with a digit.
fn leading_number(s: &[u8]) -> i64 {
    let mut n: i64 = 0;
    for &c in s {
        if !c.is_ascii_digit() {
            break;
        }
        n = n.saturating_mul(10).saturating_add(i64::from(c - b'0'));
    }
    n
}

fn first_is_digit(s: &[u8]) -> bool {
    s.first().is_some_and(u8::is_ascii_digit)
}

/// The comparison loop of `php_version_compare`, over canonical forms. Mirrors the C
/// pointer walk: `n1`/`n2` record whether the current part was followed by a `.`.
fn compare_canonical(v1: &[u8], v2: &[u8]) -> Ordering {
    // Cursor into each string; `None` once the C code's `n` would be NULL after a part.
    let mut p1 = 0usize;
    let mut p2 = 0usize;
    // Initially `n` is non-NULL either way (a `.` or the string end).
    let mut n1_some = true;
    let mut n2_some = true;
    let mut compare = Ordering::Equal;

    while p1 < v1.len() && p2 < v2.len() && n1_some && n2_some {
        let end1 = v1[p1..].iter().position(|&c| c == b'.').map(|i| p1 + i);
        let end2 = v2[p2..].iter().position(|&c| c == b'.').map(|i| p2 + i);
        n1_some = end1.is_some();
        n2_some = end2.is_some();
        let part1 = &v1[p1..end1.unwrap_or(v1.len())];
        let part2 = &v2[p2..end2.unwrap_or(v2.len())];

        compare = match (first_is_digit(part1), first_is_digit(part2)) {
            (true, true) => leading_number(part1).cmp(&leading_number(part2)),
            (false, false) => compare_special(part1, part2),
            (true, false) => compare_special(b"#N#", part2),
            (false, true) => compare_special(part1, b"#N#"),
        };
        if compare != Ordering::Equal {
            break;
        }
        if let Some(e) = end1 {
            p1 = e + 1;
        }
        if let Some(e) = end2 {
            p2 = e + 1;
        }
    }

    if compare == Ordering::Equal {
        if n1_some {
            // `p1` points at what follows; the C code inspects `*p1` (NUL at the end).
            let rest = &v1[p1.min(v1.len())..];
            if first_is_digit(rest) {
                compare = Ordering::Greater;
            } else {
                compare = php_version_compare(rest, b"#N#");
            }
        } else if n2_some {
            let rest = &v2[p2.min(v2.len())..];
            if first_is_digit(rest) {
                compare = Ordering::Less;
            } else {
                compare = php_version_compare(b"#N#", rest);
            }
        }
    }
    compare
}

#[cfg(test)]
mod tests {
    use super::version_compare;
    use std::cmp::Ordering::{self, *};

    /// `(a, b, version_compare(a, b))`, each expectation recorded from PHP 8.5.12.
    const CASES: &[(&str, &str, Ordering)] = &[
        ("1.0.0", "1.0.0", Equal),
        ("2.0.10", "2.0.9", Greater),
        ("1.0", "1.0.0", Less),
        ("1.0.0", "1.0", Greater),
        ("2.0.0", "", Greater),
        ("", "2.0.0", Less),
        ("", "", Equal),
        ("1.0rc1", "1.0", Less),
        ("1.0", "1.0pl1", Less),
        ("1.0-dev", "1.0", Less),
        ("1.0alpha", "1.0beta", Less),
        ("1.0a", "1.0alpha", Equal),
        ("1.0RC1", "1.0rc1", Equal),
        ("1.0.0", "1.0.0.0", Less),
        ("1.0", "1.0.x", Greater),
        ("1.0.x", "1.0", Less),
        ("2.1.3", "2.1.3-p1", Less),
        ("2.0.0", "02.0.0", Equal),
        ("1.10", "1.9", Greater),
        ("1..0", "1.0", Equal),
        ("1.0.", "1.0", Equal),
        ("1", "1.0", Less),
        ("abc", "1", Less),
        ("1.0.0", "none", Greater),
    ];

    #[test]
    fn matches_php() {
        for &(a, b, want) in CASES {
            assert_eq!(version_compare(a, b), want, "version_compare({a:?}, {b:?})");
        }
    }
}
