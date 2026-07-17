//! Data types for the static data domain.

use super::*;

/// One column of a table, from declarative `db_schema.xml` (static — no DB needed).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DbColumn {
    pub name: String,
    /// The `xsi:type`: `int`, `smallint`, `varchar`, `text`, `decimal`, `timestamp`, …
    pub col_type: String,
    pub nullable: bool,
    pub unsigned: bool,
    /// `length` for `varchar`/`char`; `None` otherwise.
    pub length: Option<String>,
    /// `precision`/`scale` for `decimal`.
    pub precision: Option<String>,
    pub scale: Option<String>,
    pub default: Option<String>,
    /// `identity="true"` — auto-increment.
    pub identity: bool,
    pub comment: Option<String>,
    pub source: Source,
}

/// An index (`<index>`), from `db_schema.xml`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DbIndex {
    /// `referenceId`.
    pub id: String,
    /// `indexType`: `btree`, `fulltext`, `hash`.
    pub index_type: String,
    pub columns: Vec<String>,
    pub source: Source,
}

/// A constraint (`<constraint>`): primary key, unique key, or foreign key, from `db_schema.xml`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DbConstraint {
    /// `referenceId` (`PRIMARY` for the primary key).
    pub id: String,
    /// `primary`, `unique`, or `foreign`.
    pub kind: String,
    /// Local columns covered (the referencing column for a foreign key).
    pub columns: Vec<String>,
    /// Foreign keys only: the referenced table/column and `ON DELETE` action.
    pub reference_table: Option<String>,
    pub reference_column: Option<String>,
    pub on_delete: Option<String>,
    pub source: Source,
}

/// A table merged from every module's `db_schema.xml`, in load order (a module can add columns
/// /indexes/constraints to another module's table; `disabled="true"` drops them).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DbTable {
    pub name: String,
    pub engine: Option<String>,
    pub resource: Option<String>,
    pub comment: Option<String>,
    pub columns: Vec<DbColumn>,
    pub constraints: Vec<DbConstraint>,
    pub indexes: Vec<DbIndex>,
    /// Where the table was first declared.
    pub source: Source,
}

/// One table an indexer's materialized view subscribes to (`mview.xml`): changes to it are
/// change-logged and trigger partial reindexing when the indexer runs "by schedule".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MviewSubscription {
    pub table: String,
    /// The column identifying the changed entity (usually `entity_id`).
    pub entity_column: Option<String>,
    pub source: Source,
}

/// Live indexer state (`indexer_state` joined with `mview_state`), attached via `--db`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct IndexerLive {
    /// `valid` (ready) / `invalid` (reindex required) / `working` / `suspended`;
    /// `None` = no `indexer_state` row (never initialized).
    pub status: Option<String>,
    pub updated: Option<String>,
    /// Update mode: `true` = by schedule (mview enabled), `false` = on save;
    /// `None` = no `mview_state` row.
    pub by_schedule: Option<bool>,
    /// The view's own state: `idle`/`working`/`suspended`.
    pub view_status: Option<String>,
    /// Distinct pending entities in the changelog not yet applied (schedule mode;
    /// `None` = no changelog table or mode is on-save).
    pub backlog: Option<u64>,
}

/// An indexer from `indexer.xml`, joined (on `view_id`) with its `mview.xml` view — the
/// definition plus the tables whose changes feed it. Merged across modules in load order
/// (a module can add subscriptions to another module's view).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Indexer {
    /// Indexer id, e.g. `catalog_product_price` (what `bin/magento indexer:reindex` takes).
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub class: ClassName,
    /// The `mview.xml` view this indexer subscribes through (usually equal to `id`).
    pub view_id: Option<String>,
    /// Indexers sharing one physical index; reindexing either validates both.
    pub shared_index: Option<String>,
    /// Indexer ids this one depends on (they index first).
    pub dependencies: Vec<String>,
    /// Tables the indexer's view subscribes to, from `mview.xml` (empty if no view).
    pub subscriptions: Vec<MviewSubscription>,
    /// Live status from the DB (`--db` only).
    pub live: Option<IndexerLive>,
    pub source: Source,
}

