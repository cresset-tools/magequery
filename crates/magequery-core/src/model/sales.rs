//! Data types for the sales domain.

use super::*;

/// Which sales document to look up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SalesDocKind {
    Invoice,
    Shipment,
    Creditmemo,
}

impl std::fmt::Display for SalesDocKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SalesDocKind::Invoice => "invoice",
            SalesDocKind::Shipment => "shipment",
            SalesDocKind::Creditmemo => "creditmemo",
        })
    }
}

/// One line of a sales document (price/row_total absent on shipments).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SalesDocumentItem {
    pub sku: String,
    pub name: Option<String>,
    pub qty: String,
    pub price: Option<String>,
    pub row_total: Option<String>,
}

/// One invoice / shipment / credit memo, with its order cross-link. Live DB.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SalesDocument {
    pub kind: SalesDocKind,
    pub entity_id: u32,
    pub increment_id: String,
    /// Decoded state (invoices: open/paid/canceled; memos: open/refunded/canceled;
    /// shipments have none).
    pub state: Option<String>,
    pub order_increment: Option<String>,
    pub order_status: Option<String>,
    pub created_at: Option<String>,
    pub currency: Option<String>,
    /// subtotal/tax/shipping/grand_total (+ memo adjustments); empty for shipments.
    pub totals: Vec<OrderTotal>,
    /// Invoices: the payment transaction id.
    pub transaction_id: Option<String>,
    /// Shipments: total packed qty.
    pub total_qty: Option<String>,
    pub items: Vec<SalesDocumentItem>,
    /// Shipments: `(carrier, title, number)`.
    pub tracks: Vec<(String, String, String)>,
}

/// One row of a document search.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SalesDocumentHit {
    pub increment_id: String,
    pub order_increment: Option<String>,
    pub created_at: Option<String>,
    /// Kind-specific: grand total (invoice/memo) or packed qty (shipment).
    pub amount: Option<String>,
}

/// One cart line.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct QuoteItem {
    pub sku: String,
    pub name: Option<String>,
    pub product_type: String,
    pub is_child: bool,
    pub qty: String,
    pub price: Option<String>,
    pub row_total: Option<String>,
    pub discount: Option<String>,
}

/// A quote address; the shipping one carries the chosen shipping method.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct QuoteAddress {
    pub kind: String,
    pub name: String,
    pub company: Option<String>,
    pub street: Option<String>,
    pub postcode: Option<String>,
    pub city: Option<String>,
    pub country: Option<String>,
    pub shipping_method: Option<String>,
    pub shipping_description: Option<String>,
}

/// One quote (cart) as checkout computed it. Live DB.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Quote {
    pub entity_id: u64,
    pub active: bool,
    pub store: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Seconds since the last update, DB-server clock — cart age.
    pub updated_secs: Option<i64>,
    pub converted_at: Option<String>,
    pub customer_id: Option<u32>,
    pub customer_email: Option<String>,
    pub customer_name: Option<String>,
    pub guest: bool,
    pub checkout_method: Option<String>,
    pub quote_currency: Option<String>,
    pub base_currency: Option<String>,
    pub items_qty: Option<String>,
    pub is_virtual: bool,
    pub coupon: Option<String>,
    pub applied_rule_ids: Option<String>,
    /// The order increment reserved at checkout start.
    pub reserved_order_id: Option<String>,
    /// The order this quote became, if any (`sales_order.quote_id`).
    pub order_increment: Option<String>,
    /// subtotal/shipping/tax/discount/grand_total — subtotal + grand total from the
    /// quote row, the rest from the shipping address (where checkout collects them).
    pub totals: Vec<OrderTotal>,
    pub items: Vec<QuoteItem>,
    pub addresses: Vec<QuoteAddress>,
    pub payment: Option<OrderPayment>,
}

/// One row of a quote search.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct QuoteHit {
    pub entity_id: u64,
    pub active: bool,
    pub customer_email: Option<String>,
    pub items_qty: Option<String>,
    pub grand_total: Option<String>,
    pub currency: Option<String>,
    pub updated_at: Option<String>,
}

/// One customer address, with default-billing/shipping tags.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CustomerAddress {
    pub id: u32,
    pub name: String,
    pub company: Option<String>,
    pub street: Option<String>,
    pub postcode: Option<String>,
    pub city: Option<String>,
    pub region: Option<String>,
    pub country: Option<String>,
    pub telephone: Option<String>,
    pub default_billing: bool,
    pub default_shipping: bool,
}

/// A newsletter subscription row (per store).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CustomerNewsletter {
    pub store: String,
    /// Decoded `subscriber_status`.
    pub status: String,
}

/// The customer's order summary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CustomerOrders {
    pub count: u32,
    /// Lifetime `base_grand_total` sum (base currency).
    pub lifetime: Option<String>,
    pub first_at: Option<String>,
    pub last_at: Option<String>,
    pub last_increment: Option<String>,
    pub last_status: Option<String>,
}

