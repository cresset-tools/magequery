//! Parsers for the di domain.

use super::*;

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