/// One admin configuration field from `adminhtml/system.xml` — the map from a config `path`
/// to **where it lives in the admin** (Stores → Configuration → tab → section → group → field)
/// and how it behaves. Static; merged across modules.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SystemField {
    /// `section/group/field` (or the field's explicit `<config_path>` override).
    pub path: String,
    /// The field's label (what the admin shows).
    pub label: String,
    /// Field input type: `text`, `select`, `multiselect`, `password`, …
    pub field_type: String,
    /// Tab label (the top of the breadcrumb), resolved from the section's `<tab>` reference.
    pub tab: Option<String>,
    /// Section label.
    pub section: String,
    /// Group label.
    pub group: String,
    /// Scopes the field is editable at: any of `default`, `website`, `store`.
    pub scopes: Vec<String>,
    pub source_model: Option<String>,
    pub backend_model: Option<String>,
    pub source: Source,
}

/// An admin ACL resource from `acl.xml` — one node in the permission tree (Magento Admin →
/// … → this resource). The ids that `webapi` and `system-config` cite as required
/// `<resource>`s resolve here. Merged across modules: a module attaches resources under
/// another module's by re-stating the ancestor path.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AclResource {
    /// The unique resource id, e.g. `Magento_Sales::actions_view`.
    pub id: String,
    /// Human title shown in the admin role-permissions tree (empty when the id is used only as
    /// a path anchor and never declared with a title).
    pub title: String,
    /// Parent resource id (`None` for a root, normally `Magento_Backend::admin`).
    pub parent: Option<String>,
    /// Direct child resource ids, sorted by (`sortOrder`, id).
    pub children: Vec<String>,
    pub sort_order: Option<i32>,
    pub disabled: bool,
    pub source: Source,
}

/// Where a translation row comes from, in Magento's precedence order: modules (the
/// current request's controller module wins within this layer), then language packs,
/// then themes (child over parent), then the `translation` DB table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationLayer {
    Module(ModuleName),
    /// Language pack name (`vendor/package`).
    Pack(String),
    /// Theme id (`frontend/Magento/luma`).
    Theme(String),
    Db,
}

/// One dictionary row for a phrase.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TranslationEntry {
    pub layer: TranslationLayer,
    pub value: String,
    /// `key == value`: Magento's loader *deletes* earlier translations of the key
    /// (reset to untranslated), it does not set anything.
    pub reset: bool,
    /// DB rows only: the store the row applies to.
    pub store_id: Option<u32>,
    pub source: Source,
}

/// Every dictionary row matching a phrase, in precedence order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TranslationMatch {
    pub key: String,
    pub entries: Vec<TranslationEntry>,
}

/// The result of a translation scan, with enough context to explain an empty one:
/// how many dictionaries for the locale were actually scanned, and which
/// disabled/unregistered modules ship one that therefore never loads.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Translations {
    pub locale: String,
    pub matches: Vec<TranslationMatch>,
    /// `<locale>.csv` files that exist across enabled modules, packs, and themes.
    pub dictionaries_scanned: usize,
    /// Modules that ship a dictionary for this locale but are disabled or not in
    /// config.php — their translations never load.
    pub inactive_dictionaries: Vec<ModuleName>,
}

/// One attribute in a catalog attribute group, with the module that added it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CatalogAttribute {
    pub name: String,
    pub source: Source,
}

/// A `catalog_attributes.xml` group — which attributes Magento loads in a given context
/// (`quote_item`, `wishlist_item`, `catalog_product` collections, …), merged across
/// modules (each attribute keeps the adding module's `Source`). The "why isn't my
/// attribute available on the quote item" surface.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CatalogAttributeGroup {
    pub name: String,
    /// Sorted by attribute name.
    pub attributes: Vec<CatalogAttribute>,
}

/// A theme's override of an email template file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EmailTemplateOverride {
    /// Theme id, e.g. `frontend/Magento/luma`.
    pub theme: String,
    pub file: std::path::PathBuf,
}

/// A transactional email template registered in `etc/email_templates.xml`. The `id` is
/// also the value config stores when the template is selected in the admin.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EmailTemplate {
    pub id: String,
    pub label: String,
    /// File name as declared, relative to `<module>/view/<area>/email/`.
    pub file: String,
    /// `html` or `text`.
    pub kind: String,
    /// The module whose view dir holds the file (may differ from the declaring module).
    pub module: ModuleName,
    pub area: Area,
    /// The resolved module file; `None` = declared but missing on disk (broken).
    pub path: Option<std::path::PathBuf>,
    /// Theme files that override it (which one applies depends on the active theme).
    pub theme_overrides: Vec<EmailTemplateOverride>,
    pub source: Source,
}

