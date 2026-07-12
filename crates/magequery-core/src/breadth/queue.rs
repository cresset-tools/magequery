//! Static queue indexes.

use super::*;

// ---------- message queues (communication.xml + queue_*.xml) ----------

/// The merged message-queue wiring: topics/handlers, consumers, exchange bindings, and
/// publishers, plus the topic → queue → consumer join.
pub(crate) struct MqIndex {
    topics: HashMap<String, MqTopic>,
    /// consumer name -> consumer.
    consumers: HashMap<String, MqConsumer>,
    /// (connection, exchange name) -> exchange (bindings keyed by id inside).
    exchanges: HashMap<(String, String), MqExchangeBuild>,
    /// topic -> publisher (connections kept raw; flattened in [`publisher`](Self::publisher)).
    publishers: HashMap<String, MqPublisherBuild>,
}

struct MqExchangeBuild {
    bindings: HashMap<String, MqBindingBuild>,
}

struct MqBindingBuild {
    pattern: String,
    destination: String,
    disabled: bool,
    source: Source,
}

struct MqPublisherBuild {
    queue: Option<String>,
    disabled: bool,
    /// connection name -> (exchange, disabled).
    connections: Vec<(String, Option<String>, bool)>,
    source: Source,
}

impl MqIndex {
    pub fn build(modules: &[Module], vfs: &Vfs) -> Self {
        let src = |module: &ModuleName, path: &PathBuf, line: u32| Source {
            module: module.clone(),
            file: path.clone(),
            line,
            area: Area::Global,
        };

        // communication.xml: topics by name (attrs merge non-empty), handlers by name
        // (attribute-level, like plugins — a later `disabled="true"` keeps the class).
        let mut topics: HashMap<String, MqTopic> = HashMap::new();
        for (i, path, raws) in
            read_parse(modules, vfs, Area::Global, "communication.xml", parse::communication_xml)
        {
            let module = &modules[i].name;
            for r in raws {
                let entry = topics.entry(r.name.clone()).or_insert_with(|| MqTopic {
                    name: r.name.clone(),
                    request: None,
                    response: None,
                    schema: None,
                    handlers: Vec::new(),
                    source: src(module, &path, r.line),
                });
                if r.request.is_some() {
                    entry.request = r.request;
                }
                if r.response.is_some() {
                    entry.response = r.response;
                }
                if r.schema.is_some() {
                    entry.schema = r.schema;
                }
                for h in r.handlers {
                    let source = src(module, &path, h.line);
                    match entry.handlers.iter_mut().find(|e| e.name == h.name) {
                        Some(e) => {
                            if !h.class.as_str().is_empty() {
                                e.class = h.class;
                            }
                            if !h.method.is_empty() {
                                e.method = h.method;
                            }
                            if let Some(d) = h.disabled {
                                e.disabled = d;
                            }
                            e.source = source;
                        }
                        None => entry.handlers.push(MqHandler {
                            name: h.name,
                            class: h.class,
                            method: h.method,
                            disabled: h.disabled.unwrap_or(false),
                            source,
                        }),
                    }
                }
            }
        }

        // queue_consumer.xml: consumers by name, merge non-empty.
        let mut consumers: HashMap<String, MqConsumer> = HashMap::new();
        for (i, path, raws) in
            read_parse(modules, vfs, Area::Global, "queue_consumer.xml", parse::queue_consumer_xml)
        {
            let module = &modules[i].name;
            for r in raws {
                let source = src(module, &path, r.line);
                let entry = consumers.entry(r.name.clone()).or_insert_with(|| MqConsumer {
                    name: r.name.clone(),
                    queue: String::new(),
                    connection: None,
                    consumer_instance: None,
                    handler: None,
                    max_messages: None,
                    source: source.clone(),
                });
                if !r.queue.is_empty() {
                    entry.queue = r.queue;
                }
                if r.connection.is_some() {
                    entry.connection = r.connection;
                }
                if r.consumer_instance.is_some() {
                    entry.consumer_instance = r.consumer_instance;
                }
                if r.handler.is_some() {
                    entry.handler = r.handler;
                }
                if r.max_messages.is_some() {
                    entry.max_messages = r.max_messages;
                }
                entry.source = source;
            }
        }

        // queue_topology.xml: exchanges keyed by (connection, name) — the same exchange
        // name on amqp and db is two different exchanges. Bindings by id, last-wins.
        let mut exchanges: HashMap<(String, String), MqExchangeBuild> = HashMap::new();
        for (i, path, raws) in
            read_parse(modules, vfs, Area::Global, "queue_topology.xml", parse::queue_topology_xml)
        {
            let module = &modules[i].name;
            for r in raws {
                let conn = r.connection.clone().unwrap_or_else(|| "amqp".to_string());
                let entry = exchanges
                    .entry((conn, r.name.clone()))
                    .or_insert_with(|| MqExchangeBuild { bindings: HashMap::new() });
                for b in r.bindings {
                    entry.bindings.insert(
                        b.id.clone(),
                        MqBindingBuild {
                            pattern: b.topic,
                            destination: b.destination,
                            disabled: b.disabled,
                            source: src(module, &path, b.line),
                        },
                    );
                }
            }
        }

        // queue_publisher.xml: publishers by topic; connections merged by name.
        let mut publishers: HashMap<String, MqPublisherBuild> = HashMap::new();
        for (i, path, raws) in
            read_parse(modules, vfs, Area::Global, "queue_publisher.xml", parse::queue_publisher_xml)
        {
            let module = &modules[i].name;
            for r in raws {
                let source = src(module, &path, r.line);
                let entry = publishers.entry(r.topic.clone()).or_insert_with(|| MqPublisherBuild {
                    queue: None,
                    disabled: false,
                    connections: Vec::new(),
                    source: source.clone(),
                });
                if r.queue.is_some() {
                    entry.queue = r.queue;
                }
                if let Some(d) = r.disabled {
                    entry.disabled = d;
                }
                for c in r.connections {
                    match entry.connections.iter_mut().find(|(n, _, _)| *n == c.name) {
                        Some((_, ex, dis)) => {
                            if c.exchange.is_some() {
                                *ex = c.exchange;
                            }
                            if let Some(d) = c.disabled {
                                *dis = d;
                            }
                        }
                        None => entry.connections.push((
                            c.name,
                            c.exchange,
                            c.disabled.unwrap_or(false),
                        )),
                    }
                }
                entry.source = source;
            }
        }

        Self { topics, consumers, exchanges, publishers }
    }

