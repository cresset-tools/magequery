//! Byte offset ↔ LSP position conversion for one file's text.
//!
//! LSP positions are 0-based lines + UTF-16 code units (the default encoding, the only
//! one we advertise). Config XML is overwhelmingly ASCII, but labels aren't guaranteed
//! to be, so the conversion is done right rather than assumed away.

/// Line-start offsets for one text, like core's `LineMap` but bidirectional and
/// UTF-16-aware. Built per request on the file being queried — a few µs, not worth
/// caching — the text is handed in per request.
pub(crate) struct LineIndex<'t> {
    text: &'t str,
    /// Byte offset of the start of each line.
    starts: Vec<usize>,
}

impl<'t> LineIndex<'t> {
    pub(crate) fn new(text: &'t str) -> Self {
        let mut starts = vec![0usize];
        for (offset, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                starts.push(offset + 1);
            }
        }
        Self { text, starts }
    }

    /// The byte offset of an LSP position; `None` when the position is off the end.
    pub(crate) fn offset(&self, position: lsp_types::Position) -> Option<usize> {
        let line_start = *self.starts.get(position.line as usize)?;
        let line_end = self
            .starts
            .get(position.line as usize + 1)
            .map_or(self.text.len(), |next| *next);
        let line = &self.text[line_start..line_end];

        let mut units = 0u32;
        for (offset, ch) in line.char_indices() {
            if units >= position.character {
                return Some(line_start + offset);
            }
            units += ch.len_utf16() as u32;
        }
        // Position at (or clamped past) the end of the line.
        Some(line_end)
    }

    /// The LSP position of a byte offset (which must lie on a char boundary).
    pub(crate) fn position(&self, offset: usize) -> lsp_types::Position {
        let line = match self.starts.binary_search(&offset) {
            Ok(index) => index,
            Err(index) => index - 1,
        };
        let character = self.text[self.starts[line]..offset]
            .chars()
            .map(|ch| ch.len_utf16() as u32)
            .sum();
        lsp_types::Position::new(line as u32, character)
    }

    /// The LSP range for a byte range.
    pub(crate) fn range(&self, range: std::ops::Range<usize>) -> lsp_types::Range {
        lsp_types::Range::new(self.position(range.start), self.position(range.end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Position;

    #[test]
    fn round_trips_ascii_and_multibyte_lines() {
        let text = "abc\nüber café\nlast";
        let index = LineIndex::new(text);

        // 'c' on line 0.
        assert_eq!(index.offset(Position::new(0, 2)), Some(2));
        assert_eq!(index.position(2), Position::new(0, 2));

        // 'c' of "café": ü is 2 bytes / 1 UTF-16 unit, é likewise. Byte offset of 'c' in
        // line 1 = 1 (ü=2) + 3 ("ber") + 1 (space) = 4+... compute via the index itself:
        let byte = index.offset(Position::new(1, 5)).unwrap();
        assert_eq!(&text[byte..byte + 1], "c");
        assert_eq!(index.position(byte), Position::new(1, 5));

        // End of text.
        assert_eq!(index.offset(Position::new(2, 4)), Some(text.len()));
    }

    #[test]
    fn clamps_past_end_of_line() {
        let index = LineIndex::new("ab\ncd");
        // Character beyond the line clamps to the line end (start of next line's offset).
        assert_eq!(index.offset(Position::new(0, 99)), Some(3));
        // Line beyond the text is None.
        assert_eq!(index.offset(Position::new(9, 0)), None);
    }
}
