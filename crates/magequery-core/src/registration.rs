//! What `setup:upgrade` registers besides schema, patches and config: themes and
//! message queues. `setup:db:status` checks neither, so a release that only adds
//! a theme or a queue looks up to date to it. Mage-OS added
//! `theme:registration:status` and `queue:config:status` for this; these are
//! native versions of the same checks.
//!
//! * Themes: the Theme module's `RecurringData` registers every theme a
//!   `registration.php` declares into the `theme` table. A theme on disk with no
//!   physical row there is missing (`RegistrationDetector`).
//! * Database queues: MysqlMq's `Recurring` inserts every queue of the topology
//!   config into the `queue` table, for every connection. A configured queue
//!   with no row is missing (MysqlMq's `ChangeDetector`).
//! * RabbitMQ: only the broker knows what Amqp's `Recurring` declared there, so
//!   [`Magento::amqp_topology_fingerprint`] hashes what it would declare from,
//!   for a deploy tool to compare with the fingerprint of the last release.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::configstatus::{json_encode, sha1_hex, Key, Php};
use crate::ids::Area;
use crate::{deploy, Error, Magento, Result};

impl Magento {
    /// Themes as `ComponentRegistrar` registers them, as `area/Vendor/name` with
    /// their directory: every Composer `autoload.files` entry and
    /// `app/design/*/*/*/registration.php` (`registration_globlist.php`) that
    /// registers a `ComponentRegistrar::THEME`.
    pub fn registered_themes(&self) -> Vec<(String, PathBuf)> {
        let vfs = &self.index.vfs;
        let mut files: Vec<PathBuf> = Vec::new();
        for p in &self.index.packages {
            files.extend(p.autoload_files.iter().map(|f| p.root.join(f)));
        }
        let design = self.root().join("app/design");
        for area in sorted_dirs(&design) {
            for vendor in sorted_dirs(&area) {
                for theme in sorted_dirs(&vendor) {
                    files.push(theme.join("registration.php"));
                }
            }
        }
        let mut out: Vec<(String, PathBuf)> = Vec::new();
        for f in files {
            let Ok(src) = vfs.read_to_string(&f) else { continue };
            for id in theme_registrations(&src) {
                if !out.iter().any(|(existing, _)| *existing == id) {
                    out.push((id, f.parent().map(Path::to_path_buf).unwrap_or_default()));
                }
            }
        }
        out.sort();
        out
    }

    /// Every queue name Magento's topology config holds, the list MysqlMq's
    /// `Recurring` writes to the `queue` table: each binding whose
    /// `destinationType` is `queue` (disabled ones too, as `DataMapper` reads the
    /// raw config), plus a `responseQueue.<topic>` for each synchronous topic the
    /// binding's topic names or matches.
    pub fn topology_queues(&self) -> Result<Vec<String>> {
        self.refuse_unmodelled_queue_config()?;
        let modules = &self.index.modules;
        let vfs = &self.index.vfs;
        let read = |file: &str| -> Vec<String> {
            modules
                .iter()
                .filter(|m| m.enabled)
                .filter_map(|m| vfs.read_to_string(&m.path.join("etc").join(file)).ok())
                .collect()
        };

        // Exchanges merge by (name, connection) and bindings by id, attribute by
        // attribute, in module order: the config DOM merge.
        let mut exchanges: Vec<((Option<String>, Option<String>), Vec<(String, Attrs)>)> = Vec::new();
        for xml in read("queue_topology.xml") {
            for (key, bindings) in parse_topology(&xml)? {
                let pos = match exchanges.iter().position(|(k, _)| *k == key) {
                    Some(p) => p,
                    None => {
                        exchanges.push((key, Vec::new()));
                        exchanges.len() - 1
                    }
                };
                let merged = &mut exchanges[pos].1;
                for (id, attrs) in bindings {
                    match merged.iter_mut().find(|(existing, _)| *existing == id) {
                        Some((_, old)) => old.extend(attrs),
                        None => merged.push((id, attrs)),
                    }
                }
            }
        }

        let mut topics: HashMap<String, Attrs> = HashMap::new();
        for xml in read("communication.xml") {
            for (name, attrs) in parse_topics(&xml)? {
                topics.entry(name).or_default().extend(attrs);
            }
        }
        let mut synchronous: Vec<&str> = Vec::new();
        for (name, a) in &topics {
            if is_synchronous(a)? {
                synchronous.push(name);
            }
        }
        synchronous.sort_unstable();

        // (topic, queue) of every binding whose destination is a queue.
        let mut bindings: Vec<(String, String)> = Vec::new();
        for (_, merged) in &exchanges {
            for (_, a) in merged {
                if a.get("destinationType").map_or("queue", String::as_str) == "queue" {
                    let topic = a.get("topic").cloned().unwrap_or_default();
                    bindings.push((topic, a.get("destination").cloned().unwrap_or_default()));
                }
            }
        }
        // Magento_Amqp's ConfigReaderPlugin adds the binds of the older
        // `queue.xml` format to the topology.
        if modules.iter().any(|m| m.enabled && m.name.as_str() == "Magento_Amqp") {
            let mut names: Vec<&str> = topics.keys().map(String::as_str).collect();
            names.sort_unstable();
            bindings.extend(legacy_binds(&read("queue.xml"), &names)?);
        }

        let mut queues = BTreeSet::new();
        for (topic, queue) in bindings {
            if topic.contains('*') || topic.contains('#') {
                for t in synchronous.iter().filter(|t| wildcard_match(&topic, t)) {
                    queues.insert(response_queue(t));
                }
            } else if synchronous.binary_search(&topic.as_str()).is_ok() {
                queues.insert(response_queue(&topic));
            }
            queues.insert(queue);
        }
        Ok(queues.into_iter().collect())
    }

