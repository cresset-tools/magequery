//! Semantic CSS differ (plan §7.7) — the Gate T2 comparator.
//!
//! Parses two CSS files into ordered lists of `(at-rule context, selector,
//! [(prop, value)])` entries, normalizes ONLY what the pinned less.php oracle
//! (wikimedia/less.php v5.5.1, probes in the Phase-5 oracle report) proves
//! non-semantic, and reports every residual difference. ORDER-PRESERVING:
//! the cascade is order-sensitive, so rules are never sorted — a moved rule
//! is reported as removed+added by the sequence diff.
//!
//! Normalizations applied (each pinned against the oracle probes):
//! - comment stripping (`/* … */`, string-aware);
//! - whitespace collapse outside strings; `,` canonicalized to `", "`;
//! - hex colors lowercased and `#abc` expanded to `#aabbcc` (values only);
//! - leading zero: `.5` → `0.5` (the oracle's expanded mode canonicalizes);
//! - property names lowercased.
//!
//! Deliberately NOT normalized (probed: less.php never does):
//! - `0px` stays `0px` (never collapsed to `0`);
//! - trailing zeros / number precision (both engines print 8 sig decimals);
//! - anything inside quoted strings or `url(…)`.

use std::fmt;

/// One flattened rule: the at-rule ancestry it sits in, its own prelude
/// (selector list, `@font-face`, a keyframe step, …), and its declarations.
/// Block-less at-rules (`@import …;`, `@charset …;`) become entries with the
/// full statement as `selector` and no decls.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CssRule {
    pub context:  Vec<String>,
    pub selector: String,
    pub decls:    Vec<(String, String)>,
}

impl CssRule {
    fn qualified(&self) -> String {
        if self.context.is_empty() {
            self.selector.clone()
        } else {
            format!("{} {{ {}", self.context.join(" { "), self.selector)
        }
    }
}

/// A single semantic difference. `expected` = side A (the oracle golden),
/// `actual` = side B (our output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// A rule present in the golden is absent (at this sequence position).
    RuleMissing { rule: CssRule },
    /// A rule we emit that the golden doesn't have (at this position).
    RuleExtra { rule: CssRule },
    /// Same rule, one declaration's value differs.
    DeclChanged { rule: String, prop: String, expected: String, actual: String },
    /// The golden's rule carries a declaration ours lacks.
    DeclMissing { rule: String, prop: String, value: String },
    /// Our rule carries a declaration the golden lacks.
    DeclExtra { rule: String, prop: String, value: String },
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Finding::RuleMissing { rule } => {
                write!(f, "missing rule: {}  ({} decl(s))", rule.qualified(), rule.decls.len())
            }
            Finding::RuleExtra { rule } => {
                write!(f, "extra rule:   {}  ({} decl(s))", rule.qualified(), rule.decls.len())
            }
            Finding::DeclChanged { rule, prop, expected, actual } => {
                write!(f, "changed decl: {rule} {{ {prop}: expected `{expected}`, got `{actual}` }}")
            }
            Finding::DeclMissing { rule, prop, value } => {
                write!(f, "missing decl: {rule} {{ {prop}: {value} }}")
            }
            Finding::DeclExtra { rule, prop, value } => {
                write!(f, "extra decl:   {rule} {{ {prop}: {value} }}")
            }
        }
    }
}

