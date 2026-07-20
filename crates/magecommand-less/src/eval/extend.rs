//! `:extend` (plan §2.8, §4.5) — a faithful port of less.js 4.6.7's
//! `ExtendFinderVisitor` + `ProcessExtendsVisitor` (visitors/extend-visitor.js),
//! re-targeted at the evaluator's flattened [`Out`] tree:
//!
//! * **finder** — each [`Out::Rule`] carries its [`EvExtend`]s (built by
//!   `eval_nested_ruleset` in path-major finder order); every at-rule body is
//!   its own scope (less.js pushes an `allExtends` frame for Media AND AtRule),
//!   so outer extends propagate *into* nested blocks, never out of them.
//! * **chaining** — `doExtendChaining`: an extend whose target matches another
//!   extend's self selector spawns a derived extend (circular references cut by
//!   `parent_ids`; a runaway fixpoint errors after 100 iterations with less.js's
//!   `extend circular reference detected` message).
//! * **replace** — `findMatch`/`extendSelector`: exact whole-selector match by
//!   default, `all` = non-destructive fragment match (allowBefore/allowAfter),
//!   combinator-sensitive, non-overlapping; matched fragments are replaced by
//!   the extending selector, grafting a new path onto the matched rule.
//! * **visibility** — `(reference)`-hidden output ([`Out::Hidden`]) is marked
//!   per-selector invisible first; a visible extend's graft re-enables just
//!   that path (`ensureVisibility`), and the post-pass prune drops whatever
//!   stayed dark.
//!
//! Selectors are matched at *simple-selector* granularity: the joined path
//! strings are tokenized back into (combinator, element) runs — the same
//! granularity as less.js `Element`s — and grafted paths are re-rendered with
//! the serializer's combinator spacing.

use rustc_hash::FxHashSet;

use super::{combinator_css, AtBody, Out, OutSel};
use crate::css::Warning;
use crate::error::{ErrorKind, LessError};

/// One `:extend` hanging off an output rule, as built at eval time
/// (plan §2.8): the flattened self selector path, the rendered target
/// selector, the `all` flag, and the extending side's visibility.
#[derive(Debug, Clone)]
pub(super) struct EvExtend {
    /// The joined selector path the extend is attached to (`findSelfSelectors`).
    pub self_sel: String,
    /// The rendered target selector (interpolation evaluated).
    pub target_css: String,
    /// The `all` option — fragment match (allowBefore + allowAfter).
    pub all: bool,
    /// Whether the extending side renders (`false` inside `(reference)`).
    pub visible: bool,
    /// less.js `firstExtendOnThisSelectorPath` — only the first extend of a
    /// path pushes the chained selector onto its ruleset.
    pub first_on_path: bool,
    /// An interpolated target (`.@{name}`) NEVER matches in less.js — the
    /// needle's element values stay non-string nodes after eval, and
    /// `isElementValuesEqual` rejects them (hay-side interpolation IS
    /// reparsed and matchable). The warning still shows the evaluated text.
    pub matchable: bool,
    /// Source position of the target (for warning dedup — less.js keys its
    /// no-match dedup by `(index, selector)`).
    pub span: crate::ast::Span,
}

// ---------------------------------------------------------------------------
// Selector tokens
// ---------------------------------------------------------------------------

/// One simple-selector element: a leading combinator (`""`, `" "`, `">"`, `"+"`,
/// `"~"`, `"|"`, `"^"`, `"^^"`, `"/deep/"`) + the element text.
#[derive(Debug, Clone, PartialEq)]
struct SelToken {
    comb: String,
    value: String,
    /// Glued to the PREVIOUS token by `&`-concatenation (the element-fusion
    /// marker, see `splice_parent`): `.abs-tax-total` + `-expanded`. less.php
    /// never matches an extend across or into a fused element (probed
    /// v5.5.1), so php-profile matching rejects fragments whose edge cuts a
    /// fusion; less.js DOES match element-wise through it (probed 4.6.7).
    fused: bool,
}

