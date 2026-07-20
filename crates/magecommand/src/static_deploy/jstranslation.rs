//! `js-translation.json` generation — the one deployed artifact whose bytes
//! depend on the **locale's translation dictionary**, faithfully reproducing
//! `Magento\Translation\Model\Js\DataProvider::getData` +
//! `Model\Json\PreProcessor::process` (verified against module-translation
//! source on the corpus).
//!
//! ## What the real deploy does (source-cited)
//!
//! `DeployTranslationsDictionary::deploy(area, theme, locale)` publishes the
//! `js-translation.json` asset, whose `Json\PreProcessor` calls
//! `json_encode($dataProvider->getData($themePath))` (DEFAULT flags → slashes
//! escaped). `getData`:
//!  1. collects every **base + area** `*.js` (lib `mage`/`varien`, all modules'
//!     `view/{base,area}/web`, all area themes' `web`) and every base + area
//!     static `*.html` (modules' `view/{base,area}/web/template[s]`, themes'
//!     `web/template`) — the `$themePath` argument is **ignored**, so the file
//!     set (hence the extracted phrase set) is **theme-independent**: only the
//!     locale's dictionary varies. (This is why blank and luma emit a
//!     byte-identical `js-translation.json`.)
//!  2. extracts translatable phrases from each file via five configured
//!     patterns (`Js\Config` di.xml `patterns`), after gluing string
//!     concatenations (`'a' + 'b'` → `'ab'`) and unescaping `\'`/`\"`;
//!  3. keeps a phrase iff its translation differs from itself
//!     (`$phrase != $translate->render([$phrase])`), i.e. the locale's merged
//!     dictionary has a **non-identity** entry for it;
//!  4. `ksort` (byte order on keys) → `json_encode`.
//!
//! On `en_US` (no `en_US` dictionaries on a stock install) every phrase
//! translates to itself, so the map is empty and the file is the constant
//! `[]` — the previously-only-gated case. A locale WITH dictionaries (the
//! second-locale gate) yields the real phrase→translation object.
//!
//! ## The extraction (hand-written scanners, not a regex engine)
//!
//! The five di.xml patterns are PCRE with backreferences, negative
//! lookbehind and `(*SKIP)` — none expressible in the `regex` crate. Each is
//! a small, well-defined shape, so we scan by hand (full control, no heavy
//! dep):
//!  - `i18n:"…"` / `_.i18n("…")` — phrase to the first unescaped matching
//!    quote, single line (no `/s`);
//!  - `$.mage.__('…')` / `jQuery.mage.__("…")` and `$t('…')` — phrase to the
//!    first unescaped matching quote **that is followed by `\s*)`** (dotall,
//!    the `.+?`/`(*SKIP)` semantics), may span lines;
//!  - `translate="'…'"` and `translate args=…` — the html data-bind forms.
//!
//! Because the extracted set is intersected with the (small) locale
//! dictionary, only phrases that actually translate survive; incidental
//! over-matching of untranslated strings is harmless.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

/// A module root for the translation scan / dictionary — just its directory
/// (theme `<Vendor_Module>` override dirs are found by disk listing, not by
/// name).
#[derive(Debug, Clone)]
pub struct ScanModule {
    pub dir: PathBuf,
}

// ---------------------------------------------------------------------------
// CSV dictionary
// ---------------------------------------------------------------------------

