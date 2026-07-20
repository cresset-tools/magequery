//! The `data-uri` mime table (plan §2.7, §3-G). less.js resolves mimetypes via
//! the node `mime` package; this is the web-asset subset that matters for LESS
//! sources (Luma embeds images/fonts). Charset rule mirrors
//! `mime.charsets.lookup`: `text/*` → UTF-8 (no base64), everything else binary.

/// `(extension, mime-type)`.
pub static MIME_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
    ("ico", "image/x-icon"),
    ("bmp", "image/bmp"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("ttf", "font/ttf"),
    ("otf", "font/otf"),
    ("eot", "application/vnd.ms-fontobject"),
    ("html", "text/html"),
    ("htm", "text/html"),
    ("css", "text/css"),
    ("js", "application/javascript"),
    ("json", "application/json"),
    ("txt", "text/plain"),
    ("xml", "application/xml"),
];

/// Mime type for a file path, by extension.
pub fn mime_lookup(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    MIME_TYPES
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, m)| *m)
}

/// Whether the type is ASCII/UTF-8 text (`mime.charsets.lookup` returns UTF-8
/// exactly for `text/*` plus a few JSON-ish types treated as binary by less.js).
pub fn is_text(mime: &str) -> bool {
    mime.starts_with("text/")
}