/// One admin-configurable parameter of a widget.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WidgetParam {
    pub name: String,
    /// The `xsi:type` (`text`, `select`, `block`, `conditions`, …).
    pub param_type: String,
    pub required: bool,
    pub label: String,
    pub source_model: Option<ClassName>,
    pub default: Option<String>,
}

/// A widget type declared in `etc/widget.xml` — what the admin's "Insert Widget" offers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Widget {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    /// The block class that renders it.
    pub class: ClassName,
    pub parameters: Vec<WidgetParam>,
    /// Layout containers it declares templates for.
    pub containers: Vec<String>,
    pub source: Source,
}

/// The kind of a layout operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutOpKind {
    Block,
    Container,
    ReferenceBlock,
    ReferenceContainer,
    Update,
    Move,
}

/// One operation a layout file performs on the page.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LayoutOp {
    pub kind: LayoutOpKind,
    /// Block/container name; the target handle for `Update`; the element for `Move`.
    pub name: String,
    pub class: Option<ClassName>,
    pub template: Option<String>,
    /// The enclosing named element (or `Move`'s destination).
    pub parent: Option<String>,
    /// `remove="true"` on a reference.
    pub remove: bool,
    pub source: Source,
}

/// Who provides a layout or ui component file: a module (merged in load order) or a
/// theme (applied per the active theme's ancestry — reported, not resolved, since the
/// active theme is runtime state).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutLayer {
    Module(ModuleName),
    /// Theme id, e.g. `frontend/Magento/luma`.
    Theme(String),
}

/// One layout file's contribution to a handle.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LayoutContribution {
    pub layer: LayoutLayer,
    pub file: std::path::PathBuf,
    pub ops: Vec<LayoutOp>,
}

/// Everything contributing to one layout handle in one area: module files (in load
/// order), then theme files, plus the handle-inclusion graph around it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LayoutView {
    pub handle: String,
    pub area: Area,
    pub contributions: Vec<LayoutContribution>,
    /// Handles this one pulls in (`<update handle=…>`).
    pub includes: Vec<String>,
    /// Handles that pull this one in.
    pub included_by: Vec<String>,
}

/// One physical `.phtml` file that can satisfy a template reference.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TemplateFile {
    /// Module source or theme override layer. Theme application depends on the active
    /// theme, so all candidates are reported rather than claiming one wins.
    pub layer: LayoutLayer,
    pub file: std::path::PathBuf,
}

/// One layout operation that assigns a template to a block.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TemplateUsage {
    pub handle: String,
    pub block: String,
    pub class: Option<ClassName>,
    pub source: Source,
}

/// A `Vendor_Module::path.phtml` template in one area, joined to its module file,
/// every theme override candidate, and every layout operation that uses it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Template {
    pub reference: String,
    pub area: Area,
    pub files: Vec<TemplateFile>,
    pub usages: Vec<TemplateUsage>,
}

/// One node a ui component file declares or modifies. UI component XML is
/// open-vocabulary — the element name IS the component type (`column`, `field`,
/// `dataSource`, …) — and Magento merges same-name files by matching `(element, name)`,
/// so each node is reported with both.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UiComponentOp {
    pub element: String,
    pub name: String,
    /// PHP class (`class=`) — data providers, column/filter renderers.
    pub class: Option<ClassName>,
    /// JS component (`component=`).
    pub component: Option<String>,
    /// `<field formElement=>`.
    pub form_element: Option<String>,
    pub sort_order: Option<String>,
    /// `<settings><label>` (or a button's direct `<label>`).
    pub label: Option<String>,
    /// `<settings><disabled>true</disabled>` — removes the node on merge.
    pub disabled: bool,
    /// `<settings><visible>` when stated.
    pub visible: Option<bool>,
    /// Nearest enclosing named node.
    pub parent: Option<String>,
    /// Nesting depth below the root element (captured ancestors; for tree rendering).
    pub depth: u16,
    pub source: Source,
}

/// One file's contribution to a ui component (module files in load order, then themes).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UiComponentContribution {
    pub layer: LayoutLayer,
    pub file: std::path::PathBuf,
    pub ops: Vec<UiComponentOp>,
}

/// Everything contributing to one ui component (an admin grid, form, …) in one area.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UiComponentView {
    pub name: String,
    /// The root element of the first declaring file (`listing`, `form`, …).
    pub kind: String,
    pub area: Area,
    pub contributions: Vec<UiComponentContribution>,
}