/// Parse a Magento i18n CSV into `(key, value)` rows — comma-separated,
/// `"`-quoted fields with `""` escapes, values may span lines, extra columns
/// ignored. Matches PHP `fgetcsv`'s **leading-whitespace-before-enclosure
/// skip**: a field like `, "Not yet calculated"` (space before the quote) is
/// the quoted value `Not yet calculated`, NOT the literal ` "…"` — a real
/// corpus row (`Magento_Tax`'s en_US.csv) whose identity is what deletes the
/// phrase, so getting this wrong leaks a spurious translation.
fn parse_csv(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut fields: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    // At the start of a field, before any non-whitespace content: leading
    // spaces/tabs are buffered; if a quote follows they are discarded and the
    // field is quoted, else they flush into the (unquoted) value.
    let mut at_field_start = true;
    let mut pending_ws = String::new();
    let mut chars = text.chars().peekable();

    let flush_row =
        |fields: &mut Vec<String>, field: &mut String, out: &mut Vec<(String, String)>| {
            fields.push(std::mem::take(field));
            if fields.len() >= 2 && !fields[0].is_empty() {
                out.push((fields[0].clone(), fields[1].clone()));
            }
            fields.clear();
        };

    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes => {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            }
            '"' if at_field_start => {
                // leading whitespace before the enclosure is dropped
                pending_ws.clear();
                in_quotes = true;
                at_field_start = false;
            }
            ' ' | '\t' if at_field_start => pending_ws.push(c),
            ',' if !in_quotes => {
                field.push_str(&pending_ws);
                pending_ws.clear();
                fields.push(std::mem::take(&mut field));
                at_field_start = true;
            }
            '\n' if !in_quotes => {
                field.push_str(&pending_ws);
                pending_ws.clear();
                if !field.is_empty() || !fields.is_empty() {
                    flush_row(&mut fields, &mut field, &mut out);
                }
                at_field_start = true;
            }
            '\r' if !in_quotes => {}
            _ => {
                if at_field_start {
                    field.push_str(&pending_ws);
                    pending_ws.clear();
                    at_field_start = false;
                }
                field.push(c);
            }
        }
    }
    field.push_str(&pending_ws);
    if !field.is_empty() || !fields.is_empty() {
        flush_row(&mut fields, &mut field, &mut out);
    }
    out
}

/// Merge one csv file's rows into `dict` exactly like
/// `Translate::_addData`: an **identity row (`key === value`) DELETES** any
/// existing entry for that key (`unset`), otherwise the value overwrites —
/// with a second `str_replace('""', '"')` on key and value (PHP does it after
/// the identity check). Faithfully reproduces the multi-module override where
/// e.g. `Magento_Checkout`'s identity `"Not yet calculated"` row cancels
/// `Magento_Tax`'s earlier non-identity one → the phrase stays untranslated.
fn add_data(dict: &mut BTreeMap<String, String>, rows: &[(String, String)]) {
    for (k, v) in rows {
        if k == v {
            dict.remove(k);
            continue;
        }
        let key = k.replace("\"\"", "\"");
        let val = v.replace("\"\"", "\"");
        dict.insert(key, val);
    }
}

/// Build the merged translation dictionary for `locale`, mirroring
/// `Translate::loadData` order: every enabled module's `i18n/<locale>.csv` in
/// **config.php load order** (`_loadModuleTranslation`), then the theme
/// chain's `i18n/<locale>.csv` root→child (`_loadThemeTranslation`), each
/// merged via [`add_data`] (identity-row deletes). Language packs are not
/// modeled (empty shells on this corpus). `modules` MUST be the ENABLED set
/// in load order — the real `_moduleList->getNames()`, not the (larger)
/// ComponentRegistrar scan set.
pub fn merged_dictionary(
    modules: &[ScanModule],
    theme_dirs: &[PathBuf],
    locale: &str,
) -> BTreeMap<String, String> {
    let mut dict: BTreeMap<String, String> = BTreeMap::new();
    let file = format!("{locale}.csv");
    // Module csvs, in load order.
    let mut sources: Vec<PathBuf> = modules
        .iter()
        .map(|m| m.dir.join("i18n").join(&file))
        .collect();
    // Theme chain csvs, root-first (theme_dirs is child-first from the
    // resolver — reverse so the child's overrides land last).
    for d in theme_dirs.iter().rev() {
        sources.push(d.join("i18n").join(&file));
    }
    for src in sources {
        if let Ok(text) = std::fs::read_to_string(&src) {
            add_data(&mut dict, &parse_csv(&text));
        }
    }
    dict
}

// ---------------------------------------------------------------------------
// Phrase extraction
// ---------------------------------------------------------------------------

