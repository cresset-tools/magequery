//! `requirejs-config.js` aggregation — the Magento side of the JS half of
//! `setup:static-content:deploy`, reproduced in pure Rust (no PHP, no node).
//!
//! The mechanism is a **textual concatenation, not a semantic JS merge**:
//! `Magento\Framework\RequireJs\Config::getConfig()` collects every
//! `requirejs-config.js` the theme can see, wraps each file's RAW content in
//! an IIFE, concatenates them in collector order, and wraps the whole thing
//! in one outer IIFE. Nothing is parsed — a later file's `config` object
//! simply calls `require.config()` again at runtime. So no JS parser is
//! needed here, and none is used.
//!
//! ## The two templates (verbatim from `RequireJs/Config.php`)
//!
//! Each source file goes through [`PARTIAL_CONFIG_TEMPLATE`] and the
//! concatenation through [`FULL_CONFIG_TEMPLATE`].
//!
//! **The `%usages%` quirk, reproduced faithfully:** the framework does
//!
//! ```php
//! str_replace(['%function%', '%usages%'], [$distributedConfig], TEMPLATE)
//! ```
//!
//! — a 2-element search array against a 1-element replace array, so PHP
//! substitutes the *missing* second replacement with the empty string:
//! `%usages%` is replaced by NOTHING. The result is the golden's tail —
//! `})();`, three blank lines, `})(require);` — and no trailing newline.
//! This is a bug upstream has shipped since 2014; the byte-exact gate
//! requires reproducing it, so [`FULL_CONFIG_TEMPLATE`] is applied with an
//! empty `%usages%` on purpose.
//!
//! ## Collector order (`RequireJs\Config\File\Collector\Aggregated`)
//!
//! `getFiles($theme, 'requirejs-config.js')` yields, in this order:
//!
//! 1. `lib/web/requirejs-config.js`, if it exists;
//! 2. `baseFiles` — the module-context files. Wired in
//!    `module-require-js/etc/di.xml` as
//!    `ModuleDependency(ModuleOutput(Collector\Base))`: `Base` collects every
//!    registered module's `view/base/…` (all modules) and then every module's
//!    `view/<area>/…`, `ModuleOutput` drops disabled modules, and
//!    `ModuleDependency` sorts by the module's index in
//!    `ModuleListInterface::getNames()` — i.e. `config.php` load order — with
//!    ties broken by `basename()`. Every basename here is the same
//!    (`requirejs-config.js`), so the tie is a no-op and PHP 8's **stable**
//!    `usort` preserves the collection order: for a module shipping both, its
//!    `view/base` file precedes its `view/<area>` file. [`collect_files`]
//!    therefore emits `base` then `<area>` per module in load order, which is
//!    the same sequence without needing the sort;
//! 3. for each theme in `$theme->getInheritedThemes()` — **ancestor-first,
//!    self last** (`Theme::getInheritedThemes` walks up `getParentTheme` and
//!    `array_reverse`s) — first that theme's `themeModularFiles`
//!    (`<theme>/<Vendor_Module>/requirejs-config.js`, also decorated with
//!    `ModuleDependency(ModuleOutput(…))`, so: enabled modules only, in
//!    `config.php` load order), then its `themeFiles` (the theme's own
//!    `<theme>/requirejs-config.js`, undecorated).
//!
//! Note the collector's path has NO `web/` segment: a module's
//! `view/frontend/web/requirejs-config.js` is NOT collected (one core module
//! ships exactly that and it is correctly absent from the deployed output).
//!
//! ## Out of scope here
//!
//! `Config::getConfig()` runs the result through the JS minifier when
//! `Minification::isEnabled('js')` (production mode, `.min.js` naming). This
//! module always emits the unminified form — the default-mode artifact, and
//! the one the goldens capture.
//!
//! Everything is pure file inspection over a plain Magento source tree; the
//! theme fallback chain and the enabled-module load order are reused from
//! [`super::less`] (`theme_chain`, `ThemeRef`, `ModuleRef`).

use std::path::{Path, PathBuf};

use super::less::{is_module_segment, theme_chain, ModuleRef, ThemeRef};

/// Faults reuse the static-deploy error shape (theme/module/file provenance +
/// message), so both `static` subcommands report identically.
pub type RequireJsError = super::less::LessDeployError;

