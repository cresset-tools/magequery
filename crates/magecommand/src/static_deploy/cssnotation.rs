//! The CSS notation pre-processors of the static-deploy publish path —
//! `Magento\Framework\View\Asset\PreProcessor\{VariableNotation,ModuleNotation}`
//! reproduced faithfully (framework/View/Asset/PreProcessor/VariableNotation.php,
//! ModuleNotation.php; NotationResolver/{Variable,Module}.php).
//!
//! Both processors walk the **relative** `url(...)` references of a CSS file
//! via `CssResolver::replaceRelativeUrls` (framework/View/Url/CssResolver.php)
//! and rewrite each path through a callback:
//!
//! - **VariableNotation** substitutes `{{placeholder}}` variables inside the
//!   url path. The only supported placeholder is `base_url_path`, replaced by
//!   `{{base_url_path}}<area>/<themePath>/{{locale}}` — the OUTER placeholders
//!   are kept (they are substituted at email-render time by
//!   `Magento\Email\Model\Template\Css\Processor`); any other placeholder is
//!   replaced with the empty string. Real effect on a stock deploy: the
//!   `@{baseUrl}` import in `css/email-inline.css` (the `_email-extend.less`
//!   idiom, `@baseUrl: "{{base_url_path}}/"` from lib `_email.less`).
//! - **ModuleNotation** converts `Vendor_Module::path/inside/web.ext` urls to
//!   the RELATIVE path from the current css file to that asset's deployed
//!   location in the same (area, theme, locale) context — e.g. from
//!   `Foo_Bar/styles/style.css`, `url(Bar_Baz::images/logo.gif)` becomes
//!   `../../Bar_Baz/images/logo.gif`. URLs without `::` pass through. (No css
//!   on the validated install uses the notation — the processor is part of
//!   the contract but observably a no-op there.)
//!
//! Chain order (AssetPreProcessorPool, module-developer/etc/di.xml:156-193):
//! variable_notation runs BEFORE module_notation; `css_min` follows but is
//! inert with `dev/css/minify_files` off.

/// The relative-URL matcher, `CssResolver::REGEX_CSS_RELATIVE_URLS`:
///
/// ```text
/// #url\s*\(\s*(?(?='|").)(?!http\://|https\://|/|data\:)(.+?)(?:[\#\?].*?|['"])?\s*\)#
/// ```
///
/// Reproduced as a hand scanner (no regex crate): find `url` (lowercase —
/// the PCRE has no `/i`), optional whitespace, `(`, optional whitespace, an
/// optionally consumed single opening quote, then reject absolute paths
/// (`http://`, `https://`, `/`, `data:`), then the lazily-shortest path such
/// that an optional `#`/`?' suffix or closing quote and `\s*)` follow. `.`
/// never matches a newline (no `/s` flag). Returns `(full_match_range,
/// path_range)` pairs in document order.
fn extract_relative_urls(css: &str) -> Vec<(std::ops::Range<usize>, std::ops::Range<usize>)> {
    let b = css.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let is_space = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'\x0b');
    while let Some(at) = css[i..].find("url") {
        let start = i + at;
        // Scan resumes just past the `u` on a failed attempt, like PCRE.
        i = start + 1;
        let mut p = start + 3;
        while p < b.len() && is_space(b[p]) {
            p += 1;
        }
        if p >= b.len() || b[p] != b'(' {
            continue;
        }
        p += 1;
        while p < b.len() && is_space(b[p]) {
            p += 1;
        }
        // `(?(?='|").)` — consume one char iff it is a quote.
        if p < b.len() && (b[p] == b'\'' || b[p] == b'"') {
            p += 1;
        }
        // `(?!http\://|https\://|/|data\:)`
        let rest = &css[p..];
        if rest.starts_with("http://")
            || rest.starts_with("https://")
            || rest.starts_with('/')
            || rest.starts_with("data:")
        {
            continue;
        }
        // `(.+?)(?:[#?].*?|['"])?\s*\)` — smallest end wins (lazy `.+?`);
        // the path itself can never contain a newline.
        let path_start = p;
        let mut end = path_start + 1;
        let mut found: Option<(usize, usize)> = None; // (path_end, match_end)
        'ends: while end <= b.len() {
            if end > path_start && b[end - 1] == b'\n' {
                break; // `.` cannot match \n — no longer path possible
            }
            if end == b.len() {
                break;
            }
            // Try the optional group alternatives at this path end, in the
            // PCRE's order; any success fixes the match.
            let tail = &b[end..];
            // (a) `[#?].*?\s*\)` — lazy suffix up to the first `\s*)`.
            if tail[0] == b'#' || tail[0] == b'?' {
                let mut j = end + 1;
                while j <= b.len() {
                    let mut k = j;
                    while k < b.len() && is_space(b[k]) {
                        k += 1;
                    }
                    if k < b.len() && b[k] == b')' {
                        found = Some((end, k + 1));
                        break 'ends;
                    }
                    if j < b.len() && b[j] == b'\n' {
                        break; // `.*?` cannot cross a newline
                    }
                    j += 1;
                }
            }
            // (b) closing quote, then `\s*\)`.
            if tail[0] == b'\'' || tail[0] == b'"' {
                let mut k = end + 1;
                while k < b.len() && is_space(b[k]) {
                    k += 1;
                }
                if k < b.len() && b[k] == b')' {
                    found = Some((end, k + 1));
                    break 'ends;
                }
            }
            // (c) group skipped: `\s*\)` directly.
            {
                let mut k = end;
                while k < b.len() && is_space(b[k]) {
                    k += 1;
                }
                if k < b.len() && b[k] == b')' {
                    found = Some((end, k + 1));
                    break 'ends;
                }
            }
            end += 1;
        }
        if let Some((path_end, match_end)) = found {
            out.push((start..match_end, path_start..path_end));
            i = match_end;
        }
    }
    out
}

