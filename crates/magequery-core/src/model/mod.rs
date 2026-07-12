//! The data magequery returns. Every type is owned (cloned out of the index, so callers
//! never thread the `Magento` handle's lifetime through their code) and, with the default
//! `serde` feature, serializes straight to `--json`.

use crate::ids::{Area, ClassName, EventName, ModuleName};
use crate::source::Source;

mod project;
pub use project::*;
mod wiring;
pub use wiring::*;
mod runtime;
pub use runtime::*;
mod static_config;
pub use static_config::*;
mod commerce;
pub use commerce::*;
mod sales;
pub use sales::*;
mod catalog;
pub use catalog::*;
mod admin;
pub use admin::*;
mod eav;
pub use eav::*;
mod schema;
pub use schema::*;
mod project_config;
pub use project_config::*;
