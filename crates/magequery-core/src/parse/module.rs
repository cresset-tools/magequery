//! Parsers for the module domain.

use super::*;

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