/// Parse + normalize one CSS text into the ordered rule list.
pub fn parse_css(text: &str) -> Vec<CssRule> {
    let stripped = strip_comments(text);
    let mut rules: Vec<CssRule> = Vec::new();
    // Stack of open block preludes (normalized).
    let mut stack: Vec<String> = Vec::new();
    // Declarations collected for the innermost open block.
    let mut decls: Vec<(String, String)> = Vec::new();
    // Whether the innermost block has emitted its entry (a block that holds
    // only nested blocks — e.g. `@media` — emits nothing itself).
    let mut buf = String::new();
    let mut chars = stripped.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' => {
                buf.push(c);
                let quote = c;
                while let Some(d) = chars.next() {
                    buf.push(d);
                    if d == '\\' {
                        if let Some(e) = chars.next() { buf.push(e); }
                    } else if d == quote {
                        break;
                    }
                }
            }
            '{' => {
                stack.push(buf.trim().to_string());
                buf.clear();
                decls.clear();
            }
            '}' => {
                flush_decl(&mut buf, &mut decls);
                if !decls.is_empty() {
                    let selector = normalize_selector(stack.last().map(String::as_str).unwrap_or(""));
                    let context: Vec<String> = stack[..stack.len().saturating_sub(1)]
                        .iter()
                        .map(|s| normalize_selector(s))
                        .collect();
                    rules.push(CssRule { context, selector, decls: std::mem::take(&mut decls) });
                }
                stack.pop();
                decls.clear();
            }
            ';' => {
                if stack.is_empty() {
                    // Block-less at-rule at top level (@import, @charset).
                    let stmt = buf.trim();
                    if !stmt.is_empty() {
                        rules.push(CssRule {
                            context:  Vec::new(),
                            selector: normalize_selector(stmt),
                            decls:    Vec::new(),
                        });
                    }
                    buf.clear();
                } else {
                    flush_decl(&mut buf, &mut decls);
                }
            }
            _ => buf.push(c),
        }
    }
    rules
}

/// `prop: value` accumulated in `buf` → normalized decl pushed onto `decls`.
fn flush_decl(buf: &mut String, decls: &mut Vec<(String, String)>) {
    let text = buf.trim();
    if text.is_empty() {
        buf.clear();
        return;
    }
    // Split on the first `:` that isn't inside parens/brackets (guards
    // against `url(data:…)` in a hypothetical custom property).
    let mut depth = 0usize;
    let mut split = None;
    for (i, c) in text.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ':' if depth == 0 => { split = Some(i); break; }
            _ => {}
        }
    }
    if let Some(i) = split {
        let prop = text[..i].trim().to_ascii_lowercase();
        let value = normalize_value(text[i + 1..].trim());
        decls.push((prop, value));
    }
    buf.clear();
}

/// Strip `/* … */` comments, string-aware.
fn strip_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'"' | b'\'' => {
                let quote = c;
                out.push(c as char);
                i += 1;
                while i < bytes.len() {
                    let d = bytes[i];
                    out.push(bytes[i] as char);
                    i += 1;
                    if d == b'\\' && i < bytes.len() {
                        out.push(bytes[i] as char);
                        i += 1;
                    } else if d == quote {
                        break;
                    }
                }
            }
            _ => {
                // Multi-byte chars: copy the full char.
                let ch = text[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// Collapse whitespace + canonicalize comma spacing, string-aware.
fn collapse_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut pending_space = false;
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' => {
                if pending_space && !out.is_empty() { out.push(' '); }
                pending_space = false;
                out.push(c);
                let quote = c;
                while let Some(d) = chars.next() {
                    out.push(d);
                    if d == '\\' {
                        if let Some(e) = chars.next() { out.push(e); }
                    } else if d == quote {
                        break;
                    }
                }
            }
            c if c.is_whitespace() => pending_space = true,
            ',' => {
                // drop space before, force one after
                pending_space = false;
                out.push(',');
                // eat following whitespace; emit exactly one space unless )/end
                while chars.peek().is_some_and(|d| d.is_whitespace()) { chars.next(); }
                if chars.peek().is_some_and(|d| *d != ')') { out.push(' '); }
            }
            _ => {
                if pending_space && !out.is_empty() { out.push(' '); }
                pending_space = false;
                out.push(c);
            }
        }
    }
    out
}

fn normalize_selector(text: &str) -> String {
    collapse_ws(text)
}

