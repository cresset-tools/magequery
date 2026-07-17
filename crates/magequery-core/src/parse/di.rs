//! Parsers for the di domain.

use super::*;

/// A `<plugin>` declaration as written in di.xml. Fields are `Option` so that merging
/// across modules/areas is attribute-level: a later `<plugin name="x" disabled="true"/>`
/// updates only `disabled`, keeping the `type` from an earlier declaration.
pub(crate) struct RawPlugin {
    pub name: String,
    /// The enclosing type node was spelled with a leading backslash — a
    /// DISTINCT DOM node; the plugin-list generator ltrims and appends.
    pub target_backslash: bool,
    pub class: Option<ClassName>,
    /// Whether `type=` was written with a leading backslash — the compiled
    /// plugin lists keep the raw spelling in their _data block.
    pub class_had_backslash: bool,
    pub sort_order: Option<i32>,
    pub disabled: Option<bool>,
    pub line: u32,
}

/// The DI-relevant declarations parsed out of a single di.xml file (no merging yet).
#[derive(Default)]
pub(crate) struct DiFile {
    /// `(for, type, line)`
    pub preferences: Vec<(ClassName, ClassName, u32)>,
    /// `(target type, plugin)`
    pub plugins: Vec<(ClassName, RawPlugin)>,
    /// `(name, type, line)`
    pub virtual_types: Vec<(ClassName, ClassName, u32)>,
    /// `(target type/virtualType, argument name, value, line)`
    pub arguments: Vec<(ClassName, String, RawArg, u32)>,
    /// `(type/virtualType name, shared, line)` — explicit `shared=` attributes only
    /// (Magento defaults to shared when unstated).
    pub shared: Vec<(ClassName, bool, u32)>,
    /// Every `<virtualType name=…>` occurrence, typed or anchor (no `type=`).
    /// The XML DOM merge pins a node's position at its FIRST appearance in
    /// any form, so declaration order must count anchors too.
    pub virtual_type_mentions: Vec<ClassName>,
    /// Every `<type name=…>` occurrence — the same position-pinning rule.
    /// RAW spelling: a leading backslash makes a DISTINCT DOM node whose
    /// position differs from the plain-spelled one.
    pub type_mentions: Vec<String>,
}

