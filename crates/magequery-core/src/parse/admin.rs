//! Parsers for the admin domain.

use super::*;

// ---------- admin system configuration (adminhtml/system.xml) ----------

pub(crate) struct RawSysTab {
    pub id: String,
    pub label: String,
}

pub(crate) struct RawSysField {
    pub id: String,
    pub label: String,
    pub field_type: String,
    pub config_path: Option<String>,
    pub source_model: Option<String>,
    pub backend_model: Option<String>,
    pub show_default: bool,
    pub show_website: bool,
    pub show_store: bool,
    pub line: u32,
}

pub(crate) struct RawSysGroup {
    pub id: String,
    pub label: String,
    pub fields: Vec<RawSysField>,
}

pub(crate) struct RawSysSection {
    pub id: String,
    pub label: String,
    pub tab: Option<String>,
    pub resource: Option<String>,
    pub groups: Vec<RawSysGroup>,
}

pub(crate) struct RawSystem {
    pub tabs: Vec<RawSysTab>,
    pub sections: Vec<RawSysSection>,
}

/// Where the next text run belongs (the inner leaf elements of system.xml carry their value
/// as text, not attributes).
enum SysText {
    TabLabel,
    SecLabel,
    SecTab,
    SecResource,
    GrpLabel,
    FieldLabel,
    FieldSource,
    FieldBackend,
    FieldConfigPath,
}

/// Parse a module's `adminhtml/system.xml` into the tab/section/group/field tree (raw, with
/// line numbers; provenance + cross-module merge happen in `breadth::SystemConfigIndex`).
pub(crate) fn system_xml(xml: &str) -> RawSystem {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();

    let mut tabs: Vec<RawSysTab> = Vec::new();
    let mut sections: Vec<RawSysSection> = Vec::new();
    let mut cur_tab: Option<usize> = None;
    let mut cur_section: Option<usize> = None;
    let mut cur_group: Option<usize> = None;
    let mut cur_field: Option<usize> = None;
    let mut target: Option<SysText> = None;

    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local_name(&e).as_str() {
                "tab" if attr(&e, b"id").is_some() => {
                    tabs.push(RawSysTab { id: attr(&e, b"id").unwrap_or_default(), label: String::new() });
                    cur_tab = Some(tabs.len() - 1);
                }
                // A `<tab>` with no id, inside a section, is the section's tab *reference*.
                "tab" => target = Some(SysText::SecTab),
                "section" => {
                    sections.push(RawSysSection {
                        id: attr(&e, b"id").unwrap_or_default(),
                        label: String::new(),
                        tab: None,
                        resource: None,
                        groups: Vec::new(),
                    });
                    cur_tab = None;
                    cur_section = Some(sections.len() - 1);
                    cur_group = None;
                    cur_field = None;
                }
                "group" => {
                    if let Some(s) = cur_section {
                        sections[s].groups.push(RawSysGroup {
                            id: attr(&e, b"id").unwrap_or_default(),
                            label: String::new(),
                            fields: Vec::new(),
                        });
                        cur_group = Some(sections[s].groups.len() - 1);
                        cur_field = None;
                    }
                }
                "field" => {
                    if let (Some(s), Some(g)) = (cur_section, cur_group) {
                        sections[s].groups[g].fields.push(RawSysField {
                            id: attr(&e, b"id").unwrap_or_default(),
                            label: String::new(),
                            field_type: attr(&e, b"type").unwrap_or_default(),
                            config_path: None,
                            source_model: None,
                            backend_model: None,
                            show_default: attr(&e, b"showInDefault").as_deref() == Some("1"),
                            show_website: attr(&e, b"showInWebsite").as_deref() == Some("1"),
                            show_store: attr(&e, b"showInStore").as_deref() == Some("1"),
                            line,
                        });
                        cur_field = Some(sections[s].groups[g].fields.len() - 1);
                    }
                }
                // Leaf text elements — route by the innermost open container.
                "label" => {
                    target = Some(if cur_field.is_some() {
                        SysText::FieldLabel
                    } else if cur_group.is_some() {
                        SysText::GrpLabel
                    } else if cur_section.is_some() {
                        SysText::SecLabel
                    } else {
                        SysText::TabLabel
                    });
                }
                "resource" => target = Some(SysText::SecResource),
                "source_model" => target = Some(SysText::FieldSource),
                "backend_model" => target = Some(SysText::FieldBackend),
                "config_path" => target = Some(SysText::FieldConfigPath),
                _ => {}
            },
            Ok(Event::Text(e)) => {
                let Some(t) = &target else { continue };
                let text = e.unescape().unwrap_or_default().trim().to_string();
                if text.is_empty() {
                    continue;
                }
                write_sys_text(t, &text, &mut tabs, &mut sections, cur_tab, cur_section, cur_group, cur_field);
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                // Closing a leaf element ends its text capture.
                b"label" | b"tab" | b"resource" | b"source_model" | b"backend_model"
                | b"config_path" => target = None,
                b"field" => cur_field = None,
                b"group" => cur_group = None,
                b"section" => {
                    cur_section = None;
                    cur_group = None;
                    cur_field = None;
                }
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }

    RawSystem { tabs, sections }
}