/// Tokenize a rendered selector string into simple-selector elements (the
/// less.js `Element` granularity — a compound `.a.b:hover` is three tokens).
fn tokenize(s: &str) -> Vec<SelToken> {
    let b = s.as_bytes();
    let mut i = 0usize;
    let mut toks: Vec<SelToken> = Vec::new();
    loop {
        let mut ws = false;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
            ws = true;
        }
        if i >= b.len() {
            break;
        }
        // Combinator?
        let comb = match b[i] {
            b'>' | b'+' | b'~' | b'^' => {
                let c = b[i] as char;
                i += 1;
                let mut cs = c.to_string();
                if c == '^' && i < b.len() && b[i] == b'^' {
                    cs.push('^');
                    i += 1;
                }
                while i < b.len() && b[i].is_ascii_whitespace() {
                    i += 1;
                }
                cs
            }
            b'|' => {
                i += 1;
                "|".to_string()
            }
            b'/' => {
                // A slashed combinator (`/deep/`).
                match s[i + 1..].find('/') {
                    Some(end)
                        if end > 0
                            && s[i + 1..i + 1 + end]
                                .bytes()
                                .all(|c| c.is_ascii_alphanumeric()) =>
                    {
                        let cs = s[i..i + end + 2].to_string();
                        i += end + 2;
                        while i < b.len() && b[i].is_ascii_whitespace() {
                            i += 1;
                        }
                        cs
                    }
                    _ => if ws { " " } else { "" }.to_string(),
                }
            }
            _ => if ws { " " } else { "" }.to_string(),
        };
        if i >= b.len() {
            break;
        }
        // The element-fusion marker (`\u{2}`, see `splice_parent`): a `&`
        // concatenation boundary — the fused halves are SEPARATE elements
        // (less.js Element granularity), so an extend target spelling the
        // fused text as one element must not match. Consume the marker and
        // start a fresh combinator-less token flagged as fused.
        let mut fused = false;
        if b[i] == 0x02 {
            fused = true;
            i += 1;
            if i >= b.len() {
                break;
            }
        }
        // Element value.
        let start = i;
        match b[i] {
            b'.' if !matches!(b.get(i + 1), Some(d) if d.is_ascii_digit()) => {
                i += 1;
                i = scan_ident(b, i);
            }
            b'#' => {
                i += 1;
                i = scan_ident(b, i);
            }
            b':' => {
                i += 1;
                if b.get(i) == Some(&b':') {
                    i += 1;
                }
                i = scan_ident(b, i);
                if b.get(i) == Some(&b'(') {
                    i = scan_balanced(b, i, b'(', b')');
                }
            }
            b'[' => i = scan_balanced(b, i, b'[', b']'),
            b'(' => i = scan_balanced(b, i, b'(', b')'),
            b'*' | b'&' => i += 1,
            c if c.is_ascii_digit() || c == b'.' => {
                // A keyframe percentage (`50%`, `.5%`).
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                    i += 1;
                }
                if b.get(i) == Some(&b'%') {
                    i += 1;
                }
            }
            _ => {
                i = scan_ident(b, i);
                if i == start {
                    i += 1; // never stall on unexpected input
                }
            }
        }
        toks.push(SelToken {
            comb,
            value: s[start..i].to_string(),
            fused,
        });
    }
    toks
}

/// Consume an identifier run (`a-z0-9_-`, escapes, `%`, non-ASCII).
fn scan_ident(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        match b[i] {
            b'\\' => i = (i + 2).min(b.len()),
            c if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'%' || c >= 0x80 => {
                i += 1
            }
            _ => break,
        }
    }
    i
}

/// Consume a balanced `open…close` region (string-aware).
fn scan_balanced(b: &[u8], mut i: usize, open: u8, close: u8) -> usize {
    debug_assert_eq!(b[i], open);
    i += 1;
    let mut depth = 1i32;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            c if c == open => {
                depth += 1;
                i += 1;
            }
            c if c == close => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    break;
                }
            }
            _ => i += 1,
        }
    }
    i.min(b.len())
}

/// Re-render tokens with the serializer's combinator spacing
/// ([`combinator_css`]); grafted paths must be byte-identical to joined ones.
/// A leading EXPLICIT combinator keeps its leading space (` > .b` — less.js
/// renders a first-element `>` as ` > `, E7); only the implicit descendant
/// space is trimmed.
fn render_tokens(toks: &[SelToken]) -> String {
    let mut s = String::new();
    for t in toks {
        s.push_str(&combinator_css(&t.comb));
        s.push_str(&t.value);
    }
    match toks.first() {
        Some(t) if !matches!(t.comb.as_str(), "" | " ") => s,
        _ => s.trim_start().to_string(),
    }
}

