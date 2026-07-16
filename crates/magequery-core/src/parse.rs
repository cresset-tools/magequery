//! Focused parsers for step 1: the `modules` map out of `app/etc/config.php`, and the
//! `name` + `<sequence>` out of a module's `etc/module.xml`.
//!
//! The `config.php` reader here only extracts the `modules` block — a full PHP
//! array-literal parser (for `env.php`/`config.php` `system`/`scopes`) is phase 2.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::ids::{ClassName, EventName, ModuleName};

/// Extract the `modules` map from `config.php` source, preserving declaration order
/// (which is Magento's authoritative, already-sequence-resolved load order).
pub(crate) fn config_php_modules(src: &str) -> Result<Vec<(ModuleName, bool)>, String> {
    let key = src
        .find("'modules'")
        .ok_or("no 'modules' key found in config.php")?;
    let open = src[key..]
        .find('[')
        .map(|i| key + i)
        .ok_or("malformed 'modules' entry (no opening `[`)")?;

    // The modules block is flat (values are 0/1, keys are simple quoted identifiers),
    // so a bracket-depth scan from the opening `[` reaches the matching `]` safely.
    let bytes = src.as_bytes();
    let mut depth = 0i32;
    let mut close = None;
    for (offset, &b) in bytes[open..].iter().enumerate() {
        match b {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + offset);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close.ok_or("unterminated 'modules' array")?;
    let block = &src[open + 1..close];

    let mut out = Vec::new();
    for entry in block.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (k, v) = entry
            .split_once("=>")
            .ok_or_else(|| format!("malformed module entry: `{entry}`"))?;
        let name = k.trim().trim_matches('\'').trim();
        if name.is_empty() {
            continue;
        }
        let enabled = v.trim() == "1";
        out.push((ModuleName::new(name), enabled));
    }
    Ok(out)
}

pub(crate) struct ParsedModuleXml {
    pub name: ModuleName,
    pub sequence: Vec<ModuleName>,
}

/// Parse `<module name="...">` and its `<sequence><module name="..."/></sequence>` children.
pub(crate) fn module_xml(xml: &str) -> Result<ParsedModuleXml, String> {
    let mut reader = Reader::from_str(xml);
    let mut name: Option<ModuleName> = None;
    let mut sequence = Vec::new();
    let mut in_sequence = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => return Err(e.to_string()),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"sequence" => in_sequence = true,
                b"module" => {
                    if in_sequence {
                        if let Some(n) = attr(&e, b"name") {
                            sequence.push(ModuleName::new(n));
                        }
                    } else if name.is_none() {
                        name = attr(&e, b"name").map(ModuleName::new);
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"sequence" {
                    in_sequence = false;
                }
            }
            _ => {}
        }
        buf.clear();
    }

    let name = name.ok_or("no <module name=\"...\"> element")?;
    Ok(ParsedModuleXml { name, sequence })
}

fn attr(e: &BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

/// Maps a byte offset in a source file to a 1-based line number, for provenance.
pub(crate) struct LineMap {
    /// Byte offset of the start of each line.
    starts: Vec<usize>,
}

impl LineMap {
    pub fn new(s: &str) -> Self {
        let mut starts = vec![0usize];
        for (i, b) in s.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        Self { starts }
    }

    pub fn line(&self, pos: usize) -> u32 {
        match self.starts.binary_search(&pos) {
            Ok(i) => (i + 1) as u32,
            Err(i) => i as u32,
        }
    }
}

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

// --- events.xml ---

pub(crate) struct RawObserver {
    pub name: String,
    pub instance: ClassName,
    pub disabled: Option<bool>,
    pub shared: Option<bool>,
    pub line: u32,
}

/// Parse `events.xml`: `<event name=…><observer name= instance= …/></event>`.
pub(crate) fn events_xml(xml: &str) -> Vec<(EventName, RawObserver)> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut current: Option<EventName> = None;
    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"event" => current = attr(&e, b"name").map(EventName::new),
                b"observer" => {
                    if let (Some(ev), Some(name), Some(inst)) =
                        (&current, attr(&e, b"name"), attr(&e, b"instance"))
                    {
                        out.push((
                            ev.clone(),
                            RawObserver {
                                name,
                                instance: ClassName::new(inst),
                                disabled: attr(&e, b"disabled").map(|s| matches!(s.trim(), "true" | "1")),
                                shared: attr(&e, b"shared").map(|s| matches!(s.trim(), "true" | "1")),
                                line,
                            },
                        ));
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) if e.name().as_ref() == b"event" => current = None,
            _ => {}
        }
        buf.clear();
    }
    out
}

