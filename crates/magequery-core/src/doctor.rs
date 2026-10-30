//! `doctor` — cross-index lints: the `modules --check` philosophy generalized. Every
//! check is a projection over indexes that already exist; doctor computes nothing new,
//! it just asks each index "and does what you reference actually exist?".
//!
//! Three severities, chosen so a healthy store is quiet and the exit code trustworthy:
//! * **Error** — config references something that doesn't exist, or is structurally
//!   broken (cycles, module set drift). These break at runtime.
//! * **Warning** — probably-forgotten wiring: classes that look like commands/observers/
//!   plugins but are registered nowhere, queues nothing consumes, and missing-class
//!   references nothing live can reach (stale config). Could be intentional dead code,
//!   so they never fail CI by themselves.
//! * **Info** — a missing-class reference nothing live can reach, into a package that
//!   isn't installed: an optional integration, as intended.
//!
//! A missing class only breaks something when an object that *can* be built asks for
//! it, so the DI lints follow every reference back (see [`Doctor::demand`]) and attach
//! the path they found as the finding's chain.
//!
//! A shop that has cleaned a warning up can make it blocking with
//! [`DoctorReport::deny`] (`doctor --deny <lint>`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::error::Severity;
use crate::ids::{Area, ClassName};
use crate::model::{
    ArgValue, ChainNode, DoctorFinding, DoctorLint, DoctorReport, Module, ModuleSource,
};
use crate::source::Source;
use crate::Magento;

pub(crate) fn run(mage: &Magento, source_filter: Option<ModuleSource>) -> DoctorReport {
    let mut d = Doctor {
        mage,
        findings: Vec::new(),
        seen: HashSet::new(),
        refs: HashMap::new(),
        known: Default::default(),
    };
    let profile = std::env::var_os("MQ_PROFILE").is_some();
    let phase = |label: &str, d: &mut Doctor, check: &dyn Fn(&mut Doctor)| {
        let since = std::time::Instant::now();
        check(d);
        if profile {
            eprintln!("[prof] doctor {label}: {:.2}ms", since.elapsed().as_secs_f64() * 1000.0);
        }
    };
    phase("build di index", &mut d, &|d| {
        d.mage.di_index();
    });
    phase("build event index", &mut d, &|d| {
        d.mage.events(Area::Global);
    });
    phase("build graphql/webapi", &mut d, &|d| {
        d.mage.graphql_types(None);
        d.mage.webapi(None);
        d.mage.console_commands(None);
    });
    phase("index refs", &mut d, &|d| d.index_refs());
    phase("module set", &mut d, &|d| d.check_module_set());
    phase("disabled modules", &mut d, &|d| d.check_disabled_modules());
    phase("sequence cycles", &mut d, &|d| d.check_sequence_cycles());
    phase("di", &mut d, &|d| d.check_di());
    phase("duplicate keys", &mut d, &|d| d.check_duplicate_keys());
    phase("observers", &mut d, &|d| d.check_observers());
    phase("cron", &mut d, &|d| d.check_cron());
    phase("webapi", &mut d, &|d| d.check_webapi());
    phase("commands", &mut d, &|d| d.check_commands());
    phase("mq", &mut d, &|d| d.check_mq());
    phase("graphql", &mut d, &|d| d.check_graphql());
    phase("templates", &mut d, &|d| d.check_templates());
    phase("unregistered", &mut d, &|d| d.check_unregistered(source_filter));

    let mut report = DoctorReport { findings: d.findings };
    if let Some(wanted) = source_filter {
        report.findings.retain(|f| finding_source(mage, f).is_none_or(|s| s == wanted));
    }
    report.sort();
    report
}

/// Dedup key: the same declaration surfaces in every area that inherits it.
type SeenKey = (DoctorLint, String, Option<(PathBuf, u32)>);

struct Doctor<'a> {
    mage: &'a Magento,
    findings: Vec<DoctorFinding>,
    seen: HashSet<SeenKey>,
    /// Every place the merged config asks for a class or virtual type, keyed by that
    /// name (`\Proxy` stripped): the reverse edges [`Doctor::demand`] walks.
    refs: HashMap<ClassName, Vec<Ref>>,
    /// [`Doctor::class_known`] answers: every area repeats the same references, and each
    /// answer costs file probes.
    known: std::cell::RefCell<HashMap<ClassName, bool>>,
}

/// One place the merged config asks for a class or virtual type by name.
#[derive(Clone, PartialEq, Eq)]
enum Ref {
    /// An `xsi:type="object"` argument (or array item) of `target`'s `$arg`.
    Arg { target: ClassName, arg: String, source: Source },
    /// `<preference for=".." type="name"/>`.
    Preference { for_: ClassName, source: Source },
    /// `<virtualType name="virtual_type" type="name"/>`.
    VirtualBase { virtual_type: ClassName, source: Source },
    /// An enabled `<plugin type="name"/>` on `target`.
    Plugin { target: ClassName, name: String, source: Source },
    /// Something Magento builds by itself: an observer, cron job, command, service, resolver.
    EntryPoint { what: String, source: Source },
}

/// Whether an object of some type can be built, and why (see [`DoctorFinding::chain`]).
struct Reach {
    live: bool,
    chain: Vec<ChainNode>,
}

impl Reach {
    fn live(chain: Vec<ChainNode>) -> Self {
        Reach { live: true, chain }
    }
    fn dead(chain: Vec<ChainNode>) -> Self {
        Reach { live: false, chain }
    }
}

fn node(message: String, source: Option<Source>, children: Vec<ChainNode>) -> ChainNode {
    ChainNode { message, source, children }
}

/// How many references deep [`Doctor::demand`] follows before giving up — and calling the
/// class live, since doctor must not hide an error it couldn't rule out.
const MAX_DEPTH: usize = 8;