#[allow(clippy::too_many_arguments)]
fn write_sys_text(
    target: &SysText,
    text: &str,
    tabs: &mut [RawSysTab],
    sections: &mut [RawSysSection],
    cur_tab: Option<usize>,
    cur_section: Option<usize>,
    cur_group: Option<usize>,
    cur_field: Option<usize>,
) {
    match target {
        SysText::TabLabel => {
            if let Some(t) = cur_tab {
                tabs[t].label = text.to_string();
            }
        }
        SysText::SecLabel => {
            if let Some(s) = cur_section {
                sections[s].label = text.to_string();
            }
        }
        SysText::SecTab => {
            if let Some(s) = cur_section {
                sections[s].tab = Some(text.to_string());
            }
        }
        SysText::SecResource => {
            if let Some(s) = cur_section {
                sections[s].resource = Some(text.to_string());
            }
        }
        SysText::GrpLabel => {
            if let (Some(s), Some(g)) = (cur_section, cur_group) {
                sections[s].groups[g].label = text.to_string();
            }
        }
        SysText::FieldLabel | SysText::FieldSource | SysText::FieldBackend | SysText::FieldConfigPath => {
            if let (Some(s), Some(g), Some(f)) = (cur_section, cur_group, cur_field) {
                let field = &mut sections[s].groups[g].fields[f];
                match target {
                    SysText::FieldLabel => field.label = text.to_string(),
                    SysText::FieldSource => field.source_model = Some(text.to_string()),
                    SysText::FieldBackend => field.backend_model = Some(text.to_string()),
                    SysText::FieldConfigPath => field.config_path = Some(text.to_string()),
                    _ => {}
                }
            }
        }
    }
}

// ---------- admin ACL (acl.xml) ----------

/// A `<resource>` as written in one `acl.xml`. `title` is empty when the element only re-states
/// an ancestor as a path anchor (to attach children under another module's resource); the
/// declaring module is the one that gives it a non-empty title. Parent comes from nesting.
pub(crate) struct RawAclResource {
    pub id: String,
    pub title: String,
    pub parent: Option<String>,
    pub sort_order: Option<i32>,
    pub disabled: bool,
    pub line: u32,
}

/// Parse a module's `acl.xml` into a flat list of `<resource>` declarations, each tagged with
/// its parent id (from nesting) and line. Cross-module merge happens in `breadth::AclIndex`.
pub(crate) fn acl_xml(xml: &str) -> Vec<RawAclResource> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawAclResource> = Vec::new();
    // The enclosing `<resource>` ids, so a child knows its parent.
    let mut stack: Vec<String> = Vec::new();

    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) if local_name(&e) == "resource" => {
                let id = attr(&e, b"id").unwrap_or_default();
                out.push(raw_acl(&e, id.clone(), stack.last().cloned(), line));
                // Only a non-self-closing element can hold children.
                stack.push(id);
            }
            Ok(Event::Empty(e)) if local_name(&e) == "resource" => {
                let id = attr(&e, b"id").unwrap_or_default();
                out.push(raw_acl(&e, id, stack.last().cloned(), line));
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"resource" => {
                stack.pop();
            }
            _ => {}
        }
        buf.clear();
    }
    out
}

fn raw_acl(e: &BytesStart, id: String, parent: Option<String>, line: u32) -> RawAclResource {
    RawAclResource {
        id,
        title: attr(e, b"title").unwrap_or_default(),
        parent,
        sort_order: attr(e, b"sortOrder").and_then(|s| s.parse().ok()),
        disabled: attr_true(e, b"disabled"),
        line,
    }
}

#[cfg(test)]
mod acl_tests {
    use super::acl_xml;

    #[test]
    fn nests_and_attributes_parent() {
        let xml = r#"<?xml version="1.0"?>
<config>
    <acl>
        <resources>
            <resource id="Magento_Backend::admin" title="Magento Admin">
                <resource id="Magento_Sales::sales" title="Sales" sortOrder="20">
                    <resource id="Magento_Sales::sales_order" title="Orders"/>
                </resource>
            </resource>
        </resources>
    </acl>
</config>"#;
        let res = acl_xml(xml);
        let by = |id: &str| res.iter().find(|r| r.id == id).unwrap();

        assert_eq!(by("Magento_Backend::admin").parent, None);
        assert_eq!(by("Magento_Backend::admin").title, "Magento Admin");

        let sales = by("Magento_Sales::sales");
        assert_eq!(sales.parent.as_deref(), Some("Magento_Backend::admin"));
        assert_eq!(sales.sort_order, Some(20));

        // A self-closing leaf gets the right parent and does not affect the stack.
        let order = by("Magento_Sales::sales_order");
        assert_eq!(order.parent.as_deref(), Some("Magento_Sales::sales"));
        assert_eq!(order.title, "Orders");
    }

    #[test]
    fn anchor_restatement_has_empty_title() {
        // A second module re-states ancestors (no title) only to attach a new child.
        let xml = r#"<config><acl><resources>
            <resource id="Magento_Backend::admin">
                <resource id="Magento_Sales::sales">
                    <resource id="Vendor_Module::thing" title="Thing"/>
                </resource>
            </resource>
        </resources></acl></config>"#;
        let res = acl_xml(xml);
        assert_eq!(res.iter().find(|r| r.id == "Magento_Backend::admin").unwrap().title, "");
        assert_eq!(res.iter().find(|r| r.id == "Magento_Sales::sales").unwrap().title, "");
        let thing = res.iter().find(|r| r.id == "Vendor_Module::thing").unwrap();
        assert_eq!(thing.parent.as_deref(), Some("Magento_Sales::sales"));
        assert_eq!(thing.title, "Thing");
    }
}
