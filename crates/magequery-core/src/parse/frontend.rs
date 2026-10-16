//! Parsers for the frontend domain.

use super::*;

// ---------- layout (view/<area>/layout/<handle>.xml) ----------

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawLayoutOpKind {
    Block,
    Container,
    ReferenceBlock,
    ReferenceContainer,
    Update,
    Move,
}

/// One operation a layout file performs, flattened (nesting recorded via `parent`).
pub(crate) struct RawLayoutOp {
    pub kind: RawLayoutOpKind,
    /// Block/container name; the target handle for `Update`; the element for `Move`.
    pub name: String,
    pub class: Option<ClassName>,
    pub template: Option<String>,
    /// The enclosing named element (or `Move`'s destination).
    pub parent: Option<String>,
    /// `remove="true"` on a reference.
    pub remove: bool,
    pub line: u32,
}

/// How a layout file assigns a template to a block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RawTemplateVia {
    /// `template="…"` on a block or reference.
    Attribute,
    /// `<arguments><argument name="template" xsi:type="string">`, which
    /// `Template::_construct` hands to `setTemplate()`.
    Argument,
    /// `<action method="setTemplate">` with a string first argument.
    SetTemplateAction,
}

/// One template assignment in a layout file, attributed to the enclosing block.
pub(crate) struct RawTemplateAssign {
    pub block: String,
    pub template: String,
    pub via: RawTemplateVia,
    /// The `<action ifconfig=>` flag the assignment depends on.
    pub ifconfig: Option<String>,
    pub line: u32,
}

/// One parsed layout file.
#[derive(Default)]
pub(crate) struct RawLayout {
    pub ops: Vec<RawLayoutOp>,
    pub templates: Vec<RawTemplateAssign>,
}

/// An open `<argument>` whose text is a template reference.
struct PendingTemplateArg {
    block: String,
    via: RawTemplateVia,
    ifconfig: Option<String>,
    line: u32,
    text: String,
}

/// An open `<action method="setTemplate">`. Only its first argument is the template:
/// Magento passes action arguments positionally, whatever their `name`.
struct OpenSetTemplate {
    block: String,
    ifconfig: Option<String>,
    seen_argument: bool,
}

