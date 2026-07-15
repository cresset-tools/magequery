//! Port of the `laminas/laminas-zendframework-bridge` namespace rewrite.
//!
//! Magento ships that bridge, whose *append* autoloader
//! (`Autoloader::createAppendAutoloader`) resolves a legacy `Zend\…` (or `ZF\…`,
//! `ZendXml\…`, …) class name to the real `Laminas\…` class and registers it via
//! `class_alias($laminasClass, $legacyName)`. Because the legacy name becomes an
//! alias, PHP reflection reports its type as the *canonical* Laminas class — so
//! Magento's code generator, reflecting a constructor parameter hinted
//! `\Zend\Uri\Uri`, emits `\Laminas\Uri\Uri`. A static analyzer that copies the
//! source token verbatim would diverge; [`canonical`] reproduces the rewrite so
//! callers can resolve a legacy name to what reflection would return.
//!
//! The bridge only aliases when the target actually exists, so callers must gate
//! the result on the canonical class being present in their class universe (a
//! legacy name whose Laminas counterpart is absent stays legacy). This module is
//! the pure name transform; existence gating is the caller's job.

/// `RewriteRules::namespaceRewrite()` — (legacy prefix, canonical prefix), each
/// key a full namespace segment ending in `\`. The bridge greedily matches the
/// longest segment-prefix; since every key ends at a `\` boundary, a plain
/// longest-string-prefix scan is equivalent (and, unlike the bridge's segment
/// walk, needs no special case for the two-segment `ZendService\…` keys).
const NAMESPACE_REWRITE: &[(&str, &str)] = &[
    // Expressive (must beat the bare `Zend\` prefix).
    ("Zend\\ProblemDetails\\", "Mezzio\\ProblemDetails\\"),
    ("Zend\\Expressive\\", "Mezzio\\"),
    // ZendService wrappers (two-segment keys).
    ("ZendService\\ReCaptcha\\", "Laminas\\ReCaptcha\\"),
    ("ZendService\\Twitter\\", "Laminas\\Twitter\\"),
    // Apigility / ZfCampus (specific `ZF\` keys must beat bare `ZF\`).
    ("ZF\\Apigility\\", "Laminas\\ApiTools\\"),
    ("ZF\\ComposerAutoloading\\", "Laminas\\ComposerAutoloading\\"),
    ("ZF\\DevelopmentMode\\", "Laminas\\DevelopmentMode\\"),
    // Standalone components.
    ("ZendXml\\", "Laminas\\Xml\\"),
    ("ZendOAuth\\", "Laminas\\OAuth\\"),
    ("ZendDiagnostics\\", "Laminas\\Diagnostics\\"),
    ("ZendDeveloperTools\\", "Laminas\\DeveloperTools\\"),
    // Bare catch-alls (shortest — tried last).
    ("ZF\\", "Laminas\\ApiTools\\"),
    ("Zend\\", "Laminas\\"),
];

/// The append autoloader's suffix `strtr` map, applied to the portion *after*
/// the matched prefix. Ordered longest-key-first so the identity guards
/// (`AbstractZendServer`, `ZendServer*`, `ZendMonitor`) win over the bare `Zend`
/// rule and are left untouched, exactly as PHP's `strtr` (longest key wins).
const SUFFIX_STRTR: &[(&str, &str)] = &[
    ("AbstractZendServer", "AbstractZendServer"),
    ("ZendServerDisk", "ZendServerDisk"),
    ("ZendServerShm", "ZendServerShm"),
    ("ZendMonitor", "ZendMonitor"),
    ("Expressive", "Mezzio"),
    ("Apigility", "ApiTools"),
    ("Zend", "Laminas"),
];

/// Resolve a legacy `Zend\…`-family name to the canonical `Laminas\…` name the
/// bridge would alias it to, or `None` when no rewrite prefix applies (the name
/// is not a legacy name, so reflection returns it unchanged).
///
/// This is a pure name transform. It does **not** check that the canonical class
/// exists — the bridge aliases only when it does, so callers must confirm the
/// result is a real class before substituting it (see the module docs).
pub fn canonical(name: &str) -> Option<String> {
    let bare = name.strip_prefix('\\').unwrap_or(name);
    // Longest prefix first: NAMESPACE_REWRITE is ordered so the first match is
    // the longest applicable one.
    for (legacy, canonical) in NAMESPACE_REWRITE {
        if let Some(rest) = bare.strip_prefix(legacy) {
            let rewritten = format!("{canonical}{}", strtr(rest, SUFFIX_STRTR));
            // Preserve the caller's leading-backslash convention.
            return Some(if name.starts_with('\\') {
                format!("\\{rewritten}")
            } else {
                rewritten
            });
        }
    }
    None
}

/// PHP `strtr($str, $pairs)`: scan left to right; at each position substitute the
/// longest matching key (keys never re-scanned), else copy one byte. `pairs` must
/// be ordered longest-key-first. Class names are ASCII, so byte indexing is safe.
fn strtr(s: &str, pairs: &[(&str, &str)]) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    'outer: while i < bytes.len() {
        for (from, to) in pairs {
            if bytes[i..].starts_with(from.as_bytes()) {
                out.push_str(to);
                i += from.len();
                continue 'outer;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_the_common_zend_prefix() {
        assert_eq!(canonical("Zend\\Uri\\Uri").as_deref(), Some("Laminas\\Uri\\Uri"));
        // Leading-backslash convention is preserved.
        assert_eq!(canonical("\\Zend\\Uri\\Uri").as_deref(), Some("\\Laminas\\Uri\\Uri"));
    }

    #[test]
    fn longest_prefix_wins() {
        assert_eq!(canonical("Zend\\Expressive\\Router").as_deref(), Some("Mezzio\\Router"));
        assert_eq!(
            canonical("Zend\\ProblemDetails\\Response").as_deref(),
            Some("Mezzio\\ProblemDetails\\Response")
        );
    }

    #[test]
    fn standalone_and_service_components() {
        assert_eq!(canonical("ZendXml\\Security").as_deref(), Some("Laminas\\Xml\\Security"));
        assert_eq!(
            canonical("ZendService\\ReCaptcha\\ReCaptcha").as_deref(),
            Some("Laminas\\ReCaptcha\\ReCaptcha")
        );
    }

    #[test]
    fn apigility_specific_beats_bare_zf() {
        assert_eq!(canonical("ZF\\Apigility\\Admin").as_deref(), Some("Laminas\\ApiTools\\Admin"));
        assert_eq!(canonical("ZF\\Rest\\Resource").as_deref(), Some("Laminas\\ApiTools\\Rest\\Resource"));
    }

    #[test]
    fn suffix_strtr_rewrites_embedded_zend_but_guards_zendserver() {
        // `Zend` embedded in the suffix is rewritten...
        assert_eq!(canonical("Zend\\Foo\\ZendThing").as_deref(), Some("Laminas\\Foo\\LaminasThing"));
        // ...but the identity-guarded ZendServer names are left intact.
        assert_eq!(
            canonical("Zend\\Server\\AbstractZendServer").as_deref(),
            Some("Laminas\\Server\\AbstractZendServer")
        );
    }

    #[test]
    fn non_legacy_names_are_left_alone() {
        assert_eq!(canonical("Magento\\Framework\\App"), None);
        assert_eq!(canonical("Laminas\\Uri\\Uri"), None);
        // A class literally named with a `Zend` segment but no rewrite prefix.
        assert_eq!(canonical("Acme\\Zend\\Thing"), None);
    }
}