impl Doctor<'_> {
    fn push(
        &mut self,
        lint: DoctorLint,
        severity: Severity,
        message: String,
        source: Option<Source>,
        subject: Option<String>,
    ) {
        self.push_with_chain(lint, severity, message, source, subject, Vec::new());
    }

    fn push_with_chain(
        &mut self,
        lint: DoctorLint,
        severity: Severity,
        message: String,
        source: Option<Source>,
        subject: Option<String>,
        chain: Vec<ChainNode>,
    ) {
        let key = (lint, message.clone(), source.as_ref().map(|s| (s.file.clone(), s.line)));
        if self.seen.insert(key) {
            let code = lint.code();
            self.findings.push(DoctorFinding { lint, code, severity, message, source, subject, chain });
        }
    }

    /// A reference to the missing class `missing`: an error when `reach` found a live
    /// path to it, else a warning (its package is installed, so the config is stale) or an
    /// info (no installed package provides it: an optional integration).
    fn missing_class(
        &mut self,
        lint: DoctorLint,
        missing: &ClassName,
        message: String,
        source: Source,
        reach: Reach,
    ) {
        let severity = if reach.live {
            Severity::Error
        } else if self.mage.index.resolver.provider_prefix(missing).is_some() {
            Severity::Warning
        } else {
            Severity::Info
        };
        self.push_with_chain(
            lint,
            severity,
            message,
            Some(source),
            Some(missing.to_string()),
            reach.chain,
        );
    }

    fn error(&mut self, lint: DoctorLint, message: String, source: Option<Source>) {
        self.push(lint, Severity::Error, message, source, None);
    }

    fn warn(&mut self, lint: DoctorLint, message: String, source: Option<Source>) {
        self.push(lint, Severity::Warning, message, source, None);
    }

    /// Like [`error`](Self::error)/[`warn`](Self::warn) with the finding's subject.
    fn error_on(
        &mut self,
        lint: DoctorLint,
        subject: impl std::fmt::Display,
        message: String,
        source: Option<Source>,
    ) {
        self.push(lint, Severity::Error, message, source, Some(subject.to_string()));
    }

    fn warn_on(
        &mut self,
        lint: DoctorLint,
        subject: impl std::fmt::Display,
        message: String,
        source: Option<Source>,
    ) {
        self.push(lint, Severity::Warning, message, source, Some(subject.to_string()));
    }

    /// Whether a config-referenced class is resolvable: a real source file, a virtual
    /// type, or a class Magento *generates* on demand (`\Proxy`, `…Factory`,
    /// `\Interceptor` — verified against their base — and the `extension_attributes.xml`
    /// `…Extension`/`…ExtensionInterface` classes, accepted as-is). Unsure = known:
    /// doctor must not cry wolf.
    fn class_known(&self, class: &ClassName) -> bool {
        if let Some(&known) = self.known.borrow().get(class) {
            return known;
        }
        let known = self.class_known_uncached(class);
        self.known.borrow_mut().insert(class.clone(), known);
        known
    }

    fn class_known_uncached(&self, class: &ClassName) -> bool {
        let name = class.as_str();
        if name.is_empty() {
            return true; // a missing attribute, not a missing class
        }
        if !name.contains('\\') {
            return true; // global namespace = a PHP built-in (DateTime, ArrayIterator, …)
        }
        if self.is_virtual_type(class) || self.mage.index.resolver.exists(class) {
            return true;
        }
        if !self.mage.index.resolver.has_prefix_for(class) {
            // No autoload prefix covers this namespace: the class may live in a classmap
            // (installed.json names only its dirs) — unverifiable, so assume it exists.
            return true;
        }
        if let Some(base) = name.strip_suffix("\\Proxy").or_else(|| name.strip_suffix("\\Interceptor")) {
            return self.class_known(&ClassName::new(base));
        }
        if let Some(base) = name.strip_suffix("Factory") {
            if !base.is_empty() && !base.ends_with('\\') {
                return self.class_known(&ClassName::new(base));
            }
        }
        if name.ends_with("Extension") || name.ends_with("ExtensionInterface") {
            return true; // generated from extension_attributes.xml; not verifiable here
        }
        false
    }

    fn is_virtual_type(&self, class: &ClassName) -> bool {
        Area::ALL.iter().any(|&a| self.mage.di_index().config(a).virtual_types.contains_key(class))
    }

    // ---------- module set + sequence ----------

    fn check_module_set(&mut self) {
        let check = self.mage.module_check();
        for name in &check.in_config_not_on_disk {
            // Anchor the finding on the module's own line in config.php so editors can
            // point (and quick-fix) there.
            let config_php = self.mage.root().join("app/etc/config.php");
            let line = self
                .mage
                .read_source(&config_php)
                .ok()
                .and_then(|text| {
                    let needle = format!("'{name}'");
                    text.lines().position(|l| l.contains(&needle)).map(|i| i as u32 + 1)
                })
                .unwrap_or(0);
            let source = Source {
                module: crate::ids::ModuleName::new(name.as_str()),
                file: config_php,
                line,
                area: Area::Global,
            };
            self.error_on(
                DoctorLint::ModuleMissingOnDisk,
                name,
                format!("module {name} is enabled in config.php but has no module.xml on disk"),
                Some(source),
            );
        }
        for m in &check.on_disk_not_in_config {
            self.warn_on(
                DoctorLint::ModuleNotRegistered,
                &m.name,
                format!(
                    "module {} exists on disk ({}) but is not in config.php; run `bin/magento setup:upgrade`",
                    m.name,
                    m.path.display()
                ),
                None,
            );
        }
    }

    // ---------- disabled modules ----------

    /// Installed modules switched off in config.php, and enabled DI config that
    /// reaches into them. A disabled module's code is still on the autoloader,
    /// so whether it takes part — in a class hierarchy, a plugin list, a
    /// compile — depends on which tool you ask; `setup:di:compile`, the runtime
    /// and static compilers disagree at exactly these edges. Removing the module
    /// with composer leaves nothing to disagree about. Warnings: disabling is
    /// common (MSI especially) and works; `--deny` makes them blocking.
    fn check_disabled_modules(&mut self) {
        let disabled: Vec<&Module> = self.mage.modules().iter().filter(|m| !m.enabled).collect();
        if disabled.is_empty() {
            return;
        }
        let config_php = self.mage.root().join("app/etc/config.php");
        let config_text = self.mage.read_source(&config_php).ok().unwrap_or_default();
        for m in &disabled {
            let needle = format!("'{}'", m.name);
            let line = config_text
                .lines()
                .position(|l| l.contains(&needle))
                .map_or(0, |i| i as u32 + 1);
            let source = Source {
                module: m.name.clone(),
                file: config_php.clone(),
                line,
                area: Area::Global,
            };
            self.warn_on(
                DoctorLint::ModuleDisabled,
                &m.name,
                format!(
                    "module {} is installed but disabled in config.php; remove it with composer \
(`composer remove`, or `replace` for Magento's own modules) instead",
                    m.name
                ),
                Some(source),
            );
        }

        // Which disabled module owns a class: by its file, longest module path
        // first (bundled-module packages nest), as `whatis` does. Only classes
        // whose vendor segment matches a disabled module's are looked up, so
        // the stat calls stay few.
        let vendors: HashSet<&str> =
            disabled.iter().filter_map(|m| m.name.as_str().split('_').next()).collect();
        let mut owner_cache: HashMap<ClassName, Option<String>> = HashMap::new();
        let mut owner = |class: &ClassName| -> Option<String> {
            if let Some(hit) = owner_cache.get(class) {
                return hit.clone();
            }
            let vendor = class.as_str().split('\\').next().unwrap_or("");
            let hit = vendors
                .contains(vendor)
                .then(|| self.mage.index.resolver.file_for(class))
                .flatten()
                .and_then(|file| {
                    disabled
                        .iter()
                        .filter(|m| file.starts_with(&m.path))
                        .max_by_key(|m| m.path.as_os_str().len())
                        .map(|m| m.name.to_string())
                });
            owner_cache.insert(class.clone(), hit.clone());
            hit
        };

        // (class referenced, how, where) for every DI reference to a class.
        let mut refs: Vec<(ClassName, String, Source)> = Vec::new();
        for &area in Area::ALL.iter() {
            let cfg = self.mage.di_index().config(area);
            for (for_, located) in &cfg.preferences {
                refs.push((for_.clone(), "declares a preference for".into(), located.source.clone()));
                refs.push((located.value.clone(), "prefers".into(), located.source.clone()));
            }
            for (name, vt) in &cfg.virtual_types {
                refs.push((vt.value.clone(), format!("bases virtual type {name} on"), vt.source.clone()));
            }
            for (target, by_name) in &cfg.plugins {
                for (pname, lp) in by_name {
                    if lp.disabled.unwrap_or(false) {
                        continue;
                    }
                    refs.push((target.clone(), format!("declares plugin `{pname}` on"), lp.source.clone()));
                    if let Some(class) = &lp.class {
                        refs.push((class.clone(), format!("uses as plugin `{pname}`"), lp.source.clone()));
                    }
                }
            }
            for (target, args) in &cfg.type_args {
                for (arg_name, la) in args {
                    let mut objects = HashSet::new();
                    collect_arg_objects(&la.value, &mut objects);
                    for class in objects {
                        refs.push((class, format!("injects into {target} ${arg_name}"), la.source.clone()));
                    }
                }
            }
        }
        for (class, how, source) in refs {
            let class = ClassName::new(class.as_str().trim_end_matches("\\Proxy"));
            let Some(module) = owner(&class) else { continue };
            // A disabled module's own files are never merged, so the declarer is
            // always enabled; still, never blame a module for its own classes.
            if source.module.as_str() == module {
                continue;
            }
            self.warn_on(
                DoctorLint::DisabledModuleReferenced,
                &class,
                format!("{} {how} {class}, a class of disabled module {module}", source.module),
                Some(source),
            );
        }
    }

    fn check_sequence_cycles(&mut self) {
        let modules = &self.mage.index.modules;
        let index: HashMap<&str, &Module> =
            modules.iter().map(|m| (m.name.as_str(), m)).collect();

        // Iterative DFS, 0 = unvisited, 1 = in progress, 2 = done.
        let mut state: HashMap<&str, u8> = HashMap::new();
        let mut reported: HashSet<String> = HashSet::new();
        for m in modules.iter().filter(|m| m.enabled) {
            let mut stack: Vec<&str> = vec![m.name.as_str()];
            let mut path: Vec<&str> = Vec::new();
            while let Some(&cur) = stack.last() {
                match state.get(cur).copied().unwrap_or(0) {
                    0 => {
                        state.insert(cur, 1);
                        path.push(cur);
                        if let Some(module) = index.get(cur) {
                            for dep in &module.sequence {
                                match state.get(dep.as_str()).copied().unwrap_or(0) {
                                    1 => {
                                        // Back-edge: reconstruct the cycle from `path`.
                                        let start =
                                            path.iter().position(|&p| p == dep.as_str()).unwrap_or(0);
                                        let mut cycle: Vec<&str> = path[start..].to_vec();
                                        cycle.push(dep.as_str());
                                        // Canonical form so each cycle reports once.
                                        let mut canon: Vec<&str> = cycle.clone();
                                        canon.sort_unstable();
                                        canon.dedup();
                                        if reported.insert(canon.join(",")) {
                                            self.error(
                                                DoctorLint::SequenceCycle,
                                                format!(
                                                    "module <sequence> cycle: {}",
                                                    cycle.join(" → ")
                                                ),
                                                None,
                                            );
                                        }
                                    }
                                    0 => stack.push(dep.as_str()),
                                    _ => {}
                                }
                            }
                        }
                    }
                    1 => {
                        state.insert(cur, 2);
                        stack.pop();
                        path.pop();
                    }
                    _ => {
                        stack.pop();
                    }
                }
            }
        }
    }

    // ---------- DI ----------

    fn check_di(&mut self) {
        for &area in Area::ALL.iter() {
            let cfg = self.mage.di_index().config(area);

            for (for_, located) in &cfg.preferences {
                if !self.class_known(&located.value) {
                    let reach = self.buildable(for_, &mut Vec::new());
                    self.missing_class(
                        DoctorLint::PreferenceTargetMissing,
                        &located.value,
                        format!("preference for {for_} points at missing class {}", located.value),
                        located.source.clone(),
                        reach,
                    );
                }
                // Cycle: follow to fixpoint; revisiting a node in this walk = a loop.
                let mut walk = HashSet::new();
                let mut cur = for_.clone();
                while let Some(next) = cfg.preferences.get(&cur) {
                    if next.value == cur {
                        break;
                    }
                    if !walk.insert(cur.clone()) {
                        self.error(
                            DoctorLint::PreferenceCycle,
                            format!("preference cycle involving {for_}"),
                            Some(located.source.clone()),
                        );
                        break;
                    }
                    cur = next.value.clone();
                }
            }

            for (name, vt) in &cfg.virtual_types {
                if !self.class_known(&vt.value) {
                    let reach = self.demand(name, &mut Vec::new());
                    self.missing_class(
                        DoctorLint::VirtualTypeBaseMissing,
                        &vt.value,
                        format!("virtual type {name} is based on missing class {}", vt.value),
                        vt.source.clone(),
                        reach,
                    );
                }
                let mut walk = HashSet::new();
                let mut cur = name.clone();
                while let Some(next) = cfg.virtual_types.get(&cur) {
                    if !walk.insert(cur.clone()) {
                        self.error(
                            DoctorLint::VirtualTypeCycle,
                            format!("virtual type cycle involving {name}"),
                            Some(vt.source.clone()),
                        );
                        break;
                    }
                    cur = next.value.clone();
                }
            }

            for (target, by_name) in &cfg.plugins {
                for (pname, lp) in by_name {
                    if lp.disabled.unwrap_or(false) {
                        continue; // never instantiated
                    }
                    if let Some(class) = &lp.class {
                        if !self.class_known(class) {
                            let reach = self.buildable(target, &mut Vec::new());
                            self.missing_class(
                                DoctorLint::PluginClassMissing,
                                class,
                                format!("plugin `{pname}` uses missing class {class}"),
                                lp.source.clone(),
                                reach,
                            );
                        }
                    }
                }
            }

            for (target, args) in &cfg.type_args {
                for (arg_name, la) in args {
                    self.check_arg_value(&la.value, &la.source, target, arg_name);
                }
            }
        }
    }

    /// Keys declared twice in one file never reach the merged config, so they come from
    /// the DI build's own record. An error: one of the two declarations is silently lost
    /// (a console command vanishing from `bin/magento`, a column from a grid).
    fn check_duplicate_keys(&mut self) {
        // One finding per key: a file that re-sets one argument for every payment method
        // reads as one bug, not seventy.
        let mut groups: Vec<Vec<&crate::engine::di::DuplicateKey>> = Vec::new();
        for dup in self.mage.di_index().duplicate_keys() {
            let same = |g: &Vec<&crate::engine::di::DuplicateKey>| {
                let first = g[0];
                (first.source.file == dup.source.file)
                    && first.target == dup.target
                    && first.path == dup.path
                    && first.key == dup.key
            };
            match groups.iter_mut().find(|g| same(g)) {
                Some(g) => g.push(dup),
                None => groups.push(vec![dup]),
            }
        }
        const SHOWN: usize = 5;
        for group in groups {
            let first = group[0];
            let last = group[group.len() - 1];
            let what = if first.path.is_empty() {
                format!("argument ${}", first.key)
            } else {
                format!("item `{}` in argument ${}", first.key, first.path)
            };
            let times = if group.len() == 1 { "twice".to_string() } else { format!("{} times", group.len() + 1) };
            let dropped = |(value, line): &(String, u32)| {
                node(
                    format!("declared as {value}: dropped"),
                    Some(Source { line: *line, ..first.source.clone() }),
                    vec![],
                )
            };
            let mut chain: Vec<ChainNode> = group.iter().take(SHOWN).map(|d| dropped(&d.replaced)).collect();
            if group.len() > SHOWN {
                chain.push(node(format!("… {} more dropped", group.len() - SHOWN), None, vec![]));
            }
            chain.push(node(format!("declared as {}: kept", last.winner), Some(last.source.clone()), vec![]));
            self.push_with_chain(
                DoctorLint::DuplicateArgumentKey,
                Severity::Error,
                format!(
                    "{what} of {} is declared {times} in one file; only the last one is used",
                    first.target
                ),
                Some(last.source.clone()),
                Some(first.key.clone()),
                chain,
            );
        }
    }

    fn check_arg_value(
        &mut self,
        value: &ArgValue,
        source: &Source,
        target: &ClassName,
        arg_name: &str,
    ) {
        match value {
            ArgValue::Object(o) => {
                let c = &o.class;
                if !self.class_known(c) {
                    let reach = self.arg_reach(target, arg_name, &mut Vec::new());
                    self.missing_class(
                        DoctorLint::DiArgumentClassMissing,
                        c,
                        format!("di argument ${arg_name} of {target} injects missing class {c}"),
                        source.clone(),
                        reach,
                    );
                }
            }
            ArgValue::Array(items) => {
                for item in items {
                    self.check_arg_value(&item.value, &item.source, target, arg_name);
                }
            }
            _ => {}
        }
    }

    // ---------- reachability: can anything live build it? ----------

    /// Index every reference the merged config makes to a class or virtual type: DI
    /// (arguments, preferences, virtual-type bases, plugins) across all areas, plus the
    /// entry points Magento builds on its own.
    fn index_refs(&mut self) {
        let mage = self.mage;
        let mut refs: Vec<(ClassName, Ref)> = Vec::new();
        for &area in Area::ALL.iter() {
            let cfg = mage.di_index().config(area);
            for (for_, located) in &cfg.preferences {
                let source = located.source.clone();
                refs.push((located.value.clone(), Ref::Preference { for_: for_.clone(), source }));
            }
            for (name, vt) in &cfg.virtual_types {
                let source = vt.source.clone();
                refs.push((vt.value.clone(), Ref::VirtualBase { virtual_type: name.clone(), source }));
            }
            for (target, by_name) in &cfg.plugins {
                for (name, lp) in by_name {
                    let Some(class) = lp.class.as_ref().filter(|_| !lp.disabled.unwrap_or(false)) else {
                        continue;
                    };
                    let (target, name, source) = (target.clone(), name.clone(), lp.source.clone());
                    refs.push((class.clone(), Ref::Plugin { target, name, source }));
                }
            }
            for (target, args) in &cfg.type_args {
                for (arg, la) in args {
                    let mut objects = Vec::new();
                    collect_arg_refs(&la.value, &la.source, &mut objects);
                    for (class, source) in objects {
                        let (target, arg) = (target.clone(), arg.clone());
                        refs.push((class, Ref::Arg { target, arg, source }));
                    }
                }
            }
            for (event, _) in mage.events(area) {
                for o in mage.observers(&event, area).into_iter().filter(|o| !o.disabled) {
                    let what = format!("observer `{}` on event {event}", o.name);
                    refs.push((o.instance, Ref::EntryPoint { what, source: o.source }));
                }
            }
        }
        for job in mage.cron_jobs(None, false).map(|c| c.jobs).unwrap_or_default() {
            let what = format!("cron job {}/{}", job.group, job.name);
            refs.push((job.instance, Ref::EntryPoint { what, source: job.source }));
        }
        for cmd in mage.console_commands(None) {
            let what = format!("console command `{}`", cmd.name.as_deref().unwrap_or(&cmd.item_key));
            refs.push((cmd.class, Ref::EntryPoint { what, source: cmd.source }));
        }
        for route in mage.webapi(None) {
            let what = format!("webapi {} {}", route.method, route.url);
            refs.push((route.service_class, Ref::EntryPoint { what, source: route.source }));
        }
        for t in mage.graphql_types(None) {
            if let Some(tr) = t.type_resolver {
                let what = format!("GraphQL type {}", t.name);
                refs.push((tr, Ref::EntryPoint { what, source: t.source.clone() }));
            }
            for f in t.fields {
                if let Some(r) = f.resolver {
                    let what = format!("GraphQL {}.{}", t.name, f.name);
                    refs.push((r, Ref::EntryPoint { what, source: f.source }));
                }
            }
        }

        // The same declaration surfaces once per area that inherits it.
        for (class, r) in refs {
            let name = class.as_str().strip_suffix("\\Proxy").unwrap_or(class.as_str());
            let entry = self.refs.entry(ClassName::new(name)).or_default();
            if !entry.contains(&r) {
                entry.push(r);
            }
        }
    }

    /// Whether anything that can itself be built asks for `name`. Live: the chain is the
    /// first live path found. Dead: the chain holds every dead end. Unsure (too deep)
    /// counts as live — doctor must not hide an error it couldn't rule out.
    fn demand(&self, name: &ClassName, path: &mut Vec<ClassName>) -> Reach {
        if path.len() >= MAX_DEPTH {
            return Reach::live(vec![node(format!("{name}: too deep to follow"), None, vec![])]);
        }
        if path.contains(name) {
            return Reach::dead(vec![node(format!("{name}: back on its own path (a cycle)"), None, vec![])]);
        }
        let refs = self.refs.get(name).cloned().unwrap_or_default();
        if refs.is_empty() {
            return Reach::dead(vec![node(format!("nothing in the merged config asks for {name}"), None, vec![])]);
        }
        path.push(name.clone());
        let mut dead = Vec::new();
        let mut found = None;
        for r in &refs {
            let reach = self.ref_reach(r, path);
            if reach.live {
                found = Some(reach.chain);
                break;
            }
            dead.extend(reach.chain);
        }
        path.pop();
        match found {
            Some(chain) => Reach::live(chain),
            None => Reach::dead(dead),
        }
    }

    /// Whether the reference `r` is live: one chain node describing it, with the reason
    /// underneath.
    fn ref_reach(&self, r: &Ref, path: &mut Vec<ClassName>) -> Reach {
        let (message, source, reach) = match r {
            Ref::Arg { target, arg, source } => (
                format!("injected as ${arg} into {target}"),
                source,
                self.arg_reach(target, arg, path),
            ),
            Ref::Preference { for_, source } => {
                (format!("preference for {for_}"), source, self.buildable(for_, path))
            }
            Ref::VirtualBase { virtual_type, source } => (
                format!("base of virtual type {virtual_type}"),
                source,
                self.demand(virtual_type, path),
            ),
            Ref::Plugin { target, name, source } => {
                (format!("plugin `{name}` on {target}"), source, self.buildable(target, path))
            }
            Ref::EntryPoint { what, source } => {
                return Reach::live(vec![node(what.clone(), Some(source.clone()), vec![])]);
            }
        };
        let chain = vec![node(message, Some(source.clone()), reach.chain)];
        Reach { live: reach.live, chain }
    }

    /// Whether an object of `ty` can be built. An existing class can be — anything may
    /// ask the object manager for it. A virtual type exists only when something asks for
    /// it by name, so it's as live as its [`demand`](Self::demand).
    fn buildable(&self, ty: &ClassName, path: &mut Vec<ClassName>) -> Reach {
        if self.is_virtual_type(ty) {
            let concrete = self.concrete_of(ty);
            if !self.class_known(&concrete) {
                let message = format!("virtual type {ty}'s base {}", self.missing_note(&concrete));
                return Reach::dead(vec![node(message, self.virtual_type_source(ty), vec![])]);
            }
            return self.demand(ty, path);
        }
        if self.mage.index.resolver.exists(ty) {
            return Reach::live(vec![node(format!("{ty} exists"), self.class_source(ty), vec![])]);
        }
        if self.class_known(ty) {
            return Reach::live(vec![node(format!("{ty} is generated or can't be verified"), None, vec![])]);
        }
        Reach::dead(vec![node(self.missing_note(ty), None, vec![])])
    }

    /// Whether `target` is built *and* takes `$arg`: an argument the constructor doesn't
    /// declare is never passed. (Arguments are also inherited by subclasses, which this
    /// can't enumerate — so an abstract target, or an unreadable constructor, counts as
    /// taking it.)
    fn arg_reach(&self, target: &ClassName, arg: &str, path: &mut Vec<ClassName>) -> Reach {
        let reach = self.buildable(target, path);
        if !reach.live {
            return reach;
        }
        let concrete = self.concrete_of(target);
        let abstract_ = self
            .mage
            .index
            .resolver
            .header_of(&concrete)
            .is_some_and(|h| h.is_abstract || h.is_interface);
        match self.mage.index.resolver.constructor_params(&concrete) {
            Some(params) if !abstract_ && !params.iter().any(|p| p == arg) => {
                let message = format!("{concrete}'s constructor has no ${arg} parameter");
                Reach::dead(vec![node(message, self.class_source(&concrete), vec![])])
            }
            Some(_) if !abstract_ => {
                let mut chain = reach.chain;
                chain.push(node(format!("{concrete}'s constructor takes ${arg}"), None, vec![]));
                Reach::live(chain)
            }
            _ => reach,
        }
    }

    /// The class a (possibly virtual) type is ultimately built from.
    fn concrete_of(&self, ty: &ClassName) -> ClassName {
        let mut cur = ty.clone();
        let mut seen = HashSet::new();
        while let Some(base) = Area::ALL
            .iter()
            .find_map(|&a| self.mage.di_index().config(a).virtual_types.get(&cur))
        {
            if !seen.insert(cur.clone()) {
                break;
            }
            cur = base.value.clone();
        }
        cur
    }

    fn virtual_type_source(&self, ty: &ClassName) -> Option<Source> {
        Area::ALL
            .iter()
            .find_map(|&a| self.mage.di_index().config(a).virtual_types.get(ty))
            .map(|vt| vt.source.clone())
    }

    /// Where a class is declared: its file, at the type declaration's line.
    fn class_source(&self, class: &ClassName) -> Option<Source> {
        let resolver = &self.mage.index.resolver;
        let file = resolver.file_for(class)?;
        let line = resolver.header_of(class).map_or(0, |h| h.decl_line);
        // Longest module-path prefix wins (bundled-module packages nest under one root).
        let module = self
            .mage
            .index
            .modules
            .iter()
            .filter(|m| file.starts_with(&m.path))
            .max_by_key(|m| m.path.as_os_str().len())
            .map(|m| m.name.clone())
            .unwrap_or_else(|| crate::ids::ModuleName::new(""));
        Some(Source { module, file, line, area: Area::Global })
    }

    /// "`X` does not exist", with whether its package is installed: the line that decides
    /// between stale config and an optional integration.
    fn missing_note(&self, class: &ClassName) -> String {
        match self.mage.index.resolver.provider_prefix(class) {
            Some(prefix) => format!(
                "{class} does not exist (its package {} is installed: stale config)",
                prefix.trim_end_matches('\\')
            ),
            None => {
                let ns: Vec<&str> = class.as_str().splitn(3, '\\').take(2).collect();
                format!(
                    "{class} does not exist (no installed package provides {}: an optional integration)",
                    ns.join("\\")
                )
            }
        }
    }

    // ---------- breadth indexes ----------

    fn check_observers(&mut self) {
        for &area in Area::ALL.iter() {
            for (event, _) in self.mage.events(area) {
                for o in self.mage.observers(&event, area) {
                    if !o.disabled && !self.class_known(&o.instance) {
                        self.error_on(
                            DoctorLint::ObserverClassMissing,
                            &o.instance,
                            format!(
                                "observer `{}` on event {event} uses missing class {}",
                                o.name, o.instance
                            ),
                            Some(o.source.clone()),
                        );
                    }
                }
            }
        }
    }

    /// Template references no physical file satisfies: neither the module's
    /// view/<area>|base/templates file nor any theme override exists. The template
    /// index already normalized short paths and joined candidates, so this is a pure
    /// projection; non-.phtml values (dynamic expressions) are skipped, never guessed.
    fn check_templates(&mut self) {
        for area in [Area::Frontend, Area::Adminhtml] {
            for template in self.mage.templates(area, None) {
                if !template.files.is_empty()
                    || !template.reference.ends_with(".phtml")
                    || template.usages.is_empty()
                {
                    continue;
                }
                let source = template.usages[0].source.clone();
                let count = template.usages.len();
                let suffix = if count > 1 {
                    format!(" ({count} usages)")
                } else {
                    String::new()
                };
                self.warn_on(
                    DoctorLint::TemplateFileMissing,
                    &template.reference,
                    format!(
                        "layout assigns template {} but no module or theme file provides it{suffix}",
                        template.reference
                    ),
                    Some(source),
                );
            }
        }
    }

    fn check_cron(&mut self) {
        // Static call — can't fail; a hypothetical error just skips the lint.
        for job in self.mage.cron_jobs(None, false).map(|c| c.jobs).unwrap_or_default() {
            if !self.class_known(&job.instance) {
                self.error_on(
                    DoctorLint::CronInstanceMissing,
                    &job.instance,
                    format!(
                        "cron job {}/{} uses missing class {}",
                        job.group, job.name, job.instance
                    ),
                    Some(job.source.clone()),
                );
            }
        }
    }

    fn check_webapi(&mut self) {
        let acl_known: HashSet<String> =
            self.mage.acl(None).into_iter().map(|r| r.id).collect();
        for route in self.mage.webapi(None) {
            if !self.class_known(&route.service_class) {
                self.error_on(
                    DoctorLint::WebapiServiceMissing,
                    &route.service_class,
                    format!(
                        "webapi {} {} uses missing service class {}",
                        route.method, route.url, route.service_class
                    ),
                    Some(route.source.clone()),
                );
            }
            for res in &route.resources {
                if res != "anonymous" && res != "self" && !acl_known.contains(res) {
                    self.error_on(
                        DoctorLint::AclResourceUnknown,
                        res,
                        format!(
                            "webapi {} {} requires ACL resource {res}, which no acl.xml declares",
                            route.method, route.url
                        ),
                        Some(route.source.clone()),
                    );
                }
            }
        }
    }

    fn check_commands(&mut self) {
        for cmd in self.mage.console_commands(None) {
            if !self.class_known(&cmd.class) {
                let label = cmd.name.as_deref().unwrap_or(&cmd.item_key);
                self.error_on(
                    DoctorLint::CommandClassMissing,
                    &cmd.class,
                    format!("console command `{label}` uses missing class {}", cmd.class),
                    Some(cmd.source.clone()),
                );
            }
        }
    }

    fn check_mq(&mut self) {
        for topic in self.mage.queue_topics(None) {
            for h in &topic.handlers {
                if !h.disabled && !self.class_known(&h.class) {
                    self.error(
                        DoctorLint::MqHandlerMissing,
                        format!(
                            "queue topic {} handler `{}` uses missing class {}",
                            topic.name, h.name, h.class
                        ),
                        Some(h.source.clone()),
                    );
                }
            }
            if let Some(route) = self.mage.queue_topic(&topic.name) {
                for r in &route.routes {
                    if r.consumers.is_empty() {
                        self.warn(
                            DoctorLint::QueueNoConsumer,
                            format!(
                                "queue {} (topic {}) has no consumer reading it",
                                r.queue, topic.name
                            ),
                            Some(topic.source.clone()),
                        );
                    }
                    for c in &r.consumers {
                        if let Some(handler) = &c.handler {
                            let class = handler.split_once("::").map(|(c, _)| c).unwrap_or(handler);
                            if !self.class_known(&ClassName::new(class)) {
                                self.error(
                                    DoctorLint::MqHandlerMissing,
                                    format!(
                                        "queue consumer `{}` uses missing handler class {class}",
                                        c.name
                                    ),
                                    Some(c.source.clone()),
                                );
                            }
                        }
                        if let Some(inst) = &c.consumer_instance {
                            if !self.class_known(inst) {
                                self.error(
                                    DoctorLint::MqHandlerMissing,
                                    format!(
                                        "queue consumer `{}` uses missing consumerInstance {inst}",
                                        c.name
                                    ),
                                    Some(c.source.clone()),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    fn check_graphql(&mut self) {
        for t in self.mage.graphql_types(None) {
            if let Some(tr) = &t.type_resolver {
                if !self.class_known(tr) {
                    self.error(
                        DoctorLint::GraphqlResolverMissing,
                        format!("GraphQL type {} uses missing @typeResolver {tr}", t.name),
                        Some(t.source.clone()),
                    );
                }
            }
            for f in &t.fields {
                if let Some(r) = &f.resolver {
                    if !self.class_known(r) {
                        self.error(
                            DoctorLint::GraphqlResolverMissing,
                            format!("GraphQL {}.{} uses missing @resolver {r}", t.name, f.name),
                            Some(f.source.clone()),
                        );
                    }
                }
            }
        }
    }

    // ---------- unregistered code (exists on disk, wired nowhere) ----------

    fn check_unregistered(&mut self, source_filter: Option<ModuleSource>) {
        let started = std::time::Instant::now();
        // Registered sets, each widened by ancestors so a base class whose registered
        // child extends it is "covered" and never flagged.
        let mage = self.mage;
        let mut commands: HashSet<ClassName> = mage
            .console_commands(None)
            .into_iter()
            .map(|c| ClassName::new(c.class.as_str().strip_suffix("\\Proxy").unwrap_or(c.class.as_str())))
            .collect();
        let mut observers: HashSet<ClassName> = Area::ALL
            .iter()
            .flat_map(|&a| {
                mage.events(a)
                    .into_iter()
                    .flat_map(move |(e, _)| mage.observers(&e, a))
                    .map(|o| o.instance)
            })
            .collect();
        let mut plugins: HashSet<ClassName> = Area::ALL
            .iter()
            .flat_map(|&a| {
                mage.di_index().config(a).plugins.values().flat_map(|by_name| {
                    by_name.values().filter_map(|lp| lp.class.clone())
                })
            })
            .collect();
        for set in [&mut commands, &mut observers, &mut plugins] {
            // A registered entry may be a *virtual type* (Sales registers its grid
            // observers as virtualTypes of the observer classes): the real class it's
            // built on counts as registered.
            let bases: Vec<ClassName> = set
                .iter()
                .filter_map(|c| {
                    let mut cur = c.clone();
                    let mut seen = HashSet::new();
                    let mut was_virtual = false;
                    for &area in Area::ALL.iter() {
                        while let Some(vt) = mage.di_index().config(area).virtual_types.get(&cur) {
                            if !seen.insert(cur.clone()) {
                                break;
                            }
                            was_virtual = true;
                            cur = vt.value.clone();
                        }
                    }
                    was_virtual.then_some(cur)
                })
                .collect();
            set.extend(bases);
            // And a base class whose registered child extends it is covered too. Each
            // walk parses headers off disk, so they run in parallel.
            let classes: Vec<&ClassName> = set.iter().collect();
            let ancestors: Vec<ClassName> = classes
                .par_iter()
                .flat_map_iter(|c| mage.index.resolver.ancestors(c))
                .collect();
            set.extend(ancestors);
        }

        // A class referenced anywhere in DI — a preference target (e.g. a command swapped
        // in for a core one) or an injected argument object — is wired, just not through
        // the registry the lint checks. Never flag those.
        let mut di_covered: HashSet<ClassName> = HashSet::new();
        for &area in Area::ALL.iter() {
            let cfg = mage.di_index().config(area);
            for located in cfg.preferences.values() {
                insert_stripped(&mut di_covered, &located.value);
            }
            for args in cfg.type_args.values() {
                for la in args.values() {
                    collect_arg_objects(&la.value, &mut di_covered);
                }
            }
        }

        if std::env::var_os("MQ_PROFILE").is_some() {
            eprintln!("[prof] doctor unregistered registries: {:.2}ms", started.elapsed().as_secs_f64() * 1000.0);
        }
        let modules: Vec<&Module> = self
            .mage
            .index
            .modules
            .iter()
            .filter(|m| m.enabled)
            .filter(|m| source_filter.is_none_or(|s| m.source == s))
            .collect();

        // Each module's scan is file probes and header parses: independent, so in
        // parallel; collected in module order so the output stays deterministic.
        let registered = Registered { commands, observers, plugins, di_covered };
        let found: Vec<(DoctorLint, ClassName, String, Source)> = modules
            .par_iter()
            .flat_map_iter(|m| unregistered_in(mage, m, &registered))
            .collect();
        for (lint, class, message, source) in found {
            self.warn_on(lint, &class, message, Some(source));
        }
    }
}

/// The registries [`unregistered_in`] checks a module's classes against.
struct Registered {
    commands: HashSet<ClassName>,
    observers: HashSet<ClassName>,
    plugins: HashSet<ClassName>,
    di_covered: HashSet<ClassName>,
}

/// One module's command/observer/plugin classes that are wired nowhere.
fn unregistered_in(
    mage: &Magento,
    m: &Module,
    registered: &Registered,
) -> Vec<(DoctorLint, ClassName, String, Source)> {
    let resolver = &mage.index.resolver;
    let Registered { commands, observers, plugins, di_covered } = registered;
    let mut out = Vec::new();

    // Commands registered through the bootstrap-time CommandLocator have no di.xml
    // entry; skip modules using that mechanism.
    let has_cli_commands = m.path.join("cli_commands.php").is_file();
    if !has_cli_commands {
        for (class, path) in scan_classes(mage, m, "Console") {
            if commands.contains(&class) || di_covered.contains(&class) {
                continue;
            }
            let Some(h) = resolver.header_of(&class) else { continue };
            if h.is_interface || h.is_abstract {
                continue;
            }
            let is_command = resolver
                .ancestors(&class)
                .iter()
                .any(|a| a.as_str() == "Symfony\\Component\\Console\\Command\\Command");
            if is_command {
                let message = format!(
                    "{class} extends Symfony Command but is not registered on CommandListInterface in di.xml"
                );
                out.push((DoctorLint::CommandUnregistered, class, message, src(m, path, h.decl_line)));
            }
        }
    }

    for (class, path) in scan_classes(mage, m, "Observer") {
        if observers.contains(&class) || di_covered.contains(&class) {
            continue;
        }
        let Some(h) = resolver.header_of(&class) else { continue };
        if h.is_interface || h.is_abstract {
            continue;
        }
        let is_observer = resolver
            .ancestors(&class)
            .iter()
            .any(|a| a.as_str() == "Magento\\Framework\\Event\\ObserverInterface");
        if is_observer {
            let message = format!("{class} implements ObserverInterface but no events.xml registers it");
            out.push((DoctorLint::ObserverUnregistered, class, message, src(m, path, h.decl_line)));
        }
    }

    for (class, path) in scan_classes(mage, m, "Plugin") {
        if plugins.contains(&class) || di_covered.contains(&class) {
            continue;
        }
        let Some(h) = resolver.header_of(&class) else { continue };
        if h.is_interface || h.is_abstract {
            continue;
        }
        if !resolver.plugin_methods(&class).is_empty() {
            let message = format!(
                "{class} defines before/around/after methods but no di.xml declares it as a plugin"
            );
            out.push((DoctorLint::PluginUnregistered, class, message, src(m, path, h.decl_line)));
        }
    }
    out
}

/// PHP classes under `<module>/<subdir>/`, by the `Vendor_Module` → `Vendor\Module\`
/// namespace convention — each verified by resolving the candidate name back through
/// PSR-4 to the same file, so a module with a diverging namespace is skipped, never
/// misreported.
fn scan_classes(mage: &Magento, module: &Module, subdir: &str) -> Vec<(ClassName, PathBuf)> {
    let base = module.path.join(subdir);
    if !base.is_dir() {
        return Vec::new();
    }
    let ns = module.name.as_str().replace('_', "\\");
    let mut out = Vec::new();
    walk_php(&base, 0, &mut |path| {
        let Ok(rel) = path.strip_prefix(&module.path) else { return };
        let stem = rel.with_extension("");
        let mut class = ns.clone();
        for part in stem.components() {
            class.push('\\');
            class.push_str(&part.as_os_str().to_string_lossy());
        }
        let class = ClassName::new(class);
        match mage.index.resolver.file_for(&class) {
            Some(resolved) if same_file(&resolved, path) => out.push((class, path.to_path_buf())),
            _ => {}
        }
    });
    out
}

/// Whether a finding is declared in the project's own code or in an installed package:
/// by its file (anything under `vendor/` is a package's, the rest of the checkout the
/// project's), else by the module it is about. `None` = can't tell (kept by any filter).
fn finding_source(mage: &Magento, f: &DoctorFinding) -> Option<ModuleSource> {
    if let Some(source) = &f.source {
        let file = source.file.strip_prefix(mage.root()).unwrap_or(&source.file);
        let file = file.strip_prefix(".").unwrap_or(file);
        return Some(if file.starts_with("vendor") { ModuleSource::Vendor } else { ModuleSource::App });
    }
    let subject = f.subject.as_deref()?;
    let known = mage.index.modules.iter().find(|m| m.name.as_str() == subject).map(|m| m.source);
    known.or_else(|| {
        let check = mage.module_check();
        check.on_disk_not_in_config.iter().find(|m| m.name.as_str() == subject).map(|m| m.source)
    })
}

fn src(module: &Module, file: PathBuf, line: u32) -> Source {
    Source { module: module.name.clone(), file, line, area: Area::Global }
}

pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn insert_stripped(set: &mut HashSet<ClassName>, class: &ClassName) {
    let name = class.as_str().strip_suffix("\\Proxy").unwrap_or(class.as_str());
    set.insert(ClassName::new(name));
}

/// Every `xsi:type="object"` reference in an argument tree, each with the line declaring
/// it (an array item's own, else the argument's).
fn collect_arg_refs(value: &ArgValue, source: &Source, out: &mut Vec<(ClassName, Source)>) {
    match value {
        ArgValue::Object(o) => out.push((o.class.clone(), source.clone())),
        ArgValue::Array(items) => {
            for item in items {
                collect_arg_refs(&item.value, &item.source, out);
            }
        }
        _ => {}
    }
}

/// Collect every `xsi:type="object"` reference in an argument tree.
fn collect_arg_objects(value: &ArgValue, out: &mut HashSet<ClassName>) {
    match value {
        ArgValue::Object(o) => insert_stripped(out, &o.class),
        ArgValue::Array(items) => {
            for item in items {
                collect_arg_objects(&item.value, out);
            }
        }
        _ => {}
    }
}

/// Recursive `.php` walk, skipping test fixtures.
pub(crate) fn walk_php(dir: &Path, depth: usize, f: &mut impl FnMut(&Path)) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "Test" || name == "_files" {
                continue;
            }
            walk_php(&path, depth + 1, f);
        } else if path.extension().and_then(|e| e.to_str()) == Some("php") {
            f(&path);
        }
    }
}
