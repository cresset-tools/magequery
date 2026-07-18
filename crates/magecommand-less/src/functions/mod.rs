//! Built-in function registry (plan §2.7). The less.php `Functions.php` set is
//! the Magento oracle; Luma-called ~20 are staged first, then the tail.
//!
//! The default `Call.eval` fallthrough — unknown CSS functions re-emit
//! `name(evaluated-args)` verbatim (plan §2.7/§4.8) — lives in `passthrough`.
//! Stub registry until Phase 3.

pub mod boolean;
pub mod color_blend;
pub mod color_channel;
pub mod color_def;
pub mod color_op;
pub mod data_uri;
pub mod default_fn;
pub mod list;
pub mod math;
pub mod misc;
pub mod number;
pub mod passthrough;
pub mod string;
pub mod svg;
pub mod types;