/// `CssResolver::replaceRelativeUrls`, faithfully: extract every relative
/// url, compute one replacement per UNIQUE full match (first callback result
/// wins), then apply all replacements as sequential global string replaces
/// (PHP `str_replace($patterns, $replacements, $content)`).
pub fn replace_relative_urls(css: &str, mut callback: impl FnMut(&str) -> String) -> String {
    let matches = extract_relative_urls(css);
    if matches.is_empty() {
        return css.to_string();
    }
    let mut seen: Vec<(String, String)> = Vec::new();
    for (m, p) in &matches {
        let pattern = &css[m.clone()];
        if seen.iter().any(|(pat, _)| pat == pattern) {
            continue;
        }
        let path = &css[p.clone()];
        let new_path = callback(path);
        // `str_replace($path, $newPath, $pattern)`
        let new_pattern = pattern.replace(path, &new_path);
        seen.push((pattern.to_string(), new_pattern));
    }
    let mut out = css.to_string();
    for (pattern, replacement) in &seen {
        if pattern != replacement {
            out = out.replace(pattern.as_str(), replacement);
        }
    }
    out
}

/// `NotationResolver\Variable::convertVariableNotation` over one url path:
/// every `{{[_a-zA-Z]*}}` placeholder (`VAR_REGEX`, `/i`) is substituted —
/// `base_url_path` (exact, case-sensitive switch) becomes
/// `{{base_url_path}}<area>[/<theme_path>/]{{locale}}`; anything else
/// becomes the empty string.
fn convert_variable_notation(path: &str, area: &str, theme_path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    loop {
        let Some(at) = rest.find("{{") else {
            out.push_str(rest);
            return out;
        };
        let after = &rest[at + 2..];
        let name_len = after
            .bytes()
            .take_while(|c| c.is_ascii_alphabetic() || *c == b'_')
            .count();
        if after[name_len..].starts_with("}}") {
            out.push_str(&rest[..at]);
            let name = &after[..name_len];
            if name == "base_url_path" {
                out.push_str("{{base_url_path}}");
                out.push_str(area);
                if !theme_path.is_empty() {
                    out.push('/');
                    out.push_str(theme_path);
                    out.push('/');
                }
                out.push_str("{{locale}}");
            }
            // any other placeholder → ''
            rest = &after[name_len + 2..];
        } else {
            out.push_str(&rest[..at + 2]);
            rest = &rest[at + 2..];
        }
    }
}

