//! Parsers for the queue domain.

use super::*;

// ---------- message queues (communication.xml + queue_*.xml) ----------

pub(crate) struct RawMqHandler {
    pub name: String,
    pub class: ClassName,
    pub method: String,
    /// `Option` so cross-module merge is attribute-level (a later
    /// `<handler name=… disabled="true"/>` updates only `disabled`).
    pub disabled: Option<bool>,
    pub line: u32,
}

pub(crate) struct RawMqTopic {
    pub name: String,
    pub request: Option<String>,
    pub response: Option<String>,
    /// `schema="Class::method"` — request/response derived from a service method.
    pub schema: Option<String>,
    pub handlers: Vec<RawMqHandler>,
    pub line: u32,
}

/// Parse `communication.xml`: `<topic name= request= response=|schema=><handler name=
/// type= method=/></topic>`.
pub(crate) fn communication_xml(xml: &str) -> Vec<RawMqTopic> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawMqTopic> = Vec::new();
    let mut cur: Option<usize> = None;
    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(&e).as_str() {
                "topic" => {
                    out.push(RawMqTopic {
                        name: attr(&e, b"name").unwrap_or_default(),
                        request: attr(&e, b"request"),
                        response: attr(&e, b"response"),
                        schema: attr(&e, b"schema"),
                        handlers: Vec::new(),
                        line,
                    });
                    cur = Some(out.len() - 1);
                }
                "handler" => {
                    if let Some(i) = cur {
                        out[i].handlers.push(RawMqHandler {
                            name: attr(&e, b"name").unwrap_or_default(),
                            class: ClassName::new(attr(&e, b"type").unwrap_or_default()),
                            method: attr(&e, b"method").unwrap_or_default(),
                            disabled: attr(&e, b"disabled").map(|s| matches!(s.trim(), "true" | "1")),
                            line,
                        });
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) if e.name().as_ref() == b"topic" => cur = None,
            _ => {}
        }
        buf.clear();
    }
    out
}

pub(crate) struct RawMqConsumer {
    pub name: String,
    pub queue: String,
    pub connection: Option<String>,
    pub consumer_instance: Option<ClassName>,
    /// `handler="Class::method"`.
    pub handler: Option<String>,
    pub max_messages: Option<String>,
    pub line: u32,
}

/// Parse `queue_consumer.xml`: flat `<consumer name= queue= [connection= handler=
/// consumerInstance= maxMessages=]/>` elements.
pub(crate) fn queue_consumer_xml(xml: &str) -> Vec<RawMqConsumer> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if local_name(&e) == "consumer" => {
                out.push(RawMqConsumer {
                    name: attr(&e, b"name").unwrap_or_default(),
                    queue: attr(&e, b"queue").unwrap_or_default(),
                    connection: attr(&e, b"connection"),
                    consumer_instance: attr(&e, b"consumerInstance").map(ClassName::new),
                    handler: attr(&e, b"handler"),
                    max_messages: attr(&e, b"maxMessages"),
                    line,
                });
            }
            _ => {}
        }
        buf.clear();
    }
    out
}

pub(crate) struct RawMqBinding {
    pub id: String,
    /// The AMQP routing pattern (`sales.rule.#`, `*` = one word, `#` = zero or more).
    pub topic: String,
    pub destination: String,
    pub disabled: bool,
    pub line: u32,
}

pub(crate) struct RawMqExchange {
    pub name: String,
    /// `connection` attribute; absent ⇒ the XSD default `amqp`.
    pub connection: Option<String>,
    pub bindings: Vec<RawMqBinding>,
}

/// Parse `queue_topology.xml`: `<exchange name= [connection=]><binding id= topic=
/// destination=/></exchange>`. `<arguments>` subtrees are ignored (their elements don't
/// collide with the names matched here).
pub(crate) fn queue_topology_xml(xml: &str) -> Vec<RawMqExchange> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawMqExchange> = Vec::new();
    let mut cur: Option<usize> = None;
    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(&e).as_str() {
                "exchange" => {
                    out.push(RawMqExchange {
                        name: attr(&e, b"name").unwrap_or_default(),
                        connection: attr(&e, b"connection"),
                        bindings: Vec::new(),
                    });
                    cur = Some(out.len() - 1);
                }
                "binding" => {
                    if let Some(i) = cur {
                        out[i].bindings.push(RawMqBinding {
                            id: attr(&e, b"id").unwrap_or_default(),
                            topic: attr(&e, b"topic").unwrap_or_default(),
                            destination: attr(&e, b"destination").unwrap_or_default(),
                            disabled: attr_true(&e, b"disabled"),
                            line,
                        });
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) if e.name().as_ref() == b"exchange" => cur = None,
            _ => {}
        }
        buf.clear();
    }
    out
}

