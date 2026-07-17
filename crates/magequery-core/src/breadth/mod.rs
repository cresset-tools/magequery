//! "Breadth" indexes — events/observers, cron, routes, webapi. Each is a thin projection
//! of a per-module XML file, merged in load order (per-area for events/routes; global for
//! cron/webapi). Built lazily (on first query) so they don't slow the common commands.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use rayon::prelude::*;

use std::path::Path;

use crate::ids::{Area, ClassName, EventName, ModuleName};
use crate::model::{
    AclResource, CatalogAttribute, CatalogAttributeGroup, CronJob, DbColumn, DbConstraint,
    DbIndex, DbTable, EmailTemplate,
    EmailTemplateOverride, ExtendedType,
    ExtensionAttribute, ExtensionJoin, GqlArg, GqlField, GqlKind,
    GqlType, Indexer, LayoutContribution, LayoutLayer, LayoutOp, LayoutOpKind, LayoutView,
    MenuItem, Module, Template, TemplateFile, TemplateUsage, UiComponentContribution,
    UiComponentOp, UiComponentView, Widget, WidgetParam,
    MqConsumer, MqHandler, MqPublisher, MqRoute, MqTopic, MqTopicRoute, MqVia,
    MviewSubscription, Observer, Route, SystemField, WebapiRoute,
};
use crate::engine::vfs::Vfs;
use crate::parse;
use crate::source::Source;

const REAL_AREAS: [Area; 6] = [
    Area::Frontend,
    Area::Adminhtml,
    Area::Crontab,
    Area::WebapiRest,
    Area::WebapiSoap,
    Area::Graphql,
];

fn area_path(m: &Module, area: Area, file: &str) -> PathBuf {
    match area.dir() {
        Some(dir) => m.path.join("etc").join(dir).join(file),
        None => m.path.join("etc").join(file),
    }
}

/// Read + parse `etc/[<area>/]<file>` for every **enabled** module **in parallel**
/// (Magento only loads enabled modules' configuration), returning `(module index, path,
/// parsed)` for the files that exist, in module (load) order — so the caller merges
/// sequentially and deterministically. `rayon` preserves the collect order.
fn read_parse<T: Send>(
    modules: &[Module],
    vfs: &Vfs,
    area: Area,
    file: &str,
    parse: impl Fn(&str) -> T + Sync,
) -> Vec<(usize, PathBuf, T)> {
    let jobs: Vec<(usize, PathBuf)> = modules
        .iter()
        .enumerate()
        .filter(|(_, m)| m.enabled)
        .map(|(i, m)| (i, area_path(m, area, file)))
        .collect();
    let parsed: Vec<Option<T>> = jobs
        .par_iter()
        .map(|(_, p)| vfs.read_to_string(p).ok().map(|t| parse(t.as_str())))
        .collect();
    jobs.into_iter()
        .zip(parsed)
        .filter_map(|((i, p), r)| r.map(|t| (i, p, t)))
        .collect()
}


mod events;
pub(crate) use events::{CronIndex, EventIndex};
mod routes;
pub(crate) use routes::{RouteIndex, WebapiIndex};
mod schema;
pub(crate) use schema::{CatalogAttrIndex, SchemaIndex};
mod frontend;
pub(crate) use frontend::{EmailTemplateIndex, LayoutIndex, UiComponentIndex, WidgetIndex};
mod extensions;
pub(crate) use extensions::{ExtAttrIndex, MenuIndex};
mod graphql;
pub(crate) use graphql::GqlIndex;
mod queue;
pub(crate) use queue::MqIndex;
mod indexer;
pub(crate) use indexer::IndexerIndex;
mod admin;
pub(crate) use admin::{AclIndex, SystemConfigIndex};
