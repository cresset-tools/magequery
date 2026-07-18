//! Frame + lazy variable/property resolution (plan §4.1).
//!
//! Variables are *values, not bindings*: a frame holds the unevaluated
//! declaration, evaluated against the live frames at the use site. Last-
//! declaration-wins is a hash-build reduction over `rules` in source order.

use rustc_hash::FxHashMap;

use crate::ast::Node;

/// One scope frame: identifier → its unevaluated declaration (plan §4.1),
/// keyed by `Box<str>` in a `rustc-hash` map. Stub shape until Phase 1.
pub type Frame = FxHashMap<Box<str>, Node>;