    /// Topology and topic sources this port doesn't read: remote-service
    /// bindings (a `X` → `XRemote` preference) and topics from env.php.
    fn refuse_unmodelled_queue_config(&self) -> Result<()> {
        let remote = self.di_export_ref(Area::Global).preferences.iter().find(|p| {
            p.prefer.as_str().strip_prefix(p.for_type.as_str()) == Some("Remote")
        });
        if let Some(p) = remote {
            return Err(Error::Unsupported(format!(
                "{} is a remote service; its generated queue bindings are not modelled",
                p.for_type.as_str()
            )));
        }
        if let Ok(env) = deploy::read_env(self.root(), &self.index.vfs) {
            if env.get("communication").is_some() {
                return Err(Error::Unsupported("env.php defines communication topics".into()));
            }
            if let Some(q) = env.get("queue") {
                if q.get("binds").is_some() || q.get("config").is_some() {
                    return Err(Error::Unsupported("env.php defines queue binds".into()));
                }
            }
        }
        Ok(())
    }

    /// A fingerprint of everything Amqp's `Recurring` declares on the broker
    /// from: env.php's `queue` section and every enabled module's
    /// `queue_topology.xml`, `queue.xml` and `communication.xml`, in module order. `None`
    /// when env.php configures no AMQP connection, so nothing is declared.
    ///
    /// The broker itself is never asked, so a queue deleted there by hand goes
    /// unnoticed; any change to the files (even whitespace) changes the
    /// fingerprint, which errs towards running the upgrade.
    pub fn amqp_topology_fingerprint(&self) -> Result<Option<String>> {
        let env = match deploy::read_env(self.root(), &self.index.vfs) {
            Ok(v) => Php::from_value(&v)?,
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let Some(queue) = env.get(&Key::Str("queue".into())) else { return Ok(None) };
        if !amqp_configured(queue) {
            return Ok(None);
        }
        let mut input = format!("env queue {}\n", json_encode(queue)?);
        for m in self.index.modules.iter().filter(|m| m.enabled) {
            for file in ["queue_topology.xml", "queue.xml", "communication.xml"] {
                let path = m.path.join("etc").join(file);
                if let Ok(text) = self.index.vfs.read_to_string(&path) {
                    input.push_str(&format!("{} {file} {}\n", m.name.as_str(), sha1_hex(text.as_bytes())));
                }
            }
        }
        Ok(Some(sha1_hex(input.as_bytes())))
    }
}

#[cfg(feature = "db")]
impl Magento {
    /// Themes on disk with no physical row in the `theme` table, i.e. what
    /// `setup:upgrade` would register (`theme:registration:status`).
    pub fn unregistered_themes(&self) -> Result<Vec<String>> {
        let cfg = self.db_config()?;
        let conn = crate::default_connection(&cfg)?.clone();
        let mut db = crate::db::status::open(&conn).map_err(Error::Db)?;
        let rows: Vec<(Option<String>, Option<String>)> = db
            .exec(format!("SELECT area, theme_path FROM `{}theme` WHERE type = 0", cfg.table_prefix), ())
            .map_err(Error::Db)?;
        let registered: BTreeSet<String> = rows
            .into_iter()
            .map(|(a, p)| format!("{}/{}", a.unwrap_or_default(), p.unwrap_or_default()))
            .collect();
        Ok(self.registered_themes().into_iter().map(|(id, _)| id).filter(|id| !registered.contains(id)).collect())
    }

