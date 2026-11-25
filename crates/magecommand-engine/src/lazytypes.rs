//! Which PHP 8.4 lazy-ghost pass the checkout's `setup:di:compile` runs.
//!
//! Mage-OS added a final `ModificationChain` step that records, per area, which
//! classes the compiled object factory may build as lazy ghosts. Its shape has
//! changed across releases, and a compile has to write the one the store's
//! runtime reads — the other is silently ignored:
//!
//! | checkout          | chain step              | area-file section          |
//! |-------------------|-------------------------|----------------------------|
//! | Adobe 2.4.x       | —                       | —                          |
//! | Mage-OS 3.0 – 3.2 | `Chain\NonLazyTypes`    | `nonLazyTypes` (deny-list) |
//! | Mage-OS 3.3 – 3.6 | `Chain\LazyTypes`       | `lazyTypes` (allow-list)   |
//! | Mage-OS 4.0       | `Chain\LazyTypes` + the | `lazyTypes`, stricter      |
//! |                   | `ConstructorSideEffectScanner` | eligibility          |
//!
//! The kind is read off the checkout: the pass is a class under
//! `setup/src/Magento/Setup/Module/Di/Compiler`, so its file's presence is what
//! `DiCompileCommand` wires in.
//!
//! Both passes return early below PHP 8.4 (`PHP_VERSION_ID < 80400`), so the
//! section depends on the PHP that RUNS the compile — which a static compile has
//! to be told. Unknown, it is assumed to be 8.4+: the runtime factory consults
//! the list only on 8.4+ (`Factory\Compiled`, `PHP_VERSION_ID >= 80400`), so a
//! section on an older PHP is inert, while a missing one on 8.4 silently makes
//! every class eager. The chain step itself is configured either way.
//!
//! Both are overridable through [`Target`] (`di compile --target-php`,
//! `--target-platform`), for a compile whose checkout is not the store's, or
//! whose `setup/` is missing or patched.
//!
//! https://github.com/cresset-tools/magequery/issues/219

use std::path::Path;

/// The lazy-ghost pass a checkout's compiler runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LazyKind {
    /// No lazy pass (Adobe Commerce / Magento Open Source).
    None,
    /// Mage-OS 3.0 – 3.2: `Chain\NonLazyTypes`, a `nonLazyTypes` deny-list of
    /// the candidates that may NOT be lazy.
    DenyList,
    /// Mage-OS 3.3 – 3.6: `Chain\LazyTypes`, a `lazyTypes` allow-list of the
    /// candidates that may.
    AllowList,
    /// Mage-OS 4.0: the allow-list, where a constructor must also be trusted
    /// or proven side-effect free by `Compiler\ConstructorSideEffectScanner`
    /// (ported in [`crate::sideeffect`]).
    AllowListScanned,
}

/// The platform a compile targets. A closed set: each name is the first
/// release that compiles differently from the one before, and covers every
/// release up to the next name — not an exact version. Only ever add names
/// (magebuild configs carry them); a new behaviour is a new variant plus an
/// arm in each behaviour method such as [`Platform::lazy_kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// Adobe Commerce / Magento Open Source 2.4.9+, the behaviour the whole
    /// compile follows today: no lazy pass.
    Magento2_4_9,
    /// Mage-OS 3.0 – 3.2: the `nonLazyTypes` deny-list.
    MageOs3_0,
    /// Mage-OS 3.3 – 3.x: the `lazyTypes` allow-list.
    MageOs3_3,
    /// Mage-OS 4.0+: the allow-list, gated on the side-effect scanner.
    MageOs4_0,
}

impl Platform {
    /// Every platform, by the name `--target-platform` takes.
    pub const ALL: [(&'static str, Platform); 4] = [
        ("magento-2.4.9", Platform::Magento2_4_9),
        ("mageos-3.0", Platform::MageOs3_0),
        ("mageos-3.3", Platform::MageOs3_3),
        ("mageos-4.0", Platform::MageOs4_0),
    ];

    /// The names `--target-platform` takes.
    pub fn names() -> impl Iterator<Item = &'static str> {
        Self::ALL.iter().map(|(n, _)| *n)
    }

    /// The lazy pass this platform's `setup:di:compile` runs.
    fn lazy_kind(self) -> LazyKind {
        match self {
            Platform::Magento2_4_9 => LazyKind::None,
            Platform::MageOs3_0 => LazyKind::DenyList,
            Platform::MageOs3_3 => LazyKind::AllowList,
            Platform::MageOs4_0 => LazyKind::AllowListScanned,
        }
    }
}

