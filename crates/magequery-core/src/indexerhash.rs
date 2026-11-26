//! `indexer_state.hash_config`, computed without PHP: what Indexer's `Setup\Recurring`
//! compares on every `setup:upgrade` to decide whether an indexer's configuration
//! changed (and so whether to invalidate it).
//!
//! The hash is `Encryptor::hash(Json\Encoder::encode($config), HASH_VERSION_MD5)` —
//! HMAC-MD5 under the current crypt key — of each indexer's entry in
//! `Indexer\Config::getIndexers()`. Producing that entry byte for byte means
//! porting the whole read:
//!
//! 1. `Config\Dom` merging every enabled module's `etc/indexer.xml` in load order,
//!    with the reader's id attributes (`indexer@id`, `fieldset@name`, …);
//! 2. `Indexer\Config\Converter::convert`, including its `usort` of fieldsets with
//!    a comparator that is not a consistent order — so the result depends on PHP's
//!    sort algorithm (`zend_sort`'s small-array networks, made stable as PHP 8
//!    does), which is ported for up to five fieldsets;
//! 3. `json_encode` with PHP's default escaping (`\/`, `\uXXXX`).
//!
//! Anything this cannot reproduce exactly is an `Err`, which the caller treats as
//! "ask PHP".

use std::collections::HashMap;

use md5::{Digest, Md5};
use quick_xml::events::Event;

use crate::parse::xml::{text as xml_text, Reader};

use crate::{deploy, Error, Magento, Result};

// ---------- a small DOM, enough for Config\Dom ----------

#[derive(Debug, Clone)]
enum Node {
    Element(Element),
    Text(String),
    Cdata(String),
    Other,
}

#[derive(Debug, Clone)]
struct Element {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
}

impl Element {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn set_attr(&mut self, name: &str, value: &str) {
        match self.attrs.iter_mut().find(|(k, _)| k == name) {
            Some((_, v)) => *v = value.to_string(),
            None => self.attrs.push((name.to_string(), value.to_string())),
        }
    }

    /// DOM `nodeValue` of an element: its text content.
    fn text(&self) -> String {
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

    fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|c| match c {
            Node::Element(e) => Some(e),
            _ => None,
        })
    }

    /// `Dom::_isTextNode`: exactly one child, and it is a text node.
    fn is_text_node(&self) -> bool {
        matches!(self.children.as_slice(), [Node::Text(_)])
    }

    fn is_cdata_node(&self) -> bool {
        self.children.iter().any(|c| matches!(c, Node::Cdata(_)))
            && self.children.iter().all(|c| matches!(c, Node::Cdata(_) | Node::Text(_)))
    }
}

