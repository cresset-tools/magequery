//! Parsers for the extensions domain.

use super::*;

// ---------- extension attributes (extension_attributes.xml) ----------

pub(crate) struct RawExtJoin {
    pub reference_table: String,
    pub reference_field: Option<String>,
    pub join_on_field: Option<String>,
    /// The joined fields (each `<field>`'s text).
    pub fields: Vec<String>,
}

pub(crate) struct RawExtAttr {
    /// The extended type (`<extension_attributes for=…>`).
    pub for_type: ClassName,
    pub code: String,
    /// Declared type: a class/interface or a scalar, possibly `[]`-suffixed.
    pub ty: String,
    /// ACL resources gating the attribute in webapi responses.
    pub resources: Vec<String>,
    pub join: Option<RawExtJoin>,
    pub line: u32,
}

/// Parse `extension_attributes.xml`: `<extension_attributes for=…><attribute code= type=>
/// <resources><resource ref=/></resources><join …><field>…</field></join></attribute>`.
pub(crate) fn extension_attributes_xml(xml: &str) -> Vec<RawExtAttr> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawExtAttr> = Vec::new();
    let mut for_type: Option<ClassName> = None;
    let mut cur: Option<usize> = None;
    let mut in_field = false;

    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(&e).as_str() {
                "extension_attributes" => {
                    for_type = attr(&e, b"for").map(ClassName::new);
                }
                "attribute" => {
                    if let Some(ft) = &for_type {
                        out.push(RawExtAttr {
                            for_type: ft.clone(),
                            code: attr(&e, b"code").unwrap_or_default(),
                            ty: attr(&e, b"type").unwrap_or_default(),
                            resources: Vec::new(),
                            join: None,
                            line,
                        });
                        cur = Some(out.len() - 1);
                    }
                }
                "resource" => {
                    if let (Some(i), Some(r)) = (cur, attr(&e, b"ref")) {
                        out[i].resources.push(r);
                    }
                }
                "join" => {
                    if let Some(i) = cur {
                        out[i].join = Some(RawExtJoin {
                            reference_table: attr(&e, b"reference_table").unwrap_or_default(),
                            reference_field: attr(&e, b"reference_field"),
                            join_on_field: attr(&e, b"join_on_field"),
                            fields: Vec::new(),
                        });
                    }
                }
                "field" => in_field = true,
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if in_field {
                    if let Some(join) = cur.and_then(|i| out[i].join.as_mut()) {
                        let t = e.unescape().unwrap_or_default().trim().to_string();
                        if !t.is_empty() {
                            join.fields.push(t);
                        }
                    }
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"attribute" => cur = None,
                b"extension_attributes" => for_type = None,
                b"field" => in_field = false,
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }
    out
}

#[cfg(test)]
mod ext_attr_tests {
    use super::extension_attributes_xml;

    #[test]
    fn attributes_resources_and_join() {
        let xml = r#"<config>
            <extension_attributes for="Magento\Catalog\Api\Data\ProductInterface">
                <attribute code="stock_item" type="Magento\CatalogInventory\Api\Data\StockItemInterface">
                    <resources><resource ref="Magento_CatalogInventory::cataloginventory"/></resources>
                </attribute>
                <attribute code="flags" type="string[]"/>
            </extension_attributes>
            <extension_attributes for="Magento\Sales\Api\Data\OrderInterface">
                <attribute code="bulk" type="Magento\Framework\Api\Data\X">
                    <join reference_table="magento_bulk" join_on_field="bulk_uuid" reference_field="uuid">
                        <field column="start_time">start_time</field>
                    </join>
                </attribute>
            </extension_attributes>
        </config>"#;
        let attrs = extension_attributes_xml(xml);
        assert_eq!(attrs.len(), 3);
        assert_eq!(attrs[0].for_type.as_str(), "Magento\\Catalog\\Api\\Data\\ProductInterface");
        assert_eq!(attrs[0].code, "stock_item");
        assert_eq!(attrs[0].resources, ["Magento_CatalogInventory::cataloginventory"]);
        assert_eq!(attrs[1].ty, "string[]");
        let join = attrs[2].join.as_ref().unwrap();
        assert_eq!(join.reference_table, "magento_bulk");
        assert_eq!(join.join_on_field.as_deref(), Some("bulk_uuid"));
        assert_eq!(join.fields, ["start_time"]);
    }
}

// ---------- admin menu (adminhtml/menu.xml) ----------

/// The attributes of a menu `<add>`/`<update>` — all `Option` so `<update>` merges
/// attribute-level (like plugins), and `<add>` states only what it declares.
pub(crate) struct RawMenuItem {
    pub id: String,
    pub title: Option<String>,
    /// Parent is an *attribute* here (unlike acl.xml's nesting).
    pub parent: Option<String>,
    /// The admin route, e.g. `catalog/product/`; grouping nodes have none.
    pub action: Option<String>,
    /// The guarding ACL resource id.
    pub resource: Option<String>,
    pub sort_order: Option<i32>,
    pub depends_on_module: Option<String>,
    pub depends_on_config: Option<String>,
    pub line: u32,
}

pub(crate) enum RawMenuOp {
    /// `<add>` and `<update>` merge identically for our purposes (upsert, non-empty attrs
    /// win) — Magento only distinguishes them for strict-mode validation.
    Upsert(RawMenuItem),
    Remove { id: String },
}

/// Parse `adminhtml/menu.xml`: `<menu><add id= title= parent= …/><update …/><remove id=/>`.
pub(crate) fn menu_xml(xml: &str) -> Vec<RawMenuOp> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(&e).as_str() {
                "add" | "update" => out.push(RawMenuOp::Upsert(RawMenuItem {
                    id: attr(&e, b"id").unwrap_or_default(),
                    title: attr(&e, b"title"),
                    parent: attr(&e, b"parent"),
                    action: attr(&e, b"action"),
                    resource: attr(&e, b"resource"),
                    sort_order: attr(&e, b"sortOrder").and_then(|s| s.trim().parse().ok()),
                    depends_on_module: attr(&e, b"dependsOnModule"),
                    depends_on_config: attr(&e, b"dependsOnConfig"),
                    line,
                })),
                "remove" => {
                    if let Some(id) = attr(&e, b"id") {
                        out.push(RawMenuOp::Remove { id });
                    }
                }
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }
    out
}

#[cfg(test)]
mod menu_tests {
    use super::{menu_xml, RawMenuOp};

    #[test]
    fn adds_updates_removes() {
        let xml = r#"<config><menu>
            <add id="A::a" title="Alpha" module="A" sortOrder="10" resource="A::a"/>
            <add id="A::b" title="Beta" parent="A::a" action="alpha/beta/" resource="A::b"/>
            <update id="A::b" title="Beta2"/>
            <remove id="A::gone"/>
        </menu></config>"#;
        let ops = menu_xml(xml);
        assert_eq!(ops.len(), 4);
        let RawMenuOp::Upsert(a) = &ops[0] else { panic!() };
        assert_eq!((a.id.as_str(), a.title.as_deref(), a.sort_order), ("A::a", Some("Alpha"), Some(10)));
        let RawMenuOp::Upsert(b) = &ops[1] else { panic!() };
        assert_eq!(b.parent.as_deref(), Some("A::a"));
        assert_eq!(b.action.as_deref(), Some("alpha/beta/"));
        let RawMenuOp::Upsert(u) = &ops[2] else { panic!() };
        assert_eq!((u.title.as_deref(), u.parent.as_deref()), (Some("Beta2"), None));
        assert!(matches!(&ops[3], RawMenuOp::Remove { id } if id == "A::gone"));
    }
}