/// Render a target selector the way less.js `Selector.toCSS` does in
/// warnings/errors: the first element's implicit combinator emits a leading
/// space (`' .zzz'`), explicit combinators their spaced form (`' > .t'`).
fn leading_render(toks: &[SelToken], fallback: &str) -> String {
    if toks.is_empty() {
        return format!(" {fallback}");
    }
    let mut s = String::new();
    for (i, t) in toks.iter().enumerate() {
        if i == 0 && matches!(t.comb.as_str(), "" | " ") {
            s.push(' ');
        } else {
            s.push_str(&combinator_css(&t.comb));
        }
        s.push_str(&t.value);
    }
    s
}

/// less.js `isElementValuesEqual`: string equality, with attribute selectors
/// compared structurally (quote style normalized: `[a=v]` ≡ `[a="v"]`).
fn values_eq(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    if a.starts_with('[') && b.starts_with('[') {
        if let (Some(x), Some(y)) = (parse_attr(a), parse_attr(b)) {
            return x == y;
        }
    }
    false
}

/// Split `[key op value]` into (key, op, unquoted value). Mirrors less.js
/// `isElementValuesEqual`'s structural Attribute compare: the op is found
/// OUTSIDE quotes (`[a="b~=c"]` keeps its embedded op, E8), quote style is
/// normalized, and a trailing case-flag (`[a="v" i]`) is DROPPED — less.js
/// never compares `cif` (E2).
fn parse_attr(s: &str) -> Option<(String, String, String)> {
    let inner = s.strip_prefix('[')?.strip_suffix(']')?;
    // Find the operator outside quoted regions.
    let b = inner.as_bytes();
    let mut op_at: Option<(usize, usize)> = None; // (pos, len)
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'~' | b'^' | b'$' | b'*' | b'|' if b.get(i + 1) == Some(&b'=') => {
                op_at = Some((i, 2));
                break;
            }
            b'=' => {
                op_at = Some((i, 1));
                break;
            }
            _ => i += 1,
        }
    }
    let Some((pos, len)) = op_at else {
        // Key-only form `[disabled]` — a trailing flag would be part of the
        // key text; keep verbatim (matches less.js's key compare).
        return Some((inner.trim().to_string(), String::new(), String::new()));
    };
    let key = inner[..pos].trim().to_string();
    let op = if len == 2 { inner[pos..pos + 2].to_string() } else { "=".to_string() };
    let mut val = inner[pos + len..].trim();
    // Strip the case-sensitivity flag: a single trailing `i`/`I`/`s`/`S`
    // preceded by whitespace or a closing quote.
    if val.len() >= 2 && matches!(val.as_bytes()[val.len() - 1], b'i' | b'I' | b's' | b'S') {
        let prev = val.as_bytes()[val.len() - 2];
        if prev.is_ascii_whitespace() || prev == b'"' || prev == b'\'' {
            val = val[..val.len() - 1].trim_end();
        }
    }
    let mut val = val;
    if val.len() >= 2
        && ((val.starts_with('"') && val.ends_with('"'))
            || (val.starts_with('\'') && val.ends_with('\'')))
    {
        val = &val[1..val.len() - 1];
    }
    Some((key, op, val.to_string()))
}

// ---------------------------------------------------------------------------
// findMatch / extendSelector (ports)
// ---------------------------------------------------------------------------

/// A completed match: `len` needle elements replacing `hay[index..index+len]`,
/// keeping the matched start's original combinator.
#[derive(Debug, Clone)]
struct MatchSpan {
    index: usize,
    len: usize,
    initial_comb: String,
}