/// The VariableNotation pre-processor pass: rewrite `{{placeholder}}`
/// variables inside every relative url of `css`. `theme_path` is the bare
/// theme path (`Magento/blank`), empty for theme-less contexts.
pub fn variable_notation(css: &str, area: &str, theme_path: &str) -> String {
    if !css.contains("{{") {
        return css.to_string();
    }
    replace_relative_urls(css, |path| convert_variable_notation(path, area, theme_path))
}

/// `NotationResolver\Module::convertModuleNotationToPath` for one url path:
/// no `::` → unchanged; else resolve `Vendor_Module::path` to the relative
/// path from the current css (at package-relative deployed path `css_path`)
/// to the module asset's deployed location `Vendor_Module/path` in the same
/// package. Mirrors `FileSystem::offsetPath` (trim the common leading
/// segments, then one `../` per remaining directory of the css's dir) +
/// `normalizePath(offset . '/' . basename)`.
fn convert_module_notation(path: &str, css_path: &str) -> String {
    let Some((module, rel)) = path.split_once("::") else {
        return path.to_string();
    };
    let related = format!("{module}/{rel}");
    let offset = offset_path(&related, css_path);
    let basename = related.rsplit('/').next().unwrap_or(&related);
    normalize_path(&format!("{offset}/{basename}"))
}

/// PHP `dirname` over a forward-slash path: everything before the last `/`,
/// or `.` when there is none.
fn php_dirname(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) if !dir.is_empty() => dir,
        Some(_) => "/",
        None => ".",
    }
}

/// `FileSystem::offsetPath($relatedPath, $path)`: trim the common LEADING
/// SEGMENTS of both paths (`ltrimSamePart`), then one `../` per directory
/// of `$path`'s remaining dirname, followed by `$relatedPath`'s remaining
/// dirname; trailing `/` trimmed.
fn offset_path(related: &str, path: &str) -> String {
    let related = normalize_path(related);
    let path = normalize_path(path);
    let mut one: Vec<&str> = related.split('/').collect();
    let mut two: Vec<&str> = path.split('/').collect();
    while !one.is_empty() && !two.is_empty() && one[0] == two[0] {
        one.remove(0);
        two.remove(0);
    }
    let two_rest = two.join("/");
    let one_rest = one.join("/");
    let to_dir = php_dirname(&two_rest);
    let offset = if to_dir == "." {
        String::new()
    } else {
        "../".repeat(to_dir.split('/').count())
    };
    let mut out = format!("{offset}{}", php_dirname(&one_rest));
    while out.ends_with('/') {
        out.pop();
    }
    out
}

/// `FileSystem::normalizePath`: drop `.` segments, resolve `name/..` pairs
/// (leading `..`s kept).
fn normalize_path(path: &str) -> String {
    let mut result: Vec<&str> = Vec::new();
    for part in path.split('/') {
        if part == ".." {
            if result.is_empty() || *result.last().unwrap() == ".." {
                result.push(part);
            } else {
                result.pop();
            }
        } else if part != "." {
            result.push(part);
        }
    }
    result.join("/")
}