/// The collected file name — `RequireJs\Config::CONFIG_FILE_NAME`.
pub const CONFIG_FILE_NAME: &str = "requirejs-config.js";

/// `RequireJs\Config::PARTIAL_CONFIG_TEMPLATE` — wraps EACH source file's raw
/// content. The heredoc's trailing blank line contributes the final `\n`.
pub const PARTIAL_CONFIG_TEMPLATE: &str =
    "(function() {\n%config%\nrequire.config(config);\n})();\n";

/// `RequireJs\Config::FULL_CONFIG_TEMPLATE` — wraps the concatenation.
/// `%usages%` is always replaced with the empty string (see the module docs).
pub const FULL_CONFIG_TEMPLATE: &str = "(function(require){\n%function%\n\n%usages%\n})(require);";

/// Which collector layer produced a source file — the provenance the CLI
/// prints, and what the ordering rule is expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequireJsOrigin {
    /// `lib/web/requirejs-config.js` (the library).
    Library,
    /// A module's `view/base/requirejs-config.js`.
    ModuleBase,
    /// A module's `view/<area>/requirejs-config.js`.
    ModuleArea,
    /// A theme's module context: `<theme>/<Vendor_Module>/requirejs-config.js`.
    ThemeModular,
    /// A theme's own `<theme>/requirejs-config.js`.
    ThemeOwn,
}

impl RequireJsOrigin {
    /// A short tag for CLI output.
    pub fn tag(self) -> &'static str {
        match self {
            RequireJsOrigin::Library => "lib",
            RequireJsOrigin::ModuleBase => "module:base",
            RequireJsOrigin::ModuleArea => "module:area",
            RequireJsOrigin::ThemeModular => "theme:module",
            RequireJsOrigin::ThemeOwn => "theme",
        }
    }
}

/// One collected `requirejs-config.js`, in collector order.
#[derive(Debug, Clone)]
pub struct RequireJsSource {
    /// The physical file.
    pub file: PathBuf,
    /// The owning `Vendor_Module`, when the file has a module context.
    pub module: Option<String>,
    /// The theme that contributed it, for the theme layers.
    pub theme: Option<String>,
    /// The collector layer.
    pub origin: RequireJsOrigin,
}

/// One assembled `requirejs-config.js`.
#[derive(Debug)]
pub struct RequireJsConfig {
    /// The child-first fallback chain the assembly ran against (for reporting;
    /// the collector itself walks it ancestor-first).
    pub chain: Vec<ThemeRef>,
    /// The source files, in the exact order they were concatenated.
    pub sources: Vec<RequireJsSource>,
    /// The assembled config.
    pub js: String,
}

/// Collect every `requirejs-config.js` a theme sees, in collector order (see
/// the module docs). `chain` is the CHILD-first fallback chain from
/// [`super::less::theme_chain`]; `modules` are the enabled modules in
/// `config.php` load order. Pure file inspection — nothing is read.
pub fn collect_files(
    root: &Path,
    area: &str,
    chain: &[ThemeRef],
    modules: &[ModuleRef],
) -> Vec<RequireJsSource> {
    let mut out: Vec<RequireJsSource> = Vec::new();

    // 1. the library file.
    let lib = root.join("lib").join("web").join(CONFIG_FILE_NAME);
    if lib.is_file() {
        out.push(RequireJsSource {
            file: lib,
            module: None,
            theme: None,
            origin: RequireJsOrigin::Library,
        });
    }

    // 2. module contexts: `view/base` then `view/<area>`, load order.
    for m in modules {
        for (sub, origin) in [
            ("base", RequireJsOrigin::ModuleBase),
            (area, RequireJsOrigin::ModuleArea),
        ] {
            let f = m.dir.join("view").join(sub).join(CONFIG_FILE_NAME);
            if f.is_file() {
                out.push(RequireJsSource {
                    file: f,
                    module: Some(m.name.clone()),
                    theme: None,
                    origin,
                });
            }
        }
    }

    // 3. the theme layers, ANCESTOR-first (the chain is child-first).
    for t in chain.iter().rev() {
        // 3a. `<theme>/<Vendor_Module>/requirejs-config.js` — enabled modules
        //     only (ModuleOutput), in `config.php` load order (ModuleDependency).
        let mut modular: Vec<(usize, &ModuleRef, PathBuf)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&t.dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !is_module_segment(&name) {
                    continue;
                }
                let f = e.path().join(CONFIG_FILE_NAME);
                if !f.is_file() {
                    continue;
                }
                if let Some(i) = modules.iter().position(|m| m.name == name) {
                    modular.push((i, &modules[i], f));
                }
            }
        }
        modular.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
        for (_, m, f) in modular {
            out.push(RequireJsSource {
                file: f,
                module: Some(m.name.clone()),
                theme: Some(t.id.clone()),
                origin: RequireJsOrigin::ThemeModular,
            });
        }

        // 3b. the theme's own file.
        let own = t.dir.join(CONFIG_FILE_NAME);
        if own.is_file() {
            out.push(RequireJsSource {
                file: own,
                module: None,
                theme: Some(t.id.clone()),
                origin: RequireJsOrigin::ThemeOwn,
            });
        }
    }

    out
}