fn parse_dom(xml: &str) -> std::result::Result<Element, String> {
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    let attrs_of = |e: &quick_xml::events::BytesStart| -> std::result::Result<Vec<(String, String)>, String> {
        let mut out = Vec::new();
        for a in e.attributes() {
            let a = a.map_err(|e| e.to_string())?;
            let key = a.key.as_ref().to_string();
            let value = quick_xml::escape::unescape(&a.value).map_err(|e| e.to_string())?.into_owned();
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

/// `Indexer\Config\Reader::$_idAttributes`.
fn id_attribute(path: &[&str]) -> Option<&'static str> {
    match path {
        ["config", "indexer"] => Some("id"),
        ["config", "indexer", "handler"] | ["config", "indexer", "source"] | ["config", "indexer", "fieldset"] => {
            Some("name")
        }
        ["config", "indexer", "fieldset", "field"] => Some("name"),
        ["config", "indexer", "dependencies", "indexer"] => Some("id"),
        _ => None,
    }
}

/// One XPath step: a tag name and an optional `[@attr='value']`.
type Step = (String, Option<(&'static str, String)>);

fn step_for(el: &Element, names: &[&str]) -> Step {
    let pred = id_attribute(names)
        .and_then(|a| el.attr(a).filter(|v| !v.is_empty() && *v != "0").map(|v| (a, v.to_string())));
    (el.name.clone(), pred)
}

fn step_matches(el: &Element, step: &Step) -> bool {
    el.name == step.0 && step.1.as_ref().is_none_or(|(a, v)| el.attr(a) == Some(v.as_str()))
}

/// `Dom::_getMatchedNode`: the element at an absolute path; more than one is an error.
fn matched<'a>(root: &'a mut Element, path: &[Step]) -> std::result::Result<Option<&'a mut Element>, String> {
    let Some((first, rest)) = path.split_first() else { return Ok(None) };
    if !step_matches(root, first) {
        return Ok(None);
    }
    // Count first, then descend along the single match.
    fn count(el: &Element, path: &[Step]) -> usize {
        match path.split_first() {
            None => 1,
            Some((s, rest)) => el.elements().filter(|c| step_matches(c, s)).map(|c| count(c, rest)).sum(),
        }
    }
    match count(root, rest) {
        0 => return Ok(None),
        1 => {}
        _ => return Err("more than one node matches a merge path".into()),
    }
    fn descend<'a>(el: &'a mut Element, path: &[Step]) -> Option<&'a mut Element> {
        let Some((s, rest)) = path.split_first() else { return Some(el) };
        for c in el.children.iter_mut() {
            if let Node::Element(c) = c
                && step_matches(c, s)
                && count_ok(c, rest)
            {
                return descend(c, rest);
            }
        }
        None
    }
    fn count_ok(el: &Element, path: &[Step]) -> bool {
        match path.split_first() {
            None => true,
            Some((s, rest)) => el.elements().any(|c| step_matches(c, s) && count_ok(c, rest)),
        }
    }
    Ok(descend(root, rest))
}

/// `Dom::_mergeNode`.
fn merge_node(
    base: &mut Element,
    node: &Element,
    parent: &[Step],
    names: &mut Vec<String>,
) -> std::result::Result<(), String> {
    names.push(node.name.clone());
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut path = parent.to_vec();
    path.push(step_for(node, &name_refs));
    let result = (|| {
        if let Some(m) = matched(base, &path)? {
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
                for c in node.elements() {
                    merge_node(base, c, &path, names)?;
                }
            }
            Ok(())
        } else {
            let p = matched(base, parent)?.ok_or("merge parent not found")?;
            p.children.push(Node::Element(node.clone()));
            Ok(())
        }
    })();
    names.pop();
    result
}

// ---------- PHP values as Converter builds them ----------

/// A PHP array/scalar, enough for `json_encode`.
#[derive(Debug, Clone, PartialEq)]
enum Val {
    Null,
    Str(String),
    /// Ordered keys; a key is either an int (list position) or a string.
    Arr(Vec<(Key, Val)>),
}

#[derive(Debug, Clone, PartialEq)]
enum Key {
    Int(i64),
    Str(String),
}

impl Val {
    fn arr() -> Val {
        Val::Arr(Vec::new())
    }
    fn s(v: Option<&str>) -> Val {
        v.map_or(Val::Null, |s| Val::Str(s.to_string()))
    }
    fn get(&self, k: &str) -> Option<&Val> {
        match self {
            Val::Arr(items) => items.iter().find(|(key, _)| matches!(key, Key::Str(s) if s == k)).map(|(_, v)| v),
            _ => None,
        }
    }
    fn get_mut(&mut self, k: &str) -> Option<&mut Val> {
        match self {
            Val::Arr(items) => {
                items.iter_mut().find(|(key, _)| matches!(key, Key::Str(s) if s == k)).map(|(_, v)| v)
            }
            _ => None,
        }
    }
    /// `$a[$k] = $v`: overwrite in place, or append. PHP turns a numeric string key
    /// into an int; indexer names are not numeric, so that is an error here.
    fn set(&mut self, k: &str, v: Val) -> std::result::Result<(), String> {
        if k.parse::<i64>().is_ok() {
            return Err(format!("numeric array key {k}"));
        }
        let Val::Arr(items) = self else { return Err("not an array".into()) };
        match items.iter_mut().find(|(key, _)| matches!(key, Key::Str(s) if s == k)) {
            Some((_, slot)) => *slot = v,
            None => items.push((Key::Str(k.to_string()), v)),
        }
        Ok(())
    }
    /// `$a[] = $v`.
    fn push(&mut self, v: Val) {
        if let Val::Arr(items) = self {
            let next = items.iter().filter_map(|(k, _)| if let Key::Int(i) = k { Some(*i + 1) } else { None }).max();
            items.push((Key::Int(next.unwrap_or(0).max(0)), v));
        }
    }
    fn str_of(&self, k: &str) -> Option<&str> {
        match self.get(k) {
            Some(Val::Str(s)) => Some(s),
            _ => None,
        }
    }
}

