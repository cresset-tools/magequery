//! `magecommand static` — reproduce `setup:static-content:deploy` artifacts
//! (the GENERATE tier of the roadmap). Library-first: each submodule computes
//! and returns; the CLI in `main.rs` is a thin renderer/writer on top.
//!
//! Built today: [`less`] — theme LESS assembly + compilation (plan §7/§10);
//! [`cssdiff`] — the §7.7 semantic CSS differ (the Gate T2 comparator); and
//! [`requirejs`] — `requirejs-config.js` aggregation (the JS half).

pub mod cssdiff;
pub mod less;
pub mod requirejs;