/// Parse one di.xml file. Tracks the enclosing `<type>`/`<virtualType>` so `<plugin>` and
/// `<arguments>` are attributed to their target.
pub(crate) fn di_xml(xml: &str) -> Result<DiFile, String> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut arg_buf = Vec::new();
    let mut out = DiFile::default();
    let mut current_type: Option<(ClassName, bool)> = None;

    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(e) => return Err(e.to_string()),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                if e.name().as_ref() == b"arguments" {
                    // Consume the whole <arguments>…</arguments> subtree.
                    if let Some((target, _)) = current_type.clone() {
                        let args = parse_arguments(&mut reader, &lines, &mut arg_buf);
                        for item in args {
                            out.arguments.push((target.clone(), item.key, item.value, item.line));
                        }
                    }
                } else {
                    di_open(&e, true, line, &mut out, &mut current_type);
                }
            }
            Ok(Event::Empty(e)) => di_open(&e, false, line, &mut out, &mut current_type),
            Ok(Event::End(e)) => {
                if matches!(e.name().as_ref(), b"type" | b"virtualType") {
                    current_type = None;
                }
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// A parsed argument value, carrying per-array-item line numbers but no full `Source` yet
/// (module/file/area are added at merge time in `di.rs`, where the file context is known).
pub(crate) enum RawArg {
    Object {
        class: ClassName,
        /// `shared=` attribute on the argument/item, when written.
        shared: Option<bool>,
        /// `sortOrder=` attribute (drives ObjectManager's SortItems merge).
        sort_order: Option<i32>,
    },
    Scalar { xsi_type: String, text: String },
    Array(Vec<RawItem>),
    Null,
}

/// One `<item>` of an array argument: the `sortOrder=` XML attribute lives on
/// the ITEM (any xsi:type — Magento's ArrayType interpreter sorts by it at
/// conversion time).
pub(crate) struct RawItem {
    pub key: String,
    pub value: RawArg,
    pub line: u32,
    pub sort_order: Option<i32>,
}

/// A frame on the argument-parse stack: the top-level `<arguments>` list (key `None`), an
/// `xsi:type="array"`, or an **untyped** element (no `xsi:type`) whose kind is resolved by
/// shape on close — `<item>` children ⇒ array, else its text ⇒ scalar (Magento's
/// `Mapper\Dom` infers array from item children when the type attribute is omitted, as e.g.
/// EffectConnect's `sales_order_grid` `columns` argument is written).
struct ArgFrame {
    key: Option<(String, u32)>,
    /// `sortOrder=` on the enclosing array-typed argument/item element.
    sort_order: Option<i32>,
    /// The element's `xsi:type`: `"array"` (explicit) or `""` (untyped — resolved by shape).
    xsi: String,
    /// `shared=` on the untyped element (used only if it resolves to a scalar/object).
    shared: Option<bool>,
    /// Text captured while the frame is open — used only when it closes with no `<item>`
    /// children (an untyped element that turned out to be a plain scalar).
    text: String,
    items: Vec<RawItem>,
}

/// Parse an `<arguments>` subtree (reader positioned just after the opening tag), returning
/// the top-level `(name, value, line)` triples. Reads until the matching `</arguments>`.
fn parse_arguments(
    reader: &mut Reader<&[u8]>,
    lines: &LineMap,
    buf: &mut Vec<u8>,
) -> Vec<RawItem> {
    struct Leaf {
        key: String,
        xsi: String,
        line: u32,
        text: String,
        shared: Option<bool>,
        sort_order: Option<i32>,
    }
    let mut stack = vec![ArgFrame {
        key: None,
        sort_order: None,
        xsi: String::new(),
        shared: None,
        text: String::new(),
        items: Vec::new(),
    }];
    // The scalar leaf currently being read.
    let mut leaf: Option<Leaf> = None;
    let obj_attrs = |e: &BytesStart| {
        (
            attr(e, b"shared").map(|s| matches!(s.trim(), "true" | "1")),
            attr(e, b"sortOrder").and_then(|s| s.trim().parse().ok()),
        )
    };

    loop {
        buf.clear();
        let ev = reader.read_event_into(buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) if matches!(e.name().as_ref(), b"argument" | b"item") => {
                let key = attr(&e, b"name").unwrap_or_default();
                let xsi = attr(&e, b"xsi:type").unwrap_or_default();
                let (shared, sort_order) = obj_attrs(&e);
                // An explicit array OR an untyped element opens a frame: an untyped
                // element with `<item>` children is an array (Magento infers this from
                // shape), otherwise it closes as its text/scalar. A typed scalar/object
                // element (`string`/`object`/…) reads as a leaf.
                if xsi == "array" || xsi.is_empty() {
                    stack.push(ArgFrame {
                        key: Some((key, line)),
                        sort_order,
                        xsi,
                        shared,
                        text: String::new(),
                        items: Vec::new(),
                    });
                } else {
                    leaf = Some(Leaf { key, xsi, line, text: String::new(), shared, sort_order });
                }
            }
            Ok(Event::Empty(e)) if matches!(e.name().as_ref(), b"argument" | b"item") => {
                let key = attr(&e, b"name").unwrap_or_default();
                let xsi = attr(&e, b"xsi:type").unwrap_or_default();
                let (shared, sort_order) = obj_attrs(&e);
                let value = if xsi == "array" {
                    RawArg::Array(Vec::new())
                } else {
                    scalar(&xsi, "", shared, sort_order)
                };
                push_item(&mut stack, key, value, line, sort_order);
            }
            Ok(Event::Text(e)) => {
                let t = e.unescape().unwrap_or_default();
                // Into the active scalar leaf, else the open frame's text buffer
                // (used only if that frame closes without item children).
                if let Some(l) = &mut leaf {
                    l.text.push_str(&t);
                } else if let Some(frame) = stack.last_mut() {
                    frame.text.push_str(&t);
                }
            }
            Ok(Event::CData(e)) => {
                // <![CDATA[...]]> carries the value verbatim (the primary
                // di.xml's regex arguments are written this way).
                let t = String::from_utf8_lossy(&e);
                if let Some(l) = &mut leaf {
                    l.text.push_str(&t);
                } else if let Some(frame) = stack.last_mut() {
                    frame.text.push_str(&t);
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"arguments" => break,
                b"argument" | b"item" => {
                    if let Some(l) = leaf.take() {
                        let sort_order = l.sort_order;
                        push_item(
                            &mut stack,
                            l.key,
                            scalar(&l.xsi, l.text.trim(), l.shared, l.sort_order),
                            l.line,
                            sort_order,
                        );
                    } else if stack.len() > 1 {
                        // Closing a frame — resolve its kind by shape.
                        let mut frame = stack.pop().unwrap();
                        let (key, kline) = frame.key.take().unwrap_or_default();
                        let sort_order = frame.sort_order;
                        let items = std::mem::take(&mut frame.items);
                        let value = if !items.is_empty() || frame.xsi == "array" {
                            // Explicit array, or an untyped element with item children.
                            RawArg::Array(items)
                        } else {
                            // Untyped element with no items: it's a plain scalar (its
                            // text) or null — same shape as a `leaf` would have produced.
                            scalar(&frame.xsi, frame.text.trim(), frame.shared, sort_order)
                        };
                        push_item(&mut stack, key, value, kline, sort_order);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    stack.pop().map(|f| f.items).unwrap_or_default()
}

fn push_item(
    stack: &mut [ArgFrame],
    key: String,
    value: RawArg,
    line: u32,
    sort_order: Option<i32>,
) {
    if let Some(frame) = stack.last_mut() {
        frame.items.push(RawItem { key, value, line, sort_order });
    }
}

/// Build a non-array value from an xsi:type, text, and object attributes.
fn scalar(xsi: &str, text: &str, shared: Option<bool>, sort_order: Option<i32>) -> RawArg {
    match xsi {
        "object" => RawArg::Object { class: ClassName::new(text), shared, sort_order },
        "null" => RawArg::Null,
        _ if text.is_empty() && xsi.is_empty() => RawArg::Null,
        _ => RawArg::Scalar { xsi_type: xsi.to_string(), text: text.to_string() },
    }
}

fn di_open(
    e: &BytesStart,
    is_start: bool,
    line: u32,
    out: &mut DiFile,
    current_type: &mut Option<(ClassName, bool)>,
) {
    match e.name().as_ref() {
        b"preference" => {
            if let (Some(f), Some(t)) = (attr(e, b"for"), attr(e, b"type")) {
                out.preferences.push((ClassName::new(f), ClassName::new(t), line));
            }
        }
        b"virtualType" => {
            if let Some(name) = attr(e, b"name") {
                out.virtual_type_mentions.push(ClassName::new(name.clone()));
                if let Some(t) = attr(e, b"type") {
                    out.virtual_types
                        .push((ClassName::new(name.clone()), ClassName::new(t), line));
                }
                if let Some(s) = attr(e, b"shared") {
                    out.shared.push((
                        ClassName::new(name.clone()),
                        matches!(s.trim(), "true" | "1"),
                        line,
                    ));
                }
                if is_start {
                    *current_type = Some((ClassName::new(name), false));
                }
            }
        }
        b"type" => {
            if let Some(name) = attr(e, b"name") {
                out.type_mentions.push(name.clone());
                if let Some(s) = attr(e, b"shared") {
                    out.shared.push((
                        ClassName::new(name.clone()),
                        matches!(s.trim(), "true" | "1"),
                        line,
                    ));
                }
                if is_start {
                    let backslash = name.starts_with('\\');
                    *current_type = Some((ClassName::new(name), backslash));
                }
            }
        }
        b"plugin" => {
            if let (Some((target, target_backslash)), Some(pname)) =
                (current_type.clone(), attr(e, b"name"))
            {
                out.plugins.push((
                    target,
                    RawPlugin {
                        name: pname,
                        target_backslash,
                        class_had_backslash: attr(e, b"type")
                            .map(|t| t.starts_with('\\'))
                            .unwrap_or(false),
                        class: attr(e, b"type").map(ClassName::new),
                        sort_order: attr(e, b"sortOrder").and_then(|s| s.trim().parse().ok()),
                        disabled: attr(e, b"disabled").map(|s| matches!(s.trim(), "true" | "1")),
                        line,
                    },
                ));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod di_argument_tests {
    use super::{di_xml, RawArg};

    /// An `<argument>` with no `xsi:type` but `<item>` children is an ARRAY —
    /// Magento infers the type from shape. Before the fix the items leaked as
    /// bogus top-level arguments and the array itself vanished, losing a module's
    /// merged columns (EffectConnect's `sales_order_grid` additions, G3).
    #[test]
    fn untyped_argument_with_item_children_is_an_array() {
        let xml = r#"<?xml version="1.0"?>
<config>
  <virtualType name="Magento\Sales\Model\ResourceModel\Order\Grid" type="Magento\Sales\Model\ResourceModel\Grid">
    <arguments>
      <argument name="columns">
        <item name="ec_id" xsi:type="string">sales_order.ec_id</item>
        <item name="ec_name" xsi:type="string">sales_order.ec_name</item>
      </argument>
    </arguments>
  </virtualType>
</config>"#;
        let di = di_xml(xml).unwrap();
        // Exactly ONE top-level argument, `columns` — not ec_id/ec_name leaking out.
        assert_eq!(di.arguments.len(), 1, "columns must be the only top-level arg");
        let (target, name, value, _) = &di.arguments[0];
        assert_eq!(target.as_str(), "Magento\\Sales\\Model\\ResourceModel\\Order\\Grid");
        assert_eq!(name, "columns");
        let RawArg::Array(items) = value else {
            panic!("columns must parse as an Array");
        };
        let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["ec_id", "ec_name"]);
        let RawArg::Scalar { xsi_type, text } = &items[0].value else {
            panic!("item must be a scalar string");
        };
        assert_eq!((xsi_type.as_str(), text.as_str()), ("string", "sales_order.ec_id"));
    }

    /// An untyped `<argument>` with only text (no items) stays a plain scalar —
    /// the fix must not turn every untyped argument into an array.
    #[test]
    fn untyped_argument_with_only_text_is_a_scalar() {
        let xml = r#"<?xml version="1.0"?>
<config>
  <type name="Foo\Bar">
    <arguments>
      <argument name="label">hello</argument>
    </arguments>
  </type>
</config>"#;
        let di = di_xml(xml).unwrap();
        assert_eq!(di.arguments.len(), 1);
        let RawArg::Scalar { text, .. } = &di.arguments[0].2 else {
            panic!("untyped text argument must be a scalar");
        };
        assert_eq!(text, "hello");
    }
}