/// Read the collected sources and assemble the final config — the textual
/// half of `Config::getConfig()`: each file's raw content into
/// [`PARTIAL_CONFIG_TEMPLATE`], the concatenation into
/// [`FULL_CONFIG_TEMPLATE`] with an EMPTY `%usages%`.
pub fn assemble(sources: &[RequireJsSource]) -> Result<String, RequireJsError> {
    let mut distributed = String::new();
    for s in sources {
        let content = std::fs::read(&s.file).map_err(|e| RequireJsError {
            entry: Some(CONFIG_FILE_NAME.to_string()),
            module: s.module.clone(),
            file: Some(s.file.clone()),
            message: format!("read failed: {e}"),
        })?;
        let content = String::from_utf8(content).map_err(|_| RequireJsError {
            entry: Some(CONFIG_FILE_NAME.to_string()),
            module: s.module.clone(),
            file: Some(s.file.clone()),
            message: "not valid UTF-8".to_string(),
        })?;
        distributed.push_str(&PARTIAL_CONFIG_TEMPLATE.replace("%config%", &content));
    }
    Ok(FULL_CONFIG_TEMPLATE
        .replace("%function%", &distributed)
        .replace("%usages%", ""))
}

/// Assemble a theme's `requirejs-config.js` from raw parts (no `Magento`
/// handle — tests and callers with their own discovery use this).
pub fn build(
    root: &Path,
    area: &str,
    theme_id: &str,
    themes: &[(String, PathBuf)],
    modules: &[ModuleRef],
) -> Result<RequireJsConfig, RequireJsError> {
    let chain = theme_chain(area, theme_id, themes)?;
    let sources = collect_files(root, area, &chain, modules);
    let js = assemble(&sources)?;
    Ok(RequireJsConfig { chain, sources, js })
}

/// Assemble from an open `magequery-core` handle: themes from
/// `Magento::themes()`, enabled modules in `config.php` load order from
/// `Magento::modules()` — the same discovery `static less` uses.
pub fn build_from_magento(
    magento: &magequery_core::Magento,
    area: &str,
    theme_id: &str,
) -> Result<RequireJsConfig, RequireJsError> {
    let themes = magento.themes();
    let modules: Vec<ModuleRef> = magento
        .modules()
        .iter()
        .filter(|m| m.enabled)
        .map(|m| ModuleRef {
            name: m.name.to_string(),
            dir: m.path.clone(),
        })
        .collect();
    build(magento.root(), area, theme_id, &themes, &modules)
}