/// The ModuleNotation pre-processor pass: convert `Module::path` urls of a
/// css deployed at package-relative `css_path` to context-relative paths.
pub fn module_notation(css: &str, css_path: &str) -> String {
    if !css.contains("::") {
        return css.to_string();
    }
    replace_relative_urls(css, |path| convert_module_notation(path, css_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(css: &str) -> Vec<(&str, &str)> {
        extract_relative_urls(css)
            .into_iter()
            .map(|(m, p)| (&css[m], &css[p]))
            .collect()
    }

    #[test]
    fn relative_url_extraction() {
        // quoted, unquoted, whitespace, fragment/query, absolutes excluded
        let css = "a{background:url(img/a.png)} b{background: url( 'b.gif' )}\n\
                   c{src:url(\"f.woff?v=1#x\")} d{x:url(/abs.png)} e{x:url(http://h/i.png)}\n\
                   f{x:url(data:image/png;base64,xx)} g{x:url(https://h/j.png)}";
        let got = urls(css);
        assert_eq!(
            got,
            vec![
                ("url(img/a.png)", "img/a.png"),
                ("url( 'b.gif' )", "b.gif"),
                ("url(\"f.woff?v=1#x\")", "f.woff"),
            ]
        );
        // lowercase only (no /i in the PCRE)
        assert!(urls("a{x:URL(img/a.png)}").is_empty());
    }

    #[test]
    fn replace_keeps_fragment_and_quotes() {
        let css = "a{src:url(\"f.woff?v=1#x\")}";
        let out = replace_relative_urls(css, |p| format!("pre/{p}"));
        assert_eq!(out, "a{src:url(\"pre/f.woff?v=1#x\")}");
    }

    /// The email-inline case, byte-for-byte: the LESS-emitted
    /// `{{base_url_path}}/css/email-fonts.css` url gains the static-path
    /// middle segment while keeping both outer placeholders.
    #[test]
    fn variable_notation_base_url_path() {
        let css = "@import url(\"{{base_url_path}}/css/email-fonts.css\");";
        assert_eq!(
            variable_notation(css, "frontend", "Magento/blank"),
            "@import url(\"{{base_url_path}}frontend/Magento/blank/{{locale}}/css/email-fonts.css\");"
        );
        // theme-less context: no slash-wrapped theme segment
        assert_eq!(
            variable_notation("a{x:url({{base_url_path}}i.png)}", "frontend", ""),
            "a{x:url({{base_url_path}}frontend{{locale}}i.png)}"
        );
    }

    /// Unknown placeholders are dropped (replaced with ''); placeholders
    /// outside urls are untouched.
    #[test]
    fn variable_notation_unknown_and_outside() {
        assert_eq!(
            variable_notation("a{x:url({{foo}}img/a.png)}", "frontend", "T/t"),
            "a{x:url(img/a.png)}"
        );
        let outside = "/* {{base_url_path}} */ a{color:red}";
        assert_eq!(variable_notation(outside, "frontend", "T/t"), outside);
    }

    /// The documented example: from `Foo_Bar/styles/style.css`,
    /// `url(Bar_Baz::images/logo.gif)` → `../../Bar_Baz/images/logo.gif`.
    #[test]
    fn module_notation_relative_paths() {
        assert_eq!(
            module_notation(
                "a{x:url(Bar_Baz::images/logo.gif)}",
                "Foo_Bar/styles/style.css"
            ),
            "a{x:url(../../Bar_Baz/images/logo.gif)}"
        );
        // same module: common prefix trims, one level up from styles/
        assert_eq!(
            module_notation("a{x:url(Foo_Bar::img/i.png)}", "Foo_Bar/styles/style.css"),
            "a{x:url(../img/i.png)}"
        );
        // css at the package root
        assert_eq!(
            module_notation("a{x:url(Foo_Bar::img/i.png)}", "style.css"),
            "a{x:url(Foo_Bar/img/i.png)}"
        );
        // no `::` → untouched
        let plain = "a{x:url(img/i.png)}";
        assert_eq!(module_notation(plain, "Foo_Bar/styles/style.css"), plain);
    }

    /// Duplicate urls replace consistently via the unique-pattern map.
    #[test]
    fn duplicate_patterns_replace_once() {
        let css = "a{x:url(i.png)} b{x:url(i.png)}";
        let out = replace_relative_urls(css, |p| format!("z/{p}"));
        assert_eq!(out, "a{x:url(z/i.png)} b{x:url(z/i.png)}");
    }
}
