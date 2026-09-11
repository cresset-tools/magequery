//! `Magento\Deploy\Package\Processor\PostProcessor\CssUrls` — the deploy-time
//! pass that rewrites a deployed stylesheet's relative `url()`s.
//!
//! It is registered in `module-deploy/etc/di.xml` as a package POST-processor,
//! so it runs only inside `setup:static-content:deploy` — never in a bare
//! `Less_Parser` compile. That is why compiling a materialized entry by hand
//! reproduces the source url verbatim while the deployed file carries a
//! rewritten one, and why this needed finding by elimination: less.php sets
//! `relativeUrls => false`, `ModuleNotation` early-returns for urls without
//! `::`, `VariableNotation` returns unchanged without `{{…}}`, and
//! `MergeStrategy\Direct` needs `dev/css/merge_css_files`.
//!
//! The pass, per css file in the package:
//!
//! 1. resolve each relative url against the css's own directory, package-
//!    relative. If that names a readable file IN THE PACKAGE, leave it alone;
//! 2. otherwise look the normalized id up in the PARENT packages' file maps,
//!    closest ancestor first, matching on the file's name WITHIN ITS MODULE
//!    (`images/attach.png`, not `Vendor_Module/images/attach.png`). A hit rewrites
//!    the url to `../` × (4 + the css directory's depth) followed by that
//!    file's full deployed path;
//! 3. failing that, try `<area>/Magento/base/<locale>/<id>` on disk.
//!
//! The match in step 2 ignores the module, so the first parent-package file
//! with that bare name wins regardless of which module it came from. That is
//! Magento's behaviour, not a simplification here.
//!
//! Note the rewritten target is frequently NOT deployed — the parent packages
//! it names (`base/Magento/base/default`, …) are inheritance sources, not
//! output directories, so the url can 404. We reproduce it because the point
//! is byte-parity with `setup:static-content:deploy`; the dead link is
//! Magento's, and a store already has it.

use std::collections::BTreeMap;

/// `CssUrls::getNormalizedFilePath` — Magento's own `..` collapser, quirks
/// included. NOT posix normalisation:
///
/// - a `..` deletes itself and the part before it BY INDEX, and the index walks
///   backwards past the start, so extra `..`s are simply dropped
///   (`a/../../b.png` → `b.png`, `..` → ``);
/// - `.` is not special (`./a.png` stays `./a.png`);
/// - an empty segment survives (`a//b/../c.png` → `a//c.png`);
/// - any `{{placeholder}}` anywhere switches to a different rule entirely:
///   keep the trailing run of parts up to the last placeholder part
///   (`{{base_url}}/x/a.png` → `x/a.png`, `a/{{x}}/../b.png` → `../b.png`).
///
/// Every case above is pinned in the tests against the real PHP.
pub fn normalized_file_path(url: &str) -> String {
    let parts: Vec<&str> = url.split('/').collect();

    if url.contains("{{") && url.contains("}}") {
        let mut kept: Vec<&str> = Vec::new();
        for part in parts.iter().rev() {
            if part.starts_with("{{") && part.ends_with("}}") && part.len() >= 4 {
                break;
            }
            kept.push(part);
        }
        kept.reverse();
        return kept.join("/");
    }

    // PHP's `foreach` walks a copy, so a part unset by an earlier `..` is still
    // TESTED here — only the output is affected. `prev_index` is signed and may
    // go negative, where the unset is a no-op.
    let mut removed = vec![false; parts.len()];
    let mut prev_index: i64 = 0;
    for (index, part) in parts.iter().enumerate() {
        if *part == ".." {
            removed[index] = true;
            if prev_index >= 0 && (prev_index as usize) < removed.len() {
                removed[prev_index as usize] = true;
            }
            prev_index -= 1;
        } else {
            prev_index = index as i64;
        }
    }
    parts
        .iter()
        .enumerate()
        .filter(|(i, _)| !removed[*i])
        .map(|(_, p)| *p)
        .collect::<Vec<_>>()
        .join("/")
}

