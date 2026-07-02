//! `doctor` — cross-index lints: the `modules --check` philosophy generalized. Every
//! check is a projection over indexes that already exist; doctor computes nothing new,
//! it just asks each index "and does what you reference actually exist?".
//!
//! Two severities, chosen so a healthy store is quiet and the exit code trustworthy:
//! * **Error** — config references something that doesn't exist, or is structurally
//!   broken (cycles, module set drift). These break at runtime.
//! * **Warning** — probably-forgotten wiring: classes that look like commands/observers/
//!   plugins but are registered nowhere, queues nothing consumes. Could be intentional
//!   dead code, so they never fail CI by themselves.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::error::Severity;
use crate::ids::{Area, ClassName};
use crate::model::{DoctorFinding, DoctorLint, DoctorReport, Module, ModuleSource};
use crate::source::Source;
use crate::Magento;

pub(crate) fn run(mage: &Magento, source_filter: Option<ModuleSource>) -> DoctorReport {
    let mut d = Doctor { mage, findings: Vec::new(), seen: HashSet::new() };

    d.check_module_set();
    d.check_sequence_cycles();
    d.check_di();
    d.check_observers();
    d.check_cron();
    d.check_webapi();
    d.check_commands();
    d.check_mq();
    d.check_graphql();

    d.check_unregistered(source_filter);

    // Errors first, then warnings; stable by message within a severity.
    d.findings.sort_by(|a, b| {
        let rank = |s: Severity| if s == Severity::Error { 0 } else { 1 };
        rank(a.severity).cmp(&rank(b.severity)).then_with(|| a.message.cmp(&b.message))
    });
    DoctorReport { findings: d.findings }
}

/// Dedup key: the same declaration surfaces in every area that inherits it.
type SeenKey = (DoctorLint, String, Option<(PathBuf, u32)>);

struct Doctor<'a> {
    mage: &'a Magento,
    findings: Vec<DoctorFinding>,
    seen: HashSet<SeenKey>,
}

