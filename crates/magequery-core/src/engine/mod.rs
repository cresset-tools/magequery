//! The shared analysis engine.
//!
//! These modules own the installation-wide state and primitives that every query domain
//! builds on: the virtual filesystem overlay, module/composer index, class resolver, and
//! merged DI configuration. Query-specific indexes deliberately live outside this module.

pub(crate) mod di;
pub(crate) mod index;
pub(crate) mod resolver;
pub(crate) mod vfs;