/// Value normalization: whitespace/comma collapse, hex lowercase+expand,
/// leading-zero canonicalization — everything else preserved verbatim.
fn normalize_value(text: &str) -> String {
    let collapsed = collapse_ws(text);
    let mut out = String::with_capacity(collapsed.len());
    let mut chars = collapsed.chars().peekable();
    let mut prev: Option<char> = None;
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' => {
                out.push(c);
                let quote = c;
                while let Some(d) = chars.next() {
                    out.push(d);
                    if d == '\\' {
                        if let Some(e) = chars.next() { out.push(e); }
                    } else if d == quote {
                        break;
                    }
                }
                prev = Some(quote);
            }
            '#' => {
                // Collect hex digits; lowercase; expand 3/4-digit shorthand.
                let mut hex = String::new();
                while chars.peek().is_some_and(|d| d.is_ascii_hexdigit()) {
                    hex.push(chars.next().unwrap());
                }
                let boundary_after = !chars.peek().is_some_and(|d| d.is_ascii_alphanumeric());
                if (hex.len() == 3 || hex.len() == 4 || hex.len() == 6 || hex.len() == 8)
                    && boundary_after
                {
                    let lower = hex.to_ascii_lowercase();
                    out.push('#');
                    if lower.len() <= 4 {
                        for h in lower.chars() {
                            out.push(h);
                            out.push(h);
                        }
                    } else {
                        out.push_str(&lower);
                    }
                } else {
                    out.push('#');
                    out.push_str(&hex);
                }
                prev = Some('#');
            }
            c if (c.is_ascii_digit() || c == '.')
                && !prev.is_some_and(|p| p.is_ascii_alphanumeric() || p == '.')
                && (c != '.' || chars.peek().is_some_and(|d| d.is_ascii_digit())) =>
            {
                // Numeric token: parse and re-print canonically. Both engines'
                // semantic value is round-to-8-decimals (numPrecision=8,
                // probed); PHP leaks float-print artifacts (`71.42857143000001`,
                // `1.0E-6`) that less.js never emits — canonicalizing the PRINT
                // (never the value beyond the pinned 8-decimal rounding) makes
                // the two spellings of the same number compare equal while a
                // genuine 8th-decimal difference still reports.
                let mut tok = String::new();
                tok.push(c);
                let mut seen_dot = c == '.';
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() {
                        tok.push(chars.next().unwrap());
                    } else if d == '.' && !seen_dot {
                        // Only consume the dot if a digit follows (guards
                        // `url(a-300.woff)` and version-ish `1.5.2` tails).
                        let mut ahead = chars.clone();
                        ahead.next();
                        if ahead.peek().is_some_and(|e| e.is_ascii_digit()) {
                            seen_dot = true;
                            tok.push(chars.next().unwrap());
                        } else {
                            break;
                        }
                    } else if d == 'e' || d == 'E' {
                        // Exponent (`1.0E-6`): only if sign?+digits follow.
                        let mut ahead = chars.clone();
                        ahead.next();
                        let signed = matches!(ahead.peek(), Some('+') | Some('-'));
                        if signed {
                            ahead.next();
                        }
                        if !ahead.peek().is_some_and(|e| e.is_ascii_digit()) {
                            break;
                        }
                        tok.push(chars.next().unwrap());
                        if signed {
                            tok.push(chars.next().unwrap());
                        }
                        while chars.peek().is_some_and(|e| e.is_ascii_digit()) {
                            tok.push(chars.next().unwrap());
                        }
                        break;
                    } else {
                        break;
                    }
                }
                match tok.parse::<f64>() {
                    Ok(v) if v.is_finite() => {
                        let r = (v * 1e8).round() / 1e8;
                        out.push_str(&format!("{r}"));
                    }
                    _ => out.push_str(&tok),
                }
                prev = Some('0');
            }
            _ => {
                out.push(c);
                prev = Some(c);
            }
        }
    }
    out
}

/// The diff result.
#[derive(Debug, Default)]
pub struct CssDiff {
    pub findings:       Vec<Finding>,
    pub rules_expected: usize,
    pub rules_actual:   usize,
}