impl Doctor<'_> {
    fn push(&mut self, lint: DoctorLint, severity: Severity, message: String, source: Option<Source>) {
        let key = (lint, message.clone(), source.as_ref().map(|s| (s.file.clone(), s.line)));
        if self.seen.insert(key) {
            self.findings.push(DoctorFinding { lint, severity, message, source });
        }
    }

    fn error(&mut self, lint: DoctorLint, message: String, source: Option<Source>) {
        self.push(lint, Severity::Error, message, source);
    }

    fn warn(&mut self, lint: DoctorLint, message: String, source: Option<Source>) {
        self.push(lint, Severity::Warning, message, source);
    }

    /// Whether a config-referenced class is resolvable: a real source file, a virtual
    /// type, or a class Magento *generates* on demand (`\Proxy`, `…Factory`,
    /// `\Interceptor` — verified against their base — and the `extension_attributes.xml`
    /// `…Extension`/`…ExtensionInterface` classes, accepted as-is). Unsure = known:
    /// doctor must not cry wolf.
    fn class_known(&self, class: &ClassName) -> bool {
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
            self.error(
                DoctorLint::ModuleMissingOnDisk,
                format!("module {name} is enabled in config.php but has no module.xml on disk"),
                None,
            );
        }
        for m in &check.on_disk_not_in_config {
            self.warn(
                DoctorLint::ModuleNotRegistered,
                format!(
                    "module {} exists on disk ({}) but is not in config.php — run `bin/magento setup:upgrade`",
                    m.name,
                    m.path.display()
                ),
                None,
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
                    self.error(
                        DoctorLint::PreferenceTargetMissing,
                        format!("preference for {for_} points at missing class {}", located.value),
                        Some(located.source.clone()),
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
                    self.error(
                        DoctorLint::VirtualTypeBaseMissing,
                        format!("virtual type {name} is based on missing class {}", vt.value),
                        Some(vt.source.clone()),
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

            for by_name in cfg.plugins.values() {
                for (pname, lp) in by_name {
                    if lp.disabled {
                        continue; // never instantiated
                    }
                    if let Some(class) = &lp.class {
                        if !self.class_known(class) {
                            self.error(
                                DoctorLint::PluginClassMissing,
                                format!("plugin `{pname}` uses missing class {class}"),
                                Some(lp.source.clone()),
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

    fn check_arg_value(
        &mut self,
        value: &crate::model::ArgValue,
        source: &Source,
        target: &ClassName,
        arg_name: &str,
    ) {
        use crate::model::ArgValue;
        match value {
            ArgValue::Object(c) => {
                if !self.class_known(c) {
                    self.error(
                        DoctorLint::DiArgumentClassMissing,
                        format!("di argument ${arg_name} of {target} injects missing class {c}"),
                        Some(source.clone()),
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

    // ---------- breadth indexes ----------

    fn check_observers(&mut self) {
        for &area in Area::ALL.iter() {
            for (event, _) in self.mage.events(area) {
                for o in self.mage.observers(&event, area) {
                    if !o.disabled && !self.class_known(&o.instance) {
                        self.error(
                            DoctorLint::ObserverClassMissing,
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

    fn check_cron(&mut self) {
        for job in self.mage.cron_jobs(None) {
            if !self.class_known(&job.instance) {
                self.error(
                    DoctorLint::CronInstanceMissing,
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
                self.error(
                    DoctorLint::WebapiServiceMissing,
                    format!(
                        "webapi {} {} uses missing service class {}",
                        route.method, route.url, route.service_class
                    ),
                    Some(route.source.clone()),
                );
            }
            for res in &route.resources {
                if res != "anonymous" && res != "self" && !acl_known.contains(res) {
                    self.error(
                        DoctorLint::AclResourceUnknown,
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
                self.error(
                    DoctorLint::CommandClassMissing,
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
            // And a base class whose registered child extends it is covered too.
            let ancestors: Vec<ClassName> =
                set.iter().flat_map(|c| mage.index.resolver.ancestors(c)).collect();
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

        let modules: Vec<&Module> = self
            .mage
            .index
            .modules
            .iter()
            .filter(|m| m.enabled)
            .filter(|m| source_filter.is_none_or(|s| m.source == s))
            .collect();

        for m in &modules {
            // Commands registered through the bootstrap-time CommandLocator have no
            // di.xml entry; skip modules using that mechanism.
            let has_cli_commands = m.path.join("cli_commands.php").is_file();
            if !has_cli_commands {
                for (class, path) in self.scan_classes(m, "Console") {
                    if commands.contains(&class) || di_covered.contains(&class) {
                        continue;
                    }
                    let Some(h) = self.mage.index.resolver.header_of(&class) else { continue };
                    if h.is_interface || h.is_abstract {
                        continue;
                    }
                    let is_command = self
                        .mage
                        .index
                        .resolver
                        .ancestors(&class)
                        .iter()
                        .any(|a| a.as_str() == "Symfony\\Component\\Console\\Command\\Command");
                    if is_command {
                        self.warn(
                            DoctorLint::CommandUnregistered,
                            format!(
                                "{class} extends Symfony Command but is not registered on CommandListInterface in di.xml"
                            ),
                            Some(src(m, path)),
                        );
                    }
                }
            }

            for (class, path) in self.scan_classes(m, "Observer") {
                if observers.contains(&class) || di_covered.contains(&class) {
                    continue;
                }
                let Some(h) = self.mage.index.resolver.header_of(&class) else { continue };
                if h.is_interface || h.is_abstract {
                    continue;
                }
                let is_observer = self
                    .mage
                    .index
                    .resolver
                    .ancestors(&class)
                    .iter()
                    .any(|a| a.as_str() == "Magento\\Framework\\Event\\ObserverInterface");
                if is_observer {
                    self.warn(
                        DoctorLint::ObserverUnregistered,
                        format!("{class} implements ObserverInterface but no events.xml registers it"),
                        Some(src(m, path)),
                    );
                }
            }

            for (class, path) in self.scan_classes(m, "Plugin") {
                if plugins.contains(&class) || di_covered.contains(&class) {
                    continue;
                }
                let Some(h) = self.mage.index.resolver.header_of(&class) else { continue };
                if h.is_interface || h.is_abstract {
                    continue;
                }
                if !self.mage.index.resolver.plugin_methods(&class).is_empty() {
                    self.warn(
                        DoctorLint::PluginUnregistered,
                        format!(
                            "{class} defines before/around/after methods but no di.xml declares it as a plugin"
                        ),
                        Some(src(m, path)),
                    );
                }
            }
        }
    }

    /// PHP classes under `<module>/<subdir>/`, by the `Vendor_Module` → `Vendor\Module\`
    /// namespace convention — each verified by resolving the candidate name back through
    /// PSR-4 to the same file, so a module with a diverging namespace is skipped, never
    /// misreported.
    fn scan_classes(&self, module: &Module, subdir: &str) -> Vec<(ClassName, PathBuf)> {
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
            match self.mage.index.resolver.file_for(&class) {
                Some(resolved) if same_file(&resolved, path) => out.push((class, path.to_path_buf())),
                _ => {}
            }
        });
        out
    }
}

fn src(module: &Module, file: PathBuf) -> Source {
    Source { module: module.name.clone(), file, line: 0, area: Area::Global }
}

fn same_file(a: &Path, b: &Path) -> bool {
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

/// Collect every `xsi:type="object"` reference in an argument tree.
fn collect_arg_objects(value: &crate::model::ArgValue, out: &mut HashSet<ClassName>) {
    use crate::model::ArgValue;
    match value {
        ArgValue::Object(c) => insert_stripped(out, c),
        ArgValue::Array(items) => {
            for item in items {
                collect_arg_objects(&item.value, out);
            }
        }
        _ => {}
    }
}

/// Recursive `.php` walk, skipping test fixtures.
fn walk_php(dir: &Path, depth: usize, f: &mut impl FnMut(&Path)) {
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
