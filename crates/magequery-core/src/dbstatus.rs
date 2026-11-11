//! `db status`: a native `bin/magento setup:db:status`.
//!
//! Magento's command runs four validators and exits 2 ("upgrade required") if any is not
//! up to date — or throws. Each is ported here with its exact semantics:
//!
//! * `UpToDateDeclarativeSchema` — the declarative-schema diff is empty ([`crate::schemadiff`]).
//! * `UpToDateSchema` / `UpToDateData` — every `Setup/Patch/{Schema,Data}/*.php` of every
//!   enabled module is in `patch_list`, or is a `PatchVersionInterface` patch whose
//!   `getVersion()` is `<=` the module's `setup_module` version.
//! * `OldDbValidator` — every enabled module with a `setup_version` has exactly that
//!   version (by PHP `version_compare`) as its `schema_version` and `data_version`.

use std::cmp::Ordering;

use crate::db::status::{LiveSetupState, SetupModuleRows};
use crate::ids::{ClassName, ModuleName};
use crate::model::{DbStatus, ModuleVersionMismatch, PatchKind, PendingPatch};
use crate::phpver::version_compare;
use crate::{default_connection, Error, Magento, Result};

const PATCH_VERSION_INTERFACE: &str = "Magento\\Framework\\Setup\\Patch\\PatchVersionInterface";

impl Magento {
    /// Whether `setup:upgrade` has work to do, by the four checks `bin/magento
    /// setup:db:status` runs, without booting Magento. Fails only when the database is
    /// unreachable; a check that can't run (e.g. an empty database with no
    /// `setup_module`) lands in [`DbStatus::errors`] and counts as "upgrade required",
    /// as it does in Magento.
    pub fn db_status(&self) -> Result<DbStatus> {
        // `DeploymentConfig::isAvailable()`: config.php + env.php must carry install/date.
        if !self.is_installed() {
            return Err(Error::NotInstalled);
        }
        let mut status = DbStatus {
            schema_changes: Vec::new(),
            schema_patches: Vec::new(),
            data_patches: Vec::new(),
            module_versions: Vec::new(),
            notes: Vec::new(),
            errors: Vec::new(),
        };
        // Magento builds its validators lazily, so a missing or unreachable database fails
        // each of them — and each failure counts as "upgrade required" (exit 2), not as a
        // command error.
        let connected = self.db_config().map_err(|e| e.to_string()).and_then(|cfg| {
            let conn = default_connection(&cfg).map_err(|e| e.to_string())?.clone();
            let db = crate::db::status::open(&conn)?;
            Ok((cfg, db))
        });
        let (cfg, mut db) = match connected {
            Ok(c) => c,
            Err(e) => {
                for check in ["declarative schema", "schema and data patches", "module versions"] {
                    status.errors.push(format!("{check}: {e}"));
                }
                return Ok(status);
            }
        };
        let live = crate::db::status::fetch_setup_state(&mut db, &cfg.table_prefix);
        let schema = crate::schemadiff::diff(self, &mut db, &cfg.table_prefix);
        self.absorb_db_warnings();

        match schema {
            Ok(changes) => status.schema_changes = changes,
            Err(e) => status.errors.push(format!("declarative schema: {e}")),
        }
        self.check_patches(&live, &mut status);
        self.check_module_versions(&live, &mut status);
        Ok(status)
    }

    /// `install/date` in the merged deployment config (`config.php`, then `env.php`).
    fn is_installed(&self) -> bool {
        let root = self.root();
        let vfs = &self.index.vfs;
        let has_date = |v: crate::phparray::PhpValue| {
            v.get("install").and_then(|i| i.get("date")).is_some_and(|d| !matches!(d, crate::phparray::PhpValue::Null))
        };
        crate::deploy::read_env(root, vfs).is_ok_and(has_date)
            || crate::deploy::read_config_php(root, vfs).is_ok_and(has_date)
    }