/// PHP's default `json_encode`.
fn json(v: &Val, out: &mut String) {
    match v {
        Val::Null => out.push_str("null"),
        Val::Str(s) => json_str(s, out),
        Val::Arr(items) => {
            let is_list = items.iter().enumerate().all(|(i, (k, _))| matches!(k, Key::Int(n) if *n == i as i64));
            if is_list {
                out.push('[');
                for (i, (_, v)) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    json(v, out);
                }
                out.push(']');
            } else {
                out.push('{');
                for (i, (k, v)) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    match k {
                        Key::Int(n) => json_str(&n.to_string(), out),
                        Key::Str(s) => json_str(s, out),
                    }
                    out.push(':');
                    json(v, out);
                }
                out.push('}');
            }
        }
    }
}

/// `json_encode` of a string with PHP's default flags (`\\/`, `\\uXXXX`).
pub(crate) fn json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------- Indexer\Config\Converter ----------

fn attr_val(e: &Element, name: &str) -> Val {
    Val::s(e.attr(name))
}

fn convert_indexer(node: &Element) -> std::result::Result<(String, Val), String> {
    let id = node.attr("id").unwrap_or_default().to_string();
    let mut data = Val::arr();
    data.set("indexer_id", Val::s(node.attr("id")))?;
    data.set("primary", attr_val(node, "primary"))?;
    data.set("view_id", attr_val(node, "view_id"))?;
    data.set("action_class", attr_val(node, "class"))?;
    data.set("shared_index", attr_val(node, "shared_index"))?;
    data.set("title", Val::Str(String::new()))?;
    data.set("description", Val::Str(String::new()))?;
    data.set("dependencies", Val::arr())?;
    for child in node.elements() {
        if data.get("fieldsets").is_none() {
            data.set("fieldsets", Val::arr())?;
        }
        match child.name.as_str() {
            "title" => data.set("title", Val::Str(child.text()))?,
            "description" => data.set("description", Val::Str(child.text()))?,
            "saveHandler" => data.set("saveHandler", attr_val(child, "class"))?,
            "structure" => data.set("structure", attr_val(child, "class"))?,
            "fieldset" => convert_fieldset(child, &mut data)?,
            "dependencies" => {
                let deps = data.get_mut("dependencies").ok_or("dependencies")?;
                for c in child.children.iter() {
                    if let Node::Element(c) = c
                        && c.name == "indexer"
                    {
                        deps.push(attr_val(c, "id"));
                    }
                }
            }
            _ => {}
        }
    }
    Ok((id, data))
}

