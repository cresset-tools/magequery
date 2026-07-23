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
//! ## The sibling artifacts (same deploy step, same subsystem)
//!
//! Alongside the config, the real deploy places two more files per theme
//! package, both produced here:
//!
//! - **`requirejs-min-resolver.js`** — `Config::getMinResolverCode()`: a fixed
//!   IIFE template whose ONLY variable part is the exclude condition. It
//!   starts as `url.indexOf(baseUrl)===0` and appends one
//!   `!url.match(/<regex>/)` per exclude from
//!   `Minification::getExcludes('js')` (config path `dev/js/minify_exclude`),
//!   joined with `&&`. Each exclude value becomes a JS regex literal with only
//!   `/` escaped to `\/` (`str_replace('/', '\/', $expression)`) — nothing
//!   else is escaped. The PHP's `empty($excludes) ? 'true'` fallback is DEAD
//!   CODE (the `indexOf` entry is always present); [`min_resolver_code`]
//!   reproduces the live branch only. See [`min_resolver_excludes`] for where
//!   the exclude values come from.
//! - **`mage/requirejs/mixins.js`** — a verbatim byte copy of
//!   `lib/web/mage/requirejs/mixins.js` into the theme package
//!   (`Config::MIXINS_FILE_NAME`; the deploy pipeline treats it as a plain
//!   static file, no processing). [`mixins_source_path`] names the source.
//!
//! ## Out of scope here
//!
//! `Config::getConfig()` (and `getMinResolverCode()`) run the result through
//! the JS minifier when `Minification::isEnabled('js')` (production mode,
//! `.min.js` naming). This module always emits the unminified form — the
//! default-mode artifact, and the one the goldens capture.
//!
//! Everything is pure file inspection over a plain Magento source tree; the
//! theme fallback chain and the enabled-module load order are reused from
//! [`super::less`] (`theme_chain`, `ThemeRef`, `ModuleRef`), and the
//! `dev/js/minify_exclude` values from `magequery-core`'s [`ConfigSet`]
//! (module `config.xml` `<default>` merge — no new config reader).

use std::path::{Path, PathBuf};

use magequery_core::ConfigSet;

use super::less::{is_module_segment, theme_chain, ModuleRef, ThemeRef};

/// Faults reuse the static-deploy error shape (theme/module/file provenance +
/// message), so both `static` subcommands report identically.
pub type RequireJsError = super::less::LessDeployError;

/// The collected file name — `RequireJs\Config::CONFIG_FILE_NAME`.
pub const CONFIG_FILE_NAME: &str = "requirejs-config.js";

/// The min-resolver's file name — `RequireJs\Config::MIN_RESOLVER_FILENAME`.
/// Deployed as a sibling of [`CONFIG_FILE_NAME`] in the theme package.
pub const MIN_RESOLVER_FILE_NAME: &str = "requirejs-min-resolver.js";

/// The mixins file's deployed RELATIVE path within the theme package —
/// `RequireJs\Config::MIXINS_FILE_NAME`.
pub const MIXINS_FILE_NAME: &str = "mage/requirejs/mixins.js";

/// The config path whose values become the min-resolver's exclude regexes —
/// `Minification::XML_PATH_MINIFICATION_EXCLUDES` with `%s` = `js`.
pub const MINIFY_EXCLUDE_PATH: &str = "dev/js/minify_exclude";

/// `Config::getMinResolverCode()`'s heredoc, verbatim: 4-space base indent
/// (it sits inside a PHP method), NO trailing newline (heredocs drop the
/// newline before the closing identifier). `%excludes%` stands in for the
/// PHP's `{$excludesCode}` interpolation.
pub const MIN_RESOLVER_TEMPLATE: &str = r"    (function () {
        var ctx = require.s.contexts._,
            origNameToUrl = ctx.nameToUrl,
            baseUrl = ctx.config.baseUrl;

        ctx.nameToUrl = function() {
            var url = origNameToUrl.apply(ctx, arguments);
            if (%excludes%) {
                url = url.replace(/(\.min)?\.js$/, '.min.js');
            }
            return url;
        };
    })();";

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