// --- crontab.xml ---

pub(crate) struct RawJob {
    pub group: String,
    pub name: String,
    pub instance: ClassName,
    pub method: String,
    pub schedule: Option<String>,
    pub config_path: Option<String>,
    pub line: u32,
}

/// Parse `crontab.xml`: `<group id=…><job name= instance= method=><schedule>…</schedule></job></group>`.
pub(crate) fn crontab_xml(xml: &str) -> Vec<RawJob> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawJob> = Vec::new();
    let mut group = String::new();
    // Index in `out` of the job currently being filled (for nested <schedule>/<config_path>).
    let mut cur_job: Option<usize> = None;
    let mut text_into: Option<&'static str> = None; // "schedule" | "config_path"
    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"group" => group = attr(&e, b"id").unwrap_or_default(),
                b"job" => {
                    out.push(RawJob {
                        group: group.clone(),
                        name: attr(&e, b"name").unwrap_or_default(),
                        instance: ClassName::new(attr(&e, b"instance").unwrap_or_default()),
                        method: attr(&e, b"method").unwrap_or_default(),
                        schedule: None,
                        config_path: None,
                        line,
                    });
                    cur_job = Some(out.len() - 1);
                }
                b"schedule" => text_into = Some("schedule"),
                b"config_path" => text_into = Some("config_path"),
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if let (Some(i), Some(field)) = (cur_job, text_into) {
                    let t = e.unescape().unwrap_or_default().trim().to_string();
                    if !t.is_empty() {
                        match field {
                            "schedule" => out[i].schedule = Some(t),
                            _ => out[i].config_path = Some(t),
                        }
                    }
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"job" => cur_job = None,
                b"schedule" | b"config_path" => text_into = None,
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }
    out
}

// --- routes.xml ---

pub(crate) struct RawRoute {
    pub router: String,
    pub id: String,
    pub front_name: String,
    pub modules: Vec<String>,
    pub line: u32,
}

/// Parse `routes.xml`: `<router id=…><route id= frontName=><module name=…/></route></router>`.
pub(crate) fn routes_xml(xml: &str) -> Vec<RawRoute> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawRoute> = Vec::new();
    let mut router = String::new();
    let mut cur: Option<usize> = None;
    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"router" => router = attr(&e, b"id").unwrap_or_default(),
                b"route" => {
                    out.push(RawRoute {
                        router: router.clone(),
                        id: attr(&e, b"id").unwrap_or_default(),
                        front_name: attr(&e, b"frontName").unwrap_or_default(),
                        modules: Vec::new(),
                        line,
                    });
                    cur = Some(out.len() - 1);
                }
                b"module" => {
                    if let (Some(i), Some(name)) = (cur, attr(&e, b"name")) {
                        out[i].modules.push(name);
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) if e.name().as_ref() == b"route" => cur = None,
            _ => {}
        }
        buf.clear();
    }
    out
}

// --- webapi.xml ---

pub(crate) struct RawWebapi {
    pub method: String,
    pub url: String,
    pub service_class: ClassName,
    pub service_method: String,
    pub resources: Vec<String>,
    pub line: u32,
}

/// Parse `webapi.xml`: `<route url= method=><service class= method=/><resources><resource ref=/></resources></route>`.
pub(crate) fn webapi_xml(xml: &str) -> Vec<RawWebapi> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawWebapi> = Vec::new();
    let mut cur: Option<usize> = None;
    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"route" => {
                    out.push(RawWebapi {
                        method: attr(&e, b"method").unwrap_or_default(),
                        url: attr(&e, b"url").unwrap_or_default(),
                        service_class: ClassName::new(String::new()),
                        service_method: String::new(),
                        resources: Vec::new(),
                        line,
                    });
                    cur = Some(out.len() - 1);
                }
                b"service" => {
                    if let Some(i) = cur {
                        if let Some(c) = attr(&e, b"class") {
                            out[i].service_class = ClassName::new(c);
                        }
                        if let Some(m) = attr(&e, b"method") {
                            out[i].service_method = m;
                        }
                    }
                }
                b"resource" => {
                    if let (Some(i), Some(r)) = (cur, attr(&e, b"ref")) {
                        out[i].resources.push(r);
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) if e.name().as_ref() == b"route" => cur = None,
            _ => {}
        }
        buf.clear();
    }
    out
}