fn convert_fieldset(node: &Element, data: &mut Val) -> std::result::Result<(), String> {
    let name = node.attr("name").unwrap_or_default().to_string();
    let mut fs = Val::arr();
    fs.set("source", attr_val(node, "source"))?;
    fs.set("name", attr_val(node, "name"))?;
    fs.set("provider", attr_val(node, "provider"))?;
    fs.set("fields", Val::arr())?;
    for child in node.elements() {
        match child.name.as_str() {
            "field" => {
                let fname = child.attr("name").unwrap_or_default().to_string();
                let mut f = Val::arr();
                f.set("name", attr_val(child, "name"))?;
                f.set("handler", attr_val(child, "handler"))?;
                let origin = child.attr("origin").filter(|o| !o.is_empty() && *o != "0").or(child.attr("name"));
                f.set("origin", Val::s(origin))?;
                f.set("dataType", attr_val(child, "dataType"))?;
                f.set("type", Val::Str(child.attr("xsi:type").unwrap_or_default().to_string()))?;
                let mut filters = Val::arr();
                for c in child.elements() {
                    filters.push(attr_val(c, "class"));
                }
                f.set("filters", filters)?;
                fs.get_mut("fields").ok_or("fields")?.set(&fname, f)?;
            }
            "reference" => {
                let target = child.attr("fieldset").unwrap_or_default().to_string();
                if fs.get("references").is_none() {
                    fs.set("references", Val::arr())?;
                }
                let mut r = Val::arr();
                r.set("fieldset", attr_val(child, "fieldset"))?;
                r.set("from", attr_val(child, "from"))?;
                r.set("to", attr_val(child, "to"))?;
                fs.get_mut("references").ok_or("references")?.set(&target, r)?;
                // addVirtualField() takes $data by value: no effect.
            }
            _ => {}
        }
    }
    let primary = data.str_of("primary").map(str::to_string);
    let list = data.get_mut("fieldsets").ok_or("fieldsets")?;
    list.set(&name, fs)?;
    sort_fieldsets(list, primary.as_deref())
}

/// `Converter::sorting`: `usort($data['fieldsets'], …)`.
fn sort_fieldsets(list: &mut Val, primary: Option<&str>) -> std::result::Result<(), String> {
    let Val::Arr(items) = list else { return Err("fieldsets".into()) };
    let mut vals: Vec<(usize, Val)> = items.drain(..).map(|(_, v)| v).enumerate().collect();
    let name_of = |v: &Val| v.str_of("name").map(str::to_string);
    let refs = |v: &Val, n: &Option<String>| -> bool {
        match (v.get("references"), n) {
            (Some(Val::Arr(r)), Some(n)) => r.iter().any(|(k, _)| matches!(k, Key::Str(s) if s == n)),
            _ => false,
        }
    };
    // The user comparator, then PHP 8's stable tie-break on the original order.
    let cmp = |a: &(usize, Val), b: &(usize, Val)| -> i32 {
        let (cur, par) = (&a.1, &b.1);
        let cur_name = name_of(cur);
        let par_name = name_of(par);
        let r = if (cur.get("references").is_none() && primary.map(str::to_string) == cur_name)
            || refs(par, &cur_name)
        {
            -1
        } else if par.get("references").is_none() || refs(cur, &par_name) {
            1
        } else {
            0
        };
        if r != 0 {
            r
        } else if a.0 < b.0 {
            -1
        } else {
            1
        }
    };
    zend_insert_sort(&mut vals, &cmp)?;
    *items = vals.into_iter().enumerate().map(|(i, (_, v))| (Key::Int(i as i64), v)).collect();
    Ok(())
}

/// `zend_insert_sort` for the sizes it special-cases (`zend_sort_2` … `zend_sort_5`).
fn zend_insert_sort<T>(v: &mut [T], cmp: &dyn Fn(&T, &T) -> i32) -> std::result::Result<(), String> {
    let gt = |v: &[T], i: usize, j: usize| cmp(&v[i], &v[j]) > 0;
    let sort3 = |v: &mut [T], a: usize, b: usize, c: usize| {
        if !gt(v, a, b) {
            if !gt(v, b, c) {
                return;
            }
            v.swap(b, c);
            if gt(v, a, b) {
                v.swap(a, b);
            }
            return;
        }
        if !gt(v, c, b) {
            v.swap(a, c);
            return;
        }
        v.swap(a, b);
        if gt(v, b, c) {
            v.swap(b, c);
        }
    };
    let sort4 = |v: &mut [T]| {
        sort3(v, 0, 1, 2);
        if gt(v, 2, 3) {
            v.swap(2, 3);
            if gt(v, 1, 2) {
                v.swap(1, 2);
                if gt(v, 0, 1) {
                    v.swap(0, 1);
                }
            }
        }
    };
    match v.len() {
        0 | 1 => {}
        2 => {
            if gt(v, 0, 1) {
                v.swap(0, 1);
            }
        }
        3 => sort3(v, 0, 1, 2),
        4 => sort4(v),
        5 => {
            sort4(v);
            if gt(v, 3, 4) {
                v.swap(3, 4);
                if gt(v, 2, 3) {
                    v.swap(2, 3);
                    if gt(v, 1, 2) {
                        v.swap(1, 2);
                        if gt(v, 0, 1) {
                            v.swap(0, 1);
                        }
                    }
                }
            }
        }
        n => return Err(format!("{n} fieldsets: PHP's insertion sort for more than five is not ported")),
    }
    Ok(())
}

