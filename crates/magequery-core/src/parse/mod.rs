//! Focused parsers for step 1: the `modules` map out of `app/etc/config.php`, and the
//! `name` + `<sequence>` out of a module's `etc/module.xml`.
//!
//! The `config.php` reader here only extracts the `modules` block — a full PHP
//! array-literal parser (for `env.php`/`config.php` `system`/`scopes`) is phase 2.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::ids::{ClassName, EventName, ModuleName};


mod xml;
pub(crate) use xml::*;
mod module;
pub(crate) use module::*;
mod di;
pub(crate) use di::*;
mod entrypoints;
pub(crate) use entrypoints::*;
mod config;
pub(crate) use config::*;
mod schema;
pub(crate) use schema::*;
mod admin;
pub(crate) use admin::*;
mod indexer;
pub(crate) use indexer::*;
mod translations;
pub(crate) use translations::*;
mod frontend;
pub(crate) use frontend::*;
mod extensions;
pub(crate) use extensions::*;
mod queue;
pub(crate) use queue::*;
