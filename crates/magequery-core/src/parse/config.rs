//! Parsers for the config domain.

use super::*;

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