    fn check_patches(&self, live: &LiveSetupState, status: &mut DbStatus) {
        let (applied, versions) = match (&live.patch_list, &live.setup_module) {
            (Ok(a), Ok(v)) => (a, v),
            (Err(e), _) | (_, Err(e)) => {
                status.errors.push(format!("schema and data patches: {e}"));
                return;
            }
        };
        for m in self.index.modules.iter().filter(|m| m.enabled) {
            for kind in [PatchKind::Schema, PatchKind::Data] {
                for (class, file) in crate::doctor::patch_files(&m.path, &m.name, kind) {
                    // `isSkipable…` runs first, for applied patches too, and its
                    // `class_implements()` autoloads the class: one that doesn't load is a
                    // warning, which bin/magento turns into an exception.
                    if self.index.resolver.autoloads(&class) == Some(false) {
                        status.errors.push(format!(
                            "{kind} patches: {class} doesn't autoload ({} declares no such class)",
                            file.strip_prefix(self.root()).unwrap_or(&file).display()
                        ));
                        continue;
                    }
                    if applied.contains(class.as_str()) {
                        continue;
                    }
                    if self.skippable_by_version(&class, &m.name, kind, versions, &mut status.notes) {
                        continue;
                    }
                    let pending = PendingPatch { class, module: m.name.clone(), file };
                    match kind {
                        PatchKind::Schema => status.schema_patches.push(pending),
                        PatchKind::Data => status.data_patches.push(pending),
                    }
                }
            }
        }
    }

    /// `PatchBackwardCompatability::isSkipableBy{Schema,Data}SetupVersion`: a
    /// `PatchVersionInterface` patch whose `getVersion()` is not newer than the module's
    /// recorded version was applied by an old-style upgrade script and is skipped.
    fn skippable_by_version(
        &self,
        class: &ClassName,
        module: &ModuleName,
        kind: PatchKind,
        versions: &SetupModuleRows,
        notes: &mut Vec<String>,
    ) -> bool {
        let resolver = &self.index.resolver;
        if !resolver.ancestors(class).iter().any(|a| a.as_str() == PATCH_VERSION_INTERFACE) {
            return false;
        }
        let Some(patch_version) = resolver.literal_return(class, "getVersion") else {
            notes.push(format!(
                "{class}::getVersion() isn't a literal, so it was not compared with the \
                 module's version; the patch is reported pending"
            ));
            return false;
        };
        // `(string) $moduleResource->getDbVersion()`: a missing row or NULL reads as ''.
        let db_version = versions
            .get(module.as_str())
            .and_then(|(schema, data)| match kind {
                PatchKind::Schema => schema.clone(),
                PatchKind::Data => data.clone(),
            })
            .unwrap_or_default();
        version_compare(&patch_version, &db_version) != Ordering::Greater
    }