/// Glue string concatenations exactly like `getPhrases`'
/// `preg_replace('~(["\'])\s*?\+\s*?\1~', '', $content)`: a quote, optional
/// whitespace, `+`, optional whitespace, the SAME quote → removed (so
/// `'foo' + 'bar'` becomes `'foobar'`).
fn glue_concatenations(content: &str) -> String {
    let bytes = content.as_bytes();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' || c == b'\'' {
            // try to match  <c> \s* + \s* <c>
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'+' {
                let mut k = j + 1;
                while k < bytes.len() && (bytes[k] as char).is_ascii_whitespace() {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == c {
                    // Drop the whole `<c>…+…<c>` run (join the two literals).
                    i = k + 1;
                    continue;
                }
            }
        }
        // push this byte (content is valid UTF-8; ASCII-safe indexing since we
        // only special-case ASCII quotes/`+`).
        out.push(c as char);
        i += 1;
    }
    out
}

/// Unescape `\'`→`'` and `\"`→`"` like `str_replace(["\'", '\"'], …)`.
fn unescape_phrase(s: &str) -> String {
    s.replace("\\'", "'").replace("\\\"", "\"")
}

/// Find, from `bytes[start]` (which must be the opening quote), the phrase up
/// to the first **unescaped** matching quote; single-line (a newline aborts).
/// Returns `(phrase, index-after-closing-quote)`.
fn scan_quoted_line(bytes: &[u8], start: usize) -> Option<(String, usize)> {
    let quote = bytes[start];
    let mut i = start + 1;
    let phrase_start = i;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\n' {
            return None;
        }
        if c == quote {
            // unescaped iff the run of preceding backslashes is even
            let mut bs = 0;
            let mut k = i;
            while k > phrase_start && bytes[k - 1] == b'\\' {
                bs += 1;
                k -= 1;
            }
            if bs % 2 == 0 {
                let phrase = std::str::from_utf8(&bytes[phrase_start..i]).ok()?;
                return Some((phrase.to_string(), i + 1));
            }
        }
        i += 1;
    }
    None
}

/// Find, from `bytes[start]` (opening quote), the phrase up to the first
/// **unescaped** matching quote that is followed by `\s*)` — the
/// `.+?(?<!\\)\1\s*(*SKIP)\)` dotall semantics of the `$t(`/`.mage.__(`
/// patterns (may span lines). Requires at least one char (`.+?`).
fn scan_quoted_call(bytes: &[u8], start: usize) -> Option<(String, usize)> {
    let quote = bytes[start];
    let phrase_start = start + 1;
    let mut i = phrase_start;
    while i < bytes.len() {
        if bytes[i] == quote && i > phrase_start {
            let mut bs = 0;
            let mut k = i;
            while k > phrase_start && bytes[k - 1] == b'\\' {
                bs += 1;
                k -= 1;
            }
            if bs % 2 == 0 {
                // must be followed by \s* )
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b')' {
                    let phrase = std::str::from_utf8(&bytes[phrase_start..i]).ok()?;
                    return Some((phrase.to_string(), i + 1));
                }
                // else: this quote is part of the phrase; keep scanning.
            }
        }
        i += 1;
    }
    None
}