/// The `dev/js/minify_exclude` values, in Magento's merged order — the input
/// to [`min_resolver_code`]. Mirrors `Minification::getExcludes('js')` +
/// `getMinificationExcludeValues` over the STATIC config sources:
///
/// - **Array form** (how every module declares it): each child of
///   `<minify_exclude>` in a module's `config.xml` `<default>` section is one
///   exclude, keyed by its element name (the key is only a merge identity —
///   the VALUE is the regex). Magento's DOM merge orders keys by first
///   declaration across module load order; the flattened [`ConfigSet`] loses
///   insertion order, so the order is reconstructed as (declaring module's
///   load-order position, line). Identical for distinct keys — the normal
///   case. (Known limitation: a later module OVERRIDING another module's key
///   would move it to the overrider's position, where Magento keeps the
///   original slot. Values not declared by any module file — env.php
///   `system`, `CONFIG__*` — sort last, by path.)
/// - **Legacy string form**: the value used to be ONE newline-separated
///   string; `getMinificationExcludeValues` still splits such a value. A
///   scalar at the exact path (e.g. from env.php's `system` node, which
///   `array_replace_recursive`s over the array wholesale) takes that branch.
///
/// Every value is trimmed; empties dropped — both PHP loops do exactly that.
pub fn min_resolver_excludes(config: &ConfigSet, modules: &[ModuleRef]) -> Vec<String> {
    let leaves = config.section("default", MINIFY_EXCLUDE_PATH);

    if let Some(legacy) = leaves.iter().find(|v| v.path == MINIFY_EXCLUDE_PATH) {
        return legacy
            .value
            .split('\n')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }

    let mut ordered: Vec<(usize, u32, &str, &str)> = leaves
        .iter()
        .map(|v| {
            let pos = v
                .file
                .as_deref()
                .and_then(|f| modules.iter().position(|m| f.starts_with(&m.dir)))
                .unwrap_or(usize::MAX);
            (pos, v.line, v.path.as_str(), v.value.as_str())
        })
        .collect();
    ordered.sort_unstable();
    ordered
        .into_iter()
        .map(|(_, _, _, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

/// Statically evaluate the `js` excludes contributed by DI plugins on
/// `Magento\Framework\View\Asset\Minification::getExcludes`.
///
/// The config tree is only half the list on a real store: modules append theirs
/// from a plugin, and those appends are deployed bytes (they land inside
/// `requirejs-min-resolver.js` and therefore inside the JS bundles). magecommand
/// never executes PHP, so we evaluate the one shape these plugins all take:
///
/// ```php
/// public function aroundGetExcludes($subject, callable $proceed, $contentType) {
///     $result = $proceed($contentType);
///     if ($contentType !== 'js') { return $result; }
///     $result[] = 'literal';   // …repeated
///     return $result;
/// }
/// ```
///
/// Anything else in the body (a loop, a config read, a method call feeding the
/// array) means we cannot know the values statically: that plugin is reported to the
/// caller instead of being silently dropped or half-applied.
///
/// Ordering: these are `around` plugins that append AFTER `$proceed` returns, so
/// the innermost runs first. `plugins` must arrive in Magento's execution order
/// (outermost first), and the appends are therefore applied in REVERSE.
pub fn plugin_min_excludes(
    magento: &magequery_core::Magento,
) -> (Vec<String>, Vec<String>) {
    const TARGET: &str = "Magento\\Framework\\View\\Asset\\Minification";
    let di = magento.di_export(magequery_core::Area::Global);
    let mut classes: Vec<&magequery_core::ClassName> = Vec::new();
    for p in di.plugins.iter().filter(|p| !p.disabled) {
        if p.target.as_str().trim_start_matches('\\') != TARGET {
            continue;
        }
        if let Some(c) = p.class.as_ref() {
            classes.push(c);
        }
    }

    let (mut excludes, mut unknown) = (Vec::new(), Vec::new());
    // Innermost appends first => reverse of execution order.
    for class in classes.iter().rev() {
        let Some(file) = magento.class_file(class) else {
            unknown.push(format!("{} (source not found)", class.as_str()));
            continue;
        };
        let Ok(src) = magento.read_source(&file) else {
            unknown.push(format!("{} (source unreadable)", class.as_str()));
            continue;
        };
        match literal_exclude_appends(&src) {
            Some(vals) => excludes.extend(vals),
            None => unknown.push(class.as_str().to_string()),
        }
    }
    (excludes, unknown)
}

/// Extract the `$result[] = '<literal>';` appends a `*GetExcludes` plugin makes
/// for the `js` content type, in source order — but ONLY when the whole body is
/// statically decidable. Returns `None` otherwise, so the caller can report the
/// plugin instead of emitting a half-right exclude list.
///
/// Decidable means: every statement is `$result = $proceed(…)`, a `return`, or a
/// literal append; and every block is guarded by a comparison of `$contentType`
/// against `'js'`, which we can evaluate because `getExcludes('js')` is the only
/// call whose result reaches `requirejs-min-resolver.js`. Both real shapes fall
/// out of that: an early-return guard (`if ($contentType !== 'js') { return …; }`
/// followed by appends) and a positive block (`if ($contentType == 'js') { …
/// appends … }`).
fn literal_exclude_appends(src: &str) -> Option<Vec<String>> {
    let sig = src
        .find("function aroundGetExcludes")
        .or_else(|| src.find("function afterGetExcludes"))?;
    let open = src[sig..].find('{')? + sig;
    let bytes = src.as_bytes();
    let (mut depth, mut i) = (0usize, open);
    let end = loop {
        match bytes.get(i)? {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    break i;
                }
            }
            _ => {}
        }
        i += 1;
    };

    /// One lexed event from the method body.
    enum Ev {
        /// A `;`-terminated statement.
        Stmt(String),
        /// A block opener, carrying the header text before its `{`.
        Open(String),
        Close,
    }

    // Lex quote-aware: a `//` inside `'https://…'` is not a comment and a `;`
    // inside a literal is not a terminator.
    let body: Vec<char> = src[open + 1..end].chars().collect();
    let mut evs: Vec<Ev> = Vec::new();
    let mut cur = String::new();
    let mut k = 0usize;
    while k < body.len() {
        let c = body[k];
        match c {
            '\'' | '"' => {
                cur.push(c);
                k += 1;
                while k < body.len() {
                    let d = body[k];
                    cur.push(d);
                    k += 1;
                    if d == '\\' {
                        if k < body.len() {
                            cur.push(body[k]);
                            k += 1;
                        }
                    } else if d == c {
                        break;
                    }
                }
            }
            '/' if body.get(k + 1) == Some(&'/') => {
                while k < body.len() && body[k] != '\n' {
                    k += 1;
                }
            }
            '#' => {
                while k < body.len() && body[k] != '\n' {
                    k += 1;
                }
            }
            '/' if body.get(k + 1) == Some(&'*') => {
                k += 2;
                while k + 1 < body.len() && !(body[k] == '*' && body[k + 1] == '/') {
                    k += 1;
                }
                k = (k + 2).min(body.len());
            }
            ';' => {
                evs.push(Ev::Stmt(std::mem::take(&mut cur)));
                k += 1;
            }
            '{' => {
                evs.push(Ev::Open(std::mem::take(&mut cur)));
                k += 1;
            }
            '}' => {
                let pending = std::mem::take(&mut cur);
                if !pending.trim().is_empty() {
                    evs.push(Ev::Stmt(pending));
                }
                evs.push(Ev::Close);
                k += 1;
            }
            _ => {
                cur.push(c);
                k += 1;
            }
        }
    }
    if !cur.trim().is_empty() {
        evs.push(Ev::Stmt(cur));
    }

    // Walk with a suppression stack: statements inside a block whose guard is
    // false for `'js'` contribute nothing.
    let mut suppressed: Vec<bool> = Vec::new();
    let mut out = Vec::new();
    for ev in evs {
        match ev {
            Ev::Open(header) => suppressed.push(!guard_holds_for_js(&header)?),
            Ev::Close => {
                suppressed.pop();
            }
            Ev::Stmt(stmt) => {
                let stmt = stmt.trim();
                if stmt.is_empty()
                    || stmt.starts_with("$result = $proceed(")
                    || stmt.starts_with("return ")
                {
                    continue;
                }
                let Some(rhs) = stmt.strip_prefix("$result[] =") else {
                    return None;
                };
                let value = php_string_literal(rhs.trim())?;
                if !suppressed.iter().any(|s| *s) {
                    out.push(value);
                }
            }
        }
    }
    Some(out)
}

/// Does a block header hold when `$contentType` is `'js'`? `Some(true/false)`
/// for a decidable `$contentType`-vs-`'js'` comparison (or a bare block),
/// `None` for anything we cannot evaluate — a loop, a config read, a different
/// variable.
fn guard_holds_for_js(header: &str) -> Option<bool> {
    let h = header.trim();
    if h.is_empty() {
        return Some(true); // a bare `{ … }` block
    }
    let cond = h.strip_prefix("if")?.trim();
    let cond = cond.strip_prefix('(')?.trim_end();
    let cond = cond.strip_suffix(')')?.trim();
    if !cond.contains("$contentType") {
        return None;
    }
    let js = |s: &str| s == "'js'" || s == "\"js\"";
    for (op, positive) in [("!==", false), ("===", true), ("!=", false), ("==", true)] {
        if let Some((l, r)) = cond.split_once(op) {
            let (l, r) = (l.trim(), r.trim());
            let matches_js = (l == "$contentType" && js(r)) || (r == "$contentType" && js(l));
            return matches_js.then_some(positive);
        }
    }
    None
}

/// Read one complete PHP single- or double-quoted literal; `None` if `s` is not
/// exactly that (a concatenation, a call, an unterminated string).
fn php_string_literal(s: &str) -> Option<String> {
    let mut chars = s.chars();
    let quote = chars.next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let mut value = String::new();
    let mut closed = false;
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                // PHP single quotes only escape `\` and `'`.
                Some(e) if quote == '\'' && (e == '\\' || e == '\'') => value.push(e),
                Some(e) if quote == '"' => value.push(e),
                Some(e) => {
                    value.push('\\');
                    value.push(e);
                }
                None => return None,
            }
        } else if c == quote {
            closed = true;
            break;
        } else {
            value.push(c);
        }
    }
    (closed && chars.as_str().trim().is_empty()).then_some(value)
}

