//! Focused parsers for step 1: the `modules` map out of `app/etc/config.php`, and the
//! `name` + `<sequence>` out of a module's `etc/module.xml`.
//!
//! The `config.php` reader here only extracts the `modules` block — a full PHP
//! array-literal parser (for `env.php`/`config.php` `system`/`scopes`) is phase 2.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::ids::{ClassName, EventName, ModuleName};
use crate::model::ArgValue;

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
    pub class: Option<ClassName>,
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
    pub arguments: Vec<(ClassName, String, ArgValue, u32)>,
}

/// Parse one di.xml file. Tracks the enclosing `<type>`/`<virtualType>` so `<plugin>` and
/// `<arguments>` are attributed to their target.
pub(crate) fn di_xml(xml: &str) -> Result<DiFile, String> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut arg_buf = Vec::new();
    let mut out = DiFile::default();
    let mut current_type: Option<ClassName> = None;

    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(e) => return Err(e.to_string()),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                if e.name().as_ref() == b"arguments" {
                    // Consume the whole <arguments>…</arguments> subtree.
                    if let Some(target) = current_type.clone() {
                        let args = parse_arguments(&mut reader, &lines, &mut arg_buf);
                        for (name, value, line) in args {
                            out.arguments.push((target.clone(), name, value, line));
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

/// A frame on the argument-parse stack: the top-level `<arguments>` list (key `None`) or an
/// `xsi:type="array"` (its key + the line it opened on).
struct ArgFrame {
    key: Option<(String, u32)>,
    items: Vec<(String, ArgValue, u32)>,
}

/// Parse an `<arguments>` subtree (reader positioned just after the opening tag), returning
/// the top-level `(name, value, line)` triples. Reads until the matching `</arguments>`.
fn parse_arguments(
    reader: &mut Reader<&[u8]>,
    lines: &LineMap,
    buf: &mut Vec<u8>,
) -> Vec<(String, ArgValue, u32)> {
    let mut stack = vec![ArgFrame { key: None, items: Vec::new() }];
    // The scalar leaf currently being read: (key, xsi_type, line, accumulated text).
    let mut leaf: Option<(String, String, u32, String)> = None;

    loop {
        buf.clear();
        let ev = reader.read_event_into(buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) if matches!(e.name().as_ref(), b"argument" | b"item") => {
                let key = attr(&e, b"name").unwrap_or_default();
                let xsi = attr(&e, b"xsi:type").unwrap_or_default();
                if xsi == "array" {
                    stack.push(ArgFrame { key: Some((key, line)), items: Vec::new() });
                } else {
                    leaf = Some((key, xsi, line, String::new()));
                }
            }
            Ok(Event::Empty(e)) if matches!(e.name().as_ref(), b"argument" | b"item") => {
                let key = attr(&e, b"name").unwrap_or_default();
                let xsi = attr(&e, b"xsi:type").unwrap_or_default();
                let value = if xsi == "array" { ArgValue::Array(Vec::new()) } else { scalar(&xsi, "") };
                push_item(&mut stack, key, value, line);
            }
            Ok(Event::Text(e)) => {
                if let Some((_, _, _, text)) = &mut leaf {
                    text.push_str(&e.unescape().unwrap_or_default());
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"arguments" => break,
                b"argument" | b"item" => {
                    if let Some((key, xsi, line, text)) = leaf.take() {
                        push_item(&mut stack, key, scalar(&xsi, text.trim()), line);
                    } else if stack.len() > 1 {
                        // Closing an array frame.
                        let frame = stack.pop().unwrap();
                        let (key, kline) = frame.key.unwrap_or_default();
                        let arr = ArgValue::Array(
                            frame.items.into_iter().map(|(k, v, _)| (k, v)).collect(),
                        );
                        push_item(&mut stack, key, arr, kline);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    stack.pop().map(|f| f.items).unwrap_or_default()
}

fn push_item(stack: &mut [ArgFrame], key: String, value: ArgValue, line: u32) {
    if let Some(frame) = stack.last_mut() {
        frame.items.push((key, value, line));
    }
}

/// Build a non-array value from an xsi:type and text.
fn scalar(xsi: &str, text: &str) -> ArgValue {
    match xsi {
        "object" => ArgValue::Object(ClassName::new(text.trim_start_matches('\\'))),
        "null" => ArgValue::Null,
        _ if text.is_empty() && xsi.is_empty() => ArgValue::Null,
        _ => ArgValue::Scalar { xsi_type: xsi.to_string(), text: text.to_string() },
    }
}

fn di_open(
    e: &BytesStart,
    is_start: bool,
    line: u32,
    out: &mut DiFile,
    current_type: &mut Option<ClassName>,
) {
    match e.name().as_ref() {
        b"preference" => {
            if let (Some(f), Some(t)) = (attr(e, b"for"), attr(e, b"type")) {
                out.preferences.push((ClassName::new(f), ClassName::new(t), line));
            }
        }
        b"virtualType" => {
            if let Some(name) = attr(e, b"name") {
                if let Some(t) = attr(e, b"type") {
                    out.virtual_types
                        .push((ClassName::new(name.clone()), ClassName::new(t), line));
                }
                if is_start {
                    *current_type = Some(ClassName::new(name));
                }
            }
        }
        b"type" => {
            if let Some(name) = attr(e, b"name") {
                if is_start {
                    *current_type = Some(ClassName::new(name));
                }
            }
        }
        b"plugin" => {
            if let (Some(target), Some(pname)) = (current_type.clone(), attr(e, b"name")) {
                out.plugins.push((
                    target,
                    RawPlugin {
                        name: pname,
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
