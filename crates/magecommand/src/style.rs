//! Terminal colors for magecommand's human-readable reports, styled by semantic
//! role (the magequery `style` module's pattern). `--json` output never goes
//! through here.

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

/// Decide once, at startup, whether to emit color.
///
/// `auto` colors a TTY, and also CI job logs: magecommand's main consumer is a
/// pipeline (`di verify --fail-on-diff`), whose stdout is a pipe, but GitLab and
/// GitHub render ANSI in their logs. `NO_COLOR` always wins under `auto`;
/// `CLICOLOR_FORCE`/`FORCE_COLOR` force it on for any other log viewer.
pub fn init(choice: ColorChoice) {
    let set = |var: &str| std::env::var_os(var).is_some_and(|v| !v.is_empty());
    let on = match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            !set("NO_COLOR")
                && (std::io::stdout().is_terminal()
                    || set("CLICOLOR_FORCE")
                    || set("FORCE_COLOR")
                    || set("GITLAB_CI")
                    || set("GITHUB_ACTIONS"))
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

/// A failing verdict or section heading — the thing to act on.
pub fn fail(s: &str) -> String {
    paint(fg(AnsiColor::Red).bold(), s)
}
/// A passing verdict or the explained-section heading.
pub fn pass(s: &str) -> String {
    paint(fg(AnsiColor::Green).bold(), s)
}
/// A sub-heading (which tree a difference lives in).
pub fn heading(s: &str) -> String {
    paint(Style::new().bold(), s)
}
/// A class name.
pub fn class(s: &str) -> String {
    paint(fg(AnsiColor::Cyan), s)
}
/// A file path, URL or other secondary detail.
pub fn dim(s: &str) -> String {
    paint(fg(AnsiColor::BrightBlack), s)
}
/// Diff-convention labels: a file or line only the archive (reference) has.
pub fn removed(s: &str) -> String {
    paint(fg(AnsiColor::Red), s)
}
/// A file or line only the magecommand output has.
pub fn added(s: &str) -> String {
    paint(fg(AnsiColor::Green), s)
}
/// A file both sides have, with different content.
pub fn changed(s: &str) -> String {
    paint(fg(AnsiColor::Yellow), s)
}
/// A link to an explanation.
pub fn link(s: &str) -> String {
    paint(fg(AnsiColor::Blue).underline(), s)
}