/// `Config::getMinResolverCode()`, unminified branch: the fixed template with
/// the exclude condition interpolated. The condition is
/// `url.indexOf(baseUrl)===0` plus one `!url.match(/<regex>/)` per exclude
/// (only `/` escaped, as `\/`), all joined with `&&`.
pub fn min_resolver_code(excludes: &[String]) -> String {
    let mut cond = String::from("url.indexOf(baseUrl)===0");
    for e in excludes {
        cond.push_str("&&!url.match(/");
        cond.push_str(&e.replace('/', "\\/"));
        cond.push_str("/)");
    }
    MIN_RESOLVER_TEMPLATE.replace("%excludes%", &cond)
}

/// The excludes from an open `magequery-core` handle: the static [`ConfigSet`]
/// (config.xml defaults + config.php/env.php `system` + `CONFIG__*` — no DB)
/// against the enabled modules in `config.php` load order.
pub fn min_resolver_excludes_from_magento(
    magento: &magequery_core::Magento,
) -> Result<Vec<String>, RequireJsError> {
    let config = magento.config(false).map_err(|e| RequireJsError {
        entry: Some(MIN_RESOLVER_FILE_NAME.to_string()),
        module: None,
        file: None,
        message: format!("system config unreadable: {e}"),
    })?;
    let modules: Vec<ModuleRef> = magento
        .modules()
        .iter()
        .filter(|m| m.enabled)
        .map(|m| ModuleRef {
            name: m.name.to_string(),
            dir: m.path.clone(),
        })
        .collect();
    let mut excludes = min_resolver_excludes(&config, &modules);
    // Plugin-contributed excludes (statically evaluated — see
    // `plugin_min_excludes`). On a real store these outnumber the config ones.
    let (from_plugins, unknown) = plugin_min_excludes(magento);
    for class in unknown {
        eprintln!(
            "warning: plugin {class} on Minification::getExcludes is not statically \
             evaluable — its excludes are missing from requirejs-min-resolver.js"
        );
    }
    excludes.extend(from_plugins);
    Ok(excludes)
}

