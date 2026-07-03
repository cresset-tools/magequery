//! `whatis <class>` — everything magequery knows about one class, on one screen: identity
//! (file, module, package, hierarchy), a compressed DI summary (forward + reverse), and
//! the cross-index sweep — every place any configuration references it. Pure composition
//! of existing queries; the sweep is `doctor`'s walk scoped to a single class.

use crate::ids::{Area, ClassName};
use crate::model::{ClassRef, Uses, Whatis};
use crate::{Error, Magento, Result};

pub(crate) fn run(mage: &Magento, class: &ClassName) -> Result<Whatis> {
    let file = mage.index.resolver.file_for(class);
    let header = mage.index.resolver.header_of(class);

    // Identity: kind, direct hierarchy, owning module + composer package.
    let kind = header.as_ref().map(|h| {
        if h.is_interface {
            "interface"
        } else if h.is_abstract {
            "abstract class"
        } else {
            "class"
        }
    });
    let (parents, interfaces) = header
        .as_ref()
        .map(|h| (h.extends.clone(), h.implements.clone()))
        .unwrap_or_default();

    let module = file.as_deref().and_then(|f| {
        // Longest module-path prefix wins (bundled-module packages nest under one root).
        mage.index
            .modules
            .iter()
            .filter(|m| f.starts_with(&m.path))
            .max_by_key(|m| m.path.as_os_str().len())
            .map(|m| m.name.clone())
    });
    let pkg = file.as_deref().and_then(|f| {
        f.ancestors().find_map(|a| mage.index.packages.iter().find(|p| p.root == a))
    });

    // Forward DI, compressed: the full `di` view stays one command away.
    let global = mage.di_index().config(Area::Global);
    let is_virtual_type = global.virtual_types.contains_key(class);
    let resolution = mage.resolve(class, Area::Global).ok();
    let resolves_to = resolution
        .as_ref()
        .map(|r| r.concrete.clone())
        .filter(|c| c != class);
    let instantiates = resolution.as_ref().and_then(|r| r.instantiates.clone());
    let plugin_count = resolution.as_ref().map(|r| r.plugins.len()).unwrap_or(0);
    let argument_count = resolution.as_ref().map(|r| r.arguments.len()).unwrap_or(0);

    // Reverse DI: the `uses` result, inlined.
    let uses = mage.uses(class, None).unwrap_or(Uses {
        class: class.clone(),
        preferred_for: Vec::new(),
        virtual_types: Vec::new(),
        injections: Vec::new(),
    });

    // The cross-index sweep: who runs this class.
    let mut observes = Vec::new();
    let mut seen_obs = std::collections::HashSet::new();
    for &area in Area::ALL.iter() {
        for (event, _) in mage.events(area) {
            for o in mage.observers(&event, area) {
                if o.instance == *class
                    && seen_obs.insert((o.event.clone(), o.name.clone(), o.source.file.clone()))
                {
                    observes.push(o);
                }
            }
        }
    }

    let cron_jobs: Vec<_> = mage
        .cron_jobs(None, false)
        .map(|c| c.jobs)
        .unwrap_or_default()
        .into_iter()
        .filter(|j| j.instance == *class)
        .collect();

    let webapi: Vec<_> =
        mage.webapi(None).into_iter().filter(|r| r.service_class == *class).collect();

    let command = mage.console_commands(None).into_iter().find(|c| {
        c.class == *class
            || c.class.as_str().strip_suffix("\\Proxy") == Some(class.as_str())
    });

    let mut graphql = Vec::new();
    for t in mage.graphql_types(None) {
        if t.type_resolver.as_ref() == Some(class) {
            graphql.push(ClassRef {
                role: format!("@typeResolver of {} {}", t.kind, t.name),
                source: t.source.clone(),
            });
        }
        for f in &t.fields {
            if f.resolver.as_ref() == Some(class) {
                graphql.push(ClassRef {
                    role: format!("@resolver of {}.{}", t.name, f.name),
                    source: f.source.clone(),
                });
            }
        }
    }

    let mut mq = Vec::new();
    for topic in mage.queue_topics(None) {
        for h in &topic.handlers {
            if h.class == *class {
                mq.push(ClassRef {
                    role: format!("handler `{}` of queue topic {}", h.name, topic.name),
                    source: h.source.clone(),
                });
            }
        }
        if let Some(route) = mage.queue_topic(&topic.name) {
            for r in &route.routes {
                for c in &r.consumers {
                    let handler_class =
                        c.handler.as_deref().map(|h| h.split_once("::").map(|(c, _)| c).unwrap_or(h));
                    if handler_class == Some(class.as_str())
                        || c.consumer_instance.as_ref() == Some(class)
                    {
                        mq.push(ClassRef {
                            role: format!("consumer `{}` of queue {}", c.name, r.queue),
                            source: c.source.clone(),
                        });
                    }
                }
            }
        }
    }
    mq.dedup_by(|a, b| a.role == b.role);

    // Controller URL — only worth the directory scan when the name says controller.
    let mut action_urls = Vec::new();
    if class.as_str().contains("\\Controller\\") {
        let area = if class.as_str().contains("\\Adminhtml\\") {
            Area::Adminhtml
        } else {
            Area::Frontend
        };
        action_urls =
            mage.actions(area, None).into_iter().filter(|a| a.class == *class).collect();
    }

    let w = Whatis {
        class: class.clone(),
        file,
        kind: kind.map(str::to_string),
        is_virtual_type,
        module,
        package: pkg.map(|p| p.name.clone()),
        package_version: pkg.and_then(|p| p.version.clone()),
        parents,
        interfaces,
        resolves_to,
        instantiates,
        plugin_count,
        argument_count,
        uses,
        observes,
        cron_jobs,
        webapi,
        command,
        graphql,
        mq,
        action_urls,
    };

    // A name that resolves to nothing anywhere is a typo, not an empty answer.
    if w.file.is_none() && !w.is_virtual_type && !w.is_referenced() {
        return Err(Error::ClassNotFound(class.clone()));
    }
    Ok(w)
}

impl Whatis {
    /// Whether any configuration anywhere references the class — the interesting negative
    /// (`false` on an existing file = candidate dead code).
    pub fn is_referenced(&self) -> bool {
        !self.uses.preferred_for.is_empty()
            || !self.uses.virtual_types.is_empty()
            || !self.uses.injections.is_empty()
            || !self.observes.is_empty()
            || !self.cron_jobs.is_empty()
            || !self.webapi.is_empty()
            || self.command.is_some()
            || !self.graphql.is_empty()
            || !self.mq.is_empty()
            || !self.action_urls.is_empty()
            || self.resolves_to.is_some()
            || self.plugin_count > 0
            || self.argument_count > 0
    }
}
