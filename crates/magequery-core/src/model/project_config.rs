//! Data types for the project config domain.

use super::*;

/// The kind of a setup patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchKind {
    Data,
    Schema,
}

impl std::fmt::Display for PatchKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PatchKind::Data => "data",
            PatchKind::Schema => "schema",
        })
    }
}

/// One setup patch class (`Setup/Patch/Data|Schema`), optionally with its applied state
/// from the `patch_list` table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Patch {
    pub class: ClassName,
    pub kind: PatchKind,
    pub module: ModuleName,
    /// Whether `patch_list` records it as applied; `None` = the DB wasn't consulted.
    pub applied: Option<bool>,
    pub source: Source,
}

/// All patches on disk, plus (with the DB) the applied entries no class on disk explains
/// — patches of removed modules, kept forever by `patch_list`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Patches {
    pub patches: Vec<Patch>,
    pub orphaned_applied: Vec<String>,
}

/// The auto-join a repository performs to load an extension attribute (`<join>`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExtensionJoin {
    pub reference_table: String,
    pub reference_field: Option<String>,
    pub join_on_field: Option<String>,
    pub fields: Vec<String>,
}

/// One extension attribute bolted onto an API data interface via
/// `extension_attributes.xml` — the mechanism behind the generated `…Extension` classes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExtensionAttribute {
    pub code: String,
    /// Declared type: a class/interface or a scalar, possibly `[]`-suffixed.
    pub ty: String,
    /// ACL resources gating the attribute in webapi responses.
    pub resources: Vec<String>,
    pub join: Option<ExtensionJoin>,
    pub source: Source,
}

/// An API data interface with every extension attribute modules add to it, merged in load
/// order (attributes keyed by code, last wins; each keeps the adding module's `Source` —
/// the point: `ProductInterface` is extended by inventory, gift-message, bundling, …).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExtendedType {
    pub for_type: ClassName,
    /// Sorted by code.
    pub attributes: Vec<ExtensionAttribute>,
}

/// One admin menu item from `adminhtml/menu.xml`, merged across modules in load order
/// (`<add>`/`<update>` upsert attribute-level; `<remove>` deletes). The tree the admin
/// sidebar renders — parents come from the `parent` attribute.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MenuItem {
    /// e.g. `Magento_Catalog::catalog_products`.
    pub id: String,
    pub title: String,
    pub parent: Option<String>,
    /// The admin route (`catalog/product/`); `None` for a grouping node.
    pub action: Option<String>,
    /// The ACL resource guarding it (resolve with `magequery acl <id>`).
    pub resource: Option<String>,
    pub sort_order: Option<i32>,
    /// Only shown when this module / config flag is active.
    pub depends_on_module: Option<String>,
    pub depends_on_config: Option<String>,
    /// Direct child ids, sorted by (`sortOrder`, id).
    pub children: Vec<String>,
    pub source: Source,
}

/// A REST endpoint from `webapi.xml`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WebapiRoute {
    /// HTTP method (`GET`/`POST`/…).
    pub method: String,
    pub url: String,
    pub service_class: ClassName,
    pub service_method: String,
    /// ACL resources required (`<resource ref="…"/>`), or `anonymous`/`self`.
    pub resources: Vec<String>,
    pub source: Source,
}

/// The flagship result: everything Magento needs to build `requested` in `area`, with a
/// `Source` on every contributing line.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Resolution {
    pub requested: ClassName,
    pub area: Area,
    pub concrete: ClassName,
    pub preference_chain: Vec<PreferenceStep>,
    /// If `concrete` is a virtual type, the real class it actually instantiates (following
    /// the `virtualType` `type=` chain to a non-virtual class). `None` for a real class.
    pub instantiates: Option<ClassName>,
    /// Plugins that fire on the concrete, sorted by `sort_order`. Includes plugins
    /// declared on ancestors/interfaces (see [`Plugin::declared_on`]).
    pub plugins: Vec<Plugin>,
    pub arguments: Vec<Argument>,
    /// Ancestor/interface types that contributed plugins or arguments to this resolution,
    /// nearest-first. Explains *why* an inherited plugin applies.
    pub contributing_ancestors: Vec<ClassName>,
}

/// A value computed independently for each [`Area`]. The natural representation of an
/// "all areas" answer: `Global` is the base, and the collapsed view shows only the areas
/// whose value differs from it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(transparent)]
pub struct ByArea<T> {
    entries: Vec<(Area, T)>,
}

impl<T> ByArea<T> {
    pub fn new(entries: Vec<(Area, T)>) -> Self {
        Self { entries }
    }
    pub fn get(&self, area: Area) -> Option<&T> {
        self.entries
            .iter()
            .find(|(a, _)| *a == area)
            .map(|(_, v)| v)
    }
    pub fn iter(&self) -> impl Iterator<Item = (Area, &T)> {
        self.entries.iter().map(|(a, v)| (*a, v))
    }
}

impl<T: PartialEq> ByArea<T> {
    /// The `Global` value, if present, plus the areas whose value differs from it. This is
    /// the data behind the CLI's default collapsed-diff rendering — kept in core so library
    /// users get the same "what actually differs per area" computation for free.
    pub fn deltas(&self) -> Deltas<'_, T> {
        let base = self.get(Area::Global);
        let differing = self
            .entries
            .iter()
            .filter(|(a, v)| *a != Area::Global && base != Some(v))
            .map(|(a, v)| (*a, v))
            .collect();
        Deltas { base, differing }
    }
}

/// Result of [`ByArea::deltas`].
pub struct Deltas<'a, T> {
    pub base: Option<&'a T>,
    /// Areas (excluding `Global`) whose value differs from `base`.
    pub differing: Vec<(Area, &'a T)>,
}