/// Where the verbatim [`MIXINS_FILE_NAME`] copy comes FROM:
/// `lib/web/mage/requirejs/mixins.js` under the Magento root. (The deploy
/// copies it unchanged; there is deliberately no transform to call.)
pub fn mixins_source_path(root: &Path) -> PathBuf {
    root.join("lib").join("web").join("mage").join("requirejs").join("mixins.js")
}

#[cfg(test)]
mod tests {
    use super::literal_exclude_appends;

    /// The `around` shape: an early-return guard, then literal appends. The URL
    /// literal contains `//`, which a naive comment strip would truncate.
    #[test]
    fn around_plugin_with_early_return_guard() {
        let src = r#"<?php
class P {
    public function aroundGetExcludes(Minification $subject, callable $proceed, $contentType)
    {
        $result = $proceed($contentType);
        if ($contentType !== 'js') {
            return $result;
        }
        $result[] = 'https://static-app.connect.trustedshops.com';
        $result[] = 'Firebear_ImportExport/js/lib/ace/snippets/abap';
        return $result;
    }
}"#;
        assert_eq!(
            literal_exclude_appends(src).unwrap(),
            vec![
                "https://static-app.connect.trustedshops.com".to_string(),
                "Firebear_ImportExport/js/lib/ace/snippets/abap".to_string(),
            ]
        );
    }