/// Extract every translatable phrase from one file's content, applying the
/// five configured patterns after the concatenation glue.
fn phrases_in(content: &str, out: &mut HashSet<String>) {
    let glued = glue_concatenations(content);
    let b = glued.as_bytes();
    let n = b.len();

    let is_ws = |x: u8| (x as char).is_ascii_whitespace();
    let starts = |b: &[u8], i: usize, pat: &[u8]| b[i..].starts_with(pat);

    let mut i = 0;
    while i < n {
        // Pattern 3/4: $.mage.__(  |  jQuery.mage.__(  |  $t(
        let call_len = if starts(b, i, b"$.mage.__(") {
            Some(b"$.mage.__(".len())
        } else if starts(b, i, b"jQuery.mage.__(") {
            Some(b"jQuery.mage.__(".len())
        } else if starts(b, i, b"$t(") {
            Some(b"$t(".len())
        } else {
            None
        };
        if let Some(hl) = call_len {
            let mut j = i + hl;
            while j < n && is_ws(b[j]) {
                j += 1;
            }
            if j < n && (b[j] == b'\'' || b[j] == b'"') {
                if let Some((phrase, next)) = scan_quoted_call(b, j) {
                    out.insert(unescape_phrase(&phrase));
                    i = next;
                    continue;
                }
            }
            i += hl;
            continue;
        }

        // Pattern 1: i18n:  |  _.i18n(
        let i18n_len = if starts(b, i, b"i18n:") {
            Some(b"i18n:".len())
        } else if starts(b, i, b"_.i18n(") {
            Some(b"_.i18n(".len())
        } else {
            None
        };
        if let Some(hl) = i18n_len {
            let mut j = i + hl;
            while j < n && is_ws(b[j]) {
                j += 1;
            }
            if j < n && (b[j] == b'\'' || b[j] == b'"') {
                if let Some((phrase, next)) = scan_quoted_line(b, j) {
                    out.insert(unescape_phrase(&phrase));
                    i = next;
                    continue;
                }
            }
            i += hl;
            continue;
        }

        // Pattern 2: translate="'…'"   (group1 = `"'`, phrase, then `'"`)
        // Pattern 5: translate args=("|'|"'|\"')(phrase)('\"|'"|'|")
        if starts(b, i, b"translate") {
            let after = i + b"translate".len();
            // translate args=
            if starts(b, after, b" args=") {
                let j = after + b" args=".len();
                // opening delimiter (one of " ' "' \"')
                let opens: [&[u8]; 4] = [b"\"'", b"\\\"'", b"\"", b"'"];
                let mut matched = false;
                for open in opens {
                    if starts(b, j, open) {
                        let ps = j + open.len();
                        // phrase = [^'] then .*? up to a closing delimiter
                        let closes: [&[u8]; 4] = [b"'\\\"", b"'\"", b"'", b"\""];
                        if ps < n && b[ps] != b'\'' {
                            // find earliest closing delimiter (non-greedy)
                            let mut k = ps + 1;
                            'outer: while k <= n {
                                for close in closes {
                                    if starts(b, k, close) {
                                        if let Ok(p) = std::str::from_utf8(&b[ps..k]) {
                                            if !p.contains('\n') {
                                                out.insert(unescape_phrase(p));
                                            }
                                        }
                                        i = k + close.len();
                                        matched = true;
                                        break 'outer;
                                    }
                                }
                                k += 1;
                            }
                        }
                        break;
                    }
                }
                if matched {
                    continue;
                }
            }
            // translate="'…'"
            if starts(b, after, b"=\"'") {
                let ps = after + b"=\"'".len();
                if ps < n && b[ps] != b'\'' {
                    // phrase up to `'"`
                    if let Some(rel) = find_seq(&b[ps..], b"'\"") {
                        if let Ok(p) = std::str::from_utf8(&b[ps..ps + rel]) {
                            if !p.contains('\n') {
                                out.insert(unescape_phrase(p));
                            }
                        }
                        i = ps + rel + 2;
                        continue;
                    }
                }
            }
        }

        i += 1;
    }
}

/// First index of `needle` in `hay`, or None.
fn find_seq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&k| &hay[k..k + needle.len()] == needle)
}

// ---------------------------------------------------------------------------
// File enumeration
// ---------------------------------------------------------------------------

/// Recursively collect files under `dir` whose name ends with `ext`
/// (case-sensitive, like the `*.js`/`*.html` globs), skipping dotfiles.
fn collect_ext(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            collect_ext(&p, ext, out);
        } else if name.ends_with(ext) {
            out.push(p);
        }
    }
}

/// Every js file the deploy's `getJsFiles('base', …) + getJsFiles(area, …)`
/// would scan, plus every static html the `getStaticHtmlFiles` calls would.
/// `theme_dirs_by_area` are the roots of the themes belonging to `area`.
fn scan_files(
    root: &Path,
    area: &str,
    modules: &[ScanModule],
    area_theme_dirs: &[PathBuf],
) -> Vec<PathBuf> {
    let mut js: Vec<PathBuf> = Vec::new();
    let mut html: Vec<PathBuf> = Vec::new();

    // lib/web/{mage,varien}
    for lib in ["mage", "varien"] {
        collect_ext(&root.join("lib").join("web").join(lib), ".js", &mut js);
    }

    // modules: view/{base,area}/web (js) + view/{base,area}/web/template[s] (html)
    for m in modules {
        for a in ["base", area] {
            let web = m.dir.join("view").join(a).join("web");
            collect_ext(&web, ".js", &mut js);
            collect_ext(&web.join("template"), ".html", &mut html);
            collect_ext(&web.join("templates"), ".html", &mut html);
        }
    }

    // area themes: <theme>/web + <theme>/<Vendor_Module>/web
    for t in area_theme_dirs {
        collect_ext(&t.join("web"), ".js", &mut js);
        collect_ext(&t.join("web").join("template"), ".html", &mut html);
        if let Ok(rd) = std::fs::read_dir(t) {
            for e in rd.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                // theme per-module override dirs look like `Vendor_Module`.
                if name.contains('_') && e.path().is_dir() {
                    let web = e.path().join("web");
                    collect_ext(&web, ".js", &mut js);
                    collect_ext(&web.join("template"), ".html", &mut html);
                }
            }
        }
    }

    js.extend(html);
    js
}

