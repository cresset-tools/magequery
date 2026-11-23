//! Shared XML parsing primitives.

use super::*;

/// An attribute's value with XML entities decoded (`name="&lt;=&gt;"` is
/// `<=>`), as every DOM-based reader of these files sees it. A value that
/// doesn't decode (a stray `&`) is returned as written.
pub(crate) fn attr(e: &BytesStart, key: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .map(|a| attr_value(&a))
}

fn attr_value(a: &quick_xml::events::attributes::Attribute) -> String {
    quick_xml::escape::unescape(&a.value)
        .map(|v| v.into_owned())
        .unwrap_or_else(|_| a.value.to_string())
}

/// The event reader every parser here uses: quick-xml's, with the entity
/// references in character data folded back into the text around them.
///
/// quick-xml reports `a &amp; b` as three events (`Text`, `GeneralRef`,
/// `Text`). The parsers want what a DOM gives them, one text node reading
/// `a & b`, so a run of text and references comes out as a single
/// [`Event::Text`] whose content is already decoded; read it with [`text`].
///
/// A run holding a reference that does not resolve (an entity other than the
/// five predefined ones, or a bare `&`) comes out empty rather than failing
/// the file.
pub(crate) struct Reader<'a> {
    inner: quick_xml::Reader<&'a [u8]>,
    /// The event read past the end of a text run, and the offset after it.
    peeked: Option<(quick_xml::Result<Event<'a>>, u64)>,
    /// Offset just past the event last returned.
    pos: u64,
}

impl<'a> Reader<'a> {
    pub(crate) fn from_str(xml: &'a str) -> Self {
        let mut inner = quick_xml::Reader::from_str(xml);
        // A bare `&` arrives as text instead of an error; `read_event` then
        // drops the run it sits in.
        inner.config_mut().allow_dangling_amp = true;
        Self { inner, peeked: None, pos: 0 }
    }

    /// Offset just past the event last returned by [`Self::read_event`].
    pub(crate) fn buffer_position(&self) -> u64 {
        self.pos
    }

    /// Kept so call sites read like quick-xml's buffered reader; the input is
    /// a `&str`, so events borrow from it and the buffer goes unused.
    pub(crate) fn read_event_into(&mut self, _buf: &mut Vec<u8>) -> quick_xml::Result<Event<'a>> {
        self.read_event()
    }

    pub(crate) fn read_event(&mut self) -> quick_xml::Result<Event<'a>> {
        let (ev, pos) = match self.peeked.take() {
            Some(peeked) => peeked,
            None => self.read_inner(),
        };
        self.pos = pos;
        let mut run = TextRun::default();
        match ev {
            Ok(Event::Text(t)) => run.push_text(t.into_inner()),
            Ok(Event::GeneralRef(r)) => run.push_ref(&r),
            other => return other,
        }
        loop {
            let (next, pos) = self.read_inner();
            match next {
                Ok(Event::Text(t)) => run.push_text(t.into_inner()),
                Ok(Event::GeneralRef(r)) => run.push_ref(&r),
                other => {
                    self.peeked = Some((other, pos));
                    break;
                }
            }
            self.pos = pos;
        }
        Ok(Event::Text(BytesText::from_escaped(run.finish())))
    }

    fn read_inner(&mut self) -> (quick_xml::Result<Event<'a>>, u64) {
        let ev = self.inner.read_event();
        (ev, self.inner.buffer_position())
    }
}

/// The decoded content of a text event from [`Reader`].
pub(crate) fn text<'e>(e: &'e BytesText) -> &'e str {
    e
}

/// A run of adjacent text and entity-reference events, decoded.
#[derive(Default)]
struct TextRun<'a> {
    text: Option<Cow<'a, str>>,
    unresolved: bool,
}

impl<'a> TextRun<'a> {
    fn push_text(&mut self, piece: Cow<'a, str>) {
        // Only a dangling ampersand leaves a `&` in a text event.
        self.unresolved |= piece.contains('&');
        match &mut self.text {
            None => self.text = Some(piece),
            Some(text) => text.to_mut().push_str(&piece),
        }
    }

