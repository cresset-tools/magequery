//! Data types for the commerce domain.

use super::*;

/// One node of the category tree (pre-order flattened; `level` 1 = a root tree).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CategoryTreeNode {
    pub id: u32,
    pub name: String,
    pub level: u32,
    /// Directly assigned products (`catalog_category_product`).
    pub direct_products: u32,
    /// Default-scope flags; `None` = no row.
    pub active: Option<bool>,
    pub in_menu: Option<bool>,
    pub anchor: Option<bool>,
    /// Store groups using this category as their root (roots only).
    pub root_of: Vec<String>,
}

/// A category search hit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CategoryHit {
    pub id: u32,
    pub name: String,
    pub url_key: Option<String>,
    pub level: u32,
    pub active: Option<bool>,
}

/// One "this category is invisible" finding: the category's own scope settings, or an
/// ancestor whose inactivity hides the whole subtree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CategoryVisibilityIssue {
    /// `None` = the category itself.
    pub ancestor_id: Option<u32>,
    pub ancestor_name: Option<String>,
    /// Scopes where it is effectively inactive (`all scopes`, `default`, `stores/<code>`).
    pub scopes: Vec<String>,
}

/// The per-store indexed product count (`catalog_category_product_index_store<N>` —
/// what the storefront lists, including anchor-inherited products).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CategoryIndexCount {
    pub store: String,
    pub products: u32,
}

/// One directly assigned product (`--products`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CategoryProduct {
    pub entity_id: u32,
    pub sku: String,
    pub name: Option<String>,
    pub position: i64,
}

/// One product from the category's per-store index (`--indexed`) — what the storefront
/// actually lists, including anchor-inherited products.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CategoryIndexedProduct {
    pub entity_id: u32,
    pub sku: String,
    pub name: Option<String>,
    pub position: i64,
    /// Inherited from a subcategory via anchoring (`is_parent = 0`), not assigned here.
    pub via_anchor: bool,
    /// The product's effective visibility (3 = search only — in the index but not on
    /// the category page; 1 = not visible individually).
    pub visibility: Option<i64>,
}

/// One category as the database stores it. Live DB.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Category {
    pub id: u32,
    pub path: String,
    pub level: u32,
    pub position: u32,
    pub parent_id: Option<u32>,
    pub parent_name: Option<String>,
    pub children: u32,
    /// Ancestor names past the two roots, joined ` > ` (admin style).
    pub breadcrumb: String,
    /// Per-scope attribute values (name, is_active, url_key, display_mode, …).
    pub values: Vec<ProductValue>,
    /// Why the category (or its subtree position) is invisible, when it is.
    pub visibility: Vec<CategoryVisibilityIssue>,
    pub direct_products: u32,
    /// Indexed count per store view; empty = no index tables found.
    pub indexed: Vec<CategoryIndexCount>,
    pub rewrites: Vec<ProductRewrite>,
    /// Store groups whose root this category is.
    pub root_of: Vec<String>,
    /// Directly assigned products (populated with `--products`).
    pub products: Vec<CategoryProduct>,
    /// The store view whose index was read (`--indexed`); `None` = not requested.
    pub indexed_store: Option<String>,
    /// `None` while `indexed_store` is set = the store's index table doesn't exist
    /// (the category:product indexer never ran for it).
    pub indexed_products: Option<Vec<CategoryIndexedProduct>>,
}

/// One tier-price row (`catalog_product_entity_tier_price`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TierPrice {
    /// Website code, or `(all)` for website_id 0.
    pub website: String,
    /// Customer group name, or `ALL GROUPS`.
    pub customer_group: String,
    pub qty: String,
    /// Fixed price — or `None` when the row is a percentage discount.
    pub value: Option<String>,
    pub percentage: Option<String>,
}

/// One catalog-rule price row (`catalogrule_product_price` — the rule engine's
/// materialized prices, ±1 day around today).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RulePrice {
    pub date: String,
    pub website: String,
    pub customer_group: String,
    pub rule_price: String,
}