    /// Topology queues with no row in the `queue` table, i.e. what
    /// `setup:upgrade` would add (MysqlMq's part of `queue:config:status`).
    pub fn missing_db_queues(&self) -> Result<Vec<String>> {
        let configured = self.topology_queues()?;
        let cfg = self.db_config()?;
        let conn = crate::default_connection(&cfg)?.clone();
        let mut db = crate::db::status::open(&conn).map_err(Error::Db)?;
        let rows: Vec<Option<String>> = db
            .exec(format!("SELECT DISTINCT name FROM `{}queue`", cfg.table_prefix), ())
            .map_err(Error::Db)?;
        let existing: BTreeSet<String> = rows.into_iter().flatten().collect();
        Ok(configured.into_iter().filter(|q| !existing.contains(q)).collect())
    }
}

type Attrs = HashMap<String, String>;

fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    out.sort();
    out
}

/// The ids a `registration.php` registers as themes:
/// `ComponentRegistrar::register(ComponentRegistrar::THEME, 'frontend/V/n', __DIR__)`.
fn theme_registrations(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in src.match_indices("ComponentRegistrar::THEME") {
        let rest = src[i + "ComponentRegistrar::THEME".len()..].trim_start();
        let Some(rest) = rest.strip_prefix(',') else { continue };
        let rest = rest.trim_start();
        let Some(q) = rest.chars().next().filter(|c| *c == '\'' || *c == '"') else { continue };
        if let Some(end) = rest[1..].find(q) {
            out.push(rest[1..1 + end].to_string());
        }
    }
    out
}