impl CssDiff {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Semantic diff: `expected` = the golden (oracle SCD output), `actual` = ours.
pub fn diff(expected: &str, actual: &str) -> CssDiff {
    let a = parse_css(expected);
    let b = parse_css(actual);
    let mut out = CssDiff { findings: Vec::new(), rules_expected: a.len(), rules_actual: b.len() };

    // Myers diff over canonical rule identity (context + selector + decls).
    let keys_a: Vec<String> = a.iter().map(rule_key).collect();
    let keys_b: Vec<String> = b.iter().map(rule_key).collect();
    let script = myers(&keys_a, &keys_b);

    // Walk the edit script; inside each replace hunk, pair off entries that
    // share (context, selector) to report decl-level diffs instead of a
    // whole-rule remove+add.
    let mut i = 0usize; // consumed of a
    let mut j = 0usize; // consumed of b
    for (del, ins) in script {
        // `del` entries a[i..i+del] removed, `ins` entries b[j..j+ins] added.
        let hunk_a = &a[i..i + del];
        let hunk_b = &b[j..j + ins];
        report_hunk(hunk_a, hunk_b, &mut out.findings);
        i += del + 1; // +1 for the matched element that terminated the hunk
        j += ins + 1;
    }
    out
}

fn rule_key(r: &CssRule) -> String {
    let mut s = r.qualified();
    for (p, v) in &r.decls {
        s.push('\u{1}');
        s.push_str(p);
        s.push(':');
        s.push_str(v);
    }
    s
}

fn head_key(r: &CssRule) -> String {
    r.qualified()
}

/// Report one replace hunk: pair rules with identical (context, selector)
/// greedily in order; diff their decls; leftovers become missing/extra rules.
fn report_hunk(hunk_a: &[CssRule], hunk_b: &[CssRule], findings: &mut Vec<Finding>) {
    let mut used_b = vec![false; hunk_b.len()];
    let mut pair_of_a: Vec<Option<usize>> = vec![None; hunk_a.len()];
    let mut next_b = 0usize;
    for (ai, ra) in hunk_a.iter().enumerate() {
        let ka = head_key(ra);
        if let Some(bi) = (next_b..hunk_b.len()).find(|&bi| !used_b[bi] && head_key(&hunk_b[bi]) == ka) {
            used_b[bi] = true;
            pair_of_a[ai] = Some(bi);
            next_b = bi + 1;
        }
    }
    // Emit in a-order, interleaving unmatched b entries at their positions.
    let mut bi_cursor = 0usize;
    for (ai, ra) in hunk_a.iter().enumerate() {
        match pair_of_a[ai] {
            Some(bi) => {
                while bi_cursor < bi {
                    if !used_b[bi_cursor] {
                        findings.push(Finding::RuleExtra { rule: hunk_b[bi_cursor].clone() });
                        used_b[bi_cursor] = true;
                    }
                    bi_cursor += 1;
                }
                diff_decls(ra, &hunk_b[bi], findings);
                bi_cursor = bi + 1;
            }
            None => findings.push(Finding::RuleMissing { rule: ra.clone() }),
        }
    }
    for (bi, rb) in hunk_b.iter().enumerate() {
        if !used_b[bi] {
            findings.push(Finding::RuleExtra { rule: rb.clone() });
        }
    }
}

/// Decl-level diff for two rules with the same qualified selector.
fn diff_decls(ra: &CssRule, rb: &CssRule, findings: &mut Vec<Finding>) {
    let name = ra.qualified();
    // Order-preserving pass: align by (prop) greedily like the hunk pairing —
    // decls with the same prop pair up in order; leftover = missing/extra.
    let mut used_b = vec![false; rb.decls.len()];
    for (pa, va) in &ra.decls {
        let hit = rb.decls.iter().enumerate()
            .find(|(bi, (pb, _))| !used_b[*bi] && pb == pa);
        match hit {
            Some((bi, (_, vb))) => {
                used_b[bi] = true;
                if vb != va {
                    findings.push(Finding::DeclChanged {
                        rule:     name.clone(),
                        prop:     pa.clone(),
                        expected: va.clone(),
                        actual:   vb.clone(),
                    });
                }
            }
            None => findings.push(Finding::DeclMissing {
                rule:  name.clone(),
                prop:  pa.clone(),
                value: va.clone(),
            }),
        }
    }
    for (bi, (pb, vb)) in rb.decls.iter().enumerate() {
        if !used_b[bi] {
            findings.push(Finding::DeclExtra {
                rule:  name.clone(),
                prop:  pb.clone(),
                value: vb.clone(),
            });
        }
    }
}

/// Myers O(ND) diff. Returns hunks as `(deleted, inserted)` counts, one per
/// matched element (the final sentinel match closes the trailing hunk):
/// walking the script, each tuple means "delete `del` from a, insert `ins`
/// from b, then one element matches" — with a virtual match at the end.
fn myers(a: &[String], b: &[String]) -> Vec<(usize, usize)> {
    // Trim common prefix/suffix first (the dominant case: nearly-equal files).
    let mut pre = 0usize;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut post = 0usize;
    while post < a.len() - pre && post < b.len() - pre
        && a[a.len() - 1 - post] == b[b.len() - 1 - post]
    {
        post += 1;
    }
    let ca = &a[pre..a.len() - post];
    let cb = &b[pre..b.len() - post];

    let n = ca.len();
    let m = cb.len();
    let max = n + m;
    // Edit script via standard Myers with trace.
    let offset = max;
    let mut v = vec![0isize; 2 * max + 2];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found_d = None;
    'outer: for d in 0..=(max as isize) {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let idx = (k + offset as isize) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
                v[idx + 1]
            } else {
                v[idx - 1] + 1
            };
            let mut y = x - k;
            while (x as usize) < n && (y as usize) < m && ca[x as usize] == cb[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x as usize >= n && y as usize >= m {
                found_d = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let d_final = found_d.unwrap_or(0);

    // Backtrack to per-position ops: 'D' delete-from-a, 'I' insert-from-b, 'M' match.
    let mut ops: Vec<u8> = Vec::new();
    let mut x = n as isize;
    let mut y = m as isize;
    let mut d = d_final;
    while d > 0 {
        let vv = &trace[d as usize];
        let k = x - y;
        let idx = (k + offset as isize) as usize;
        let (prev_k, op) = if k == -d || (k != d && vv[idx - 1] < vv[idx + 1]) {
            (k + 1, b'I')
        } else {
            (k - 1, b'D')
        };
        let prev_idx = (prev_k + offset as isize) as usize;
        let prev_x = vv[prev_idx];
        let prev_y = prev_x - prev_k;
        // snake
        while x > if op == b'D' { prev_x + 1 } else { prev_x }
            && y > if op == b'I' { prev_y + 1 } else { prev_y }
        {
            ops.push(b'M');
            x -= 1;
            y -= 1;
        }
        ops.push(op);
        if op == b'D' { x -= 1; } else { y -= 1; }
        d -= 1;
    }
    while x > 0 && y > 0 {
        ops.push(b'M');
        x -= 1;
        y -= 1;
    }
    ops.reverse();

    // Fold ops into hunks (del, ins) per match, with a trailing sentinel.
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    let mut del = 0usize;
    let mut ins = 0usize;
    for op in ops {
        match op {
            b'D' => del += 1,
            b'I' => ins += 1,
            _ => {
                hunks.push((del, ins));
                del = 0;
                ins = 0;
            }
        }
    }
    // Trailing hunk (closed by the virtual end-match).
    hunks.push((del, ins));
    // Prepend `pre` empty hunks so the caller's index walk stays aligned:
    // simpler — since prefix elements are exact matches, emit them as
    // zero-hunks by adjusting: the caller advances i/j by del+1/ins+1 per
    // hunk, so `pre` matches = `pre` (0,0) hunks before, `post` after (the
    // final sentinel already covers position n+m).
    let mut full = vec![(0usize, 0usize); pre];
    full.extend(hunks);
    full.extend(std::iter::repeat((0, 0)).take(post));
    full
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_media() {
        let css = "@media screen { @font-face { font-family: 'X'; src: url('a.woff'); } }";
        let rules = parse_css(css);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].context, vec!["@media screen"]);
        assert_eq!(rules[0].selector, "@font-face");
        assert_eq!(rules[0].decls[0], ("font-family".into(), "'X'".into()));
    }