/// One `catalog_product_index_price` row — what the storefront actually reads.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct IndexedPrice {
    pub website: String,
    pub customer_group: String,
    pub price: Option<String>,
    pub final_price: Option<String>,
    pub min_price: Option<String>,
    pub max_price: Option<String>,
    pub tier_price: Option<String>,
}

/// One component of a composite product (configurable variant or grouped associate),
/// identity-flavored: which option combination it is, whether it's enabled, its stock.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProductChild {
    pub sku: String,
    pub entity_id: u32,
    /// `status` decoded; `None` = no status row.
    pub enabled: Option<bool>,
    /// Configurables: the child's value per super attribute, resolved to option labels,
    /// in super-attribute order (`["Blue", "32"]`; `-` for a missing value). Empty for
    /// grouped associates.
    pub options: Vec<String>,
    /// Legacy `cataloginventory_stock_item` qty; `None` = no row.
    pub qty: Option<String>,
    pub in_stock: Option<bool>,
    /// Grouped: the default add-to-cart qty from the link attributes.
    pub default_qty: Option<String>,
}

/// One selectable product inside a bundle option.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BundleSelection {
    pub sku: String,
    pub entity_id: u32,
    pub enabled: Option<bool>,
    pub qty: String,
    pub is_default: bool,
    /// Fixed-price bundles: the selection's price adjustment.
    pub price: Option<String>,
    /// The adjustment is a percentage of the bundle price, not a fixed amount.
    pub price_percent: bool,
    pub in_stock: Option<bool>,
}

/// One bundle option (`catalog_product_bundle_option`) with its selections.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BundleOption {
    /// Default-store title.
    pub title: String,
    pub required: bool,
    /// `select`/`radio`/`checkbox`/`multi`.
    pub input_type: String,
    pub selections: Vec<BundleSelection>,
}

/// One component's price summary — a composite product's storefront price derives from
/// its components (configurable variants, grouped associates, bundle selections), so
/// these lines explain the parent's index min/max.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ChildPrice {
    pub sku: String,
    pub entity_id: u32,
    /// `status` decoded; `None` = no status row.
    pub enabled: Option<bool>,
    /// Default-scope EAV values.
    pub price: Option<String>,
    pub special_price: Option<String>,
    /// The child's own index `final_price` range across (website, group) rows;
    /// both `None` = not indexed (excluded from the parent's price).
    pub final_min: Option<String>,
    pub final_max: Option<String>,
    /// Fixed-price bundles: this selection's price adjustment.
    pub selection_price: Option<String>,
    /// The adjustment is a percentage, not an amount.
    pub selection_percent: bool,
}

/// Every price the database stores for one product: the EAV price attributes per scope,
/// tier prices, materialized catalog-rule prices, and the price index.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProductPrices {
    pub entity_id: u32,
    pub sku: String,
    pub type_id: String,
    /// `catalog/price/scope`: `true` = website-scoped prices, `false` = global.
    pub price_scope_website: bool,
    /// The price-ish EAV attributes (`price`, `special_price` + validity dates, `cost`,
    /// `msrp`, …), with every scope row.
    pub attributes: Vec<ProductValue>,
    pub tier_prices: Vec<TierPrice>,
    pub rule_prices: Vec<RulePrice>,
    pub index: Vec<IndexedPrice>,
    /// Component prices: configurable variants, grouped associates, or bundle
    /// selections (empty for simples).
    pub children: Vec<ChildPrice>,
    /// Bundles: `fixed` or `dynamic` (the `price_type` attribute).
    pub bundle_price_type: Option<String>,
    pub matched_by_id: bool,
}

/// CMS entry kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CmsKind {
    Page,
    Block,
}

impl std::fmt::Display for CmsKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CmsKind::Page => "cms-page",
            CmsKind::Block => "cms-block",
        })
    }
}

