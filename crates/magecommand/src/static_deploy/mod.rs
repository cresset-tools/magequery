//! `magecommand static` — reproduce `setup:static-content:deploy` artifacts
//! (the GENERATE tier of the roadmap). Library-first: each submodule computes
//! and returns; the CLI in `main.rs` is a thin renderer/writer on top.
//!
//! Built today: [`less`] — theme LESS assembly + compilation (plan §7/§10);
//! [`cssdiff`] — the §7.7 semantic CSS differ (the Gate T2 comparator);
//! [`requirejs`] — `requirejs-config.js` aggregation (the JS half);
//! [`bundle`] — `js/bundle/bundle<N>.js` generation (SCD JS bundling); and
//! [`minify`] — `.min.css`/`.min.js` production (semantic-equivalence, the
//! one deliberately non-byte-parity piece).

pub mod bundle;
pub mod cssdiff;
pub mod less;
pub mod minify;
pub mod requirejs;