// --- config.xml (<default>/<websites>/<stores> system-config defaults) ---

struct CfgFrame {
    name: String,
    text: String,
    had_children: bool,
    line: u32,
}

/// Flatten a `config.xml` into `(scope, path, value, line)` leaves. The first element under
/// `<config>` is the scope: `default` (→ scope `default`) or `websites`/`stores` (→
/// `<type>/<code>`), with the remaining nesting forming the `a/b/c` config path.
pub(crate) fn config_xml_defaults(xml: &str) -> Vec<(String, String, String, u32)> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut stack: Vec<CfgFrame> = Vec::new();

    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                if let Some(top) = stack.last_mut() {
                    top.had_children = true;
                }
                stack.push(CfgFrame {
                    name: local_name(&e),
                    text: String::new(),
                    had_children: false,
                    line,
                });
            }
            Ok(Event::Empty(e)) => {
                if let Some(top) = stack.last_mut() {
                    top.had_children = true;
                }
                emit_leaf(&stack, &local_name(&e), "", line, &mut out);
            }
            Ok(Event::Text(e)) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&e.unescape().unwrap_or_default());
                }
            }
            Ok(Event::End(_)) => {
                if let Some(frame) = stack.pop() {
                    if !frame.had_children {
                        let text = frame.text.trim();
                        if !text.is_empty() {
                            emit_leaf(&stack, &frame.name, text, frame.line, &mut out);
                        }
                    }
                }
            }
            _ => {}
        }
        buf.clear();
    }
    out
}

fn emit_leaf(
    ancestors: &[CfgFrame],
    leaf: &str,
    value: &str,
    line: u32,
    out: &mut Vec<(String, String, String, u32)>,
) {
    // ancestors includes the <config> root at [0]; drop it.
    let mut names: Vec<&str> = ancestors.iter().map(|f| f.name.as_str()).collect();
    names.push(leaf);
    if names.len() < 2 {
        return;
    }
    let names = &names[1..]; // skip <config>

    let (scope, rest): (String, &[&str]) = match names[0] {
        "default" => ("default".to_string(), &names[1..]),
        "websites" | "stores" if names.len() >= 2 => {
            (format!("{}/{}", names[0], names[1]), &names[2..])
        }
        _ => return,
    };
    if rest.is_empty() {
        return;
    }
    out.push((scope, rest.join("/"), value.to_string(), line));
}

fn local_name(e: &BytesStart) -> String {
    let raw = e.name();
    let name = raw.as_ref();
    let name = name.rsplit(|&b| b == b':').next().unwrap_or(name);
    String::from_utf8_lossy(name).into_owned()
}

/// `true` only for an explicit `="true"`. Used for the boolean schema attributes
/// (`nullable`, `unsigned`, `identity`, `disabled`) whose absence means `false`.
fn attr_true(e: &BytesStart, key: &[u8]) -> bool {
    attr(e, key).as_deref() == Some("true")
}

// ---------- declarative schema (db_schema.xml) ----------

pub(crate) struct RawColumn {
    pub name: String,
    pub col_type: String,
    pub nullable: bool,
    pub unsigned: bool,
    pub length: Option<String>,
    pub precision: Option<String>,
    pub scale: Option<String>,
    pub default: Option<String>,
    pub identity: bool,
    pub comment: Option<String>,
    pub disabled: bool,
    pub line: u32,
}

pub(crate) struct RawConstraint {
    pub id: String,
    pub kind: String,
    pub columns: Vec<String>,
    pub reference_table: Option<String>,
    pub reference_column: Option<String>,
    pub on_delete: Option<String>,
    pub disabled: bool,
    pub line: u32,
}

pub(crate) struct RawIndex {
    pub id: String,
    pub index_type: String,
    pub columns: Vec<String>,
    pub disabled: bool,
    pub line: u32,
}

pub(crate) struct RawTable {
    pub name: String,
    pub engine: Option<String>,
    pub resource: Option<String>,
    pub comment: Option<String>,
    pub disabled: bool,
    pub columns: Vec<RawColumn>,
    pub constraints: Vec<RawConstraint>,
    pub indexes: Vec<RawIndex>,
    pub line: u32,
}

