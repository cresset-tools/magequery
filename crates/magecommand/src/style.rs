//! Terminal colors for magecommand's human-readable reports, styled by semantic
//! role with magequery's palette (its `style` module): a given kind of entity is
//! the same color in both tools. `--json` output never goes through here, and
//! neither does output a script or the PHP LESS adapter reads (`static less
//! --file`, `cssdiff`'s summary, `di digest`).

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

use anstyle::{AnsiColor, Style};
use clap::ValueEnum;

static ENABLED: AtomicBool = AtomicBool::new(false);
/// The same decision for stderr, where diagnostics go: it is a terminal on its own.
static ENABLED_ERR: AtomicBool = AtomicBool::new(false);

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
    let auto = |terminal: bool| {
        !set("NO_COLOR")
            && (terminal
                || set("CLICOLOR_FORCE")
                || set("FORCE_COLOR")
                || set("GITLAB_CI")
                || set("GITHUB_ACTIONS"))
    };
    let (out, err) = match choice {
        ColorChoice::Always => (true, true),
        ColorChoice::Never => (false, false),
        ColorChoice::Auto => {
            (auto(std::io::stdout().is_terminal()), auto(std::io::stderr().is_terminal()))
        }
    };
    ENABLED.store(out, Ordering::Relaxed);
    ENABLED_ERR.store(err, Ordering::Relaxed);
}

/// The `--color` choice from the raw arguments. Help is printed while clap parses,
/// before [`init`] can run, so the choice has to be read ahead of it.
pub fn choice_from_args(args: &[std::ffi::OsString]) -> ColorChoice {
    let mut it = args.iter().filter_map(|a| a.to_str());
    while let Some(arg) = it.next() {
        let value = match arg {
            "--color" => it.next(),
            a => a.strip_prefix("--color="),
        };
        if let Some(v) = value {
            return ColorChoice::from_str(v, true).unwrap_or_default();
        }
    }
    ColorChoice::default()
}

/// clap's color setting for help and usage errors, following `--color`.
pub fn clap_color(choice: ColorChoice) -> clap::ColorChoice {
    match choice {
        ColorChoice::Auto => clap::ColorChoice::Auto,
        ColorChoice::Always => clap::ColorChoice::Always,
        ColorChoice::Never => clap::ColorChoice::Never,
    }
}

const fn fg(c: AnsiColor) -> Style {
    Style::new().fg_color(Some(anstyle::Color::Ansi(c)))
}

/// The help screen in magequery's palette: bold headings, commands and flags green,
/// value placeholders de-emphasized.
pub const HELP: clap::builder::Styles = clap::builder::Styles::styled()
    .header(Style::new().bold())
    .usage(Style::new().bold())
    .literal(fg(AnsiColor::Green))
    .placeholder(fg(AnsiColor::BrightBlack))
    .valid(fg(AnsiColor::Green))
    .invalid(fg(AnsiColor::Yellow))
    .error(fg(AnsiColor::Red).bold());

fn paint_if(on: &AtomicBool, style: Style, s: &str) -> String {
    if on.load(Ordering::Relaxed) {
        format!("{}{s}{}", style.render(), style.render_reset())
    } else {
        s.to_string()
    }
}

fn paint(style: Style, s: &str) -> String {
    paint_if(&ENABLED, style, s)
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
/// An area (`frontend`, `adminhtml`, `global`).
pub fn area(s: &str) -> String {
    paint(fg(AnsiColor::Yellow), s)
}
/// A named thing a report line is about: a theme, a LESS entry, an artifact.
pub fn name(s: &str) -> String {
    paint(fg(AnsiColor::Green), s)
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

/// Diagnostics on stderr, colored by whether stderr is a terminal.
pub mod stderr {
    use super::{fg, paint_if, ENABLED_ERR};
    use anstyle::{AnsiColor, Style};

    /// The `error:` label.
    pub fn error(s: &str) -> String {
        paint_if(&ENABLED_ERR, fg(AnsiColor::Red).bold(), s)
    }
    /// The `warning:` label.
    pub fn warning(s: &str) -> String {
        paint_if(&ENABLED_ERR, fg(AnsiColor::Yellow).bold(), s)
    }
    /// The `note:` label.
    pub fn note(s: &str) -> String {
        paint_if(&ENABLED_ERR, fg(AnsiColor::Blue).bold(), s)
    }
    /// A progress tag (`watch:`) or other named thing.
    pub fn name(s: &str) -> String {
        paint_if(&ENABLED_ERR, fg(AnsiColor::Green), s)
    }
    /// Secondary detail.
    pub fn dim(s: &str) -> String {
        paint_if(&ENABLED_ERR, fg(AnsiColor::BrightBlack), s)
    }
    /// Plain emphasis.
    pub fn bold(s: &str) -> String {
        paint_if(&ENABLED_ERR, Style::new().bold(), s)
    }
}