    fn push_ref(&mut self, reference: &BytesRef) {
        let text = self.text.get_or_insert_with(Cow::default).to_mut();
        match reference.resolve_char_ref() {
            Ok(Some(ch)) => text.push(ch),
            Ok(None) => match quick_xml::escape::resolve_predefined_entity(reference) {
                Some(value) => text.push_str(value),
                None => self.unresolved = true,
            },
            Err(_) => self.unresolved = true,
        }
    }

    fn finish(self) -> Cow<'a, str> {
        if self.unresolved {
            return Cow::default();
        }
        self.text.unwrap_or_default()
    }
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
    name.rsplit(':').next().unwrap_or(name).to_string()
}

/// `true` only for an explicit `="true"`. Used for the boolean schema attributes
/// (`nullable`, `unsigned`, `identity`, `disabled`) whose absence means `false`.
pub(crate) fn attr_true(e: &BytesStart, key: &str) -> bool {
    attr(e, key).as_deref() == Some("true")
}

/// The `xsi:type` attribute (the schema element discriminator), namespace-prefix agnostic.
pub(crate) fn xsi_type(e: &BytesStart) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        let key = a.key.as_ref();
        (key == "xsi:type" || key.ends_with(":type")).then(|| attr_value(&a))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every text event of `xml`, with the offset the reader reports after it.
    fn texts(xml: &str) -> Vec<(String, u64)> {
        let mut reader = Reader::from_str(xml);
        let mut out = Vec::new();
        loop {
            match reader.read_event().expect("well-formed") {
                Event::Eof => return out,
                Event::Text(t) => out.push((text(&t).to_string(), reader.buffer_position())),
                _ => {}
            }
        }
    }

    #[test]
    fn references_fold_into_the_text_around_them() {
        let xml = "<a>Tom &amp; Jerry &#60;3 &#x41;</a>";
        let end = xml.find("</a>").unwrap() as u64;
        assert_eq!(texts(xml), [("Tom & Jerry <3 A".to_string(), end)]);
    }

    #[test]
    fn a_reference_on_its_own_is_a_text_event() {
        assert_eq!(texts("<a>&lt;</a>")[0].0, "<");
    }

    #[test]
    fn the_event_after_a_text_run_is_not_lost() {
        let mut reader = Reader::from_str("<a>x &amp; y<b/></a>");
        let mut names = Vec::new();
        loop {
            match reader.read_event().expect("well-formed") {
                Event::Eof => break,
                Event::Start(e) | Event::Empty(e) => names.push(local_name(&e)),
                _ => {}
            }
        }
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn cdata_stays_its_own_event() {
        let mut reader = Reader::from_str("<a>x<![CDATA[&amp;]]>y</a>");
        let mut seen = Vec::new();
        loop {
            match reader.read_event().expect("well-formed") {
                Event::Eof => break,
                Event::Text(t) => seen.push(format!("text:{}", text(&t))),
                Event::CData(c) => seen.push(format!("cdata:{}", &*c)),
                _ => {}
            }
        }
        assert_eq!(seen, ["text:x", "cdata:&amp;", "text:y"]);
    }

    #[test]
    fn a_run_with_an_unresolvable_reference_is_empty() {
        assert_eq!(texts("<a>one &nbsp; two</a>")[0].0, "");
        assert_eq!(texts("<a>fish & chips</a>")[0].0, "");
    }

    #[test]
    fn attribute_values_decode_and_fall_back_to_the_raw_text() {
        let mut reader = Reader::from_str(r#"<a name="&lt;=&gt;" raw="a & b"/>"#);
        let Event::Empty(e) = reader.read_event().unwrap() else { panic!("expected an element") };
        assert_eq!(attr(&e, "name").as_deref(), Some("<=>"));
        assert_eq!(attr(&e, "raw").as_deref(), Some("a & b"));
        assert_eq!(attr(&e, "missing"), None);
    }
}