/// One customer as the database stores it. Live DB.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Customer {
    pub entity_id: u32,
    pub email: String,
    pub name: String,
    pub group: Option<String>,
    pub website: Option<String>,
    /// The `created_in` store-name snapshot.
    pub created_in: Option<String>,
    pub created_at: Option<String>,
    pub active: bool,
    /// `false` = a confirmation token is pending — the account can't log in yet.
    pub confirmed: bool,
    pub locked: bool,
    pub lock_expires: Option<String>,
    pub failures: u32,
    pub last_login: Option<String>,
    pub last_logout: Option<String>,
    pub dob: Option<String>,
    pub taxvat: Option<String>,
    pub addresses: Vec<CustomerAddress>,
    pub newsletter: Vec<CustomerNewsletter>,
    /// Custom (non-static) customer EAV attribute values.
    pub values: Vec<ProductValue>,
    pub orders: CustomerOrders,
    /// Guest orders placed with the same email (not linked to the account).
    pub guest_orders: u32,
    pub matched_by_id: bool,
}

/// One row of a customer search.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CustomerHit {
    pub entity_id: u32,
    pub email: String,
    pub name: String,
    pub group: Option<String>,
    pub created_at: Option<String>,
}

/// One order total line (order currency; `base` shown when the currencies differ).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OrderTotal {
    pub key: String,
    pub amount: Option<String>,
    pub base_amount: Option<String>,
}

/// One order line, with the full quantity lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OrderItem {
    pub sku: String,
    pub name: Option<String>,
    pub product_type: String,
    /// A composite's child row (qty bookkeeping lives on the parent).
    pub is_child: bool,
    pub qty_ordered: String,
    pub qty_invoiced: String,
    pub qty_shipped: String,
    pub qty_refunded: String,
    pub qty_canceled: String,
    pub price: Option<String>,
    pub row_total: Option<String>,
}

/// A billing or shipping address snapshot on the order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OrderAddress {
    /// `billing` / `shipping`.
    pub kind: String,
    pub name: String,
    pub company: Option<String>,
    pub street: Option<String>,
    pub postcode: Option<String>,
    pub city: Option<String>,
    pub country: Option<String>,
    pub telephone: Option<String>,
}

/// The order's payment: method plus the PSP's `additional_information` blob.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OrderPayment {
    pub method: Option<String>,
    pub last_trans_id: Option<String>,
    /// Flattened top-level entries of `additional_information` (nested values as JSON).
    pub additional: Vec<(String, String)>,
}

/// One `sales_payment_transaction` row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OrderTransaction {
    pub txn_id: String,
    pub kind: String,
    pub closed: bool,
    pub created_at: Option<String>,
}

/// An invoice or credit memo attached to the order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OrderDocument {
    pub increment_id: String,
    /// Decoded state (`open`/`paid`/`refunded`/`canceled`).
    pub state: Option<String>,
    pub total: Option<String>,
    pub created_at: Option<String>,
}

/// One shipment with its tracking numbers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OrderShipment {
    pub increment_id: String,
    pub qty: Option<String>,
    pub created_at: Option<String>,
    /// `(carrier, title, number)`.
    pub tracks: Vec<(String, String, String)>,
}

/// One status-history/comment row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OrderComment {
    pub status: Option<String>,
    pub comment: Option<String>,
    pub created_at: Option<String>,
    pub notified: bool,
}

/// One order as the database stores it (`sales_order` + its satellites). Live DB.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Order {
    pub entity_id: u32,
    pub increment_id: String,
    pub state: Option<String>,
    pub status: Option<String>,
    /// The status's admin label (`sales_order_status`).
    pub status_label: Option<String>,
    pub store: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub customer_id: Option<u32>,
    pub customer_email: Option<String>,
    pub customer_name: Option<String>,
    pub guest: bool,
    pub order_currency: Option<String>,
    pub base_currency: Option<String>,
    pub total_qty: Option<String>,
    pub coupon: Option<String>,
    pub applied_rule_ids: Option<String>,
    pub shipping_method: Option<String>,
    pub shipping_description: Option<String>,
    pub totals: Vec<OrderTotal>,
    pub items: Vec<OrderItem>,
    pub addresses: Vec<OrderAddress>,
    pub payment: Option<OrderPayment>,
    pub transactions: Vec<OrderTransaction>,
    pub invoices: Vec<OrderDocument>,
    pub shipments: Vec<OrderShipment>,
    pub creditmemos: Vec<OrderDocument>,
    pub history: Vec<OrderComment>,
    /// Present in `sales_order_grid` — `false` = the admin grid can't see this order
    /// (grid indexer behind).
    pub in_grid: bool,
    pub quote_id: Option<u64>,
    pub matched_by_id: bool,
}

/// One row of an order search.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OrderHit {
    pub entity_id: u32,
    pub increment_id: String,
    pub status: Option<String>,
    pub grand_total: Option<String>,
    pub currency: Option<String>,
    pub customer_email: Option<String>,
    pub created_at: Option<String>,
}