    /// The `after` shape: the appends live INSIDE a positive guard block.
    #[test]
    fn after_plugin_with_positive_guard_block() {
        let src = r#"<?php
class P {
    public function afterGetExcludes(Minification $subject, $result, $contentType)
    {
        if ($contentType == 'js') {
            $result[] = 'js.mollie.com';
        }
        return $result;
    }
}"#;
        assert_eq!(literal_exclude_appends(src).unwrap(), vec!["js.mollie.com".to_string()]);
    }

    /// A guard that is false for `js` contributes nothing — its block is
    /// suppressed rather than flattened into the result.
    #[test]
    fn appends_under_a_non_js_guard_are_suppressed() {
        let src = r#"<?php
class P {
    public function afterGetExcludes($subject, $result, $contentType)
    {
        if ($contentType !== 'js') {
            $result[] = 'only-for-css';
        }
        $result[] = 'always';
        return $result;
    }
}"#;
        assert_eq!(literal_exclude_appends(src).unwrap(), vec!["always".to_string()]);
    }

    /// Anything we cannot evaluate statically must be REJECTED, so the caller
    /// reports the plugin instead of emitting a half-right exclude list.
    #[test]
    fn non_literal_bodies_are_rejected() {
        let computed = r#"<?php
    public function afterGetExcludes($subject, $result, $contentType)
    {
        $result[] = $this->config->getValue('some/path');
        return $result;
    }"#;
        assert!(literal_exclude_appends(computed).is_none());

        let looped = r#"<?php
    public function afterGetExcludes($subject, $result, $contentType)
    {
        foreach ($this->paths as $p) {
            $result[] = 'x';
        }
        return $result;
    }"#;
        assert!(literal_exclude_appends(looped).is_none());

        let concatenated = r#"<?php
    public function afterGetExcludes($subject, $result, $contentType)
    {
        $result[] = 'a' . $suffix;
        return $result;
    }"#;
        assert!(literal_exclude_appends(concatenated).is_none());
    }

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