    #[test]
    fn self_diff_is_clean() {
        let css = "body { margin: 0; }\n.a, .b { color: #FFF; }\n@media (min-width: 768px) { .c { top: .5px; } }";
        let d = diff(css, css);
        assert!(d.is_clean(), "{:?}", d.findings);
    }

    #[test]
    fn normalization_equivalences_pass() {
        let a = ".a { color: #ABC; margin: .5px 0px; }";
        let b = ".a{color:#aabbcc;margin:0.5px 0px}";
        let d = diff(a, b);
        assert!(d.is_clean(), "{:?}", d.findings);
    }

    #[test]
    fn zero_px_is_not_zero() {
        // Probed: less.php never normalizes 0px → 0. These must differ.
        let d = diff(".a { margin: 0px; }", ".a { margin: 0; }");
        assert_eq!(d.findings.len(), 1);
        assert!(matches!(&d.findings[0], Finding::DeclChanged { prop, .. } if prop == "margin"));
    }

    #[test]
    fn one_mutation_yields_one_finding() {
        let a = "body { margin: 0; padding: 0; }\n.x { color: red; }\n.y { top: 1px; }";
        let b = "body { margin: 1px; padding: 0; }\n.x { color: red; }\n.y { top: 1px; }";
        let d = diff(a, b);
        assert_eq!(d.findings.len(), 1, "{:?}", d.findings);
        assert_eq!(
            d.findings[0],
            Finding::DeclChanged {
                rule:     "body".into(),
                prop:     "margin".into(),
                expected: "0".into(),
                actual:   "1px".into(),
            }
        );
    }