/// The directory part of a package-relative path, as PHP's
/// `pathinfo($p, PATHINFO_DIRNAME)` gives it — `.` when there is no slash.
fn dirname(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => ".",
    }
}

/// `'../../../../' . str_repeat('../', count(explode('/', $cssFileBasePath)))`
/// — four fixed levels for `<area>/<Vendor>/<theme>/<locale>`, plus one per
/// segment of the css's own directory. `pathinfo` yields `.` for a file at the
/// package root, which still counts as one segment.
fn up_prefix(css_dir: &str) -> String {
    "../".repeat(4 + css_dir.split('/').count())
}

/// One parent package's files, in the order the package collected them:
/// name-within-module → full deployed path.
pub type ParentFiles = Vec<(String, String)>;

/// The package a stylesheet is being rewritten for, and how to ask whether a
/// path exists — everything `rewrite_url` needs beyond the url itself.
pub struct UrlContext<'a> {
    pub package_path: &'a str,
    pub area: &'a str,
    pub locale: &'a str,
    /// Parent packages, closest ancestor first.
    pub parents: &'a [ParentFiles],
    /// Is this package-relative path a file we deployed?
    pub in_package: &'a dyn Fn(&str) -> bool,
    /// Is this static-root-relative path a file on disk? (step 3)
    pub on_disk: &'a dyn Fn(&str) -> bool,
}

/// What a css file's relative url should become, or `None` to leave it.
pub fn rewrite_url(css_path: &str, url: &str, ctx: &UrlContext) -> Option<String> {
    let UrlContext { package_path, area, locale, parents, in_package, on_disk } = ctx;
    let css_dir = dirname(css_path);

    // Step 1 — already resolves inside this package.
    let within = normalized_file_path(&format!("{package_path}/{css_dir}/{url}"));
    if in_package(&within) {
        return None;
    }

    let lookup = normalized_file_path(&format!("{css_dir}/{url}"));
    if lookup.is_empty() {
        return None;
    }

    // Step 2 — closest ancestor first, matching the bare name within a module.
    for files in parents.iter() {
        if let Some((_, deployed)) = files.iter().find(|(name, _)| *name == lookup) {
            return Some(format!("{}{}", up_prefix(css_dir), deployed));
        }
    }

    // Step 3 — the base-theme package for this area/locale, on disk.
    let in_base = format!("{area}/Magento/base/{locale}/{lookup}");
    if on_disk(&in_base) {
        return Some(format!("{}{}", up_prefix(css_dir), in_base));
    }
    None
}

/// The parent package paths of `<area>/<theme>/<locale>`, CLOSEST ANCESTOR
/// FIRST — the order `getFileFromParent` searches.
///
/// Mirrors `Package::collectParentPaths` (which builds them farthest-first and
/// is reversed twice on the way to the lookup): the package's own theme at the
/// default locale, then each ancestor theme the same way, then `Magento/base`,
/// then the whole ladder again under the `base` area.
pub fn parent_package_paths(
    area: &str,
    theme: &str,
    locale: &str,
    theme_ancestry: &[String],
) -> Vec<String> {
    const BASE_AREA: &str = "base";
    const BASE_THEME: &str = "Magento/base";
    const BASE_LOCALE: &str = "default";

    let mut out: Vec<String> = Vec::new();
    let push = |p: String, out: &mut Vec<String>| {
        if !out.contains(&p) {
            out.push(p);
        }
    };

    for current_area in [area, BASE_AREA] {
        // The theme ladder: this theme, its ancestors, then the base theme.
        let mut themes: Vec<&str> = Vec::new();
        if current_area == area {
            themes.push(theme);
        }
        themes.extend(theme_ancestry.iter().map(String::as_str));
        themes.push(BASE_THEME);

        for t in themes {
            if !(current_area == area && t == theme && locale == BASE_LOCALE) {
                let own = current_area == area && t == theme && locale != BASE_LOCALE;
                if !own {
                    push(format!("{current_area}/{t}/{locale}"), &mut out);
                }
                push(format!("{current_area}/{t}/{BASE_LOCALE}"), &mut out);
            }
        }
        if area == BASE_AREA {
            break;
        }
    }
    // `collectParentPaths` never lists the package itself.
    let own = format!("{area}/{theme}/{locale}");
    out.retain(|p| *p != own);
    // Already closest-first: `collectParentPaths` pushes in that order, and the
    // two `array_reverse`es between it and `getFileFromParent` cancel out.
    out
}