/// less.js `findMatch` over a flattened element list: every completed,
/// non-overlapping needle occurrence (combinator-sensitive; `allow_before`/
/// `allow_after` = the `all` option, otherwise whole-selector exact).
fn find_match(
    needle: &[SelToken],
    hay: &[SelToken],
    allow_before: bool,
    allow_after: bool,
) -> Vec<MatchSpan> {
    struct Potential {
        index: usize,
        matched: usize,
    }
    if needle.is_empty() {
        return Vec::new();
    }
    // Fast pre-scan: a match must consume the whole needle consecutively, so
    // if no hay token can accept needle[0] the potentials machinery (and its
    // per-token bookkeeping) can never produce one. Same predicate as the
    // first-token acceptance below — behavior-identical, allocation-free.
    if !hay.iter().any(|h| values_eq(&needle[0].value, &h.value)) {
        return Vec::new();
    }
    let mut potentials: Vec<Potential> = Vec::new();
    let mut matches: Vec<MatchSpan> = Vec::new();
    for (hi, h) in hay.iter().enumerate() {
        if allow_before || hi == 0 {
            potentials.push(Potential {
                index: hi,
                matched: 0,
            });
        }
        let mut i = 0usize;
        while i < potentials.len() {
            let pm = &mut potentials[i];
            // The joined path implies a descendant combinator at its start.
            let target_comb = if h.comb.is_empty() && hi == 0 { " " } else { h.comb.as_str() };
            if !values_eq(&needle[pm.matched].value, &h.value)
                || (pm.matched > 0 && needle[pm.matched].comb != target_comb)
            {
                potentials.remove(i);
                continue;
            }
            pm.matched += 1;
            if pm.matched == needle.len() {
                if !allow_after && hi + 1 < hay.len() {
                    potentials.remove(i);
                    continue;
                }
                matches.push(MatchSpan {
                    index: pm.index,
                    len: needle.len(),
                    // The starting token's combinator, cloned only now that a
                    // real match exists (it was captured eagerly before).
                    initial_comb: hay[pm.index].comb.clone(),
                });
                // Matches never overlap — restart matching after this one.
                potentials.clear();
                break;
            }
            i += 1;
        }
    }
    matches
}

/// Does this match's fragment edge cut a `&`-fusion boundary? (php profile:
/// such matches are rejected — less.php never extends into a fused element.)
fn cuts_fusion(hay: &[SelToken], m: &MatchSpan) -> bool {
    hay[m.index].fused || hay.get(m.index + m.len).is_some_and(|t| t.fused)
}

/// less.js `extendSelector`: replace each matched fragment with the
/// replacement selector (its first element takes the match's combinator).
fn extend_selector(matches: &[MatchSpan], hay: &[SelToken], repl: &[SelToken]) -> Vec<SelToken> {
    let mut out: Vec<SelToken> = Vec::new();
    let mut cur = 0usize;
    for m in matches {
        out.extend_from_slice(&hay[cur..m.index]);
        out.push(SelToken {
            comb: m.initial_comb.clone(),
            value: repl[0].value.clone(),
            fused: false,
        });
        out.extend_from_slice(&repl[1..]);
        cur = m.index + m.len;
    }
    out.extend_from_slice(&hay[cur..]);
    out
}

// ---------------------------------------------------------------------------
// The processing pass
// ---------------------------------------------------------------------------

/// One extend in flight (a less.js `Extend` object): identity for circular
/// cuts, tokenized self + target, and the rule (by scope index) whose paths a
/// chained selector is pushed onto.
#[derive(Clone)]
struct Work {
    id: u64,
    parent_ids: Vec<u64>,
    self_tokens: Vec<SelToken>,
    target: Vec<SelToken>,
    target_css: String,
    all: bool,
    visible: bool,
    first_on_path: bool,
    /// Index of the carrying rule in ITS scope's `outs` — only valid for the
    /// scope's own extends (chaining targets), never for inherited ones.
    rule_idx: usize,
    /// `false` for interpolated targets (see [`EvExtend::matchable`]).
    matchable: bool,
    /// Source span of the target (warning dedup key).
    span: crate::ast::Span,
}

struct State {
    next_id: u64,
    found: FxHashSet<u64>,
    /// less.php profile: reject extend matches whose fragment edge cuts a
    /// `&`-fusion boundary (see [`SelToken::fused`]).
    php_mode: bool,
}

