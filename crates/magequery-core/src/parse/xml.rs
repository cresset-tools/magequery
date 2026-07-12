//! Shared XML parsing primitives.

use super::*;

pub(crate) fn attr(e: &BytesStart, key: &[u8]) -> Option<String> {
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

pub(crate) fn local_name(e: &BytesStart) -> String {
    let raw = e.name();
    let name = raw.as_ref();
    let name = name.rsplit(|&b| b == b':').next().unwrap_or(name);
    String::from_utf8_lossy(name).into_owned()
}

/// `true` only for an explicit `="true"`. Used for the boolean schema attributes
/// (`nullable`, `unsigned`, `identity`, `disabled`) whose absence means `false`.
pub(crate) fn attr_true(e: &BytesStart, key: &[u8]) -> bool {
    attr(e, key).as_deref() == Some("true")
}

/// The `xsi:type` attribute (the schema element discriminator), namespace-prefix agnostic.
pub(crate) fn xsi_type(e: &BytesStart) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        let key = a.key.as_ref();
        (key == b"xsi:type" || key.ends_with(b":type")).then(|| String::from_utf8_lossy(&a.value).into_owned())
    })
}