    /// Topics whose name contains `filter` (or all, when `None`), sorted by name.
    pub fn topics(&self, filter: Option<&str>) -> Vec<MqTopic> {
        let mut v: Vec<MqTopic> = self
            .topics
            .values()
            .filter(|t| filter.is_none_or(|f| t.name.contains(f)))
            .cloned()
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Every queue the static config knows — consumer `queue=`, publisher direct
    /// `queue=`, binding destinations — with the consumers reading each. Sorted by name.
    /// Only `queue_backlog` (the live-count join) consumes it, hence the gate.
    #[cfg(feature = "db")]
    pub fn queues(&self) -> Vec<(String, Vec<String>)> {
        let mut map: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for c in self.consumers.values() {
            map.entry(c.queue.clone()).or_default().push(c.name.clone());
        }
        for p in self.publishers.values() {
            if let Some(q) = &p.queue {
                map.entry(q.clone()).or_default();
            }
        }
        for ex in self.exchanges.values() {
            for b in ex.bindings.values() {
                map.entry(b.destination.clone()).or_default();
            }
        }
        map.into_iter()
            .map(|(q, mut cs)| {
                cs.sort();
                (q, cs)
            })
            .collect()
    }

    /// The publisher for `topic`, flattened to its enabled `<connection>` (Magento allows
    /// exactly one enabled connection per publisher).
    fn publisher(&self, topic: &str) -> Option<MqPublisher> {
        let p = self.publishers.get(topic)?;
        let conn = p.connections.iter().find(|(_, _, disabled)| !disabled);
        Some(MqPublisher {
            topic: topic.to_string(),
            queue: p.queue.clone(),
            connection: conn.map(|(n, _, _)| n.clone()),
            exchange: conn.and_then(|(_, e, _)| e.clone()),
            disabled: p.disabled,
            source: p.source.clone(),
        })
    }

    /// The full journey of one topic (exact name): its queues (via the publisher's direct
    /// `queue=` and/or every enabled binding whose pattern matches) and each queue's
    /// consumers. `None` when the topic appears in neither `communication.xml` nor
    /// `queue_publisher.xml`.
    pub fn topic_route(&self, name: &str) -> Option<MqTopicRoute> {
        let publisher = self.publisher(name);
        // A topic declared only in queue_publisher.xml (no communication.xml entry) still
        // gets a route, with an empty handler list and the publisher's provenance.
        let topic = match self.topics.get(name) {
            Some(t) => t.clone(),
            None => MqTopic {
                name: name.to_string(),
                request: None,
                response: None,
                schema: None,
                handlers: Vec::new(),
                source: publisher.as_ref()?.source.clone(),
            },
        };

        let mut routes: Vec<MqRoute> = Vec::new();
        if let Some(p) = &publisher {
            if let Some(q) = &p.queue {
                let i = route_for(&mut routes, q);
                routes[i].via.push(MqVia::PublisherQueue { source: p.source.clone() });
            }
        }
        let mut keys: Vec<&(String, String)> = self.exchanges.keys().collect();
        keys.sort();
        for key in keys {
            let (conn, ex_name) = key;
            let ex = &self.exchanges[key];
            let mut ids: Vec<&String> = ex.bindings.keys().collect();
            ids.sort();
            for id in ids {
                let b = &ex.bindings[id];
                if b.disabled || !topic_matches(&b.pattern, name) {
                    continue;
                }
                let i = route_for(&mut routes, &b.destination);
                routes[i].via.push(MqVia::Binding {
                    exchange: ex_name.clone(),
                    connection: conn.clone(),
                    id: id.clone(),
                    pattern: b.pattern.clone(),
                    source: b.source.clone(),
                });
            }
        }

        for route in &mut routes {
            route.consumers = self
                .consumers
                .values()
                .filter(|c| c.queue == route.queue)
                .cloned()
                .collect();
            route.consumers.sort_by(|a, b| a.name.cmp(&b.name));
        }
        routes.sort_by(|a, b| a.queue.cmp(&b.queue));

        Some(MqTopicRoute { topic, publisher, routes })
    }
}

/// The index of `queue`'s route in `routes`, appending an empty one on first sight.
fn route_for(routes: &mut Vec<MqRoute>, queue: &str) -> usize {
    match routes.iter().position(|r| r.queue == queue) {
        Some(i) => i,
        None => {
            routes.push(MqRoute { queue: queue.to_string(), via: Vec::new(), consumers: Vec::new() });
            routes.len() - 1
        }
    }
}

/// AMQP topic-exchange pattern match: `.`-separated words, `*` = exactly one word,
/// `#` = zero or more words.
fn topic_matches(pattern: &str, topic: &str) -> bool {
    fn rec(p: &[&str], t: &[&str]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(&"#"), _) => rec(&p[1..], t) || (!t.is_empty() && rec(p, &t[1..])),
            (Some(&"*"), Some(_)) => rec(&p[1..], &t[1..]),
            (Some(&w), Some(&tw)) if w == tw => rec(&p[1..], &t[1..]),
            _ => false,
        }
    }
    rec(
        &pattern.split('.').collect::<Vec<_>>(),
        &topic.split('.').collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod mq_match_tests {
    use super::topic_matches;

    #[test]
    fn amqp_topic_patterns() {
        assert!(topic_matches("a.b.c", "a.b.c"));
        assert!(!topic_matches("a.b.c", "a.b.d"));
        assert!(topic_matches("a.*.c", "a.b.c"));
        assert!(!topic_matches("a.*.c", "a.b.b.c")); // * is exactly one word
        assert!(topic_matches("#", "anything.at.all"));
        assert!(topic_matches("a.#", "a"));
        assert!(topic_matches("a.#", "a.b.c"));
        assert!(!topic_matches("a.#", "b.a"));
        assert!(topic_matches("#.c", "a.b.c"));
    }
}