impl State {
    fn fresh_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

/// Run the extend pass over the flattened output tree, then resolve
/// `(reference)` visibility (drop what stayed dark). Mutates `outs` in place.
pub(super) fn apply(
    outs: &mut Vec<Out>,
    warnings: &mut Vec<Warning>,
    php_mode: bool,
) -> Result<(), LessError> {
    // MarkVisibleSelectors: everything under a Hidden wrapper starts dark.
    for o in outs.iter_mut() {
        mark_hidden(o, false);
    }

    let mut st = State { next_id: 0, found: FxHashSet::default(), php_mode };
    let root_own = process_scope(outs, &[], &mut st)?;

    // less.js `checkExtendsForNonMatched(root.allExtends)`: only root-scope
    // ORIGINAL extends warn (chained ones have longer parent chains). The
    // dedup key is `(index, selector)` — two extends at different source
    // positions with the same unmatched target warn TWICE — and the selector
    // renders through `toCSS`, whose implicit leading combinator gives the
    // quoted text a leading space (`extend ' .zzz' has no matches`).
    let mut warned: FxHashSet<(usize, usize, String)> = FxHashSet::default();
    for w in &root_own {
        if !st.found.contains(&w.id)
            && warned.insert((w.span.start, w.span.end, w.target_css.clone()))
        {
            warnings.push(Warning {
                message: format!(
                    "extend '{}' has no matches",
                    leading_render(&w.target, &w.target_css)
                ),
                filename: None,
                line: None,
            });
        }
    }

    // Prune: unwrap Hidden nodes keeping only extend-re-enabled content.
    let pruned = std::mem::take(outs);
    outs.extend(pruned.into_iter().filter_map(prune));
    Ok(())
}

/// Recursively mark rule selectors under `(reference)` wrappers invisible.
fn mark_hidden(out: &mut Out, dark: bool) {
    match out {
        Out::Hidden(inner) => mark_hidden(inner, true),
        // The less.php-profile visibility shield: darkness stops here — the
        // content of a visibly-defined mixin stays visible (§3).
        Out::Visible(inner) => mark_hidden(inner, false),
        Out::Rule { selectors, .. } if dark => {
            for s in selectors.iter_mut() {
                s.visible = false;
            }
        }
        Out::At { body: AtBody::Rules(rules), .. } => {
            for r in rules.iter_mut() {
                mark_hidden(r, dark);
            }
        }
        _ => {}
    }
}

/// Post-extend visibility resolution: a `Hidden` subtree keeps only rules with
/// a re-enabled (visible) path; at-rule shells survive only around surviving
/// content; everything else (comments, inline payloads, bare decls) stays dark.
fn prune(out: Out) -> Option<Out> {
    match out {
        Out::Hidden(inner) => prune_hidden(*inner),
        Out::Visible(inner) => prune(*inner),
        Out::At { header, body: AtBody::Rules(rules) } => Some(Out::At {
            header,
            body: AtBody::Rules(rules.into_iter().filter_map(prune).collect()),
        }),
        other => Some(other),
    }
}

fn prune_hidden(out: Out) -> Option<Out> {
    match out {
        // A shield inside a hidden region: its content renders (recursing
        // through `prune` so deeper Hidden subtrees still resolve normally).
        Out::Visible(inner) => prune(*inner),
        Out::Rule { ref selectors, .. } => {
            if selectors.iter().any(|s| s.visible) {
                Some(out)
            } else {
                None
            }
        }
        Out::At { header, body: AtBody::Rules(rules) } => {
            let kept: Vec<Out> = rules.into_iter().filter_map(prune_hidden).collect();
            if kept.is_empty() {
                None
            } else {
                Some(Out::At { header, body: AtBody::Rules(kept) })
            }
        }
        Out::Hidden(inner) => prune_hidden(*inner),
        _ => None,
    }
}

/// Reach the rule payload of a scope entry (transparently through `Hidden`).
fn rule_of_mut(out: &mut Out) -> Option<&mut Out> {
    match out {
        Out::Rule { .. } => Some(out),
        Out::Hidden(inner) | Out::Visible(inner) => rule_of_mut(inner.as_mut()),
        _ => None,
    }
}

/// Process one scope (the root, or an at-rule body): collect its own extends,
/// chain against them, match everything (own + chained + inherited) against
/// this scope's rules, then recurse into nested at-rule bodies. Returns the
/// scope's own (original) extends for the root non-match check.
fn process_scope(
    outs: &mut Vec<Out>,
    inherited: &[Work],
    st: &mut State,
) -> Result<Vec<Work>, LessError> {
    // 1. Finder: this scope's own extends, in rule order.
    let mut own: Vec<Work> = Vec::new();
    for (i, out) in outs.iter_mut().enumerate() {
        let Some(Out::Rule { extends, .. }) = rule_of_mut(out) else { continue };
        for ev in extends.iter() {
            let id = st.fresh_id();
            own.push(Work {
                id,
                parent_ids: vec![id],
                self_tokens: tokenize(&ev.self_sel),
                target: tokenize(&ev.target_css),
                target_css: ev.target_css.clone(),
                all: ev.all,
                visible: ev.visible,
                first_on_path: ev.first_on_path,
                rule_idx: i,
                matchable: ev.matchable,
                span: ev.span,
            });
        }
    }

    // 2. Chaining fixpoint: combined = own ++ inherited (matching order); the
    //    chain targets stay this scope's own extends (less.js visitMedia).
    let own_len = own.len();
    let mut combined: Vec<Work> = own;
    combined.extend(inherited.iter().cloned());
    do_chaining(&mut combined, own_len, outs, st)?;

    // 3. Replace: match every extend against this scope's rules' ORIGINAL
    //    paths, appending grafts after the loop (less.js visitRuleset).
    for out in outs.iter_mut() {
        let Some(Out::Rule { selectors, extend_on_every_path, .. }) = rule_of_mut(out) else {
            continue;
        };
        if *extend_on_every_path {
            continue;
        }
        let n_orig = selectors.len();
        let hay: Vec<Option<Vec<SelToken>>> = selectors[..n_orig]
            .iter()
            .map(|s| if s.has_extend { None } else { Some(tokenize(&s.css)) })
            .collect();
        let mut to_add: Vec<OutSel> = Vec::new();
        for ex in &combined {
            if !ex.matchable {
                continue;
            }
            for h in hay.iter().flatten() {
                let mut matches = find_match(&ex.target, h, ex.all, ex.all);
                if st.php_mode {
                    matches.retain(|m| !cuts_fusion(h, m));
                }
                if matches.is_empty() {
                    continue;
                }
                st.found.insert(ex.id);
                let grafted = extend_selector(&matches, h, &ex.self_tokens);
                to_add.push(OutSel {
                    css: render_tokens(&grafted),
                    visible: ex.visible,
                    has_extend: false,
                });
            }
        }
        selectors.extend(to_add);
    }

    // 4. Recurse into nested at-rule scopes with the combined list.
    for out in outs.iter_mut() {
        fn at_rules_mut(out: &mut Out) -> Option<&mut Vec<Out>> {
            match out {
                Out::At { body: AtBody::Rules(rules), .. } => Some(rules),
                Out::Hidden(b) | Out::Visible(b) => at_rules_mut(b.as_mut()),
                _ => None,
            }
        }
        let inner = at_rules_mut(out);
        if let Some(rules) = inner {
            process_scope(rules, &combined, st)?;
        }
    }

    combined.truncate(own_len);
    Ok(combined)
}

/// less.js `doExtendChaining`: extends whose target matches another extend's
/// self selector spawn derived extends (and push the derived selector onto the
/// target's rule); iterate until stable, erroring past 100 rounds.
fn do_chaining(
    combined: &mut Vec<Work>,
    own_len: usize,
    outs: &mut [Out],
    st: &mut State,
) -> Result<(), LessError> {
    let mut src_start = 0usize; // first round: all of combined are sources
    for iteration in 0.. {
        let mut to_add: Vec<Work> = Vec::new();
        for si in src_start..combined.len() {
            for ti in 0..own_len {
                let src = &combined[si];
                let tgt = &combined[ti];
                if !src.matchable || src.parent_ids.contains(&tgt.id) {
                    continue;
                }
                let mut matches = find_match(&src.target, &tgt.self_tokens, src.all, src.all);
                if st.php_mode {
                    matches.retain(|m| !cuts_fusion(&tgt.self_tokens, m));
                }
                if matches.is_empty() {
                    continue;
                }
                st.found.insert(src.id);
                let new_tokens = extend_selector(&matches, &tgt.self_tokens, &src.self_tokens);
                let id = st.fresh_id();
                let mut parent_ids = vec![id];
                parent_ids.extend_from_slice(&tgt.parent_ids);
                parent_ids.extend_from_slice(&src.parent_ids);
                let derived = Work {
                    id,
                    parent_ids,
                    self_tokens: new_tokens.clone(),
                    target: tgt.target.clone(),
                    target_css: tgt.target_css.clone(),
                    all: tgt.all,
                    visible: tgt.visible,
                    first_on_path: tgt.first_on_path,
                    rule_idx: tgt.rule_idx,
                    matchable: tgt.matchable,
                    span: tgt.span,
                };
                if tgt.first_on_path {
                    // Push the chained selector onto the target's rule (it
                    // carries an extendList → never re-matched directly).
                    let src_visible = combined[si].visible;
                    if let Some(Out::Rule { selectors, .. }) =
                        rule_of_mut(&mut outs[derived.rule_idx])
                    {
                        selectors.push(OutSel {
                            css: render_tokens(&new_tokens),
                            visible: src_visible,
                            has_extend: true,
                        });
                    }
                }
                to_add.push(derived);
            }
        }
        if to_add.is_empty() {
            return Ok(());
        }
        if iteration > 100 {
            let w = &to_add[0];
            return Err(LessError::new(
                ErrorKind::Runtime,
                format!(
                    "extend circular reference detected. One of the circular extends is currently: {}:extend({})",
                    render_tokens(&w.self_tokens),
                    leading_render(&w.target, &w.target_css)
                ),
            ));
        }
        src_start = combined.len();
        combined.extend(to_add);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<SelToken> {
        tokenize(s)
    }

    #[test]
    fn tokenize_splits_compounds_and_combinators() {
        let t = toks(".a.b:hover > .c [x=\"y\"]");
        let flat: Vec<(String, String)> =
            t.iter().map(|t| (t.comb.clone(), t.value.clone())).collect();
        assert_eq!(
            flat,
            vec![
                ("".into(), ".a".into()),
                ("".into(), ".b".into()),
                ("".into(), ":hover".into()),
                (">".into(), ".c".into()),
                (" ".into(), "[x=\"y\"]".into()),
            ]
        );
        // Round-trip stability through the serializer spacing.
        assert_eq!(render_tokens(&t), ".a.b:hover > .c [x=\"y\"]");
    }

    #[test]
    fn exact_vs_all_matching() {
        // Exact: needle must equal the whole selector.
        assert_eq!(find_match(&toks(".a"), &toks(".a"), false, false).len(), 1);
        assert!(find_match(&toks(".a"), &toks(".a.b"), false, false).is_empty());
        assert!(find_match(&toks(".b"), &toks(".a .b"), false, false).is_empty());
        // `all`: fragment anywhere, including inside compounds; combinators count.
        assert_eq!(find_match(&toks(".a"), &toks(".a.b"), true, true).len(), 1);
        assert_eq!(find_match(&toks(".a"), &toks(".x .a > .a"), true, true).len(), 2);
        assert!(find_match(&toks(".a > .b"), &toks(".a .b"), true, true).is_empty());
        // Attribute quote normalization.
        assert_eq!(find_match(&toks("[d=\"t\"]"), &toks("[d=t]"), true, true).len(), 1);
        assert!(find_match(&toks("[d]"), &toks("[d=t]"), true, true).is_empty());
    }

    #[test]
    fn extend_selector_grafts_fragments() {
        // .rep:extend(.replace all) over `.replace.replace .replace`.
        let hay = toks(".replace.replace .replace");
        let m = find_match(&toks(".replace"), &hay, true, true);
        assert_eq!(m.len(), 3);
        let out = extend_selector(&m, &hay, &toks(".rep"));
        assert_eq!(render_tokens(&out), ".rep.rep .rep");
        // Suffix elements survive: `.h .h-nav:before` ← extend(.h .h-nav all).
        let hay = toks(".h .h-nav:before");
        let m = find_match(&toks(".h .h-nav"), &hay, true, true);
        let out = extend_selector(&m, &hay, &toks(".f .f-nav"));
        assert_eq!(render_tokens(&out), ".f .f-nav:before");
    }
}