/// Apply a computed url map to one css file's content, the way
/// `CssUrls::updateCssUrls` does: a literal `str_replace` of the url TEXT, so
/// every occurrence in the file changes, not just the one that was resolved.
pub fn apply_url_map(content: &str, map: &BTreeMap<String, String>) -> String {
    let mut out = content.to_owned();
    for (from, to) in map {
        if from != to {
            out = out.replace(from.as_str(), to.as_str());
        }
    }
    out
}

/// Run the pass over one written package, rewriting its css files in place.
///
/// Ordered after `write_package` because that is where Magento runs it: the
/// post-processor reads the deployed file and writes it back, and its
/// "does this resolve?" checks consult the real static tree, so the files have
/// to be there.
///
/// The parent-package file maps are derived from this package's own COPY
/// entries rather than by building the ancestor packages: a module asset at
/// `view/<area>/web/<rel>` belongs to `<area>/Magento/base/default` as
/// `<Module>/<rel>`, and its source path carries the area it came from. That
/// reproduces the ladder for module assets, which is what stylesheets
/// reference. (Limitation: a THEME-owned asset in an ancestor theme package is
/// not modelled, and where two modules deploy the same bare name the entry
/// order decides, as file order does in PHP.)
pub fn process_package(
    files: &[(String, Option<std::path::PathBuf>)],
    package_dir: &std::path::Path,
    static_root: &std::path::Path,
    area: &str,
    theme_path: &str,
    locale: &str,
) -> std::io::Result<usize> {
    let package_path = format!("{area}/{theme_path}/{locale}");

    // Module assets, grouped by the parent package they also belong to, in the
    // ladder's search order.
    let mut by_parent: BTreeMap<String, ParentFiles> = BTreeMap::new();
    for (path, source) in files {
        let Some(source) = source else { continue };
        let Some((module, name)) = path.split_once('/') else { continue };
        if !module.contains('_') {
            continue; // `css/…`, `js/…` — theme files, not a module asset
        }
        let Some(src_area) = source_area(source) else { continue };
        let parent = format!("{src_area}/Magento/base/default");
        by_parent
            .entry(parent.clone())
            .or_default()
            .push((name.to_owned(), format!("{parent}/{module}/{name}")));
    }
    let ladder = parent_package_paths(area, theme_path, locale, &[]);
    let parents: Vec<ParentFiles> = ladder
        .iter()
        .filter_map(|p| by_parent.get(p).cloned())
        .collect();

    let in_package = |p: &str| static_root.join(p).is_file();
    let on_disk = |p: &str| static_root.join(p).is_file();
    let ctx = UrlContext {
        package_path: &package_path,
        area,
        locale,
        parents: &parents,
        in_package: &in_package,
        on_disk: &on_disk,
    };

    let mut rewritten = 0usize;
    for (path, _) in files.iter().filter(|(p, _)| p.ends_with(".css")) {
        let file = package_dir.join(path);
        let Ok(content) = std::fs::read_to_string(&file) else { continue };

        // One decision per DISTINCT url text — `updateCssUrls` then replaces
        // every occurrence of it, so a url resolved once is rewritten wherever
        // it appears in the file.
        let mut map: BTreeMap<String, String> = BTreeMap::new();
        super::cssnotation::replace_relative_urls(&content, |url| {
            if !map.contains_key(url) {
                if let Some(new) = rewrite_url(path, url, &ctx) {
                    map.insert(url.to_owned(), new);
                }
            }
            url.to_owned()
        });
        if map.is_empty() {
            continue;
        }
        let updated = apply_url_map(&content, &map);
        if updated != content {
            std::fs::write(&file, updated)?;
            rewritten += 1;
        }
    }
    Ok(rewritten)
}

