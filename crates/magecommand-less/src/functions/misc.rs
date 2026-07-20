//! Misc/resource fns: `image-size`/`image-width`/`image-height` (plan §2.7,
//! §C-assets) — real file reads through the resolver's binary hook, with
//! PNG/GIF/JPEG/SVG header sniffing (enough for the fixture corpus; the node
//! implementation delegates to the `image-size` package).

use super::string::string_value;
use crate::ast::Node;
use crate::resolver::ImportResolver;
use crate::value::Dimension;

/// Which projection of the size the function returns.
pub(crate) enum SizeAxis {
    Both,
    Width,
    Height,
}

pub(crate) fn image_size(
    args: &[Node],
    axis: SizeAxis,
    resolver: &dyn ImportResolver,
    current_dir: &str,
) -> Option<Node> {
    let path = string_value(args.first()?)?;
    let bytes = resolver.load_binary(&path, current_dir)?;
    let (w, h) = sniff(&bytes)?;
    let dim = |v: f64| Node::Dimension(Dimension::with_unit(v, "px"));
    Some(match axis {
        SizeAxis::Width => dim(w),
        SizeAxis::Height => dim(h),
        SizeAxis::Both => Node::Expression(vec![dim(w), dim(h)]),
    })
}

/// `(width, height)` from the image header.
fn sniff(b: &[u8]) -> Option<(f64, f64)> {
    // PNG: 8-byte signature, IHDR at 16..24.
    if b.len() >= 24 && b.starts_with(&[0x89, b'P', b'N', b'G']) {
        let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
        let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
        return Some((w as f64, h as f64));
    }
    // GIF: little-endian u16 pair at 6..10.
    if b.len() >= 10 && (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        let w = u16::from_le_bytes([b[6], b[7]]);
        let h = u16::from_le_bytes([b[8], b[9]]);
        return Some((w as f64, h as f64));
    }
    // JPEG: scan markers for SOF0..SOF15 (excluding DHT/DAC/RST).
    if b.len() >= 4 && b[0] == 0xFF && b[1] == 0xD8 {
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = b[i + 1];
            if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                let h = u16::from_be_bytes([b[i + 5], b[i + 6]]);
                let w = u16::from_be_bytes([b[i + 7], b[i + 8]]);
                return Some((w as f64, h as f64));
            }
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            i += 2 + len;
        }
        return None;
    }
    // SVG: width/height attributes on the root element.
    let text = std::str::from_utf8(b).ok()?;
    if text.contains("<svg") {
        let attr = |name: &str| -> Option<f64> {
            let pos = text.find(&format!("{name}=\""))?;
            let rest = &text[pos + name.len() + 2..];
            let end = rest.find('"')?;
            rest[..end].trim_end_matches("px").parse().ok()
        };
        if let (Some(w), Some(h)) = (attr("width"), attr("height")) {
            return Some((w, h));
        }
    }
    None
}