/// `Converter::sortByDependencies`, then each indexer's `dependencies` re-listed in
/// that order.
fn sort_by_dependencies(indexers: Vec<(String, Val)>) -> std::result::Result<Vec<(String, Val)>, String> {
    let deps_of = |v: &Val| -> Vec<String> {
        match v.get("dependencies") {
            Some(Val::Arr(d)) => d.iter().filter_map(|(_, v)| if let Val::Str(s) = v { Some(s.clone()) } else { None }).collect(),
            _ => Vec::new(),
        }
    };
    let map: HashMap<String, Vec<String>> = indexers.iter().map(|(id, v)| (id.clone(), deps_of(v))).collect();
    fn expand(map: &HashMap<String, Vec<String>>, id: &str, acc: &mut Vec<String>) -> std::result::Result<Vec<String>, String> {
        acc.push(id.to_string());
        let direct = map.get(id).cloned().unwrap_or_default();
        let mut result = direct.clone();
        for dep in &direct {
            if acc.contains(dep) {
                return Err(format!("circular indexer dependency {id} → {dep}"));
            }
            if !map.contains_key(dep) {
                return Err(format!("indexer {id} depends on unknown {dep}"));
            }
            result.extend(expand(map, dep, &mut acc.clone())?);
        }
        let mut seen = Vec::new();
        for r in result {
            if !seen.contains(&r) {
                seen.push(r);
            }
        }
        Ok(seen)
    }
    let mut expanded: Vec<(String, Vec<String>)> = Vec::new();
    for (id, _) in &indexers {
        expanded.push((id.clone(), expand(&map, id, &mut Vec::new())?));
    }
    let n = expanded.len();
    for i in 0..n.saturating_sub(1) {
        for j in i + 1..n {
            if expanded[i].1.contains(&expanded[j].0) {
                expanded.swap(i, j);
            }
        }
    }
    let order: Vec<String> = expanded.into_iter().map(|(id, _)| id).collect();
    let mut by_id: HashMap<String, Val> = indexers.into_iter().collect();
    let mut out = Vec::new();
    for id in &order {
        let mut v = by_id.remove(id).ok_or("indexer vanished")?;
        let deps = deps_of(&v);
        let mut list = Val::arr();
        for o in order.iter().filter(|o| deps.contains(o)) {
            list.push(Val::Str(o.clone()));
        }
        v.set("dependencies", list)?;
        out.push((id.clone(), v));
    }
    Ok(out)
}

fn hmac_md5(key: &[u8], data: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut k = if key.len() > BLOCK { Md5::digest(key).to_vec() } else { key.to_vec() };
    k.resize(BLOCK, 0);
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let inner = Md5::new().chain_update(&ipad).chain_update(data).finalize();
    let outer = Md5::new().chain_update(&opad).chain_update(inner).finalize();
    outer.iter().map(|b| format!("{b:02x}")).collect()
}

