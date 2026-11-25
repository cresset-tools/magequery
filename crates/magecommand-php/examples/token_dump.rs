//! Differential harness for [`magecommand_php::token`]: prints each file's
//! tokens as `kind\ttext` (newlines escaped), to diff against PHP's
//! `token_get_all` mapped onto the same kinds.
//!
//! ```sh
//! cargo run --release -p magecommand-php --example token_dump -- <file>...
//! ```

use std::io::Write;

fn main() {
    let out = std::io::stdout();
    let mut out = std::io::BufWriter::new(out.lock());
    for path in std::env::args().skip(1) {
        let src = std::fs::read(&path).expect("read");
        let start = src.windows(5).position(|w| w.eq_ignore_ascii_case(b"<?php")).map_or(0, |i| i + 5);
        writeln!(out, "FILE\t{path}").unwrap();
        for t in magecommand_php::token::tokens(&src, start) {
            writeln!(out, "{:?}\t{}", t.kind, t.text.replace('\\', "\\\\").replace('\n', "\\n")).unwrap();
        }
    }
}
