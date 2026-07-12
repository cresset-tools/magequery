//! Data types for the eav domain.

use super::*;

/// What a setup-script call does to an attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EavSetupKind {
    Add,
    Update,
    Remove,
}

/// The literal kind of a setup-property value (for PHP-style rendering).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EavValueKind {
    Str,
    Num,
    Bool,
    Null,
    Class,
    /// Non-literal (nested array, variable, concatenation) — display text only.
    Other,
}

/// One scalar property from an `addAttribute` array argument.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EavSetupProp {
    pub key: String,
    pub value: String,
    pub kind: EavValueKind,
}

/// One `addAttribute`/`updateAttribute`/`removeAttribute` call found in a setup script —
/// the static "who created/touched this attribute" join.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EavSetupRef {
    pub kind: EavSetupKind,
    /// Resolved entity-type code (`catalog_product`) when recognizable, else the raw
    /// expression as written.
    pub entity: String,
    pub entity_known: bool,
    pub code: String,
    pub props: Vec<EavSetupProp>,
    pub source: Source,
}

/// One row of `eav_entity_type`, with its attribute count.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EavEntityType {
    pub code: String,
    pub entity_table: Option<String>,
    pub attributes: usize,
}

/// An attribute's scope (`catalog_eav_attribute.is_global` decoded).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EavScope {
    Store,
    Global,
    Website,
}

impl std::fmt::Display for EavScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EavScope::Store => "store",
            EavScope::Global => "global",
            EavScope::Website => "website",
        })
    }
}

/// Catalog-specific attribute behavior (`catalog_eav_attribute`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EavCatalogFlags {
    pub scope: EavScope,
    pub searchable: bool,
    pub filterable: bool,
    pub filterable_in_search: bool,
    pub comparable: bool,
    pub used_in_listing: bool,
    pub used_for_sort_by: bool,
    pub visible_on_front: bool,
    /// Product types the attribute applies to (empty = all).
    pub apply_to: Vec<String>,
}

/// One EAV attribute from the live database.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EavAttribute {
    pub code: String,
    pub entity_type: String,
    pub attribute_id: u32,
    pub label: Option<String>,
    /// `varchar`/`int`/`decimal`/`text`/`datetime`/`static`.
    pub backend_type: String,
    pub frontend_input: Option<String>,
    pub required: bool,
    pub unique: bool,
    /// Created through the admin (vs installed by code).
    pub user_defined: bool,
    pub default_value: Option<String>,
    pub source_model: Option<ClassName>,
    pub backend_model: Option<ClassName>,
    pub frontend_model: Option<ClassName>,
    /// Present when a `catalog_eav_attribute` row exists.
    pub catalog: Option<EavCatalogFlags>,
    /// The table its values live in; `None` for `static` (a column on the entity table).
    pub value_table: Option<String>,
}

/// An attribute-set membership: which set, in which group.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EavSetMembership {
    pub set: String,
    pub group: String,
}

/// The full "what IS this attribute" card: the live row plus set membership, options,
/// and the static setup-script join (who created/modified it).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EavAttributeCard {
    pub attribute: EavAttribute,
    /// The entity's base table (for the `static` backend-type case).
    pub entity_table: Option<String>,
    pub sets: Vec<EavSetMembership>,
    /// How many attribute sets the entity has in total.
    pub total_sets: usize,
    /// Admin-scope option labels (table-source attributes).
    pub options: Vec<String>,
    pub setup_refs: Vec<EavSetupRef>,
}