impl Magento {
    /// Each indexer's `hash_config`, as Indexer `Setup\Recurring` would compute it.
    #[doc(hidden)]
    pub fn indexer_config_hashes(&self) -> Result<Vec<(String, String)>> {
        let err = |detail: String| Error::Parse { file: "app/etc/env.php".into(), detail };
        let env = deploy::read_env(self.root(), &self.index.vfs)?;
        let keys = deploy::crypt_keys(&env);
        let key = keys.last().ok_or_else(|| err("no crypt key".into()))?;
        // `Encryptor::decodeKey`.
        let key: Vec<u8> = match key.strip_prefix("base64") {
            Some(b64) => {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| err(format!("crypt key: {e}")))?
            }
            None => key.as_bytes().to_vec(),
        };
        Ok(self.indexer_config_json()?.into_iter().map(|(id, json)| (id, hmac_md5(&key, json.as_bytes()))).collect())
    }

    /// Each indexer's entry of `Indexer\Config::getIndexers()`, as `Json\Encoder`
    /// writes it, in the config's order.
    #[doc(hidden)]
    pub fn indexer_config_json(&self) -> Result<Vec<(String, String)>> {
        let mut modules: Vec<_> = self.index.modules.iter().filter(|m| m.enabled).collect();
        modules.sort_by_key(|m| m.load_order);
        let mut merged: Option<Element> = None;
        for m in modules {
            let file = m.path.join("etc/indexer.xml");
            let Ok(text) = self.index.vfs.read_to_string(&file) else { continue };
            let dom = parse_dom(&text).map_err(|detail| Error::Parse { file: file.clone(), detail })?;
            match merged.as_mut() {
                None => merged = Some(dom),
                Some(base) => merge_node(base, &dom, &[], &mut Vec::new()).map_err(|detail| Error::Parse { file, detail })?,
            }
        }
        let Some(root) = merged else { return Ok(Vec::new()) };

        let err = |detail: String| Error::Parse { file: "etc/indexer.xml".into(), detail };
        let mut indexers = Vec::new();
        for node in root.elements().filter(|e| e.name == "indexer") {
            indexers.push(convert_indexer(node).map_err(err)?);
        }
        let mut indexers = sort_by_dependencies(indexers).map_err(err)?;

        // Magento_Catalog's `IndexerConfigData` plugins drop the flat indexers from
        // `Config\Data::get()` unless flat catalog is switched on.
        if self.index.modules.iter().any(|m| m.enabled && m.name.as_str() == "Magento_Catalog") {
            for (id, path) in [
                ("catalog_category_flat", "catalog/frontend/flat_catalog_category"),
                ("catalog_product_flat", "catalog/frontend/flat_catalog_product"),
            ] {
                if !self.flag_at_default_store(path)? {
                    indexers.retain(|(i, _)| i != id);
                }
            }
        }

        Ok(indexers
            .into_iter()
            .map(|(id, v)| {
                let mut s = String::new();
                json(&v, &mut s);
                (id, s)
            })
            .collect())
    }

    /// `ScopeConfig::isSetFlag($path, SCOPE_STORE)` as setup:upgrade sees it. Only
    /// answered when nothing below the default scope sets the path, since which
    /// store a CLI run resolves to is not worth porting for this.
    fn flag_at_default_store(&self, path: &str) -> Result<bool> {
        let set = self.config(true)?;
        if set.scopes_for(path).iter().any(|v| v.scope != "default") {
            return Err(Error::Parse {
                file: "core_config_data".into(),
                detail: format!("{path} is set per website or store"),
            });
        }
        Ok(set.get("default", path).is_some_and(|v| !v.value.is_empty() && v.value != "0"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_md5_matches_rfc_2104() {
        // RFC 2104 test case 2.
        assert_eq!(hmac_md5(b"Jefe", b"what do ya want for nothing?"), "750c783e6ab0b503eaa86e310a5db738");
    }

    #[test]
    fn json_escapes_like_php() {
        let mut s = String::new();
        json_str("Magento\\Foo/Bar é", &mut s);
        assert_eq!(s, "\"Magento\\\\Foo\\/Bar \\u00e9\"");
    }
}
