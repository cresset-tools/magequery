//! Parsers for the indexer domain.

use super::*;

// ---------- indexers (indexer.xml + mview.xml) ----------

pub(crate) struct RawIndexer {
    pub id: String,
    pub view_id: Option<String>,
    pub class: Option<ClassName>,
    pub shared_index: Option<String>,
    pub title: String,
    pub description: Option<String>,
    /// Ids of indexers this one depends on (`<dependencies><indexer id=…/></dependencies>`).
    pub dependencies: Vec<String>,
    pub line: u32,
}

/// Parse a module's `indexer.xml`. An `<indexer>` directly under `<config>` is a
/// definition; an `<indexer>` inside `<dependencies>` is a *reference* to another indexer —
/// routed by the `in_dependencies` context, which (like the schema parser) is opened only
/// by a `Start` event so a self-closing `<dependencies/>` can't capture what follows.
pub(crate) fn indexer_xml(xml: &str) -> Vec<RawIndexer> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawIndexer> = Vec::new();
    let mut cur: Option<usize> = None;
    let mut in_dependencies = false;
    let mut text_into: Option<&'static str> = None; // "title" | "description"

    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                // Only a real `Start` opens the dependencies context (a self-closing
                // `<dependencies/>` has no matching `End` to clear it).
                if local_name(&e) == "dependencies" {
                    in_dependencies = true;
                }
                indexer_element(&e, line, in_dependencies, &mut out, &mut cur, &mut text_into);
            }
            Ok(Event::Empty(e)) => {
                indexer_element(&e, line, in_dependencies, &mut out, &mut cur, &mut text_into)
            }
            Ok(Event::Text(e)) => {
                if let (Some(i), Some(field)) = (cur, text_into) {
                    let t = e.unescape().unwrap_or_default().trim().to_string();
                    if !t.is_empty() {
                        match field {
                            "title" => out[i].title = t,
                            _ => out[i].description = Some(t),
                        }
                    }
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"indexer" if !in_dependencies => cur = None,
                b"dependencies" => in_dependencies = false,
                b"title" | b"description" => text_into = None,
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Handle one `Start`/`Empty` indexer.xml element: an `<indexer>` is a definition at the
/// top level but a dependency *reference* inside `<dependencies>`; `<title>`/`<description>`
/// carry their value as text.
fn indexer_element(
    e: &BytesStart,
    line: u32,
    in_dependencies: bool,
    out: &mut Vec<RawIndexer>,
    cur: &mut Option<usize>,
    text_into: &mut Option<&'static str>,
) {
    match local_name(e).as_str() {
        "indexer" => {
            if in_dependencies {
                if let (Some(i), Some(id)) = (*cur, attr(e, b"id")) {
                    out[i].dependencies.push(id);
                }
            } else {
                out.push(RawIndexer {
                    id: attr(e, b"id").unwrap_or_default(),
                    view_id: attr(e, b"view_id"),
                    class: attr(e, b"class").map(ClassName::new),
                    shared_index: attr(e, b"shared_index"),
                    title: String::new(),
                    description: None,
                    dependencies: Vec::new(),
                    line,
                });
                *cur = Some(out.len() - 1);
            }
        }
        "title" => *text_into = Some("title"),
        "description" => *text_into = Some("description"),
        _ => {}
    }
}

pub(crate) struct RawSubscription {
    pub table: String,
    pub entity_column: Option<String>,
    pub line: u32,
}

pub(crate) struct RawMview {
    pub id: String,
    pub subscriptions: Vec<RawSubscription>,
}

/// Parse a module's `mview.xml`: `<view id=><subscriptions><table name= entity_column=/>
/// </subscriptions></view>`. Only the id (the join key to `indexer.xml`'s `view_id`) and
/// the subscriptions are read; the view's own class/group aren't surfaced anywhere yet.
pub(crate) fn mview_xml(xml: &str) -> Vec<RawMview> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawMview> = Vec::new();
    let mut cur: Option<usize> = None;

    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(&e).as_str() {
                "view" => {
                    out.push(RawMview {
                        id: attr(&e, b"id").unwrap_or_default(),
                        subscriptions: Vec::new(),
                    });
                    cur = Some(out.len() - 1);
                }
                "table" => {
                    if let (Some(i), Some(name)) = (cur, attr(&e, b"name")) {
                        out[i].subscriptions.push(RawSubscription {
                            table: name,
                            entity_column: attr(&e, b"entity_column"),
                            line,
                        });
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) if e.name().as_ref() == b"view" => cur = None,
            _ => {}
        }
        buf.clear();
    }
    out
}

#[cfg(test)]
mod indexer_tests {
    use super::{indexer_xml, mview_xml};

    #[test]
    fn dependencies_are_references_not_definitions() {
        let xml = r#"<?xml version="1.0"?>
<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
    <indexer id="catalogrule_product" view_id="catalogrule_product" class="A\B" shared_index="rule">
        <title translate="true">Catalog Rule Product</title>
        <description>Indexed rule/product association</description>
        <dependencies>
            <indexer id="catalogrule_rule" />
            <indexer id="catalog_product_price" />
        </dependencies>
    </indexer>
    <indexer id="second" class="C\D">
        <title>Second</title>
    </indexer>
</config>"#;
        let idx = indexer_xml(xml);
        // The two <indexer>s inside <dependencies> must not become definitions.
        assert_eq!(idx.len(), 2);
        let first = &idx[0];
        assert_eq!(first.id, "catalogrule_product");
        assert_eq!(first.view_id.as_deref(), Some("catalogrule_product"));
        assert_eq!(first.shared_index.as_deref(), Some("rule"));
        assert_eq!(first.title, "Catalog Rule Product");
        assert_eq!(first.description.as_deref(), Some("Indexed rule/product association"));
        assert_eq!(first.dependencies, ["catalogrule_rule", "catalog_product_price"]);
        // The indexer after the dependencies block is its own definition again.
        assert_eq!(idx[1].id, "second");
        assert_eq!(idx[1].title, "Second");
        assert!(idx[1].dependencies.is_empty());
    }

    #[test]
    fn mview_views_and_subscriptions() {
        let xml = r#"<config>
            <view id="catalog_product_price" class="A\B" group="indexer">
                <subscriptions>
                    <table name="catalog_product_entity" entity_column="entity_id" />
                    <table name="catalog_product_website" entity_column="product_id" />
                </subscriptions>
            </view>
        </config>"#;
        let views = mview_xml(xml);
        assert_eq!(views.len(), 1);
        let v = &views[0];
        assert_eq!(v.id, "catalog_product_price");
        assert_eq!(v.subscriptions.len(), 2);
        assert_eq!(v.subscriptions[0].table, "catalog_product_entity");
        assert_eq!(v.subscriptions[1].entity_column.as_deref(), Some("product_id"));
    }
}
