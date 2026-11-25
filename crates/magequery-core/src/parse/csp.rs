//! Parser for `csp_whitelist.xml` (Magento_Csp).

use super::*;

pub(crate) struct RawCspValue {
    pub id: String,
    /// `host` or `hash` (the XSD's `cspValueType`); anything else is kept verbatim.
    pub kind: String,
    pub algorithm: Option<String>,
    pub value: String,
    pub line: u32,
}

pub(crate) struct RawCspPolicy {
    pub id: String,
    pub values: Vec<RawCspValue>,
}

/// Parse a `csp_whitelist.xml`: `<policies><policy id=><values><value id= type=
/// [algorithm=]>text</value></values></policy></policies>`. A policy may carry several
/// `<values>` blocks; their values are collected in document order.
pub(crate) fn csp_whitelist_xml(xml: &str) -> Vec<RawCspPolicy> {
    let lines = LineMap::new(xml);
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out: Vec<RawCspPolicy> = Vec::new();
    let mut cur_policy: Option<usize> = None;
    // The `<value>` whose text we're inside (only a real `Start` opens one).
    let mut cur_value: Option<RawCspValue> = None;

    loop {
        let ev = reader.read_event_into(&mut buf);
        let line = lines.line(reader.buffer_position() as usize);
        match ev {
            Err(_) | Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => match local_name(&e).as_str() {
                "policy" => {
                    out.push(RawCspPolicy { id: attr(&e, b"id").unwrap_or_default(), values: Vec::new() });
                    cur_policy = Some(out.len() - 1);
                }
                "value" if cur_policy.is_some() => {
                    cur_value = Some(RawCspValue {
                        id: attr(&e, b"id").unwrap_or_default(),
                        kind: attr(&e, b"type").unwrap_or_default(),
                        algorithm: attr(&e, b"algorithm"),
                        value: String::new(),
                        line,
                    });
                }
                _ => {}
            },
            Ok(Event::Empty(e)) if local_name(&e) == "policy" => {
                // `<policy id="x"/>`: Magento's converter still registers the (empty)
                // policy, but it must not adopt later values.
                out.push(RawCspPolicy { id: attr(&e, b"id").unwrap_or_default(), values: Vec::new() });
                cur_policy = None;
            }
            Ok(Event::Text(e)) => {
                if let Some(v) = cur_value.as_mut() {
                    v.value.push_str(&e.unescape().unwrap_or_default());
                }
            }
            Ok(Event::CData(e)) => {
                if let Some(v) = cur_value.as_mut() {
                    v.value.push_str(&String::from_utf8_lossy(&e));
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"value" => {
                    if let (Some(i), Some(mut v)) = (cur_policy, cur_value.take()) {
                        v.value = v.value.trim().to_string();
                        out[i].values.push(v);
                    }
                }
                b"policy" => cur_policy = None,
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }
    out
}

#[cfg(test)]
mod csp_tests {
    use super::csp_whitelist_xml;

    #[test]
    fn policies_hosts_and_hashes() {
        let xml = r#"<?xml version="1.0"?>
<csp_whitelist xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
    <policies>
        <policy id="script-src">
            <values>
                <value id="gtag" type="host">*.googletagmanager.com</value>
                <value id="inline1" type="hash" algorithm="sha256">B2yPHKaXnvFWtRChIbabYmUBFZdVfKKXHbWtWidDVF8=</value>
            </values>
            <values>
                <value id="second-block" type="host"> https://example.com </value>
            </values>
        </policy>
        <policy id="img-src"/>
        <policy id="media-src">
            <values>
                <value id="yt" type="host">www.youtube.com</value>
            </values>
        </policy>
    </policies>
</csp_whitelist>"#;
        let p = csp_whitelist_xml(xml);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].id, "script-src");
        assert_eq!(p[0].values.len(), 3);
        assert_eq!(p[0].values[0].id, "gtag");
        assert_eq!(p[0].values[0].kind, "host");
        assert_eq!(p[0].values[0].value, "*.googletagmanager.com");
        assert_eq!(p[0].values[0].line, 6);
        assert_eq!(p[0].values[1].kind, "hash");
        assert_eq!(p[0].values[1].algorithm.as_deref(), Some("sha256"));
        assert_eq!(p[0].values[2].value, "https://example.com");
        assert_eq!(p[1].id, "img-src");
        assert!(p[1].values.is_empty());
        assert_eq!(p[2].values[0].value, "www.youtube.com");
    }
}