/// Parse a module's `db_schema.xml` into raw tables (each carrying line numbers; module/area
/// provenance is attached at merge). A `<column>` directly under `<table>` is a definition
/// (carries an `xsi:type`); a `<column>` inside a `<constraint>`/`<index>` is a column
/// *reference* (only `name`), so we route it by the current context.
pub(crate) fn db_schema_xml(xml: &str) -> Vec<RawTable> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut tables: Vec<RawTable> = Vec::new();
    // Index into the current table's `constraints`/`indexes` when inside one (for child columns).
    let mut in_constraint: Option<usize> = None;
    let mut in_index: Option<usize> = None;

    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                schema_element(&e, line, false, &mut tables, &mut in_constraint, &mut in_index)
            }
            Ok(Event::Empty(e)) => {
                schema_element(&e, line, true, &mut tables, &mut in_constraint, &mut in_index)
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                // A self-closing (`Empty`) constraint/index never opened a context, so only
                // the matching `End` of a real `Start` clears it.
                b"constraint" => in_constraint = None,
                b"index" => in_index = None,
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }
    tables
}

/// Handle one `Start`/`Empty` schema element. `is_empty` marks a self-closing element, which
/// must not open a child context (it has no matching `End`).
fn schema_element(
    e: &BytesStart,
    line: u32,
    is_empty: bool,
    tables: &mut Vec<RawTable>,
    in_constraint: &mut Option<usize>,
    in_index: &mut Option<usize>,
) {
    match local_name(e).as_str() {
        "table" => tables.push(RawTable {
            name: attr(e, b"name").unwrap_or_default(),
            engine: attr(e, b"engine"),
            resource: attr(e, b"resource"),
            comment: attr(e, b"comment"),
            disabled: attr_true(e, b"disabled"),
            columns: Vec::new(),
            constraints: Vec::new(),
            indexes: Vec::new(),
            line,
        }),
        "column" => {
            let Some(t) = tables.last_mut() else { return };
            if let Some(ci) = *in_index {
                if let Some(name) = attr(e, b"name") {
                    t.indexes[ci].columns.push(name);
                }
            } else if let Some(ci) = *in_constraint {
                if let Some(name) = attr(e, b"name") {
                    t.constraints[ci].columns.push(name);
                }
            } else {
                t.columns.push(RawColumn {
                    name: attr(e, b"name").unwrap_or_default(),
                    col_type: xsi_type(e).unwrap_or_default(),
                    nullable: attr_true(e, b"nullable"),
                    unsigned: attr_true(e, b"unsigned"),
                    length: attr(e, b"length"),
                    precision: attr(e, b"precision"),
                    scale: attr(e, b"scale"),
                    default: attr(e, b"default"),
                    identity: attr_true(e, b"identity"),
                    comment: attr(e, b"comment"),
                    disabled: attr_true(e, b"disabled"),
                    line,
                });
            }
        }
        "constraint" => {
            let Some(t) = tables.last_mut() else { return };
            // Foreign keys reference a single local column via the `column` attr.
            let columns = attr(e, b"column").into_iter().collect();
            t.constraints.push(RawConstraint {
                id: attr(e, b"referenceId").unwrap_or_default(),
                kind: xsi_type(e).unwrap_or_default(),
                columns,
                reference_table: attr(e, b"referenceTable"),
                reference_column: attr(e, b"referenceColumn"),
                on_delete: attr(e, b"onDelete"),
                disabled: attr_true(e, b"disabled"),
                line,
            });
            if !is_empty {
                *in_constraint = Some(t.constraints.len() - 1);
            }
        }
        "index" => {
            let Some(t) = tables.last_mut() else { return };
            t.indexes.push(RawIndex {
                id: attr(e, b"referenceId").unwrap_or_default(),
                index_type: attr(e, b"indexType").unwrap_or_else(|| "btree".into()),
                columns: Vec::new(),
                disabled: attr_true(e, b"disabled"),
                line,
            });
            if !is_empty {
                *in_index = Some(t.indexes.len() - 1);
            }
        }
        _ => {}
    }
}

/// The `xsi:type` attribute (the schema element discriminator), namespace-prefix agnostic.
fn xsi_type(e: &BytesStart) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        let key = a.key.as_ref();
        (key == b"xsi:type" || key.ends_with(b":type")).then(|| String::from_utf8_lossy(&a.value).into_owned())
    })
}

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

