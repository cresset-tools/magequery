//! The single source of truth for terminal colors. Every renderer styles by **semantic
//! role** (class, module, area, path, …) so a given kind of entity is the same color in
//! every command. Retheme here. Core never emits escapes — this is CLI-only.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

use anstyle::{AnsiColor, Style};
use clap::ValueEnum;

static ENABLED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Default, ValueEnum)]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

/// Decide once, at startup, whether to emit color. Honors `--color`, `NO_COLOR`, and TTY.
pub fn init(choice: ColorChoice) {
    let on = match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
        }
    };
    ENABLED.store(on, Ordering::Relaxed);
}

fn fg(c: AnsiColor) -> Style {
    Style::new().fg_color(Some(c.into()))
}

fn paint(style: Style, s: &str) -> String {
    if ENABLED.load(Ordering::Relaxed) {
        format!("{}{s}{}", style.render(), style.render_reset())
    } else {
        s.to_string()
    }
}

/// A fully-qualified class/interface name.
pub fn class(s: &str) -> String {
    paint(fg(AnsiColor::Cyan), s)
}
/// A module identifier (`Vendor_Module`).
pub fn module(s: &str) -> String {
    paint(fg(AnsiColor::Magenta), s)
}
/// An area tag (`base`, `frontend`, …).
pub fn area(s: &str) -> String {
    paint(fg(AnsiColor::Yellow), s)
}
/// A file path or `file:line`.
pub fn path(s: &str) -> String {
    paint(fg(AnsiColor::BrightBlack), s)
}
/// A declaration name (plugin name, event name).
pub fn name(s: &str) -> String {
    paint(fg(AnsiColor::Green), s)
}
/// An interception kind word (`before`/`around`/`after`).
pub fn kind(s: &str) -> String {
    paint(fg(AnsiColor::Blue), s)
}
/// The target method / actual implementation — emphasized.
pub fn target(s: &str) -> String {
    paint(Style::new().bold(), s)
}
/// Positive state (`on`, enabled).
pub fn ok(s: &str) -> String {
    paint(fg(AnsiColor::Green), s)
}
/// Error / disabled state.
pub fn err(s: &str) -> String {
    paint(fg(AnsiColor::Red), s)
}
/// Secondary / de-emphasized text (labels, separators, numbers).
pub fn dim(s: &str) -> String {
    paint(fg(AnsiColor::BrightBlack), s)
}

// --- literal syntax colors (di.xml argument values) ---

/// A string literal (rendered quoted).
pub fn string_lit(s: &str) -> String {
    paint(fg(AnsiColor::Green), s)
}
/// A numeric literal.
pub fn number(s: &str) -> String {
    paint(fg(AnsiColor::Yellow), s)
}
/// A keyword literal (`true`/`false`/`null`).
pub fn keyword(s: &str) -> String {
    paint(fg(AnsiColor::Magenta), s)
}