    fn check_module_versions(&self, live: &LiveSetupState, status: &mut DbStatus) {
        let versions = match &live.setup_module {
            Ok(v) => v,
            Err(e) => {
                status.errors.push(format!("module versions: {e}"));
                return;
            }
        };
        for m in self.index.modules.iter().filter(|m| m.enabled) {
            // `empty($configVer)`: absent, '' and '0' all mean "not versioned".
            let Some(required) = m.setup_version.as_deref().filter(|v| !v.is_empty() && *v != "0")
            else {
                continue;
            };
            let row = versions.get(m.name.as_str());
            for kind in [PatchKind::Schema, PatchKind::Data] {
                let current = row.and_then(|(schema, data)| match kind {
                    PatchKind::Schema => schema.clone(),
                    PatchKind::Data => data.clone(),
                });
                // `version_compare($configVer, $dbVer)`: PHP passes a NULL/false version
                // as ''.
                if version_compare(required, current.as_deref().unwrap_or("")) != Ordering::Equal {
                    status.module_versions.push(ModuleVersionMismatch {
                        module: m.name.clone(),
                        kind,
                        // Magento prints a falsy version as "none".
                        current: current.filter(|v| !v.is_empty() && v != "0"),
                        required: required.to_string(),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    
    use crate::db::status::LiveSetupState;
    use crate::model::DbStatus;
    use crate::Magento;

    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("magequery-dbstatus-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Tree(dir)
        }
        fn write(&self, rel: &str, body: &str) {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn patch(class: &str, versioned: Option<&str>) -> String {
        let (implements, body) = match versioned {
            Some(ret) => (
                ", PatchVersionInterface",
                format!("    const V = '1.1.0';\n    public static function getVersion()\n    {{\n        return {ret};\n    }}\n"),
            ),
            None => ("", String::new()),
        };
        format!(
            "<?php\nnamespace Acme\\Widget\\Setup\\Patch\\Data;\n\n\
             use Magento\\Framework\\Setup\\Patch\\DataPatchInterface;\n\
             use Magento\\Framework\\Setup\\Patch\\PatchVersionInterface;\n\n\
             class {class} implements DataPatchInterface{implements}\n{{\n{body}}}\n"
        )
    }

    fn status(tree: &Tree, setup_module: &[(&str, Option<&str>, Option<&str>)], applied: &[&str]) -> DbStatus {
        let mage = Magento::open(&tree.0).unwrap();
        let live = LiveSetupState {
            setup_module: Ok(setup_module
                .iter()
                .map(|(m, s, d)| (m.to_string(), (s.map(String::from), d.map(String::from))))
                .collect::<HashMap<_, _>>()),
            patch_list: Ok(applied.iter().map(|s| s.to_string()).collect::<HashSet<_>>()),
        };
        let mut st = DbStatus {
            schema_changes: Vec::new(),
            schema_patches: Vec::new(),
            data_patches: Vec::new(),
            module_versions: Vec::new(),
            notes: Vec::new(),
            errors: Vec::new(),
        };
        mage.check_patches(&live, &mut st);
        mage.check_module_versions(&live, &mut st);
        st
    }

    fn fixture(name: &str) -> Tree {
        let t = Tree::new(name);
        t.write(
            "app/etc/config.php",
            "<?php\nreturn ['modules' => ['Acme_Widget' => 1, 'Acme_Old' => 1, 'Acme_Plain' => 1, 'Acme_Off' => 0]];\n",
        );
        t.write("app/code/Acme/Widget/etc/module.xml", r#"<config><module name="Acme_Widget" setup_version="1.0.0"/></config>"#);
        t.write("app/code/Acme/Old/etc/module.xml", r#"<config><module name="Acme_Old" setup_version="2.0.0"/></config>"#);
        t.write("app/code/Acme/Plain/etc/module.xml", r#"<config><module name="Acme_Plain"/></config>"#);
        t.write("app/code/Acme/Off/etc/module.xml", r#"<config><module name="Acme_Off" setup_version="9.9.9"/></config>"#);
        let d = "app/code/Acme/Widget/Setup/Patch/Data";
        t.write(&format!("{d}/Applied.php"), &patch("Applied", None));
        t.write(&format!("{d}/Pending.php"), &patch("Pending", None));
        t.write(&format!("{d}/Versioned.php"), &patch("Versioned", Some("'1.0.0'")));
        t.write(&format!("{d}/Newer.php"), &patch("Newer", Some("self::V")));
        t.write(&format!("{d}/Computed.php"), &patch("Computed", Some("'1.' . '0.0'")));
        // Not globbed by Magento: a subdirectory and a dotfile.
        t.write(&format!("{d}/Nested/Deep.php"), &patch("Deep", None));
        t.write(&format!("{d}/.Hidden.php"), &patch("Hidden", None));
        // A disabled module's patches are never read.
        t.write("app/code/Acme/Off/Setup/Patch/Data/Never.php", "<?php class Never {}");
        t
    }

    #[test]
    fn patches_follow_magentos_reader_and_version_skip() {
        let t = fixture("patches");
        let st = status(
            &t,
            &[("Acme_Widget", Some("1.0.0"), Some("1.0.0")), ("Acme_Old", Some("2.0.0"), Some("2.0.0"))],
            &["Acme\\Widget\\Setup\\Patch\\Data\\Applied"],
        );
        let pending: Vec<&str> = st.data_patches.iter().map(|p| p.class.as_str()).collect();
        assert_eq!(
            pending,
            [
                // Not a literal getVersion(): reported pending, with a note.
                "Acme\\Widget\\Setup\\Patch\\Data\\Computed",
                // getVersion() 1.1.0 (via self::V) > data_version 1.0.0.
                "Acme\\Widget\\Setup\\Patch\\Data\\Newer",
                "Acme\\Widget\\Setup\\Patch\\Data\\Pending",
            ]
        );
        assert!(st.schema_patches.is_empty());
        assert_eq!(st.notes.len(), 1, "{:?}", st.notes);
        assert!(st.notes[0].contains("Computed::getVersion()"));
        assert!(st.module_versions.is_empty());
    }

    #[test]
    fn applied_check_is_exact_like_in_array() {
        // patch_list holding a leading-backslash spelling doesn't count in Magento.
        let t = fixture("exact");
        let st = status(
            &t,
            &[("Acme_Widget", Some("1.0.0"), Some("1.0.0")), ("Acme_Old", Some("2.0.0"), Some("2.0.0"))],
            &["\\Acme\\Widget\\Setup\\Patch\\Data\\Applied"],
        );
        assert!(st.data_patches.iter().any(|p| p.class.as_str().ends_with("\\Applied")));
    }

    #[test]
    fn module_versions_compare_with_version_compare() {
        let t = fixture("versions");
        let st = status(
            &t,
            // Acme_Widget's data_version lags; Acme_Old has no row; Acme_Plain has no
            // setup_version and is never checked; disabled Acme_Off is ignored.
            &[("Acme_Widget", Some("1.0.0"), Some("0.9.0")), ("Acme_Plain", None, None)],
            &[],
        );
        let got: Vec<(String, String, Option<String>, String)> = st
            .module_versions
            .iter()
            .map(|v| (v.module.as_str().to_string(), v.kind.to_string(), v.current.clone(), v.required.clone()))
            .collect();
        assert_eq!(
            got,
            [
                ("Acme_Widget".into(), "data".into(), Some("0.9.0".into()), "1.0.0".into()),
                ("Acme_Old".into(), "schema".into(), None, "2.0.0".into()),
                ("Acme_Old".into(), "data".into(), None, "2.0.0".into()),
            ]
        );
        // With data_version 0.9.0 the 1.0.0 patch is no longer skippable.
        assert!(st.data_patches.iter().any(|p| p.class.as_str().ends_with("\\Versioned")));
    }

    #[test]
    fn a_patch_file_that_does_not_autoload_fails_the_check_even_when_applied() {
        let t = fixture("autoload");
        // File name and class disagree: Magento's reader derives `…\Misnamed`.
        t.write(
            "app/code/Acme/Widget/Setup/Patch/Data/Misnamed.php",
            &patch("SomethingElse", None),
        );
        let applied = [
            "Acme\\Widget\\Setup\\Patch\\Data\\Applied",
            "Acme\\Widget\\Setup\\Patch\\Data\\Misnamed",
        ];
        let st = status(&t, &[("Acme_Widget", Some("1.0.0"), Some("1.0.0"))], &applied);
        assert_eq!(st.errors.len(), 1, "{:?}", st.errors);
        assert!(st.errors[0].contains("Misnamed doesn't autoload"));
        assert!(!st.is_up_to_date());
    }

    #[test]
    fn missing_bookkeeping_tables_count_as_upgrade_required() {
        let t = fixture("missing");
        let mage = Magento::open(&t.0).unwrap();
        let live = LiveSetupState {
            setup_module: Err("Table 'x.setup_module' doesn't exist".into()),
            patch_list: Err("Table 'x.patch_list' doesn't exist".into()),
        };
        let mut st = DbStatus {
            schema_changes: Vec::new(),
            schema_patches: Vec::new(),
            data_patches: Vec::new(),
            module_versions: Vec::new(),
            notes: Vec::new(),
            errors: Vec::new(),
        };
        mage.check_patches(&live, &mut st);
        mage.check_module_versions(&live, &mut st);
        assert_eq!(st.errors.len(), 2);
        assert!(!st.is_up_to_date());
    }
}
