//! A small DOM and `Config\Dom`'s merge, shared by the config readers ported
//! here (indexer.xml, di.xml): parse each file, merge it onto the first by the
//! reader's id attributes, in load order.

use std::collections::HashMap;

use quick_xml::events::Event;

use crate::parse::xml::{text as xml_text, Reader};

/// What a `Config\Dom` merge is configured with: the reader's id attributes (by
/// element path from the root) and its type attribute (`xsi:type` for di.xml),
/// whose change makes a node replace the one it matches.
pub(crate) struct MergeConfig {
    pub(crate) id_attribute: fn(&[&str]) -> Option<&'static str>,
    pub(crate) type_attribute: Option<&'static str>,
}


#[derive(Debug, Clone)]
pub(crate) enum Node {
    Element(Element),
    Text(String),
    Cdata(String),
    Other,
}

#[derive(Debug, Clone)]
pub(crate) struct Element {
    pub(crate) name: String,
    pub(crate) attrs: Vec<(String, String)>,
    pub(crate) children: Vec<Node>,
}

impl Element {
    pub(crate) fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub(crate) fn set_attr(&mut self, name: &str, value: &str) {
        match self.attrs.iter_mut().find(|(k, _)| k == name) {
            Some((_, v)) => *v = value.to_string(),
            None => self.attrs.push((name.to_string(), value.to_string())),
        }
    }

    /// DOM `nodeValue` of an element: its text content.
    #[cfg_attr(not(feature = "db"), allow(dead_code))]
    pub(crate) fn text(&self) -> String {
        let mut out = String::new();
        fn walk(e: &Element, out: &mut String) {
            for c in &e.children {
                match c {
                    Node::Text(t) | Node::Cdata(t) => out.push_str(t),
                    Node::Element(e) => walk(e, out),
                    Node::Other => {}
                }
            }
        }
        walk(self, &mut out);
        out
    }

    pub(crate) fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|c| match c {
            Node::Element(e) => Some(e),
            _ => None,
        })
    }

    /// `Dom::_isTextNode`: exactly one child, and it is a text node.
    pub(crate) fn is_text_node(&self) -> bool {
        matches!(self.children.as_slice(), [Node::Text(_)])
    }

    pub(crate) fn is_cdata_node(&self) -> bool {
        self.children.iter().any(|c| matches!(c, Node::Cdata(_)))
            && self.children.iter().all(|c| matches!(c, Node::Cdata(_) | Node::Text(_)))
    }
}