    #[test]
    fn number_print_artifacts_normalize() {
        // less.php leaks PHP float-print artifacts for the same pinned
        // 8-decimal value; the canonicalizer makes the spellings equal…
        let d = diff(
            ".a { font-size: 71.42857143000001%; top: 1.0E-6px; }",
            ".a { font-size: 71.42857143%; top: 0.000001px; }",
        );
        assert!(d.is_clean(), "{:?}", d.findings);
        // …while a genuine 8th-decimal difference still reports.
        let d = diff(".a { width: 33.33333333%; }", ".a { width: 33.33333334%; }");
        assert_eq!(d.findings.len(), 1);
    }

    #[test]
    fn missing_and_extra_rules_reported() {
        let a = ".a { color: red; }\n.b { color: blue; }\n.c { color: green; }";
        let b = ".a { color: red; }\n.d { color: black; }\n.c { color: green; }";
        let d = diff(a, b);
        assert_eq!(d.findings.len(), 2, "{:?}", d.findings);
        assert!(matches!(&d.findings[0], Finding::RuleMissing { rule } if rule.selector == ".b"));
        assert!(matches!(&d.findings[1], Finding::RuleExtra { rule } if rule.selector == ".d"));
    }

    #[test]
    fn missing_decl_reported() {
        let a = ".a { color: red; top: 1px; }";
        let b = ".a { color: red; }";
        let d = diff(a, b);
        assert_eq!(d.findings.len(), 1);
        assert!(matches!(&d.findings[0], Finding::DeclMissing { prop, .. } if prop == "top"));
    }

    #[test]
    fn order_matters() {
        // Same rules, different order: never silently equal.
        let a = ".a { color: red; }\n.b { color: blue; }";
        let b = ".b { color: blue; }\n.a { color: red; }";
        let d = diff(a, b);
        assert!(!d.is_clean());
    }

    #[test]
    fn blockless_atrules_and_comments() {
        let a = "@charset \"utf-8\";\n/* note */ .a { color: red; /* mid */ }";
        let b = "@charset \"utf-8\"; .a { color: red; }";
        let d = diff(a, b);
        assert!(d.is_clean(), "{:?}", d.findings);
    }

    #[test]
    fn strings_are_verbatim() {
        // Inside strings nothing is normalized (spacing, case, commas).
        let d = diff(
            ".a { content: \"A  ,B\"; }",
            ".a { content: \"A ,B\"; }",
        );
        assert_eq!(d.findings.len(), 1);
    }
}