/// One CMS page or block row. The same identifier can exist as several rows scoped to
/// different stores — lookups return every row, never a silent pick. Live DB.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CmsEntry {
    pub kind: CmsKind,
    pub id: u32,
    pub identifier: String,
    pub title: String,
    pub active: bool,
    /// Store codes; `(all stores)` for the store-0 assignment.
    pub stores: Vec<String>,
    pub created: Option<String>,
    pub updated: Option<String>,
    /// Pages only.
    pub page_layout: Option<String>,
    pub meta_title: Option<String>,
    /// A layout-update XML is attached (pages; a classic invisible behavior source).
    pub has_layout_update: bool,
    pub content_len: usize,
    /// First chunk of the content, whitespace-collapsed.
    pub content_preview: String,
    /// Full content — populated only when requested.
    pub content: Option<String>,
}

/// One row of a CMS search.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CmsHit {
    pub id: u32,
    pub identifier: String,
    pub title: String,
    pub active: bool,
    pub stores: Vec<String>,
}

/// One API integration (`integration` + its oauth token state + granted ACL resources).
/// Token secrets are never exposed — only presence/revocation. Live DB.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Integration {
    pub id: u32,
    pub name: String,
    pub email: Option<String>,
    pub endpoint: Option<String>,
    /// `inactive` / `active` / `recreated`.
    pub status: String,
    /// `manual` (admin-created) / `config` (defined in a module's config).
    pub setup: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Access-token state: `none` / `active` / `revoked`.
    pub token: String,
    /// Granted `Magento_Backend::all`.
    pub all_resources: bool,
    /// Granted resources, titled from the static acl.xml index (like admin roles).
    pub rules: Vec<AdminRule>,
}

/// One tax rate (`tax_calculation_rate`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaxRate {
    pub id: u32,
    pub code: String,
    pub country: String,
    /// Region code, `*` for all.
    pub region: String,
    /// `*` for all (or a zip range collapsed to `from–to`).
    pub postcode: String,
    pub rate: String,
}

/// One tax class, with whether any rule references it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaxClassInfo {
    pub id: u32,
    pub name: String,
    /// `CUSTOMER` / `PRODUCT`.
    pub class_type: String,
    /// Referenced by at least one tax rule — a product class in no rule is untaxed.
    pub in_rules: bool,
}

/// One tax rule with its class combination and rates.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaxRule {
    pub id: u32,
    pub code: String,
    pub priority: u32,
    pub calculate_subtotal: bool,
    pub customer_classes: Vec<String>,
    pub product_classes: Vec<String>,
    pub rates: Vec<TaxRate>,
}

/// The tax picture: classes, rules, and rates no rule uses. Live DB.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaxInfo {
    pub classes: Vec<TaxClassInfo>,
    pub rules: Vec<TaxRule>,
    /// Configured but referenced by no rule.
    pub unused_rates: Vec<TaxRate>,
}

/// One catalog price rule (`catalogrule`), with its applied state. Live DB.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CatalogRule {
    pub rule_id: u32,
    pub name: String,
    pub description: Option<String>,
    pub active: bool,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    /// Today is inside [from_date, to_date], per the DB clock.
    pub in_window: bool,
    /// Decoded `simple_action`.
    pub action: String,
    pub sort_order: u32,
    pub stop_rules_processing: bool,
    pub websites: Vec<String>,
    pub customer_groups: Vec<String>,
    /// Raw `conditions_serialized` — displayed, not interpreted.
    pub conditions: Option<String>,
    /// Distinct products in `catalogrule_product` — the materialized matches. 0 while
    /// everything else is green = "Apply Rules"/the catalogrule indexer never ran, or
    /// the conditions match nothing.
    pub matched_products: u32,
}

/// One row of a catalog-rule list.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CatalogRuleHit {
    pub rule_id: u32,
    pub name: String,
    pub active: bool,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    pub matched_products: u32,
}

/// One coupon of a cart price rule.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RuleCoupon {
    pub code: String,
    pub times_used: u64,
    /// `None`/0 = unlimited.
    pub usage_limit: Option<u64>,
    pub usage_per_customer: Option<u64>,
    pub expiration_date: Option<String>,
    /// Past expiration, per the DB clock.
    pub expired: bool,
}