/// The view area a module source file sits in (`…/view/<area>/web/…`).
fn source_area(source: &std::path::Path) -> Option<String> {
    let parts: Vec<&str> = source.iter().filter_map(|c| c.to_str()).collect();
    let i = parts.iter().position(|p| *p == "view")?;
    let area = parts.get(i + 1)?;
    (parts.get(i + 2) == Some(&"web")).then(|| (*area).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row was produced by running Magento's own
    /// `getNormalizedFilePath` (see `scripts/`), including the cases that make
    /// it not-posix: `.` untouched, an empty segment kept, `..` walking off the
    /// front, and the placeholder branch.
    #[test]
    fn normalization_matches_magentos_own() {
        let cases = [
            ("css/../../images/attach.png", "images/attach.png"),
            ("Mageants_X/css/source/../../images/a.png", "Mageants_X/images/a.png"),
            ("css/../images/a.png", "images/a.png"),
            ("a/b/c/../../d.png", "a/d.png"),
            ("../a.png", "a.png"),
            ("a/../../b.png", "b.png"),
            ("a/b/../c/../d.png", "a/d.png"),
            ("images/a.png", "images/a.png"),
            ("./a.png", "./a.png"),
            ("a//b/../c.png", "a//c.png"),
            ("{{base_url}}/x/a.png", "x/a.png"),
            ("a/{{x}}/../b.png", "../b.png"),
            ("..", ""),
            ("../..", ""),
            ("a/..", ""),
            ("a/../..", ""),
            ("a/b/../../../c.png", "c.png"),
        ];
        for (input, expected) in cases {
            assert_eq!(normalized_file_path(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn dirname_and_depth_follow_pathinfo() {
        assert_eq!(dirname("css/styles.css"), "css");
        assert_eq!(dirname("Vendor_Mod/css/source/x.css"), "Vendor_Mod/css/source");
        assert_eq!(dirname("styles.css"), ".");
        // Four fixed levels (`<area>/<Vendor>/<theme>/<locale>`) plus the css
        // directory's own depth; a root-level file still counts one for `.`.
        assert_eq!(up_prefix("css"), "../".repeat(5));
        assert_eq!(up_prefix("Vendor_Mod/css/source"), "../".repeat(7));
        assert_eq!(up_prefix("."), "../".repeat(5));
    }

    /// The real case: `css/styles.css` refers to `../../images/attach.png`, which
    /// escapes the package; the file exists in a parent package as
    /// `images/attach.png` under `Acme_Attachment`.
    #[test]
    fn a_url_escaping_the_package_resolves_to_the_parent_packages_copy() {
        let parents = vec![vec![(
            "images/attach.png".to_string(),
            "base/Magento/base/default/Acme_Attachment/images/attach.png".to_string(),
        )]];
        let got = rewrite_url(
            "css/styles.css",
            "../../images/attach.png",
            &UrlContext {
                package_path: "adminhtml/Magento/backend/en_US",
                area: "adminhtml",
                locale: "en_US",
                parents: &parents,
                in_package: &|_| false,
                on_disk: &|_| false,
            },
        );
        assert_eq!(
            got.as_deref(),
            Some("../../../../../base/Magento/base/default/Acme_Attachment/images/attach.png")
        );
    }

    /// Step 1 wins: a url that already resolves inside the package is left
    /// exactly as the stylesheet wrote it.
    #[test]
    fn a_url_resolving_inside_the_package_is_untouched() {
        let parents = vec![vec![(
            "images/a.png".to_string(),
            "base/Magento/base/default/M/images/a.png".to_string(),
        )]];
        let got = rewrite_url(
            "css/styles.css",
            "../images/a.png",
            &UrlContext {
                package_path: "adminhtml/Magento/backend/en_US",
                area: "adminhtml",
                locale: "en_US",
                parents: &parents,
                in_package: &|p| p == "adminhtml/Magento/backend/en_US/images/a.png",
                on_disk: &|_| false,
            },
        );
        assert_eq!(got, None, "an in-package url must not be rewritten");
    }

    /// Step 3: no parent package has it, but the area's base-theme package does
    /// on disk.
    #[test]
    fn the_base_theme_package_is_the_last_resort() {
        let got = rewrite_url(
            "css/styles.css",
            "../../images/a.png",
            &UrlContext {
                package_path: "adminhtml/Magento/backend/en_US",
                area: "adminhtml",
                locale: "en_US",
                parents: &[],
                in_package: &|_| false,
                on_disk: &|p| p == "adminhtml/Magento/base/en_US/images/a.png",
            },
        );
        assert_eq!(
            got.as_deref(),
            Some("../../../../../adminhtml/Magento/base/en_US/images/a.png")
        );
    }

    /// Nothing anywhere: the url stands, rather than being rewritten to a guess.
    #[test]
    fn an_unresolvable_url_is_left_alone() {
        let got = rewrite_url(
            "css/styles.css",
            "../../images/nope.png",
            &UrlContext {
                package_path: "adminhtml/Magento/backend/en_US",
                area: "adminhtml",
                locale: "en_US",
                parents: &[],
                in_package: &|_| false,
                on_disk: &|_| false,
            },
        );
        assert_eq!(got, None);
    }

    /// Closest ancestor wins, which is what `array_reverse(getParentPackages())`
    /// buys — the ladder is searched from the nearest theme outward.
    #[test]
    fn the_closest_parent_package_wins() {
        let parents = vec![
            vec![("i/a.png".into(), "adminhtml/Magento/base/en_US/M/i/a.png".into())],
            vec![("i/a.png".into(), "base/Magento/base/default/M/i/a.png".into())],
        ];
        let got = rewrite_url(
            "css/s.css",
            "../i/a.png",
            &UrlContext {
                package_path: "adminhtml/Magento/backend/en_US",
                area: "adminhtml",
                locale: "en_US",
                parents: &parents,
                in_package: &|_| false,
                on_disk: &|_| false,
            },
        );
        assert!(got.unwrap().ends_with("adminhtml/Magento/base/en_US/M/i/a.png"));
    }

    /// The ladder for a themed, non-default-locale package: every ancestor at
    /// this locale and at `default`, then the base theme, then the whole thing
    /// again under the `base` area — closest first, and never the package
    /// itself.
    #[test]
    fn the_parent_ladder_matches_collect_parent_paths() {
        let got = parent_package_paths("adminhtml", "Magento/backend", "en_US", &[]);
        assert!(!got.contains(&"adminhtml/Magento/backend/en_US".to_string()), "never itself");
        assert_eq!(got.first().unwrap(), "adminhtml/Magento/backend/default");
        assert_eq!(got.last().unwrap(), "base/Magento/base/default");
        for expected in [
            "adminhtml/Magento/backend/default",
            "adminhtml/Magento/base/en_US",
            "adminhtml/Magento/base/default",
            "base/Magento/base/en_US",
            "base/Magento/base/default",
        ] {
            assert!(got.contains(&expected.to_string()), "missing {expected} in {got:?}");
        }
    }

    /// `updateCssUrls` is a literal string replace over the file, so a url that
    /// appears twice is rewritten twice.
    #[test]
    fn the_url_map_is_applied_literally_everywhere() {
        let mut map = BTreeMap::new();
        map.insert("../a.png".to_string(), "../../b.png".to_string());
        let css = ".x{background:url(../a.png)}.y{background:url(../a.png)}";
        assert_eq!(
            apply_url_map(css, &map),
            ".x{background:url(../../b.png)}.y{background:url(../../b.png)}"
        );
    }
}