    fn excl(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Zero excludes: the whole artifact is the fixed template with the
    /// always-present `indexOf` condition — locked byte for byte (4-space
    /// base indent, blank line, NO trailing newline: the heredoc's shape).
    #[test]
    fn min_resolver_with_no_excludes_is_the_bare_template() {
        assert_eq!(
            min_resolver_code(&[]),
            "    (function () {\n\
             \x20       var ctx = require.s.contexts._,\n\
             \x20           origNameToUrl = ctx.nameToUrl,\n\
             \x20           baseUrl = ctx.config.baseUrl;\n\
             \n\
             \x20       ctx.nameToUrl = function() {\n\
             \x20           var url = origNameToUrl.apply(ctx, arguments);\n\
             \x20           if (url.indexOf(baseUrl)===0) {\n\
             \x20               url = url.replace(/(\\.min)?\\.js$/, '.min.js');\n\
             \x20           }\n\
             \x20           return url;\n\
             \x20       };\n\
             \x20   })();"
        );
    }

    /// One exclude: `/` (and ONLY `/`) is escaped to `\/` in the regex
    /// literal, appended as `&&!url.match(/…/)`.
    #[test]
    fn min_resolver_escapes_slashes_in_one_exclude() {
        let code = min_resolver_code(&excl(&["/hugerte/"]));
        assert!(
            code.contains("if (url.indexOf(baseUrl)===0&&!url.match(/\\/hugerte\\//)) {"),
            "{code}"
        );
        // Regex metacharacters other than `/` pass through untouched.
        let dot = min_resolver_code(&excl(&["\\.min\\.js"]));
        assert!(dot.contains("&&!url.match(/\\.min\\.js/)"), "{dot}");
    }

    /// Two excludes reproduce the reference install's exact condition line.
    #[test]
    fn min_resolver_joins_two_excludes_with_and() {
        let code = min_resolver_code(&excl(&["/hugerte/", "/v1/songbird"]));
        assert!(
            code.contains(
                "if (url.indexOf(baseUrl)===0&&!url.match(/\\/hugerte\\//)&&!url.match(/\\/v1\\/songbird/)) {"
            ),
            "{code}"
        );
    }

    /// A minimal openable Magento root: `app/etc/config.php` + two app/code
    /// modules with `module.xml` and a `config.xml` `<default>` each. Module
    /// LOAD order (config.php) deliberately disagrees with the alphabetical
    /// order of both the module names and the exclude keys, so an
    /// order-by-the-wrong-thing bug cannot pass.
    fn synth_config_root(env_php: Option<&str>) -> tempfile::TempDir {
        let td = tempfile::tempdir().expect("tempdir");
        let r = td.path();
        let w = |rel: &str, content: &str| {
            let p = r.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        };
        w(
            "app/etc/config.php",
            "<?php\nreturn [\n    'modules' => [\n        'Acme_Zeta' => 1,\n        'Acme_Alpha' => 1,\n    ]\n];\n",
        );
        // Loads FIRST, declares key `zebra` (alphabetically last).
        w(
            "app/code/Acme/Zeta/etc/module.xml",
            "<?xml version=\"1.0\"?><config><module name=\"Acme_Zeta\"/></config>\n",
        );
        w(
            "app/code/Acme/Zeta/etc/config.xml",
            "<?xml version=\"1.0\"?><config><default><dev><js><minify_exclude>\
             <zebra>/first/</zebra>\
             </minify_exclude></js></dev></default></config>\n",
        );
        // Loads SECOND — the "later module adds an exclude" case — key `alpha`.
        w(
            "app/code/Acme/Alpha/etc/module.xml",
            "<?xml version=\"1.0\"?><config><module name=\"Acme_Alpha\"/></config>\n",
        );
        w(
            "app/code/Acme/Alpha/etc/config.xml",
            "<?xml version=\"1.0\"?><config><default><dev><js><minify_exclude>\
             <alpha>/second/</alpha>\
             </minify_exclude></js></dev></default></config>\n",
        );
        if let Some(env) = env_php {
            w("app/etc/env.php", env);
        }
        td
    }

    /// The exclude pipeline end to end over the real ConfigSet: a later
    /// module's added exclude is picked up, and the order is module LOAD
    /// order (then line), not key order.
    #[test]
    fn excludes_merge_across_modules_in_load_order() {
        let td = synth_config_root(None);
        let magento = magequery_core::Magento::open(td.path()).expect("open synthetic root");
        let excludes = min_resolver_excludes_from_magento(&magento).expect("excludes");
        // Alphabetical (by key OR by module name) would say /second/ first.
        assert_eq!(excludes, vec!["/first/".to_string(), "/second/".to_string()]);
    }

    /// The legacy string form: a scalar at the exact path (here from env.php's
    /// `system` node, which replaces the array wholesale) splits on newlines,
    /// trimmed, empties dropped — `getMinificationExcludeValues`'s conversion.
    #[test]
    fn legacy_string_exclude_splits_on_newlines() {
        let td = synth_config_root(Some(
            "<?php\nreturn [\n    'system' => [\n        'default' => [\n            'dev' => ['js' => ['minify_exclude' => \"/a/\\n\\n  /b/  \\n\"]]\n        ]\n    ]\n];\n",
        ));
        let magento = magequery_core::Magento::open(td.path()).expect("open synthetic root");
        let excludes = min_resolver_excludes_from_magento(&magento).expect("excludes");
        assert_eq!(excludes, vec!["/a/".to_string(), "/b/".to_string()]);
    }

    #[test]
    fn mixins_source_is_the_lib_file() {
        assert_eq!(
            mixins_source_path(Path::new("/srv/m2")),
            PathBuf::from("/srv/m2/lib/web/mage/requirejs/mixins.js")
        );
    }
}