/// One cart price rule (`salesrule`), with the why-won't-it-apply facts. Live DB.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SalesRule {
    pub rule_id: u32,
    pub name: String,
    pub description: Option<String>,
    pub active: bool,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    /// Today is inside [from_date, to_date], per the DB clock.
    pub in_window: bool,
    /// Decoded `coupon_type`: no coupon / specific coupon / auto-generated.
    pub coupon_type: String,
    /// Decoded `simple_action` + amount (`10% off`, `5.00 off cart`, …).
    pub action: String,
    pub apply_to_shipping: bool,
    pub free_shipping: bool,
    /// Later (higher sort_order) rules don't run after this one matches.
    pub stop_rules_processing: bool,
    pub sort_order: u32,
    /// 0 = unlimited.
    pub uses_per_customer: u64,
    pub uses_per_coupon: u64,
    /// Rule-level usage counter.
    pub times_used: u64,
    pub websites: Vec<String>,
    pub customer_groups: Vec<String>,
    /// Raw `conditions_serialized` — displayed, not interpreted.
    pub conditions: Option<String>,
    pub coupon_count: u32,
    /// First few coupons (auto-generated rules can have thousands).
    pub coupons: Vec<RuleCoupon>,
    /// Set when the lookup came in via a coupon code.
    pub matched_coupon: Option<RuleCoupon>,
}

/// One row of a sales-rule search.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SalesRuleHit {
    pub rule_id: u32,
    pub name: String,
    pub active: bool,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
}

/// One store view in the scope tree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StoreViewNode {
    pub id: u32,
    pub code: String,
    pub name: String,
    pub active: bool,
    /// The group's default view.
    pub is_default: bool,
}

/// One store group in the scope tree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StoreGroupNode {
    pub id: u32,
    pub name: String,
    pub root_category_id: u32,
    pub root_category: Option<String>,
    /// The website's default group.
    pub is_default: bool,
    pub views: Vec<StoreViewNode>,
}

/// One website in the scope tree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WebsiteNode {
    pub id: u32,
    pub code: String,
    pub name: String,
    pub is_default: bool,
    pub groups: Vec<StoreGroupNode>,
}

/// The full scope tree (websites → groups → views, admin scopes excluded) plus the
/// currency rate table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StoreTree {
    pub websites: Vec<WebsiteNode>,
    /// `(from, to, rate)` from `directory_currency_rate`.
    pub currency_rates: Vec<(String, String, String)>,
}

/// One state assignment of an order status.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OrderStatusState {
    pub state: String,
    /// The default status for that state.
    pub is_default: bool,
    pub visible_on_front: bool,
}

/// One order status with its state mapping(s). A status mapped to no state is
/// assignable manually but never set by Magento itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OrderStatus {
    pub status: String,
    pub label: String,
    pub states: Vec<OrderStatusState>,
}

/// One customer group with its tax class and how many accounts belong to it. The
/// built-in `NOT LOGGED IN` group (id 0) is the guest bucket: it has no stored
/// members, so `members` is 0 by design. Live DB.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CustomerGroup {
    pub id: i64,
    pub code: String,
    pub tax_class_id: u32,
    /// The tax class name (`tax_class.class_name`); `None` if the id resolves to no row.
    pub tax_class: Option<String>,
    /// Accounts in `customer_entity` whose `group_id` is this group.
    pub members: u64,
}

/// One sales increment sequence (per entity type × store).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SalesSequence {
    /// `order` / `invoice` / `shipment` / `creditmemo` (+ custom types).
    pub entity_type: String,
    pub store: String,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    pub step: u64,
    pub active: bool,
    /// Highest issued sequence value; `None` = nothing issued yet.
    pub current: Option<u64>,
    /// The next increment id, computed with Magento's default pattern
    /// (`prefix + 9-digit zero-padded value + suffix`) — custom patterns not modeled.
    pub next_increment: String,
    pub max_value: Option<u64>,
    pub warning_value: Option<u64>,
}