impl std::str::FromStr for Platform {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Self::ALL.iter().find(|(n, _)| *n == s).map(|(_, p)| *p).ok_or_else(|| {
            let names: Vec<_> = Self::names().collect();
            format!("`{s}`: expected one of {}", names.join(", "))
        })
    }
}

/// What the compile is told about the store, over what the checkout says.
#[derive(Clone, Copy, Debug, Default)]
pub struct Target<'a> {
    /// The PHP version the store runs (`8.4`, `8.3.12`).
    pub php: Option<&'a str>,
    /// The platform release, instead of reading the pass off `setup/`.
    pub platform: Option<Platform>,
}

/// What the compile emits for the lazy pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LazyPass {
    pub kind: LazyKind,
    /// Whether the area files carry the section: the targeted PHP is 8.4+.
    pub section: bool,
}

impl LazyPass {
    /// The pass for the checkout at `root`. The kind comes from
    /// `target.platform`, else from the checkout's compiler classes. The
    /// section from `target.php`, else `composer.json`'s exact
    /// `config.platform.php`, else 8.4+ is assumed.
    pub fn detect(root: &Path, target: &Target) -> Self {
        let section = match target.php.map(str::to_owned).or_else(|| platform_php(root)) {
            Some(v) => php_at_least_8_4(&v).unwrap_or(true),
            None => true,
        };
        let kind = target.platform.map_or_else(|| detect_kind(root), Platform::lazy_kind);
        LazyPass { kind, section }
    }

    /// The `ModificationChain` `modificationsList` entry the compile command
    /// configures for this pass: its key and the pass class.
    pub fn chain_step(&self) -> Option<(&'static str, &'static str)> {
        match self.kind {
            LazyKind::None => None,
            LazyKind::DenyList => {
                Some(("NonLazyTypes", "Magento\\Setup\\Module\\Di\\Compiler\\Config\\Chain\\NonLazyTypes"))
            }
            LazyKind::AllowList | LazyKind::AllowListScanned => {
                Some(("LazyTypes", "Magento\\Setup\\Module\\Di\\Compiler\\Config\\Chain\\LazyTypes"))
            }
        }
    }

    /// The area-file section this pass writes, when it writes one.
    pub fn section_name(&self) -> Option<&'static str> {
        if !self.section {
            return None;
        }
        match self.kind {
            LazyKind::None => None,
            LazyKind::DenyList => Some("nonLazyTypes"),
            LazyKind::AllowList | LazyKind::AllowListScanned => Some("lazyTypes"),
        }
    }
}

/// The checkout's `Magento\Setup\Module\Di\Compiler` directory: `setup/` at the
/// root (where magento2-base's installer copies it, and in a git checkout), or
/// still under `vendor/magento/magento2-base`.
fn compiler_dir(root: &Path) -> Option<std::path::PathBuf> {
    const REL: &str = "setup/src/Magento/Setup/Module/Di/Compiler";
    [root.join(REL), root.join("vendor/magento/magento2-base").join(REL)]
        .into_iter()
        .find(|d| d.is_dir())
}

fn detect_kind(root: &Path) -> LazyKind {
    let Some(dir) = compiler_dir(root) else { return LazyKind::None };
    if dir.join("Config/Chain/LazyTypes.php").is_file() {
        if dir.join("ConstructorSideEffectScanner.php").is_file() {
            LazyKind::AllowListScanned
        } else {
            LazyKind::AllowList
        }
    } else if dir.join("Config/Chain/NonLazyTypes.php").is_file() {
        LazyKind::DenyList
    } else {
        LazyKind::None
    }
}

