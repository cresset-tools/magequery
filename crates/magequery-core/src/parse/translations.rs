//! Parsers for the translations domain.

use super::*;

// ---------- translations (i18n/<locale>.csv + language.xml) ----------

/// Parse a Magento i18n CSV into `(key, value, line)` rows: comma-separated, `"`-quoted
/// fields with `""` escapes, values may span lines. Extra columns (legacy module scoping)
/// are ignored.
pub(crate) fn i18n_csv(text: &str) -> Vec<(String, String, u32)> {
    let mut out = Vec::new();
    let mut fields: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut line: u32 = 1;
    let mut row_line: u32 = 1;
    let mut chars = text.chars().peekable();

    let flush_row = |fields: &mut Vec<String>, field: &mut String, out: &mut Vec<(String, String, u32)>, row_line: u32| {
        fields.push(std::mem::take(field));
        if fields.len() >= 2 && !fields[0].is_empty() {
            out.push((fields[0].clone(), fields[1].clone(), row_line));
        }
        fields.clear();
    };

    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes => {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            }
            '"' if field.is_empty() => in_quotes = true,
            ',' if !in_quotes => fields.push(std::mem::take(&mut field)),
            '\n' if !in_quotes => {
                line += 1;
                if !field.is_empty() || !fields.is_empty() {
                    flush_row(&mut fields, &mut field, &mut out, row_line);
                }
                row_line = line;
            }
            '\r' if !in_quotes => {}
            _ => {
                if c == '\n' {
                    line += 1;
                }
                field.push(c);
            }
        }
    }
    if !field.is_empty() || !fields.is_empty() {
        flush_row(&mut fields, &mut field, &mut out, row_line);
    }
    out
}

/// Parse a language pack's `language.xml`: `(code, sort_order)`.
pub(crate) fn language_xml(xml: &str) -> (Option<String>, Option<i32>) {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut code = None;
    let mut sort = None;
    let mut into: Option<&'static str> = None;
    loop {
        match reader.read_event_into(&mut buf) {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => match local_name(&e).as_str() {
                "code" => into = Some("code"),
                "sort_order" => into = Some("sort"),
                _ => {}
            },
            Ok(Event::Text(e)) => {
                let t = e.unescape().unwrap_or_default().trim().to_string();
                if !t.is_empty() {
                    match into {
                        Some("code") => code = Some(t),
                        Some("sort") => sort = t.parse().ok(),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(_)) => into = None,
            _ => {}
        }
        buf.clear();
    }
    (code, sort)
}

#[cfg(test)]
mod i18n_tests {
    use super::i18n_csv;

    #[test]
    fn quotes_escapes_and_multiline() {
        let csv = "\"Add to Cart\",\"In winkelwagen\"\n\"Say \"\"hi\"\"\",\"Zeg \"\"hoi\"\"\"\n\"Plain\",\"Plain\"\n\"Multi\nline\",\"Waarde\",\"module\",\"extra\"\n";
        let rows = i18n_csv(csv);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0], ("Add to Cart".into(), "In winkelwagen".into(), 1));
        assert_eq!(rows[1].0, "Say \"hi\"");
        assert_eq!(rows[1].1, "Zeg \"hoi\"");
        // Identity row still parsed (the *loader* treats it as a reset, not the parser).
        assert_eq!(rows[2].0, "Plain");
        // Multiline key + extra legacy columns ignored.
        assert_eq!(rows[3].0, "Multi\nline");
        assert_eq!(rows[3].1, "Waarde");
        assert_eq!(rows[3].2, 4);
    }
}

// ---------- catalog attribute groups (etc/catalog_attributes.xml) ----------

pub(crate) struct RawCatalogAttr {
    pub group: String,
    pub attribute: String,
    pub line: u32,
}

/// Parse `catalog_attributes.xml`: `<group name=…><attribute name=…/></group>`.
pub(crate) fn catalog_attributes_xml(xml: &str) -> Vec<RawCatalogAttr> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut group = String::new();
    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(&e).as_str() {
                "group" => group = attr(&e, b"name").unwrap_or_default(),
                "attribute" => {
                    if let Some(name) = attr(&e, b"name") {
                        if !group.is_empty() {
                            out.push(RawCatalogAttr { group: group.clone(), attribute: name, line });
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) if e.name().as_ref() == b"group" => group.clear(),
            _ => {}
        }
        buf.clear();
    }
    out
}

// ---------- email templates (etc/email_templates.xml) ----------

pub(crate) struct RawEmailTemplate {
    /// The id is also the value stored in config when the template is selected.
    pub id: String,
    pub label: String,
    /// File name relative to `<module>/view/<area>/email/`.
    pub file: String,
    /// `html` or `text`.
    pub kind: String,
    /// The module whose view dir holds the file (may differ from the declaring module).
    pub module: String,
    pub area: String,
    pub line: u32,
}

/// Parse `email_templates.xml`: flat `<template id= label= file= type= module= area=/>`.
pub(crate) fn email_templates_xml(xml: &str) -> Vec<RawEmailTemplate> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if local_name(&e) == "template" => {
                out.push(RawEmailTemplate {
                    id: attr(&e, b"id").unwrap_or_default(),
                    label: attr(&e, b"label").unwrap_or_default(),
                    file: attr(&e, b"file").unwrap_or_default(),
                    kind: attr(&e, b"type").unwrap_or_default(),
                    module: attr(&e, b"module").unwrap_or_default(),
                    area: attr(&e, b"area").unwrap_or_else(|| "frontend".to_string()),
                    line,
                });
            }
            _ => {}
        }
        buf.clear();
    }
    out
}

// ---------- widgets (etc/widget.xml) ----------