pub(crate) fn parse_dom(xml: &str) -> std::result::Result<Element, String> {
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    let attrs_of = |e: &quick_xml::events::BytesStart| -> std::result::Result<Vec<(String, String)>, String> {
        let mut out = Vec::new();
        for a in e.attributes() {
            let a = a.map_err(|e| e.to_string())?;
            let key = a.key.as_ref().to_string();
            // Attribute-value normalization (XML 1.0 §3.3.3): literal line ends
            // and tabs become spaces; character references are kept.
            let raw = a.value.replace("\r\n", "\n").replace(['\t', '\n', '\r'], " ");
            let value = quick_xml::escape::unescape(&raw).map_err(|e| e.to_string())?.into_owned();
            out.push((key, value));
        }
        Ok(out)
    };
    loop {
        match reader.read_event().map_err(|e| e.to_string())? {
            Event::Start(e) => stack.push(Element {
                name: e.name().as_ref().to_string(),
                attrs: attrs_of(&e)?,
                children: Vec::new(),
            }),
            Event::Empty(e) => {
                let el = Element {
                    name: e.name().as_ref().to_string(),
                    attrs: attrs_of(&e)?,
                    children: Vec::new(),
                };
                match stack.last_mut() {
                    Some(p) => p.children.push(Node::Element(el)),
                    None => root = Some(el),
                }
            }
            Event::End(_) => {
                let el = stack.pop().ok_or("unbalanced end tag")?;
                match stack.last_mut() {
                    Some(p) => p.children.push(Node::Element(el)),
                    None => root = Some(el),
                }
            }
            Event::Text(t) => {
                if let Some(p) = stack.last_mut() {
                    let s = xml_text(&t).to_string();
                    // libxml coalesces adjacent text (entity boundaries split quick-xml's).
                    match p.children.last_mut() {
                        Some(Node::Text(prev)) => prev.push_str(&s),
                        _ => p.children.push(Node::Text(s)),
                    }
                }
            }
            Event::CData(t) => {
                if let Some(p) = stack.last_mut() {
                    p.children.push(Node::Cdata((*t).to_string()));
                }
            }
            Event::Comment(_) | Event::PI(_) => {
                if let Some(p) = stack.last_mut() {
                    p.children.push(Node::Other);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    root.ok_or_else(|| "no document element".into())
}

/// One XPath step: a tag name and an optional `[@attr='value']`.
type Step = (String, Option<(&'static str, String)>);

fn step_for(cfg: &MergeConfig, el: &Element, names: &[&str]) -> Step {
    let pred = (cfg.id_attribute)(names)
        .and_then(|a| el.attr(a).filter(|v| !v.is_empty() && *v != "0").map(|v| (a, v.to_string())));
    (el.name.clone(), pred)
}

fn step_matches(el: &Element, step: &Step) -> bool {
    el.name == step.0 && step.1.as_ref().is_none_or(|(a, v)| el.attr(a) == Some(v.as_str()))
}

/// `Dom::merge`: merge document `dom` onto `base`, both rooted at the
/// document element.
///
/// `Dom::_mergeNode` finds a node's match with an absolute XPath from the root
/// (more than one match is an error). Below a unique match the only candidates
/// are that match's children, so this looks there — the same answer without
/// re-walking the tree — and indexes the root's children, which every
/// top-level node of every file is looked up among.
pub(crate) fn merge_document(cfg: &MergeConfig, base: &mut Element, dom: Element) -> std::result::Result<(), String> {
    if base.name != dom.name {
        return Err(format!("document element <{}> does not match <{}>", dom.name, base.name));
    }
    let root_name = dom.name.clone();
    let mut names = vec![dom.name.clone()];
    for (k, v) in &dom.attrs {
        base.set_attr(k, v);
    }
    if dom.children.is_empty() {
        return Ok(());
    }
    // The root's element children by name, and by name + id attribute value.
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    let mut by_id: HashMap<(String, String), Vec<usize>> = HashMap::new();
    let index = |by_name: &mut HashMap<String, Vec<usize>>, by_id: &mut HashMap<(String, String), Vec<usize>>, i: usize, el: &Element| {
        by_name.entry(el.name.clone()).or_default().push(i);
        if let Some(a) = (cfg.id_attribute)(&[root_name.as_str(), el.name.as_str()])
            && let Some(v) = el.attr(a)
        {
            by_id.entry((el.name.clone(), v.to_owned())).or_default().push(i);
        }
    };
    for (i, c) in base.children.iter().enumerate() {
        if let Node::Element(el) = c {
            index(&mut by_name, &mut by_id, i, el);
        }
    }
    for node in dom.children.into_iter().filter_map(|c| match c {
        Node::Element(e) => Some(e),
        _ => None,
    }) {
        names.push(node.name.clone());
        let step = {
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            step_for(cfg, &node, &refs)
        };
        let candidates = match &step.1 {
            Some((_, v)) => by_id.get(&(step.0.clone(), v.clone())),
            None => by_name.get(&step.0),
        };
        let matches: Vec<usize> = candidates
            .map(|c| c.iter().copied().filter(|&i| matches!(&base.children[i], Node::Element(e) if step_matches(e, &step))).collect())
            .unwrap_or_default();
        match matches.as_slice() {
            [] => {
                let i = base.children.len();
                index(&mut by_name, &mut by_id, i, &node);
                base.children.push(Node::Element(node));
            }
            [i] => {
                let Node::Element(m) = &mut base.children[*i] else { unreachable!() };
                merge_matched(cfg, m, node, &mut names)?;
            }
            _ => return Err("more than one node matches a merge path".into()),
        }
        names.pop();
    }
    Ok(())
}

/// `Dom::_mergeNode` for a node whose match `m` was found.
fn merge_matched(
    cfg: &MergeConfig,
    m: &mut Element,
    node: Element,
    names: &mut Vec<String>,
) -> std::result::Result<(), String> {
    // A different type attribute: the node replaces its match in place.
    if let Some(ty) = cfg.type_attribute
        && let (Some(a), Some(b)) = (node.attr(ty), m.attr(ty))
        && a != b
    {
        *m = node;
        return Ok(());
    }
    for (k, v) in &node.attrs {
        m.set_attr(k, v);
    }
    if node.children.is_empty() {
        return Ok(());
    }
    if node.is_text_node() {
        if m.children.is_empty() || m.is_text_node() || m.is_cdata_node() {
            let Node::Text(t) = &node.children[0] else { unreachable!() };
            m.children = vec![Node::Text(t.clone())];
        }
    } else if node.is_cdata_node() && (m.is_text_node() || m.is_cdata_node()) {
        let cdata = node.children.iter().find_map(|c| match c {
            Node::Cdata(t) => Some(t.clone()),
            _ => None,
        });
        if let Some(t) = cdata {
            m.children = vec![Node::Cdata(t)];
        }
    } else {
        for c in node.children.into_iter().filter_map(|c| match c {
            Node::Element(e) => Some(e),
            _ => None,
        }) {
            names.push(c.name.clone());
            let step = {
                let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                step_for(cfg, &c, &refs)
            };
            let matches: Vec<usize> = m
                .children
                .iter()
                .enumerate()
                .filter(|(_, n)| matches!(n, Node::Element(e) if step_matches(e, &step)))
                .map(|(i, _)| i)
                .collect();
            match matches.as_slice() {
                [] => m.children.push(Node::Element(c)),
                [i] => {
                    let Node::Element(child) = &mut m.children[*i] else { unreachable!() };
                    merge_matched(cfg, child, c, names)?;
                }
                _ => return Err("more than one node matches a merge path".into()),
            }
            names.pop();
        }
    }
    Ok(())
}