/// `composer.json`'s `config.platform.php`: the PHP version composer resolves
/// for, which a pinned build sets to the production PHP.
fn platform_php(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("composer.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.pointer("/config/platform/php")?.as_str().map(str::to_owned)
}

/// Whether a plain PHP version (`8.4`, `8.3.12`, `8.4.0-dev`) is 8.4 or newer;
/// `None` when it is not one (a constraint like `~8.3.0||~8.4.0`).
fn php_at_least_8_4(version: &str) -> Option<bool> {
    let mut parts = version.trim().split(['.', '-', '+']);
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = match parts.next() {
        Some(m) => m.parse().ok()?,
        None => 0,
    };
    Some((major, minor) >= (8, 4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn php_versions() {
        assert_eq!(php_at_least_8_4("8.4"), Some(true));
        assert_eq!(php_at_least_8_4("8.4.0-dev"), Some(true));
        assert_eq!(php_at_least_8_4("8.5.1"), Some(true));
        assert_eq!(php_at_least_8_4("8.3.12"), Some(false));
        assert_eq!(php_at_least_8_4("9"), Some(true));
        assert_eq!(php_at_least_8_4("~8.3.0||~8.4.0"), None);
        assert_eq!(php_at_least_8_4("^8.2"), None);
    }

    fn checkout(files: &[&str], composer: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            let p = dir.path().join("setup/src/Magento/Setup/Module/Di/Compiler").join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "<?php\n").unwrap();
        }
        if let Some(c) = composer {
            std::fs::write(dir.path().join("composer.json"), c).unwrap();
        }
        dir
    }

    #[test]
    fn kind_follows_the_checkouts_pass() {
        let kind =
            |files: &[&str]| LazyPass::detect(checkout(files, None).path(), &Target::default()).kind;
        assert_eq!(kind(&[]), LazyKind::None);
        assert_eq!(kind(&["Config/Chain/BackslashTrim.php"]), LazyKind::None);
        assert_eq!(kind(&["Config/Chain/NonLazyTypes.php"]), LazyKind::DenyList);
        assert_eq!(kind(&["Config/Chain/LazyTypes.php"]), LazyKind::AllowList);
        assert_eq!(
            kind(&["Config/Chain/LazyTypes.php", "ConstructorSideEffectScanner.php"]),
            LazyKind::AllowListScanned
        );
    }

    #[test]
    fn section_follows_the_target_php() {
        let allow = ["Config/Chain/LazyTypes.php"];
        let platform = |v: &str| format!(r#"{{"config": {{"platform": {{"php": "{v}"}}}}}}"#);

        let php = |v| Target { php: Some(v), platform: None };
        let none = Target::default();

        let c = checkout(&allow, None);
        assert_eq!(LazyPass::detect(c.path(), &none).section_name(), Some("lazyTypes"));
        assert_eq!(LazyPass::detect(c.path(), &php("8.3")).section_name(), None);

        let c = checkout(&allow, Some(&platform("8.3.12")));
        assert_eq!(LazyPass::detect(c.path(), &none).section_name(), None);
        // An explicit target wins over composer.json.
        assert_eq!(LazyPass::detect(c.path(), &php("8.4")).section_name(), Some("lazyTypes"));
        // A step is configured whatever the PHP.
        assert_eq!(LazyPass::detect(c.path(), &none).chain_step().map(|s| s.0), Some("LazyTypes"));

        let c = checkout(&["Config/Chain/NonLazyTypes.php"], Some(&platform("8.4.1")));
        assert_eq!(LazyPass::detect(c.path(), &none).section_name(), Some("nonLazyTypes"));
    }

    #[test]
    fn platforms_parse_and_pick_their_pass() {
        let kind = |s: &str| s.parse::<Platform>().map(Platform::lazy_kind);
        assert_eq!(kind("magento-2.4.9"), Ok(LazyKind::None));
        assert_eq!(kind("mageos-3.0"), Ok(LazyKind::DenyList));
        assert_eq!(kind("mageos-3.3"), Ok(LazyKind::AllowList));
        assert_eq!(kind("mageos-4.0"), Ok(LazyKind::AllowListScanned));
        // Names, not versions: a release inside a range is not one of them.
        assert!(kind("mageos-3.6").is_err());
        assert!(kind("magento-2.4.8").is_err());
        assert!(kind("magento").is_err());
        assert!(kind("openmage").is_err());
    }

    #[test]
    fn a_target_platform_overrides_the_checkout() {
        // A 3.6 checkout compiled for 4.0, and a bare checkout for Mage-OS 3.3+.
        let c = checkout(&["Config/Chain/LazyTypes.php"], None);
        let t = Target { php: None, platform: Some(Platform::MageOs4_0) };
        assert_eq!(LazyPass::detect(c.path(), &t).kind, LazyKind::AllowListScanned);
        let c = checkout(&[], None);
        let t = Target { php: None, platform: Some(Platform::MageOs3_3) };
        assert_eq!(LazyPass::detect(c.path(), &t).section_name(), Some("lazyTypes"));
    }
}