/// Parse one layout XML: blocks/containers declared, references modified or removed,
/// `<update handle=>` includes, and `<move>`s — each with its enclosing element — plus
/// every template assignment (attribute, `template` argument, `setTemplate` action).
pub(crate) fn layout_xml(xml: &str) -> RawLayout {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawLayoutOp> = Vec::new();
    let mut templates: Vec<RawTemplateAssign> = Vec::new();
    // Names of the enclosing block/container/reference elements.
    let mut stack: Vec<String> = Vec::new();
    let mut action: Option<OpenSetTemplate> = None;
    // The block whose direct `<arguments>` is open.
    let mut arguments_of: Option<String> = None;
    let mut pending: Option<PendingTemplateArg> = None;

    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                match local_name(&e).as_str() {
                    "action" => {
                        let set_template = attr(&e, b"method")
                            .is_some_and(|m| m.eq_ignore_ascii_case("setTemplate"));
                        action = stack.last().filter(|_| set_template).map(|block| {
                            OpenSetTemplate {
                                block: block.clone(),
                                ifconfig: attr(&e, b"ifconfig"),
                                seen_argument: false,
                            }
                        });
                    }
                    "arguments" => arguments_of = stack.last().cloned(),
                    "argument" => {
                        let string = xsi_type(&e).as_deref() == Some("string");
                        if let Some(a) = action.as_mut() {
                            if !a.seen_argument && string {
                                pending = Some(PendingTemplateArg {
                                    block: a.block.clone(),
                                    via: RawTemplateVia::SetTemplateAction,
                                    ifconfig: a.ifconfig.clone(),
                                    line,
                                    text: String::new(),
                                });
                            }
                            a.seen_argument = true;
                        } else if let Some(block) = &arguments_of {
                            if string && attr(&e, b"name").as_deref() == Some("template") {
                                pending = Some(PendingTemplateArg {
                                    block: block.clone(),
                                    via: RawTemplateVia::Argument,
                                    ifconfig: None,
                                    line,
                                    text: String::new(),
                                });
                            }
                        }
                    }
                    _ => {}
                }
                if let Some(op) = layout_op(&e, line, &stack) {
                    let named = matches!(
                        op.kind,
                        RawLayoutOpKind::Block
                            | RawLayoutOpKind::Container
                            | RawLayoutOpKind::ReferenceBlock
                            | RawLayoutOpKind::ReferenceContainer
                    );
                    if named {
                        stack.push(op.name.clone());
                    }
                    out.push(op);
                }
            }
            Ok(Event::Empty(e)) => {
                // A self-closing argument has no value but still takes the first slot.
                if local_name(&e) == "argument" {
                    if let Some(a) = action.as_mut() {
                        a.seen_argument = true;
                    }
                }
                if let Some(op) = layout_op(&e, line, &stack) {
                    out.push(op);
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(p) = pending.as_mut() {
                    p.text.push_str(&t.unescape().unwrap_or_default());
                }
            }
            Ok(Event::CData(t)) => {
                if let Some(p) = pending.as_mut() {
                    p.text.push_str(&String::from_utf8_lossy(&t));
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let raw = name.as_ref();
                let local = raw.rsplit(|&b| b == b':').next().unwrap_or(raw);
                match local {
                    b"block" | b"container" | b"referenceBlock" | b"referenceContainer" => {
                        stack.pop();
                    }
                    b"action" => action = None,
                    b"arguments" => arguments_of = None,
                    b"argument" => {
                        if let Some(p) = pending.take() {
                            let template = p.text.trim();
                            if !template.is_empty() {
                                templates.push(RawTemplateAssign {
                                    block: p.block,
                                    template: template.to_string(),
                                    via: p.via,
                                    ifconfig: p.ifconfig,
                                    line: p.line,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        buf.clear();
    }
    // `template="…"` on the element itself.
    for op in &out {
        if let Some(t) = &op.template {
            templates.push(RawTemplateAssign {
                block: op.name.clone(),
                template: t.clone(),
                via: RawTemplateVia::Attribute,
                ifconfig: None,
                line: op.line,
            });
        }
    }
    RawLayout { ops: out, templates }
}

fn layout_op(e: &BytesStart, line: u32, stack: &[String]) -> Option<RawLayoutOp> {
    let parent = stack.last().cloned();
    let kind = match local_name(e).as_str() {
        "block" => RawLayoutOpKind::Block,
        "container" => RawLayoutOpKind::Container,
        "referenceBlock" => RawLayoutOpKind::ReferenceBlock,
        "referenceContainer" => RawLayoutOpKind::ReferenceContainer,
        "update" => {
            return Some(RawLayoutOp {
                kind: RawLayoutOpKind::Update,
                name: attr(e, b"handle").unwrap_or_default(),
                class: None,
                template: None,
                parent,
                remove: false,
                line,
            })
        }
        "move" => {
            return Some(RawLayoutOp {
                kind: RawLayoutOpKind::Move,
                name: attr(e, b"element").unwrap_or_default(),
                class: None,
                template: None,
                parent: attr(e, b"destination"),
                remove: false,
                line,
            })
        }
        _ => return None,
    };
    Some(RawLayoutOp {
        kind,
        name: attr(e, b"name").unwrap_or_default(),
        class: attr(e, b"class").map(ClassName::new),
        template: attr(e, b"template"),
        parent,
        remove: attr(e, b"remove").as_deref() == Some("true"),
        line,
    })
}

#[cfg(test)]
mod layout_tests {
    use super::{layout_xml, RawLayoutOpKind};

    #[test]
    fn ops_nesting_and_removal() {
        let xml = r#"<page>
            <update handle="catalog_product_opengraph"/>
            <body>
                <referenceBlock name="head.components">
                    <block class="A\B" name="child" template="M::t.phtml"/>
                </referenceBlock>
                <referenceBlock name="gone" remove="true"/>
                <referenceContainer name="content">
                    <container name="wrap">
                        <block class="C\D" name="deep"/>
                    </container>
                </referenceContainer>
                <move element="child" destination="wrap" after="-"/>
            </body>
        </page>"#;
        let ops = layout_xml(xml).ops;
        let by = |n: &str| ops.iter().find(|o| o.name == n).unwrap();

        assert!(matches!(by("catalog_product_opengraph").kind, RawLayoutOpKind::Update));
        assert_eq!(by("child").parent.as_deref(), Some("head.components"));
        assert_eq!(by("child").template.as_deref(), Some("M::t.phtml"));
        assert!(by("gone").remove);
        assert_eq!(by("deep").parent.as_deref(), Some("wrap"));
        assert_eq!(by("wrap").parent.as_deref(), Some("content"));
        let mv = ops.iter().find(|o| matches!(o.kind, RawLayoutOpKind::Move)).unwrap();
        assert_eq!((mv.name.as_str(), mv.parent.as_deref()), ("child", Some("wrap")));
        // update, 2 referenceBlocks, referenceContainer, container, 2 blocks, move.
        assert_eq!(ops.len(), 8);
    }

    #[test]
    fn template_assignments_in_every_form() {
        let xml = r#"<page xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
            <body>
                <referenceBlock name="result.category">
                    <action method="setTemplate">
                        <argument name="template" xsi:type="string">M::result/category.phtml</argument>
                    </action>
                </referenceBlock>
                <referenceBlock name="header.search">
                    <action ifconfig="a/b/c" method="setTemplate">
                        <argument name="tpl" xsi:type="string"> M::form.mini.phtml </argument>
                    </action>
                    <action method="setTitle">
                        <argument name="template" xsi:type="string">not/a/template.phtml</argument>
                    </action>
                </referenceBlock>
                <referenceBlock name="overlay" template="M::overlay.phtml"/>
                <block class="A\B" name="hyva.style">
                    <arguments>
                        <argument name="template" xsi:type="string">M::args.phtml</argument>
                        <argument name="other" xsi:type="string">M::ignored.phtml</argument>
                        <argument name="view_model" xsi:type="object">A\Vm</argument>
                    </arguments>
                </block>
                <referenceBlock name="second.arg">
                    <action method="setTemplate">
                        <argument name="first" xsi:type="helper" helper="A\H::get"/>
                        <argument name="second" xsi:type="string">M::not-first.phtml</argument>
                    </action>
                </referenceBlock>
            </body>
        </page>"#;
        let parsed = layout_xml(xml);
        let mut got: Vec<_> = parsed
            .templates
            .iter()
            .map(|t| (t.block.as_str(), t.template.as_str(), t.via, t.ifconfig.as_deref(), t.line))
            .collect();
        got.sort_by_key(|g| g.4);
        use super::RawTemplateVia as V;
        assert_eq!(
            got,
            vec![
                ("result.category", "M::result/category.phtml", V::SetTemplateAction, None, 5),
                ("header.search", "M::form.mini.phtml", V::SetTemplateAction, Some("a/b/c"), 10),
                ("overlay", "M::overlay.phtml", V::Attribute, None, 16),
                ("hyva.style", "M::args.phtml", V::Argument, None, 19),
            ]
        );
        // The setTemplate actions add no ops of their own.
        assert_eq!(parsed.ops.len(), 5);
    }
}

// ---------- ui components (view/<area>/ui_component/<name>.xml) ----------

/// One node a ui component file declares or modifies. The XML is open-vocabulary —
/// the element name IS the component type (`column`, `field`, `dataSource`, …) and
/// Magento merges same-name files by matching `(element, name)`.
pub(crate) struct RawUiOp {
    pub element: String,
    pub name: String,
    /// PHP class (`class=`) — data providers, column/filter renderers.
    pub class: Option<ClassName>,
    /// JS component (`component=`).
    pub component: Option<String>,
    /// `<field formElement=>`.
    pub form_element: Option<String>,
    pub sort_order: Option<String>,
    /// `<settings><label>` text (or a button's direct `<label>`).
    pub label: Option<String>,
    /// `<settings><disabled>true</disabled>` — removes the node on merge.
    pub disabled: bool,
    /// `<settings><visible>` when stated.
    pub visible: Option<bool>,
    /// Nearest enclosing captured node.
    pub parent: Option<String>,
    /// Count of captured ancestors (for tree rendering).
    pub depth: u16,
    pub line: u32,
}

pub(crate) struct RawUiComponent {
    /// The root element (`listing`, `form`, …).
    pub kind: Option<String>,
    pub ops: Vec<RawUiOp>,
}

/// Parse one ui component XML into its component nodes. Any element with a `name`
/// attribute is a node, EXCEPT inside `<argument>` (config data trees — `<item name=>`
/// is a key, not a component) and inside `<settings>` (semantic config — `<param>`/
/// `<option>`/`<link>` all carry `name`), where only `<button>` is still a component.
/// `<settings>` children `label`/`disabled`/`visible` are routed to the enclosing node.
pub(crate) fn ui_component_xml(xml: &str) -> RawUiComponent {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut kind: Option<String> = None;
    let mut ops: Vec<RawUiOp> = Vec::new();
    // Every open element: (local name, index into `ops` when captured).
    let mut stack: Vec<(String, Option<usize>)> = Vec::new();

    let capture = |e: &BytesStart,
                   line: u32,
                   stack: &[(String, Option<usize>)],
                   ops: &[RawUiOp]|
     -> Option<RawUiOp> {
        let element = local_name(e);
        if element == "argument" || stack.iter().any(|(n, _)| n == "argument") {
            return None;
        }
        if stack.iter().any(|(n, _)| n == "settings") && element != "button" {
            return None;
        }
        let name = attr(e, b"name")?;
        let captured_ancestors: Vec<usize> =
            stack.iter().filter_map(|(_, idx)| *idx).collect();
        Some(RawUiOp {
            element,
            name,
            class: attr(e, b"class").map(ClassName::new),
            component: attr(e, b"component"),
            form_element: attr(e, b"formElement"),
            sort_order: attr(e, b"sortOrder"),
            label: None,
            disabled: false,
            visible: None,
            parent: captured_ancestors.last().map(|&i| ops[i].name.clone()),
            depth: captured_ancestors.len() as u16,
            line,
        })
    };

    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let element = local_name(&e);
                if stack.is_empty() {
                    kind = Some(element.clone());
                    stack.push((element, None));
                } else {
                    let idx = capture(&e, line, &stack, &ops).map(|op| {
                        ops.push(op);
                        ops.len() - 1
                    });
                    stack.push((element, idx));
                }
            }
            Ok(Event::Empty(e)) => {
                if !stack.is_empty() {
                    if let Some(op) = capture(&e, line, &stack, &ops) {
                        ops.push(op);
                    }
                }
            }
            Ok(Event::Text(t)) => {
                // `<label>`/`<disabled>`/`<visible>` route to the enclosing captured
                // node — through at most one `<settings>` (a button's label is direct).
                let Some((leaf, None)) = stack.last() else { continue };
                if !matches!(leaf.as_str(), "label" | "disabled" | "visible") {
                    continue;
                }
                let target = match &stack[..stack.len() - 1] {
                    [.., (_, Some(idx))] => *idx,
                    [.., (_, Some(idx)), (s, None)] if s == "settings" => *idx,
                    _ => continue,
                };
                let text = t.unescape().unwrap_or_default().trim().to_string();
                match leaf.as_str() {
                    "label" => ops[target].label = Some(text),
                    "disabled" => ops[target].disabled = text == "true",
                    _ => ops[target].visible = Some(text == "true"),
                }
            }
            Ok(Event::End(_)) => {
                stack.pop();
            }
            _ => {}
        }
        buf.clear();
    }
    RawUiComponent { kind, ops }
}

#[cfg(test)]
mod ui_component_tests {
    use super::ui_component_xml;

    #[test]
    fn nodes_suppression_and_settings_routing() {
        let xml = r#"<listing xmlns:xsi="x">
            <argument name="data">
                <item name="js_config"><item name="provider">p</item></item>
            </argument>
            <settings>
                <buttons>
                    <button name="add">
                        <url path="sales/order_create/start"/>
                        <label translate="true">Create New Order</label>
                    </button>
                </buttons>
            </settings>
            <dataSource name="grid_source" component="Magento_Ui/js/grid/provider">
                <dataProvider class="A\B\Provider" name="grid_source">
                    <settings><requestFieldName>id</requestFieldName></settings>
                </dataProvider>
            </dataSource>
            <columns name="cols">
                <column name="store_id" class="C\Store" sortOrder="20">
                    <settings>
                        <label translate="true">Purchase Point</label>
                        <visible>false</visible>
                        <options class="Src"/>
                        <option name="not_a_node"><label>Nope</label></option>
                    </settings>
                </column>
                <column name="old"><settings><disabled>true</disabled></settings></column>
                <field name="qty" formElement="input"/>
            </columns>
        </listing>"#;
        let parsed = ui_component_xml(xml);
        assert_eq!(parsed.kind.as_deref(), Some("listing"));
        let by = |n: &str| parsed.ops.iter().find(|o| o.name == n).unwrap();

        // Argument trees are data, not components; settings captures only <button>.
        assert!(!parsed.ops.iter().any(|o| o.name == "js_config" || o.name == "provider"));
        assert!(!parsed.ops.iter().any(|o| o.name == "not_a_node"));
        assert_eq!(by("add").label.as_deref(), Some("Create New Order")); // direct label
        assert_eq!(by("grid_source").element, "dataSource"); // first (element, name) wins the find
        assert_eq!(by("grid_source").component.as_deref(), Some("Magento_Ui/js/grid/provider"));
        let provider = parsed.ops.iter().find(|o| o.element == "dataProvider").unwrap();
        assert_eq!(provider.class.as_ref().unwrap().as_str(), "A\\B\\Provider");
        assert_eq!((provider.parent.as_deref(), provider.depth), (Some("grid_source"), 1));
        assert_eq!(by("store_id").label.as_deref(), Some("Purchase Point"));
        assert_eq!(by("store_id").visible, Some(false));
        assert_eq!(by("store_id").sort_order.as_deref(), Some("20"));
        assert_eq!(by("store_id").parent.as_deref(), Some("cols"));
        assert!(by("old").disabled);
        assert_eq!(by("qty").form_element.as_deref(), Some("input"));
        // button, dataSource, dataProvider, columns, 2 columns, field.
        assert_eq!(parsed.ops.len(), 7);
    }
}
