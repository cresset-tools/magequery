//! Data types for the catalog domain.

use super::*;

/// One scope's value of a product attribute: the raw stored value plus the resolved
/// human label when the attribute's options make that possible.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProductScopeValue {
    /// `default` (store_id 0) or the store view code.
    pub store: String,
    pub value: String,
    /// Option/source label (`1` → `Enabled`, option id → admin label).
    pub label: Option<String>,
}

/// One product attribute with every scope value the database stores for it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProductValue {
    pub attribute: String,
    /// `varchar`/`int`/`decimal`/`text`/`datetime` — which value table the rows live in.
    pub backend_type: String,
    pub input: Option<String>,
    /// Default-scope row first, then store overrides sorted by store code.
    pub scopes: Vec<ProductScopeValue>,
}

/// One MSI source's stock for a product (`inventory_source_item`, keyed by SKU).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProductSourceStock {
    pub source: String,
    pub quantity: String,
    pub in_stock: bool,
}

/// The legacy `cataloginventory_stock_item` row — what pre-MSI extensions still read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProductLegacyStock {
    pub qty: String,
    pub in_stock: bool,
    pub manage_stock: bool,
}

/// A category assignment, with the admin-style breadcrumb (root levels skipped).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProductCategory {
    pub id: u32,
    pub breadcrumb: String,
}

/// One `url_rewrite` row pointing at the product.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProductRewrite {
    pub request_path: String,
    pub store: String,
    /// 0 = internal rewrite, else the HTTP redirect code.
    pub redirect: u16,
}

/// One entry of a product's media gallery (`catalog_product_entity_media_gallery`),
/// with the store-0 label/position and the image roles it fills.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProductMedia {
    /// The stored file path (e.g. `/a/b/shoe.jpg`) or the external-video url.
    pub file: String,
    /// `image` or `external-video`.
    pub media_type: String,
    /// The store-0 (default) label, if any.
    pub label: Option<String>,
    pub position: i32,
    /// Hidden from the gallery (`disabled` on the gallery row or its store-0 value).
    pub disabled: bool,
    /// Image roles this file fills, from the product's role attributes: any of
    /// `base` (`image`), `small` (`small_image`), `thumbnail`, `swatch` (`swatch_image`).
    pub roles: Vec<String>,
}

/// One product as the database stores it: identity, per-scope EAV values, stock (MSI +
/// legacy), categories, rewrites, and configurable links. Live DB.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Product {
    pub entity_id: u32,
    pub sku: String,
    pub type_id: String,
    pub attribute_set: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Website codes the product is assigned to.
    pub websites: Vec<String>,
    pub values: Vec<ProductValue>,
    /// MSI per-source stock (empty when MSI tables are absent).
    pub stock: Vec<ProductSourceStock>,
    pub legacy_stock: Option<ProductLegacyStock>,
    pub categories: Vec<ProductCategory>,
    pub rewrites: Vec<ProductRewrite>,
    /// Media gallery entries, in position order.
    pub media: Vec<ProductMedia>,
    /// Configurable parents this product is a variant of (SKUs).
    pub parents: Vec<String>,
    /// The attributes a configurable is configured by (`catalog_product_super_attribute`).
    pub super_attributes: Vec<String>,
    /// Configurable variants / grouped associates, with their identity essentials.
    pub children: Vec<ProductChild>,
    /// Bundle options with their selections (bundles only).
    pub bundle_options: Vec<BundleOption>,
    /// The lookup resolved via entity_id, not SKU (numeric query, no SKU match).
    pub matched_by_id: bool,
}

/// One row of a product search (`sku` substring).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProductHit {
    pub entity_id: u32,
    pub sku: String,
    pub type_id: String,
    pub name: Option<String>,
    /// `status` decoded; `None` = no status row.
    pub enabled: Option<bool>,
}

/// One product a link points at (or, in a reverse view, that points at this product),
/// enriched with the state that decides whether it actually renders in the block.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProductLinkTarget {
    /// The `position` link attribute (0 when unset); the block's display order.
    pub position: i32,
    pub sku: String,
    pub name: Option<String>,
    /// `status` decoded; `None` = no status row (treated as enabled by Magento).
    pub enabled: Option<bool>,
    /// Decoded visibility label (e.g. `Catalog, Search`).
    pub visibility: Option<String>,
    /// Legacy `is_in_stock`; `None` = no stock row.
    pub in_stock: Option<bool>,
    /// The product won't render in the block: disabled, or Not Visible Individually.
    pub hidden: bool,
}

/// The merchandising links of one product: related, up-sells, cross-sells
/// (`catalog_product_link` types 1/4/5). Live DB.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProductLinks {
    pub entity_id: u32,
    pub sku: String,
    pub type_id: String,
    pub name: Option<String>,
    /// `true` = the reverse view: products that link *to* this one.
    pub reverse: bool,
    pub related: Vec<ProductLinkTarget>,
    pub up_sells: Vec<ProductLinkTarget>,
    pub cross_sells: Vec<ProductLinkTarget>,
    /// The lookup resolved via entity_id, not SKU (numeric query, no SKU match).
    pub matched_by_id: bool,
}

/// One queue's live backlog (MysqlMq driver tables) joined with the static topology.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct QueueBacklog {
    pub queue: String,
    /// Consumers reading it (`queue_consumer.xml`).
    pub consumers: Vec<String>,
    /// The queue exists in the db driver's `queue` table. `false` = the static config
    /// names it but the db driver doesn't know it (amqp-only, or setup:upgrade pending) —
    /// its broker-side backlog isn't inspectable from here.
    pub in_db: bool,
    /// In the DB but no static config references it (a removed module's leftover).
    pub orphaned: bool,
    /// Waiting messages (status `new`).
    pub new: u32,
    pub in_progress: u32,
    /// Failed, will be retried.
    pub retry: u32,
    pub error: u32,
    /// Complete/to-be-deleted rows awaiting cleanup.
    pub done: u32,
    /// Age of the oldest waiting (new/retry) message, DB-server clock.
    pub oldest_waiting_secs: Option<i64>,
}