pub(crate) struct RawMqPubConnection {
    pub name: String,
    pub exchange: Option<String>,
    pub disabled: Option<bool>,
}

pub(crate) struct RawMqPublisher {
    pub topic: String,
    /// The direct-to-queue shorthand (`<publisher topic=… queue=…/>`), bypassing
    /// exchange/binding indirection.
    pub queue: Option<String>,
    /// `Option` for attribute-level cross-module merge (see [`RawMqHandler::disabled`]).
    pub disabled: Option<bool>,
    pub connections: Vec<RawMqPubConnection>,
    pub line: u32,
}

/// Parse `queue_publisher.xml`: `<publisher topic= [queue=] [disabled=]><connection name=
/// exchange= [disabled=]/></publisher>`.
pub(crate) fn queue_publisher_xml(xml: &str) -> Vec<RawMqPublisher> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawMqPublisher> = Vec::new();
    let mut cur: Option<usize> = None;
    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(&e).as_str() {
                "publisher" => {
                    out.push(RawMqPublisher {
                        topic: attr(&e, b"topic").unwrap_or_default(),
                        queue: attr(&e, b"queue"),
                        disabled: attr(&e, b"disabled").map(|s| matches!(s.trim(), "true" | "1")),
                        connections: Vec::new(),
                        line,
                    });
                    cur = Some(out.len() - 1);
                }
                "connection" => {
                    if let Some(i) = cur {
                        out[i].connections.push(RawMqPubConnection {
                            name: attr(&e, b"name").unwrap_or_default(),
                            exchange: attr(&e, b"exchange"),
                            disabled: attr(&e, b"disabled").map(|s| matches!(s.trim(), "true" | "1")),
                        });
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) if e.name().as_ref() == b"publisher" => cur = None,
            _ => {}
        }
        buf.clear();
    }
    out
}

#[cfg(test)]
mod mq_tests {
    use super::{communication_xml, queue_publisher_xml, queue_topology_xml};

    #[test]
    fn communication_topics_and_handlers() {
        let xml = r#"<config>
            <topic name="sales_rule.codegenerator" request="Magento\SalesRule\Api\Data\CouponGenerationSpecInterface">
                <handler name="codegeneratorProcessor" type="Magento\SalesRule\Model\Coupon\Consumer" method="process" />
            </topic>
            <topic name="async.op" schema="Magento\Foo\Api\BarInterface::execute"/>
        </config>"#;
        let topics = communication_xml(xml);
        assert_eq!(topics.len(), 2);
        assert_eq!(topics[0].name, "sales_rule.codegenerator");
        assert_eq!(topics[0].handlers.len(), 1);
        assert_eq!(topics[0].handlers[0].method, "process");
        assert_eq!(topics[1].schema.as_deref(), Some("Magento\\Foo\\Api\\BarInterface::execute"));
        assert!(topics[1].handlers.is_empty());
    }

    #[test]
    fn topology_bindings_attach_to_their_exchange() {
        let xml = r#"<config>
            <exchange name="magento">
                <binding id="b1" topic="a.#" destination="q1"/>
            </exchange>
            <exchange name="magento-db" connection="db">
                <binding id="b2" topic="a.b" destination="q2" disabled="true"/>
            </exchange>
        </config>"#;
        let ex = queue_topology_xml(xml);
        assert_eq!(ex.len(), 2);
        assert_eq!(ex[0].connection, None); // ⇒ amqp default at merge
        assert_eq!(ex[0].bindings[0].topic, "a.#");
        assert_eq!(ex[1].connection.as_deref(), Some("db"));
        assert!(ex[1].bindings[0].disabled);
    }

    #[test]
    fn publisher_direct_queue_and_connections() {
        let xml = r#"<config>
            <publisher topic="t.direct" queue="q.direct"/>
            <publisher topic="t.exchange">
                <connection name="amqp" exchange="magento" disabled="false"/>
                <connection name="db" exchange="magento-db" disabled="true"/>
            </publisher>
        </config>"#;
        let pubs = queue_publisher_xml(xml);
        assert_eq!(pubs[0].queue.as_deref(), Some("q.direct"));
        assert!(pubs[0].connections.is_empty());
        assert_eq!(pubs[1].connections.len(), 2);
        assert_eq!(pubs[1].connections[0].disabled, Some(false));
        assert_eq!(pubs[1].connections[1].disabled, Some(true));
    }
}