/// The `pub/static` placement — `Config::getConfigFileRelativePath()`, i.e.
/// `pub/static/<area>/<Vendor>/<theme>/<locale>/requirejs-config.js`.
pub fn output_path(root: &Path, area: &str, theme_id: &str, locale: &str) -> PathBuf {
    let theme_path = theme_id
        .strip_prefix(&format!("{area}/"))
        .unwrap_or(theme_id);
    root.join("pub")
        .join("static")
        .join(area)
        .join(theme_path)
        .join(locale)
        .join(CONFIG_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic Magento-shaped tree exercising every collector layer:
    /// the library file, a module with BOTH `view/base` and `view/frontend`,
    /// a module whose file the base theme overrides, a disabled module the
    /// `ModuleOutput` decorator must drop, a theme-own file on each theme,
    /// and a child theme that overrides a module context.
    fn synth_tree() -> tempfile::TempDir {
        let td = tempfile::tempdir().expect("tempdir");
        let r = td.path();
        let w = |rel: &str, content: &str| {
            let p = r.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        };

        w("lib/web/requirejs-config.js", "LIB\n");

        // Acme_Base ships base + frontend; Acme_Widgets frontend only.
        w("vendor/acme/module-base/view/base/requirejs-config.js", "BASE-base\n");
        w("vendor/acme/module-base/view/frontend/requirejs-config.js", "BASE-frontend\n");
        w("vendor/acme/module-widgets/view/frontend/requirejs-config.js", "WIDGETS\n");
        // Under `web/` — NOT a collector path (Magento globs `view/<area>/<file>`).
        w("vendor/acme/module-widgets/view/frontend/web/requirejs-config.js", "IGNORED\n");
        // A module absent from the enabled list: ModuleOutput drops its theme context.
        w("vendor/acme/module-off/view/frontend/requirejs-config.js", "OFF\n");

        // Base theme: one module context + its own file.
        w("vendor/acme/theme-base/theme.xml", "<theme><title>Base</title></theme>\n");
        w("vendor/acme/theme-base/Acme_Widgets/requirejs-config.js", "THEME-BASE/Acme_Widgets\n");
        w("vendor/acme/theme-base/Acme_Off/requirejs-config.js", "THEME-BASE/Acme_Off\n");
        w("vendor/acme/theme-base/requirejs-config.js", "THEME-BASE-own\n");

        // Child theme: overrides the same module context + its own file.
        w(
            "vendor/acme/theme-child/theme.xml",
            "<theme><title>Child</title><parent>Acme/base</parent></theme>\n",
        );
        w("vendor/acme/theme-child/Acme_Widgets/requirejs-config.js", "THEME-CHILD/Acme_Widgets\n");
        w("vendor/acme/theme-child/requirejs-config.js", "THEME-CHILD-own\n");

        td
    }

    fn themes(root: &Path) -> Vec<(String, PathBuf)> {
        vec![
            ("frontend/Acme/base".to_string(), root.join("vendor/acme/theme-base")),
            ("frontend/Acme/child".to_string(), root.join("vendor/acme/theme-child")),
        ]
    }

    /// `config.php` load order — Widgets deliberately BEFORE Base, so a
    /// module-order bug can't hide behind alphabetical order.
    fn modules(root: &Path) -> Vec<ModuleRef> {
        vec![
            ModuleRef {
                name: "Acme_Widgets".to_string(),
                dir: root.join("vendor/acme/module-widgets"),
            },
            ModuleRef {
                name: "Acme_Base".to_string(),
                dir: root.join("vendor/acme/module-base"),
            },
        ]
    }

    fn built(root: &Path, theme: &str) -> RequireJsConfig {
        build(root, "frontend", theme, &themes(root), &modules(root)).expect("build")
    }

    #[test]
    fn order_is_lib_then_modules_then_theme_layers_ancestor_first() {
        let td = synth_tree();
        let cfg = built(td.path(), "Acme/child");
        let got: Vec<(&str, String)> = cfg
            .sources
            .iter()
            .map(|s| {
                (
                    s.origin.tag(),
                    // Windows renders native separators — normalize so the
                    // assertion is separator-agnostic.
                    s.file
                        .strip_prefix(td.path())
                        .unwrap()
                        .display()
                        .to_string()
                        .replace('\\', "/"),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("lib", "lib/web/requirejs-config.js".to_string()),
                // config.php load order: Widgets first, then Base — and within
                // Acme_Base, `view/base` before `view/frontend`.
                ("module:area", "vendor/acme/module-widgets/view/frontend/requirejs-config.js".into()),
                ("module:base", "vendor/acme/module-base/view/base/requirejs-config.js".into()),
                ("module:area", "vendor/acme/module-base/view/frontend/requirejs-config.js".into()),
                // Ancestor theme first, its module contexts before its own file.
                ("theme:module", "vendor/acme/theme-base/Acme_Widgets/requirejs-config.js".into()),
                ("theme", "vendor/acme/theme-base/requirejs-config.js".into()),
                ("theme:module", "vendor/acme/theme-child/Acme_Widgets/requirejs-config.js".into()),
                ("theme", "vendor/acme/theme-child/requirejs-config.js".into()),
            ]
        );
    }

    /// A theme context for a module the enabled list doesn't carry is dropped
    /// by the `ModuleOutput` decorator; a `view/<area>/web/` file is not a
    /// collector path at all.
    fn assert_dropped(cfg: &RequireJsConfig) {
        assert!(
            !cfg.js.contains("THEME-BASE/Acme_Off"),
            "disabled module's theme context leaked:\n{}",
            cfg.js
        );
        assert!(!cfg.js.contains("OFF\n"), "disabled module leaked");
        assert!(!cfg.js.contains("IGNORED"), "view/<area>/web/ file leaked");
    }

    #[test]
    fn disabled_modules_and_non_collector_paths_are_excluded() {
        let td = synth_tree();
        assert_dropped(&built(td.path(), "Acme/child"));
        assert_dropped(&built(td.path(), "Acme/base"));
    }

    /// A theme override does NOT replace the module's own file — the
    /// aggregator concatenates both, the later one winning at RUNTIME by
    /// calling `require.config()` again. (This is where requirejs differs from
    /// the LESS `//@magento_import` collator, which removes-and-reappends.)
    #[test]
    fn theme_override_appends_after_the_module_file() {
        let td = synth_tree();
        let cfg = built(td.path(), "Acme/child");
        let module_at = cfg.js.find("WIDGETS\n").expect("module file present");
        let base_at = cfg.js.find("THEME-BASE/Acme_Widgets").expect("base override");
        let child_at = cfg.js.find("THEME-CHILD/Acme_Widgets").expect("child override");
        assert!(module_at < base_at && base_at < child_at, "{}", cfg.js);
    }

    /// Each source is wrapped in its own IIFE with a `require.config(config);`
    /// call, and the whole thing in the outer `(function(require){…})(require);`.
    #[test]
    fn iife_wrapping_per_file_and_overall() {
        let td = synth_tree();
        let cfg = built(td.path(), "Acme/base");
        assert!(cfg.js.starts_with("(function(require){\n(function() {\nLIB\n\nrequire.config(config);\n})();\n"));
        assert_eq!(
            cfg.js.matches("require.config(config);").count(),
            cfg.sources.len()
        );
        assert_eq!(cfg.js.matches("(function() {").count(), cfg.sources.len());
    }

    /// The `%usages%` quirk: `str_replace` with a 2-element search array and a
    /// 1-element replace array blanks `%usages%`, so the tail is `})();`, three
    /// blank lines, `})(require);` — and NO trailing newline.
    #[test]
    fn usages_placeholder_is_replaced_with_nothing() {
        let td = synth_tree();
        let cfg = built(td.path(), "Acme/base");
        assert!(!cfg.js.contains("%usages%"));
        assert!(!cfg.js.contains("%function%"));
        assert!(
            cfg.js.ends_with("})();\n\n\n\n})(require);"),
            "tail: {:?}",
            &cfg.js[cfg.js.len() - 40..]
        );
    }

    /// Nothing on disk is not an error: an empty collection still produces the
    /// well-formed outer wrapper (what a bare theme with no JS deploys).
    #[test]
    fn missing_files_are_tolerated() {
        let td = tempfile::tempdir().unwrap();
        let r = td.path();
        std::fs::create_dir_all(r.join("vendor/acme/theme-base")).unwrap();
        std::fs::write(r.join("vendor/acme/theme-base/theme.xml"), "<theme/>\n").unwrap();
        let themes = vec![(
            "frontend/Acme/base".to_string(),
            r.join("vendor/acme/theme-base"),
        )];
        let modules = vec![ModuleRef {
            name: "Acme_Ghost".to_string(),
            dir: r.join("vendor/acme/module-ghost"),
        }];
        let cfg = build(r, "frontend", "Acme/base", &themes, &modules).expect("build");
        assert!(cfg.sources.is_empty());
        assert_eq!(cfg.js, "(function(require){\n\n\n\n})(require);");
    }

    #[test]
    fn output_path_is_the_locale_root() {
        let p = output_path(Path::new("/srv/m2"), "frontend", "Magento/luma", "en_US");
        assert_eq!(
            p,
            PathBuf::from("/srv/m2/pub/static/frontend/Magento/luma/en_US/requirejs-config.js")
        );
        // An area-qualified id resolves to the same place.
        assert_eq!(
            output_path(Path::new("/srv/m2"), "frontend", "frontend/Magento/luma", "en_US"),
            p
        );
    }
}