/// `queue_topology.xml` → exchanges keyed by their (name, connection)
/// attributes, each with its bindings keyed by `id` and their attributes.
#[allow(clippy::type_complexity)]
fn parse_topology(xml: &str) -> Result<Vec<((Option<String>, Option<String>), Vec<(String, Attrs)>)>> {
    let mut reader = Reader::from_str(xml);
    let mut out: Vec<((Option<String>, Option<String>), Vec<(String, Attrs)>)> = Vec::new();
    let mut in_exchange = false;
    let mut depth_in_binding = 0usize;
    loop {
        match reader.read_event() {
            Err(e) => return Err(Error::Unsupported(format!("queue_topology.xml: {e}"))),
            Ok(Event::Eof) => break,
            Ok(ev @ (Event::Start(_) | Event::Empty(_))) if depth_in_binding == 0 => {
                let empty = matches!(ev, Event::Empty(_));
                let (Event::Start(e) | Event::Empty(e)) = ev else { unreachable!() };
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                let attrs = attributes(&e);
                match name.as_str() {
                    "exchange" => {
                        out.push(((attrs.get("name").cloned(), attrs.get("connection").cloned()), Vec::new()));
                        in_exchange = !empty;
                    }
                    "binding" if in_exchange => {
                        let id = attrs.get("id").cloned().unwrap_or_default();
                        if let Some((_, bindings)) = out.last_mut() {
                            bindings.push((id, attrs));
                        }
                        if !empty {
                            depth_in_binding = 1;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Start(_)) if depth_in_binding > 0 => depth_in_binding += 1,
            Ok(Event::End(e)) => {
                if depth_in_binding > 0 {
                    depth_in_binding -= 1;
                } else if e.local_name().as_ref() == b"exchange" {
                    in_exchange = false;
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// The binds of the older `queue.xml` format (`TopicConfig::buildBinds`), as
/// (topic, queue). Brokers merge by `topic`; a broker's `<queue>` children carry
/// no id, so a later file's `<queue>` merges into the single one already there,
/// as the config DOM merge does. A wildcard broker topic stands for every
/// communication topic it matches, and for nothing when none does.
fn legacy_binds(files: &[String], topic_names: &[&str]) -> Result<Vec<(String, String)>> {
    let mut brokers: Vec<(String, Vec<Attrs>)> = Vec::new();
    for xml in files {
        for (topic, queues) in parse_brokers(xml)? {
            match brokers.iter_mut().find(|(t, _)| *t == topic) {
                None => brokers.push((topic, queues)),
                Some((_, existing)) => {
                    for q in queues {
                        match existing.len() {
                            0 => existing.push(q),
                            1 => existing[0].extend(q),
                            _ => {
                                return Err(Error::Unsupported(format!(
                                    "queue.xml broker `{topic}` has several <queue> nodes to merge into"
                                )))
                            }
                        }
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    for (topic, queues) in brokers {
        let targets: Vec<String> = if topic.contains('*') || topic.contains('#') {
            topic_names.iter().filter(|t| wildcard_match(&topic, t)).map(|t| t.to_string()).collect()
        } else {
            vec![topic]
        };
        for t in targets {
            for q in &queues {
                out.push((t.clone(), q.get("name").cloned().unwrap_or_default()));
            }
        }
    }
    Ok(out)
}

/// `queue.xml` → brokers by `topic`, each with its `<queue>` attributes.
fn parse_brokers(xml: &str) -> Result<Vec<(String, Vec<Attrs>)>> {
    let mut reader = Reader::from_str(xml);
    let mut out: Vec<(String, Vec<Attrs>)> = Vec::new();
    let mut in_broker = false;
    loop {
        match reader.read_event() {
            Err(e) => return Err(Error::Unsupported(format!("queue.xml: {e}"))),
            Ok(Event::Eof) => break,
            Ok(ev @ (Event::Start(_) | Event::Empty(_))) => {
                let empty = matches!(ev, Event::Empty(_));
                let (Event::Start(e) | Event::Empty(e)) = ev else { unreachable!() };
                match e.local_name().as_ref() {
                    b"broker" => {
                        out.push((attributes(&e).get("topic").cloned().unwrap_or_default(), Vec::new()));
                        in_broker = !empty;
                    }
                    b"queue" if in_broker => {
                        if let Some((_, queues)) = out.last_mut() {
                            queues.push(attributes(&e));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"broker" => in_broker = false,
            _ => {}
        }
    }
    Ok(out)
}

/// `communication.xml` → topics by name with their attributes.
fn parse_topics(xml: &str) -> Result<Vec<(String, Attrs)>> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Err(e) => return Err(Error::Unsupported(format!("communication.xml: {e}"))),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.local_name().as_ref() == b"topic" => {
                let attrs = attributes(&e);
                out.push((attrs.get("name").cloned().unwrap_or_default(), attrs));
            }
            _ => {}
        }
    }
    Ok(out)
}

fn attributes(e: &quick_xml::events::BytesStart) -> Attrs {
    e.attributes()
        .flatten()
        .map(|a| {
            let key = String::from_utf8_lossy(a.key.local_name().as_ref()).into_owned();
            let value = a.unescape_value().map(|v| v.into_owned()).unwrap_or_default();
            (key, value)
        })
        .collect()
}

/// The communication converter: a topic is synchronous when `is_synchronous` is
/// absent or true and it has a response, either through `schema` (a service
/// method) or `request` with `response`.
fn is_synchronous(a: &Attrs) -> Result<bool> {
    let flag = match a.get("is_synchronous").map(|v| v.trim()) {
        None => true,
        Some("true" | "1") => true,
        Some("false" | "0") => false,
        Some(other) => return Err(Error::Unsupported(format!("is_synchronous=\"{other}\""))),
    };
    let has_response = a.contains_key("schema") || (a.contains_key("request") && a.contains_key("response"));
    Ok(flag && has_response)
}

/// `ResponseQueueNameBuilder::getQueueName`.
fn response_queue(topic: &str) -> String {
    format!("responseQueue.{}", topic.replace('-', "_"))
}

/// `DataMapper::buildWildcardPattern` applied: `#` is one or more of any
/// character, `*` one or more non-dots, the rest literal, anchored both ends.
fn wildcard_match(pattern: &str, topic: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'#') => (1..=t.len()).any(|n| go(&p[1..], &t[n..])),
            Some(b'*') => {
                let run = t.iter().take_while(|&&c| c != b'.').count();
                (1..=run).any(|n| go(&p[1..], &t[n..]))
            }
            Some(&c) => t.first() == Some(&c) && go(&p[1..], &t[1..]),
        }
    }
    go(pattern.as_bytes(), topic.as_bytes())
}

/// Whether env.php's `queue` section configures an AMQP connection: `amqp`
/// with a host, or any entry under `connections`.
fn amqp_configured(queue: &Php) -> bool {
    let host = queue.get(&Key::Str("amqp".into())).and_then(|a| a.get(&Key::Str("host".into())));
    let has_host = matches!(host, Some(Php::Str(h)) if !h.is_empty());
    let connections = matches!(queue.get(&Key::Str("connections".into())), Some(Php::Arr(c)) if !c.is_empty());
    has_host || connections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_ids_come_from_the_registration_call() {
        let src = r#"<?php
use Magento\Framework\Component\ComponentRegistrar;
ComponentRegistrar::register(ComponentRegistrar::THEME, 'frontend/Hyva/default', __DIR__);
\Magento\Framework\Component\ComponentRegistrar::register(
    \Magento\Framework\Component\ComponentRegistrar::THEME,
    "adminhtml/Acme/admin",
    __DIR__
);
ComponentRegistrar::register(ComponentRegistrar::MODULE, 'Acme_Foo', __DIR__);
"#;
        assert_eq!(theme_registrations(src), ["frontend/Hyva/default", "adminhtml/Acme/admin"]);
    }

    #[test]
    fn wildcards_follow_the_data_mapper_pattern() {
        assert!(wildcard_match("async.#", "async.V1.products.POST"));
        assert!(!wildcard_match("async.#", "async."));
        assert!(wildcard_match("sales.*.created", "sales.order.created"));
        assert!(!wildcard_match("sales.*.created", "sales.order.x.created"));
        assert!(wildcard_match("exact.topic", "exact.topic"));
        assert!(!wildcard_match("exact.topic", "exact.topic.more"));
    }

    #[test]
    fn topology_bindings_keep_their_attributes_and_skip_arguments() {
        let xml = r#"<config>
  <exchange name="magento-db" type="topic" connection="db">
    <binding id="a" topic="t.one" destinationType="queue" destination="q.one"/>
    <binding id="b" topic="t.two" destination="q.two" disabled="true">
      <arguments><argument name="x" xsi:type="string">y</argument></arguments>
    </binding>
  </exchange>
  <exchange name="magento"><binding id="c" topic="t.#" destination="q.three"/></exchange>
</config>"#;
        let ex = parse_topology(xml).unwrap();
        assert_eq!(ex.len(), 2);
        assert_eq!(ex[0].0, (Some("magento-db".into()), Some("db".into())));
        assert_eq!(ex[0].1.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(ex[0].1[1].1["disabled"], "true");
        assert_eq!(ex[1].0, (Some("magento".into()), None));
        assert_eq!(ex[1].1[0].1["destination"], "q.three");
    }

    #[test]
    fn legacy_brokers_merge_and_expand_wildcards() {
        let a = r#"<config><broker topic="product_alert" exchange="magento"><queue name="product_alert.queue" consumer="c"/></broker>
            <broker topic="async.#"><queue name="async.all"/></broker><broker topic="none.*"><queue name="never"/></broker></config>"#;
        let b = r#"<config><broker topic="product_alert"><queue name="renamed.queue"/></broker></config>"#;
        let binds = legacy_binds(&[a.into(), b.into()], &["async.one", "async.two", "product_alert"]).unwrap();
        assert_eq!(
            binds,
            [
                ("product_alert".to_string(), "renamed.queue".to_string()),
                ("async.one".into(), "async.all".into()),
                ("async.two".into(), "async.all".into()),
            ]
        );
    }

    #[test]
    fn synchronous_needs_a_response_and_no_false_flag() {
        let a = |pairs: &[(&str, &str)]| -> Attrs { pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
        assert!(is_synchronous(&a(&[("request", "X"), ("response", "Y")])).unwrap());
        assert!(is_synchronous(&a(&[("schema", "X::m")])).unwrap());
        assert!(!is_synchronous(&a(&[("request", "X")])).unwrap());
        assert!(!is_synchronous(&a(&[("schema", "X::m"), ("is_synchronous", "false")])).unwrap());
    }

    #[test]
    fn amqp_needs_a_host_or_named_connections() {
        let php = |src: &str| Php::from_value(&crate::phparray::parse(src).unwrap()).unwrap();
        assert!(amqp_configured(&php("<?php return ['amqp' => ['host' => 'rabbit']];")));
        assert!(!amqp_configured(&php("<?php return ['amqp' => ['host' => '']];")));
        assert!(!amqp_configured(&php("<?php return ['consumers_wait_for_messages' => 1];")));
        assert!(amqp_configured(&php("<?php return ['connections' => ['x' => []]];")));
    }
}