/// Parse one layout XML: blocks/containers declared, references modified or removed,
/// `<update handle=>` includes, and `<move>`s — each with its enclosing element.
pub(crate) fn layout_xml(xml: &str) -> Vec<RawLayoutOp> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawLayoutOp> = Vec::new();
    // Names of the enclosing block/container/reference elements.
    let mut stack: Vec<String> = Vec::new();

    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
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
                if let Some(op) = layout_op(&e, line, &stack) {
                    out.push(op);
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let raw = name.as_ref();
                let local = raw.rsplit(|&b| b == b':').next().unwrap_or(raw);
                if matches!(local, b"block" | b"container" | b"referenceBlock" | b"referenceContainer")
                {
                    stack.pop();
                }
            }
            _ => {}
        }
        buf.clear();
    }
    out
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
        let ops = layout_xml(xml);
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

#[cfg(test)]
mod schema_tests {
    use super::db_schema_xml;

    const XML: &str = r#"<?xml version="1.0"?>
<schema xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
    <table name="store_group" resource="default" engine="innodb" comment="Store Groups">
        <column xsi:type="smallint" name="group_id" unsigned="true" nullable="false" identity="true"/>
        <column xsi:type="varchar" name="code" nullable="true" length="32"/>
        <column xsi:type="decimal" name="rate" scale="4" precision="12" nullable="true"/>
        <column xsi:type="int" name="legacy" disabled="true"/>
        <constraint xsi:type="primary" referenceId="PRIMARY">
            <column name="group_id"/>
        </constraint>
        <constraint xsi:type="foreign" referenceId="FK_GROUP_WEBSITE" table="store_group"
                    column="website_id" referenceTable="store_website" referenceColumn="website_id"
                    onDelete="CASCADE"/>
        <index referenceId="IDX_CODE" indexType="btree">
            <column name="code"/>
        </index>
    </table>
</schema>"#;

    #[test]
    fn parses_columns_constraints_indexes() {
        let tables = db_schema_xml(XML);
        assert_eq!(tables.len(), 1);
        let t = &tables[0];
        assert_eq!(t.name, "store_group");
        assert_eq!(t.engine.as_deref(), Some("innodb"));

        // Column definitions (the `disabled` one is still parsed; it's dropped at merge time).
        let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["group_id", "code", "rate", "legacy"]);
        let group_id = &t.columns[0];
        assert_eq!(group_id.col_type, "smallint");
        assert!(group_id.unsigned && group_id.identity && !group_id.nullable);
        assert_eq!(t.columns[1].length.as_deref(), Some("32"));
        assert_eq!((t.columns[2].precision.as_deref(), t.columns[2].scale.as_deref()), (Some("12"), Some("4")));
        assert!(t.columns[3].disabled);

        // A `<column>` inside a constraint/index is a *reference*, not a new column definition.
        assert_eq!(t.columns.len(), 4);

        // Primary (child column) vs foreign (self-closing, attrs only).
        let primary = t.constraints.iter().find(|c| c.id == "PRIMARY").unwrap();
        assert_eq!(primary.kind, "primary");
        assert_eq!(primary.columns, ["group_id"]);
        let fk = t.constraints.iter().find(|c| c.id == "FK_GROUP_WEBSITE").unwrap();
        assert_eq!(fk.kind, "foreign");
        assert_eq!(fk.columns, ["website_id"]); // from the `column` attr
        assert_eq!(fk.reference_table.as_deref(), Some("store_website"));
        assert_eq!(fk.on_delete.as_deref(), Some("CASCADE"));

        // The index's child column must NOT have leaked into the table columns.
        let idx = &t.indexes[0];
        assert_eq!(idx.id, "IDX_CODE");
        assert_eq!(idx.columns, ["code"]);
    }

    #[test]
    fn self_closing_foreign_does_not_capture_following_columns() {
        // A foreign constraint is `Empty` (no End); the next table's columns must stay separate.
        let xml = r#"<schema xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
            <table name="a">
                <constraint xsi:type="foreign" referenceId="FK" column="x" referenceTable="b" referenceColumn="y"/>
                <column xsi:type="int" name="after_fk"/>
            </table>
        </schema>"#;
        let tables = db_schema_xml(xml);
        let t = &tables[0];
        // `after_fk` is a real column def, not swallowed as the FK's reference column.
        assert_eq!(t.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["after_fk"]);
        assert_eq!(t.constraints[0].columns, ["x"]);
    }
}
