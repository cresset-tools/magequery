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
    pub arguments: Vec<(ClassName, String, RawArg, u32)>,
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

/// A parsed argument value, carrying per-array-item line numbers but no full `Source` yet
/// (module/file/area are added at merge time in `di.rs`, where the file context is known).
pub(crate) enum RawArg {
    Object(ClassName),
    Scalar { xsi_type: String, text: String },
    /// `(key, value, line)` per item.
    Array(Vec<(String, RawArg, u32)>),
    Null,
}

/// A frame on the argument-parse stack: the top-level `<arguments>` list (key `None`) or an
/// `xsi:type="array"` (its key + the line it opened on).
struct ArgFrame {
    key: Option<(String, u32)>,
    items: Vec<(String, RawArg, u32)>,
}

/// Parse an `<arguments>` subtree (reader positioned just after the opening tag), returning
/// the top-level `(name, value, line)` triples. Reads until the matching `</arguments>`.
fn parse_arguments(
    reader: &mut Reader<&[u8]>,
    lines: &LineMap,
    buf: &mut Vec<u8>,
) -> Vec<(String, RawArg, u32)> {
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
                let value = if xsi == "array" { RawArg::Array(Vec::new()) } else { scalar(&xsi, "") };
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
                        push_item(&mut stack, key, RawArg::Array(frame.items), kline);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    stack.pop().map(|f| f.items).unwrap_or_default()
}

fn push_item(stack: &mut [ArgFrame], key: String, value: RawArg, line: u32) {
    if let Some(frame) = stack.last_mut() {
        frame.items.push((key, value, line));
    }
}

/// Build a non-array value from an xsi:type and text.
fn scalar(xsi: &str, text: &str) -> RawArg {
    match xsi {
        "object" => RawArg::Object(ClassName::new(text)),
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