// ---------------------------------------------------------------------------
// Public entry
// ---------------------------------------------------------------------------

/// The extracted, translatable-in-general phrase set for `(area)` — the
/// expensive, locale-INDEPENDENT half. Cache this per area across locales.
pub fn extract_area_phrases(
    root: &Path,
    area: &str,
    modules: &[ScanModule],
    area_theme_dirs: &[PathBuf],
) -> HashSet<String> {
    let files = scan_files(root, area, modules, area_theme_dirs);
    files
        .par_iter()
        .fold(HashSet::new, |mut acc, f| {
            if let Ok(content) = std::fs::read_to_string(f) {
                phrases_in(&content, &mut acc);
            }
            acc
        })
        .reduce(HashSet::new, |mut a, b| {
            a.extend(b);
            a
        })
}

/// The `js-translation.json` body for a locale: intersect the extracted
/// phrase set with the locale's dictionary, drop identity translations,
/// `ksort` by key (byte order), PHP `json_encode` (slashes escaped, non-ASCII
/// `\uXXXX`). Empty → the constant `[]` (the en_US case).
pub fn js_translation_json(dict: &BTreeMap<String, String>, phrases: &HashSet<String>) -> String {
    let mut entries: Vec<(&str, &str)> = dict
        .iter()
        .filter(|(k, v)| k != v && phrases.contains(k.as_str()))
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    // BTreeMap already yields keys in byte order (== PHP ksort for these).
    entries.sort_by(|a, b| a.0.cmp(b.0));
    super::files::dictionary_json(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glue_joins_concatenated_literals() {
        assert_eq!(glue_concatenations("'foo' + 'bar'"), "'foobar'");
        assert_eq!(glue_concatenations("\"a\"+\"b\""), "\"ab\"");
        // mismatched quotes are NOT glued
        assert_eq!(glue_concatenations("'foo' + \"bar\""), "'foo' + \"bar\"");
    }

    #[test]
    fn extracts_t_and_mage_calls() {
        let mut out = HashSet::new();
        phrases_in(
            "var x = $t('Cancel Order'); $.mage.__('Close'); jQuery.mage.__(\"Confirm\")",
            &mut out,
        );
        assert!(out.contains("Cancel Order"));
        assert!(out.contains("Close"));
        assert!(out.contains("Confirm"));
    }

    #[test]
    fn t_call_phrase_with_inner_paren_and_percent() {
        let mut out = HashSet::new();
        phrases_in("$t('File \"%1\" exceeds the maximum allowed size.')", &mut out);
        assert!(out.contains("File \"%1\" exceeds the maximum allowed size."));
    }

    #[test]
    fn i18n_binding() {
        let mut out = HashSet::new();
        phrases_in("<span data-bind=\"i18n: 'Loading items...'\"></span>", &mut out);
        assert!(out.contains("Loading items..."));
    }

    #[test]
    fn identity_and_untranslated_excluded() {
        let mut dict = BTreeMap::new();
        dict.insert("Close".to_string(), "Fermer".to_string());
        dict.insert("Same".to_string(), "Same".to_string()); // identity
        let mut phrases = HashSet::new();
        phrases.insert("Close".to_string());
        phrases.insert("Same".to_string());
        phrases.insert("Untranslated".to_string()); // not in dict
        let json = js_translation_json(&dict, &phrases);
        assert_eq!(json, "{\"Close\":\"Fermer\"}");
    }

    #[test]
    fn empty_is_bracket_pair() {
        let dict = BTreeMap::new();
        let phrases = HashSet::new();
        assert_eq!(js_translation_json(&dict, &phrases), "[]");
    }
}