pub(crate) struct RawWidgetParam {
    pub name: String,
    /// The `xsi:type` (`text`, `select`, `block`, …).
    pub param_type: String,
    pub required: bool,
    pub label: String,
    pub source_model: Option<ClassName>,
    /// `<value>` default.
    pub default: Option<String>,
}

pub(crate) struct RawWidget {
    pub id: String,
    pub class: Option<ClassName>,
    pub label: String,
    pub description: Option<String>,
    pub parameters: Vec<RawWidgetParam>,
    pub containers: Vec<String>,
    pub line: u32,
}

/// Parse `widget.xml`. The subtleties: a `<parameter>` inside `<depends>` is a
/// *reference* to another parameter (never a definition — the db_schema column-reference
/// pattern), and `<label>` occurs at widget, parameter, and option level — routed to the
/// innermost open context, with option labels ignored.
pub(crate) fn widget_xml(xml: &str) -> Vec<RawWidget> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawWidget> = Vec::new();
    let mut cur: Option<usize> = None;
    let mut cur_param: Option<usize> = None;
    let mut in_depends = false;
    let mut in_options = false;
    let mut text_into: Option<&'static str> = None; // "label" | "description" | "value"

    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                match local_name(&e).as_str() {
                    "depends" => in_depends = true,
                    "options" => in_options = true,
                    _ => {}
                }
                widget_element(&e, line, in_depends, in_options, &mut out, &mut cur, &mut cur_param, &mut text_into);
            }
            Ok(Event::Empty(e)) => widget_element(
                &e, line, in_depends, in_options, &mut out, &mut cur, &mut cur_param, &mut text_into,
            ),
            Ok(Event::Text(e)) => {
                let (Some(field), Some(i)) = (text_into, cur) else { continue };
                let t = e.unescape().unwrap_or_default().trim().to_string();
                if t.is_empty() {
                    continue;
                }
                match (field, cur_param) {
                    ("label", Some(p)) => out[i].parameters[p].label = t,
                    ("label", None) => out[i].label = t,
                    ("description", _) => out[i].description = Some(t),
                    ("value", Some(p)) => out[i].parameters[p].default = Some(t),
                    _ => {}
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"widget" => {
                    cur = None;
                    cur_param = None;
                }
                b"parameter" if !in_depends => cur_param = None,
                b"depends" => in_depends = false,
                b"options" => in_options = false,
                b"label" | b"description" | b"value" => text_into = None,
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn widget_element(
    e: &BytesStart,
    line: u32,
    in_depends: bool,
    in_options: bool,
    out: &mut Vec<RawWidget>,
    cur: &mut Option<usize>,
    cur_param: &mut Option<usize>,
    text_into: &mut Option<&'static str>,
) {
    match local_name(e).as_str() {
        "widget" => {
            out.push(RawWidget {
                id: attr(e, b"id").unwrap_or_default(),
                class: attr(e, b"class").map(ClassName::new),
                label: String::new(),
                description: None,
                parameters: Vec::new(),
                containers: Vec::new(),
                line,
            });
            *cur = Some(out.len() - 1);
        }
        "parameter" if !in_depends => {
            if let Some(i) = *cur {
                out[i].parameters.push(RawWidgetParam {
                    name: attr(e, b"name").unwrap_or_default(),
                    param_type: xsi_type(e).unwrap_or_default(),
                    required: attr_true(e, b"required"),
                    label: String::new(),
                    source_model: attr(e, b"source_model").map(ClassName::new),
                    default: None,
                });
                *cur_param = Some(out[i].parameters.len() - 1);
            }
        }
        "container" => {
            if let (Some(i), Some(name)) = (*cur, attr(e, b"name")) {
                if !out[i].containers.contains(&name) {
                    out[i].containers.push(name);
                }
            }
        }
        // Text leaves — option labels are ignored (in_options).
        "label" if !in_options => *text_into = Some("label"),
        "description" => *text_into = Some("description"),
        "value" if !in_options => *text_into = Some("value"),
        _ => {}
    }
}

#[cfg(test)]
mod widget_tests {
    use super::widget_xml;

    #[test]
    fn params_depends_and_options() {
        let xml = r#"<widgets>
            <widget id="products_list" class="Magento\CatalogWidget\Block\Product\ProductsList">
                <label>Catalog Products List</label>
                <description>List of Products</description>
                <parameters>
                    <parameter name="title" xsi:type="text" required="false"><label>Title</label></parameter>
                    <parameter name="products_per_page" xsi:type="text" required="true">
                        <label>Per Page</label>
                        <depends><parameter name="show_pager" value="1"/></depends>
                        <value>5</value>
                    </parameter>
                    <parameter name="template" xsi:type="select" required="true">
                        <label>Template</label>
                        <options>
                            <option name="default" value="grid.phtml" selected="true"><label>Grid</label></option>
                        </options>
                    </parameter>
                </parameters>
                <containers><container name="sidebar.main"><template name="default" value="g"/></container></containers>
            </widget>
        </widgets>"#;
        let widgets = widget_xml(xml);
        assert_eq!(widgets.len(), 1);
        let w = &widgets[0];
        assert_eq!(w.id, "products_list");
        assert_eq!(w.label, "Catalog Products List");
        assert_eq!(w.description.as_deref(), Some("List of Products"));
        // The <parameter> inside <depends> must not become a definition.
        assert_eq!(w.parameters.len(), 3);
        assert_eq!(w.parameters[1].name, "products_per_page");
        assert!(w.parameters[1].required);
        assert_eq!(w.parameters[1].default.as_deref(), Some("5"));
        // The option's label must not overwrite the parameter's.
        assert_eq!(w.parameters[2].label, "Template");
        assert_eq!(w.containers, ["sidebar.main"]);
    }
}
