//! Live database queries for the sales domain.

use super::*;

/// One sales document, raw.
pub(crate) struct DbSalesDocument {
    pub entity_id: u32,
    pub increment_id: String,
    pub state: Option<i64>,
    pub created_at: Option<String>,
    pub currency: Option<String>,
    /// `(key, amount, base)`.
    pub totals: Vec<(String, Option<String>, Option<String>)>,
    pub transaction_id: Option<String>,
    pub total_qty: Option<String>,
    pub order_increment: Option<String>,
    pub order_status: Option<String>,
    /// `(sku, name, qty, price, row_total)`.
    pub items: Vec<(String, Option<String>, String, Option<String>, Option<String>)>,
    pub tracks: Vec<(Option<String>, Option<String>, Option<String>)>,
}

pub(crate) fn fetch_sales_document(
    conn: &DbConnection,
    table_prefix: &str,
    kind: crate::model::SalesDocKind,
    increment: &str,
) -> Result<Option<DbSalesDocument>, String> {
    use crate::model::SalesDocKind as K;
    use mysql::params;
    use mysql::prelude::Queryable;

    let mut c = connect(conn)?;
    let p = table_prefix;
    let s = |r: &mut mysql::Row, i: usize| r.take::<Option<String>, _>(i).flatten();

    let head = match kind {
        K::Invoice => format!(
            "SELECT entity_id, order_id, state, CAST(created_at AS CHAR), \
             order_currency_code, transaction_id, NULL, \
             CAST(subtotal AS CHAR), CAST(base_subtotal AS CHAR), \
             CAST(tax_amount AS CHAR), CAST(base_tax_amount AS CHAR), \
             CAST(shipping_amount AS CHAR), CAST(base_shipping_amount AS CHAR), \
             CAST(grand_total AS CHAR), CAST(base_grand_total AS CHAR) \
             FROM {p}sales_invoice WHERE increment_id = :v"
        ),
        K::Creditmemo => format!(
            "SELECT entity_id, order_id, state, CAST(created_at AS CHAR), \
             order_currency_code, NULL, NULL, \
             CAST(subtotal AS CHAR), CAST(base_subtotal AS CHAR), \
             CAST(tax_amount AS CHAR), CAST(base_tax_amount AS CHAR), \
             CAST(shipping_amount AS CHAR), CAST(base_shipping_amount AS CHAR), \
             CAST(grand_total AS CHAR), CAST(base_grand_total AS CHAR), \
             CAST(adjustment_positive AS CHAR), CAST(adjustment_negative AS CHAR) \
             FROM {p}sales_creditmemo WHERE increment_id = :v"
        ),
        K::Shipment => format!(
            "SELECT entity_id, order_id, NULL, CAST(created_at AS CHAR), NULL, NULL, \
             CAST(total_qty AS CHAR) \
             FROM {p}sales_shipment WHERE increment_id = :v"
        ),
    };
    let row: Option<mysql::Row> =
        c.exec_first(head, params! { "v" => increment }).map_err(clean_err)?;
    let Some(mut row) = row else { return Ok(None) };

    let entity_id: u32 = row.take::<Option<u32>, _>(0).flatten().unwrap_or(0);
    let order_id: u32 = row.take::<Option<u32>, _>(1).flatten().unwrap_or(0);
    let state: Option<i64> = row.take::<Option<i64>, _>(2).flatten();
    let created_at = s(&mut row, 3);
    let currency = s(&mut row, 4);
    let transaction_id = s(&mut row, 5);
    let total_qty = s(&mut row, 6);
    let mut totals: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    if !matches!(kind, K::Shipment) {
        for (i, key) in ["subtotal", "tax", "shipping", "grand_total"].iter().enumerate() {
            totals.push((key.to_string(), s(&mut row, 7 + i * 2), s(&mut row, 8 + i * 2)));
        }
        if matches!(kind, K::Creditmemo) {
            totals.push(("adjustment_positive".to_string(), s(&mut row, 15), None));
            totals.push(("adjustment_negative".to_string(), s(&mut row, 16), None));
        }
    }

    let (order_increment, order_status): (Option<String>, Option<String>) = c
        .exec_first::<(String, Option<String>), _, _>(
            format!("SELECT increment_id, status FROM {p}sales_order WHERE entity_id = :v"),
            params! { "v" => order_id },
        )
        .map_err(clean_err)?
        .map(|(i, st)| (Some(i), st))
        .unwrap_or((None, None));

    let item_table = match kind {
        K::Invoice => "sales_invoice_item",
        K::Shipment => "sales_shipment_item",
        K::Creditmemo => "sales_creditmemo_item",
    };
    let price_cols = if matches!(kind, K::Shipment) {
        "NULL, NULL"
    } else {
        "CAST(price AS CHAR), CAST(row_total AS CHAR)"
    };
    let items: Vec<(String, Option<String>, Option<String>, Option<String>, Option<String>)> = c
        .exec(
            format!(
                "SELECT sku, name, CAST(qty AS CHAR), {price_cols} \
                 FROM {p}{item_table} WHERE parent_id = :v ORDER BY entity_id"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;
    let items = items
        .into_iter()
        .map(|(sku, name, qty, price, row_total)| {
            (sku, name, qty.unwrap_or_else(|| "0".to_string()), price, row_total)
        })
        .collect();

    let tracks: Vec<(Option<String>, Option<String>, Option<String>)> =
        if matches!(kind, K::Shipment) {
            c.exec(
                format!(
                    "SELECT carrier_code, title, track_number FROM {p}sales_shipment_track \
                     WHERE parent_id = :v ORDER BY entity_id"
                ),
                params! { "v" => entity_id },
            )
            .map_err(clean_err)?
        } else {
            Vec::new()
        };

    Ok(Some(DbSalesDocument {
        entity_id,
        increment_id: increment.to_string(),
        state,
        created_at,
        currency,
        totals,
        transaction_id,
        total_qty,
        order_increment,
        order_status,
        items,
        tracks,
    }))
}

/// Document search by increment substring, newest first.
#[allow(clippy::type_complexity)]
pub(crate) fn fetch_sales_documents_like(
    conn: &DbConnection,
    table_prefix: &str,
    kind: crate::model::SalesDocKind,
    needle: &str,
    limit: usize,
) -> Result<(Vec<(String, Option<String>, Option<String>, Option<String>)>, bool), String> {
    use crate::model::SalesDocKind as K;
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let (table, amount) = match kind {
        K::Invoice => ("sales_invoice", "CAST(d.grand_total AS CHAR)"),
        K::Shipment => ("sales_shipment", "CAST(d.total_qty AS CHAR)"),
        K::Creditmemo => ("sales_creditmemo", "CAST(d.grand_total AS CHAR)"),
    };
    let rows: Vec<(String, Option<String>, Option<String>, Option<String>)> = c
        .exec(
            format!(
                "SELECT d.increment_id, o.increment_id, CAST(d.created_at AS CHAR), {amount} \
                 FROM {p}{table} d LEFT JOIN {p}sales_order o ON o.entity_id = d.order_id \
                 WHERE d.increment_id LIKE :pat ORDER BY d.entity_id DESC LIMIT {}",
                limit + 1
            ),
            params! { "pat" => format!("%{needle}%") },
        )
        .map_err(clean_err)?;
    let truncated = rows.len() > limit;
    Ok((rows.into_iter().take(limit).collect(), truncated))
}

/// Everything about one quote (cart), raw. Quote tables carry no `sales_` prefix.
pub(crate) struct DbQuote {
    pub entity_id: u64,
    pub active: bool,
    pub store: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub updated_secs: Option<i64>,
    pub converted_at: Option<String>,
    pub customer_id: Option<u32>,
    pub customer_email: Option<String>,
    pub customer_firstname: Option<String>,
    pub customer_lastname: Option<String>,
    pub guest: bool,
    pub checkout_method: Option<String>,
    pub quote_currency: Option<String>,
    pub base_currency: Option<String>,
    pub items_qty: Option<String>,
    pub is_virtual: bool,
    pub coupon: Option<String>,
    pub applied_rule_ids: Option<String>,
    pub reserved_order_id: Option<String>,
    pub order_increment: Option<String>,
    /// `(subtotal, base_subtotal, grand_total, base_grand_total)` from the quote row.
    pub quote_totals: (Option<String>, Option<String>, Option<String>, Option<String>),
    /// From the shipping address: `(shipping, base_shipping, tax, base_tax, discount,
    /// base_discount)`.
    #[allow(clippy::type_complexity)]
    pub address_totals:
        (Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>),
    /// `(sku, name, type, is_child, qty, price, row_total, discount)`.
    #[allow(clippy::type_complexity)]
    pub items: Vec<(
        String,
        Option<String>,
        String,
        bool,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    )>,
    /// `(kind, firstname, lastname, company, street, postcode, city, country,
    /// shipping_method, shipping_description)`.
    #[allow(clippy::type_complexity)]
    pub addresses: Vec<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )>,
    /// `(method, additional_information JSON)`.
    pub payment: Option<(Option<String>, Option<String>)>,
}

pub(crate) fn fetch_quote(
    conn: &DbConnection,
    table_prefix: &str,
    id: u64,
) -> Result<Option<DbQuote>, String> {
    use mysql::params;
    use mysql::prelude::Queryable;

    let mut c = connect(conn)?;
    let p = table_prefix;

    let row: Option<mysql::Row> = c
        .exec_first(
            format!(
                "SELECT entity_id, is_active, store_id, CAST(created_at AS CHAR), \
                 CAST(updated_at AS CHAR), TIMESTAMPDIFF(SECOND, updated_at, NOW()), \
                 CAST(converted_at AS CHAR), customer_id, customer_email, \
                 customer_firstname, customer_lastname, customer_is_guest, \
                 checkout_method, quote_currency_code, base_currency_code, \
                 CAST(items_qty AS CHAR), is_virtual, coupon_code, applied_rule_ids, \
                 reserved_order_id, CAST(subtotal AS CHAR), CAST(base_subtotal AS CHAR), \
                 CAST(grand_total AS CHAR), CAST(base_grand_total AS CHAR) \
                 FROM {p}quote WHERE entity_id = :v"
            ),
            params! { "v" => id },
        )
        .map_err(clean_err)?;
    let Some(mut row) = row else { return Ok(None) };
    let s = |r: &mut mysql::Row, i: usize| r.take::<Option<String>, _>(i).flatten();
    let n = |r: &mut mysql::Row, i: usize| r.take::<Option<i64>, _>(i).flatten().unwrap_or(0);
    let entity_id = n(&mut row, 0) as u64;
    let active = n(&mut row, 1) != 0;
    let store_id: Option<u32> = row.take::<Option<u32>, _>(2).flatten();
    let created_at = s(&mut row, 3);
    let updated_at = s(&mut row, 4);
    let updated_secs = row.take::<Option<i64>, _>(5).flatten();
    let converted_at = s(&mut row, 6);
    let customer_id: Option<u32> = row.take::<Option<u32>, _>(7).flatten();
    let customer_email = s(&mut row, 8);
    let customer_firstname = s(&mut row, 9);
    let customer_lastname = s(&mut row, 10);
    let guest = n(&mut row, 11) != 0;
    let checkout_method = s(&mut row, 12);
    let quote_currency = s(&mut row, 13);
    let base_currency = s(&mut row, 14);
    let items_qty = s(&mut row, 15);
    let is_virtual = n(&mut row, 16) != 0;
    let coupon = s(&mut row, 17);
    let applied_rule_ids = s(&mut row, 18);
    let reserved_order_id = s(&mut row, 19);
    let quote_totals = (s(&mut row, 20), s(&mut row, 21), s(&mut row, 22), s(&mut row, 23));

    let store: Option<String> = match store_id {
        Some(sid) => c
            .exec_first(
                format!("SELECT code FROM {p}store WHERE store_id = :v"),
                params! { "v" => sid },
            )
            .map_err(clean_err)?,
        None => None,
    };
    let order_increment: Option<String> = c
        .exec_first(
            format!("SELECT increment_id FROM {p}sales_order WHERE quote_id = :v"),
            params! { "v" => entity_id },
        )
        .ok()
        .flatten();

    type QItemRow =
        (String, Option<String>, String, Option<u64>, Option<String>, Option<String>, Option<String>, Option<String>);
    let items: Vec<QItemRow> = c
        .exec(
            format!(
                "SELECT sku, name, product_type, parent_item_id, CAST(qty AS CHAR), \
                 CAST(price AS CHAR), CAST(row_total AS CHAR), \
                 CAST(discount_amount AS CHAR) \
                 FROM {p}quote_item WHERE quote_id = :v ORDER BY item_id"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;
    let items = items
        .into_iter()
        .map(|(sku, name, ty, parent, qty, price, row_total, discount)| {
            (
                sku,
                name,
                ty,
                parent.is_some(),
                qty.unwrap_or_else(|| "0".to_string()),
                price,
                row_total,
                discount,
            )
        })
        .collect();

    type QAddrRow = mysql::Row;
    let addr_rows: Vec<QAddrRow> = c
        .exec(
            format!(
                "SELECT address_type, firstname, lastname, company, street, postcode, \
                 city, country_id, shipping_method, shipping_description, \
                 CAST(shipping_amount AS CHAR), CAST(base_shipping_amount AS CHAR), \
                 CAST(tax_amount AS CHAR), CAST(base_tax_amount AS CHAR), \
                 CAST(discount_amount AS CHAR), CAST(base_discount_amount AS CHAR) \
                 FROM {p}quote_address WHERE quote_id = :v ORDER BY address_type"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;
    let mut addresses = Vec::new();
    let mut address_totals = (None, None, None, None, None, None);
    for mut r in addr_rows {
        let kind = s(&mut r, 0).unwrap_or_default();
        if kind == "shipping" {
            address_totals = (
                s(&mut r, 10),
                s(&mut r, 11),
                s(&mut r, 12),
                s(&mut r, 13),
                s(&mut r, 14),
                s(&mut r, 15),
            );
        }
        addresses.push((
            kind,
            s(&mut r, 1),
            s(&mut r, 2),
            s(&mut r, 3),
            s(&mut r, 4),
            s(&mut r, 5),
            s(&mut r, 6),
            s(&mut r, 7),
            s(&mut r, 8),
            s(&mut r, 9),
        ));
    }

    let payment: Option<(Option<String>, Option<String>)> = c
        .exec_first(
            format!(
                "SELECT method, additional_information FROM {p}quote_payment \
                 WHERE quote_id = :v"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;

    Ok(Some(DbQuote {
        entity_id,
        active,
        store,
        created_at,
        updated_at,
        updated_secs,
        converted_at,
        customer_id,
        customer_email,
        customer_firstname,
        customer_lastname,
        guest,
        checkout_method,
        quote_currency,
        base_currency,
        items_qty,
        is_virtual,
        coupon,
        applied_rule_ids,
        reserved_order_id,
        order_increment,
        quote_totals,
        address_totals,
        items,
        addresses,
        payment,
    }))
}

/// Quote search by customer email substring, newest first.
#[allow(clippy::type_complexity)]
pub(crate) fn fetch_quotes_like(
    conn: &DbConnection,
    table_prefix: &str,
    needle: &str,
    limit: usize,
) -> Result<
    (Vec<(u64, bool, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)>, bool),
    String,
> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let rows: Vec<(u64, i64, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)> =
        c.exec(
            format!(
                "SELECT entity_id, is_active, customer_email, CAST(items_qty AS CHAR), \
                 CAST(grand_total AS CHAR), quote_currency_code, CAST(updated_at AS CHAR) \
                 FROM {p}quote WHERE customer_email LIKE :pat \
                 ORDER BY entity_id DESC LIMIT {}",
                limit + 1
            ),
            params! { "pat" => format!("%{needle}%") },
        )
        .map_err(clean_err)?;
    let truncated = rows.len() > limit;
    Ok((
        rows.into_iter()
            .take(limit)
            .map(|(id, a, e, q, g, cur, u)| (id, a != 0, e, q, g, cur, u))
            .collect(),
        truncated,
    ))
}

/// How to look a customer up.
pub(crate) enum CustomerIdent<'a> {
    Email(&'a str),
    Id(u32),
}

/// Everything about one customer, raw.
pub(crate) struct DbCustomer {
    pub entity_id: u32,
    pub email: String,
    pub firstname: Option<String>,
    pub lastname: Option<String>,
    pub group: Option<String>,
    pub website: Option<String>,
    pub created_in: Option<String>,
    pub created_at: Option<String>,
    pub active: bool,
    pub confirmed: bool,
    pub locked: bool,
    pub lock_expires: Option<String>,
    pub failures: u32,
    pub dob: Option<String>,
    pub taxvat: Option<String>,
    pub last_login: Option<String>,
    pub last_logout: Option<String>,
    /// `(id, firstname, lastname, company, street, postcode, city, region, country,
    /// telephone, is_default_billing, is_default_shipping)`.
    #[allow(clippy::type_complexity)]
    pub addresses: Vec<(
        u32,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        bool,
        bool,
    )>,
    /// `(store code, subscriber_status)`.
    pub newsletter: Vec<(String, i64)>,
    /// Custom EAV values (customer value tables have no store scope — store_id 0).
    pub values: Vec<DbProductValue>,
    /// `(count, lifetime base sum, first, last)`.
    pub order_stats: (u32, Option<String>, Option<String>, Option<String>),
    pub last_order: Option<(String, Option<String>)>,
    pub guest_orders: u32,
}

pub(crate) fn fetch_customer(
    conn: &DbConnection,
    table_prefix: &str,
    ident: CustomerIdent<'_>,
) -> Result<Option<DbCustomer>, String> {
    use mysql::params;
    use mysql::prelude::Queryable;

    let mut c = connect(conn)?;
    let p = table_prefix;

    let base = format!(
        "SELECT e.entity_id, e.email, e.firstname, e.lastname, g.customer_group_code, \
         w.code, e.created_in, CAST(e.created_at AS CHAR), e.is_active, e.confirmation, \
         (e.lock_expires IS NOT NULL AND e.lock_expires > NOW()), \
         CAST(e.lock_expires AS CHAR), e.failures_num, CAST(e.dob AS CHAR), e.taxvat, \
         e.default_billing, e.default_shipping \
         FROM {p}customer_entity e \
         LEFT JOIN {p}customer_group g ON g.customer_group_id = e.group_id \
         LEFT JOIN {p}store_website w ON w.website_id = e.website_id"
    );
    let row: Option<mysql::Row> = match ident {
        CustomerIdent::Email(email) => c
            .exec_first(format!("{base} WHERE e.email = :v"), params! { "v" => email })
            .map_err(clean_err)?,
        CustomerIdent::Id(id) => c
            .exec_first(format!("{base} WHERE e.entity_id = :v"), params! { "v" => id })
            .map_err(clean_err)?,
    };
    let Some(mut row) = row else { return Ok(None) };
    let s = |r: &mut mysql::Row, i: usize| r.take::<Option<String>, _>(i).flatten();
    let n = |r: &mut mysql::Row, i: usize| r.take::<Option<i64>, _>(i).flatten().unwrap_or(0);
    let entity_id = n(&mut row, 0) as u32;
    let email = s(&mut row, 1).unwrap_or_default();
    let firstname = s(&mut row, 2);
    let lastname = s(&mut row, 3);
    let group = s(&mut row, 4);
    let website = s(&mut row, 5);
    let created_in = s(&mut row, 6);
    let created_at = s(&mut row, 7);
    let active = n(&mut row, 8) != 0;
    let confirmed = s(&mut row, 9).is_none();
    let locked = n(&mut row, 10) != 0;
    let lock_expires = s(&mut row, 11);
    let failures = n(&mut row, 12) as u32;
    let dob = s(&mut row, 13);
    let taxvat = s(&mut row, 14);
    let default_billing: Option<u32> = row.take::<Option<u32>, _>(15).flatten();
    let default_shipping: Option<u32> = row.take::<Option<u32>, _>(16).flatten();

    type AddrRow =
        (u32, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);
    let addr_rows: Vec<AddrRow> = c
        .exec(
            format!(
                "SELECT entity_id, firstname, lastname, company, street, postcode, city, \
                 region, country_id, telephone FROM {p}customer_address_entity \
                 WHERE parent_id = :v ORDER BY entity_id"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;
    let addresses = addr_rows
        .into_iter()
        .map(|(id, f, l, co, st, pc, ci, re, cn, ph)| {
            (
                id,
                f,
                l,
                co,
                st,
                pc,
                ci,
                re,
                cn,
                ph,
                default_billing == Some(id),
                default_shipping == Some(id),
            )
        })
        .collect();

    let (last_login, last_logout) = c
        .exec_first::<(Option<String>, Option<String>), _, _>(
            format!(
                "SELECT CAST(last_login_at AS CHAR), CAST(last_logout_at AS CHAR) \
                 FROM {p}customer_log WHERE customer_id = :v"
            ),
            params! { "v" => entity_id },
        )
        .ok()
        .flatten()
        .unwrap_or((None, None));

    let stores: std::collections::HashMap<u32, String> =
        c.query(format!("SELECT store_id, code FROM {p}store")).map_err(clean_err)?
            .into_iter()
            .collect();
    let newsletter: Vec<(String, i64)> = c
        .exec(
            format!(
                "SELECT ns.store_id, ns.subscriber_status FROM {p}newsletter_subscriber ns \
                 WHERE ns.customer_id = :v OR ns.subscriber_email = :email \
                 ORDER BY ns.store_id"
            ),
            params! { "v" => entity_id, "email" => email.as_str() },
        )
        .map(|rows: Vec<(u32, i64)>| rows)
        .unwrap_or_default()
        .into_iter()
        .map(|(store_id, status)| {
            let code =
                stores.get(&store_id).cloned().unwrap_or_else(|| format!("store/{store_id}"));
            (code, status)
        })
        .collect();

    // Custom EAV values — customer value tables aren't store-scoped.
    let mut values: Vec<DbProductValue> = Vec::new();
    for table in ["varchar", "int", "decimal", "datetime", "text"] {
        let rows: Vec<(String, String, Option<String>, Option<String>, u32, Option<String>)> = c
            .exec(
                format!(
                    "SELECT a.attribute_code, a.backend_type, a.frontend_input, \
                     a.source_model, a.attribute_id, CAST(v.value AS CHAR) \
                     FROM {p}customer_entity_{table} v \
                     JOIN {p}eav_attribute a ON a.attribute_id = v.attribute_id \
                     WHERE v.entity_id = :v"
                ),
                params! { "v" => entity_id },
            )
            .unwrap_or_default();
        for (attribute, backend_type, input, source_model, attribute_id, value) in rows {
            values.push(DbProductValue {
                attribute,
                backend_type,
                input,
                source_model,
                attribute_id,
                store_id: 0,
                value,
            });
        }
    }

    let order_stats: (u32, Option<String>, Option<String>, Option<String>) = c
        .exec_first::<(u64, Option<String>, Option<String>, Option<String>), _, _>(
            format!(
                "SELECT COUNT(*), CAST(SUM(base_grand_total) AS CHAR), \
                 CAST(MIN(created_at) AS CHAR), CAST(MAX(created_at) AS CHAR) \
                 FROM {p}sales_order WHERE customer_id = :v"
            ),
            params! { "v" => entity_id },
        )
        .ok()
        .flatten()
        .map(|(n, sum, first, last)| (n as u32, sum, first, last))
        .unwrap_or((0, None, None, None));
    let last_order: Option<(String, Option<String>)> = c
        .exec_first(
            format!(
                "SELECT increment_id, status FROM {p}sales_order \
                 WHERE customer_id = :v ORDER BY entity_id DESC LIMIT 1"
            ),
            params! { "v" => entity_id },
        )
        .ok()
        .flatten();
    let guest_orders: u64 = c
        .exec_first(
            format!(
                "SELECT COUNT(*) FROM {p}sales_order \
                 WHERE customer_email = :email AND (customer_id IS NULL OR customer_id <> :v)"
            ),
            params! { "email" => email.as_str(), "v" => entity_id },
        )
        .ok()
        .flatten()
        .unwrap_or(0);

    Ok(Some(DbCustomer {
        entity_id,
        email,
        firstname,
        lastname,
        group,
        website,
        created_in,
        created_at,
        active,
        confirmed,
        locked,
        lock_expires,
        failures,
        dob,
        taxvat,
        last_login,
        last_logout,
        addresses,
        newsletter,
        values,
        order_stats,
        last_order,
        guest_orders: guest_orders as u32,
    }))
}

/// Customer search by email or name substring, newest first.
#[allow(clippy::type_complexity)]
pub(crate) fn fetch_customers_like(
    conn: &DbConnection,
    table_prefix: &str,
    needle: &str,
    limit: usize,
) -> Result<(Vec<(u32, String, Option<String>, Option<String>, Option<String>, Option<String>)>, bool), String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let rows: Vec<(u32, String, Option<String>, Option<String>, Option<String>, Option<String>)> = c
        .exec(
            format!(
                "SELECT e.entity_id, e.email, CONCAT_WS(' ', e.firstname, e.lastname), \
                 g.customer_group_code, CAST(e.created_at AS CHAR), NULL \
                 FROM {p}customer_entity e \
                 LEFT JOIN {p}customer_group g ON g.customer_group_id = e.group_id \
                 WHERE e.email LIKE :pat OR CONCAT_WS(' ', e.firstname, e.lastname) LIKE :pat \
                 ORDER BY e.entity_id DESC LIMIT {}",
                limit + 1
            ),
            params! { "pat" => format!("%{needle}%") },
        )
        .map_err(clean_err)?;
    let truncated = rows.len() > limit;
    Ok((rows.into_iter().take(limit).collect(), truncated))
}

/// How to look an order up.
pub(crate) enum OrderIdent<'a> {
    Increment(&'a str),
    Id(u32),
}

/// Everything about one order, raw. Sales tables are flat — no EAV.
pub(crate) struct DbOrder {
    pub entity_id: u32,
    pub increment_id: String,
    pub state: Option<String>,
    pub status: Option<String>,
    pub status_label: Option<String>,
    pub store: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub customer_id: Option<u32>,
    pub customer_email: Option<String>,
    pub customer_firstname: Option<String>,
    pub customer_lastname: Option<String>,
    pub guest: bool,
    pub base_currency: Option<String>,
    pub order_currency: Option<String>,
    pub coupon: Option<String>,
    pub applied_rule_ids: Option<String>,
    pub shipping_method: Option<String>,
    pub shipping_description: Option<String>,
    pub total_qty: Option<String>,
    pub quote_id: Option<u64>,
    /// `(key, amount, base_amount)` in canonical order.
    pub totals: Vec<(String, Option<String>, Option<String>)>,
    /// `(sku, name, type, is_child, ordered, invoiced, shipped, refunded, canceled,
    /// price, row_total)`.
    #[allow(clippy::type_complexity)]
    pub items: Vec<(
        String,
        Option<String>,
        String,
        bool,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    )>,
    /// `(kind, firstname, lastname, company, street, postcode, city, country, phone)`.
    #[allow(clippy::type_complexity)]
    pub addresses: Vec<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )>,
    /// `(method, last_trans_id, additional_information JSON)`.
    pub payment: Option<(Option<String>, Option<String>, Option<String>)>,
    /// `(txn_id, txn_type, is_closed, created_at)`.
    pub transactions: Vec<(String, String, bool, Option<String>)>,
    /// `(increment_id, state code, grand_total, created_at)`.
    pub invoices: Vec<(String, Option<i64>, Option<String>, Option<String>)>,
    /// `(shipment entity_id, increment_id, total_qty, created_at)`.
    pub shipments: Vec<(u32, String, Option<String>, Option<String>)>,
    /// `(shipment entity_id, carrier, title, number)`.
    pub tracks: Vec<(u32, Option<String>, Option<String>, Option<String>)>,
    pub creditmemos: Vec<(String, Option<i64>, Option<String>, Option<String>)>,
    /// `(status, comment, created_at, notified)`.
    pub history: Vec<(Option<String>, Option<String>, Option<String>, bool)>,
    pub in_grid: bool,
}

pub(crate) fn fetch_order(
    conn: &DbConnection,
    table_prefix: &str,
    ident: OrderIdent<'_>,
) -> Result<Option<DbOrder>, String> {
    use mysql::params;
    use mysql::prelude::Queryable;

    let mut c = connect(conn)?;
    let p = table_prefix;

    let base = format!(
        "SELECT entity_id, increment_id, state, status, store_id, \
         CAST(created_at AS CHAR), CAST(updated_at AS CHAR), customer_id, customer_email, \
         customer_firstname, customer_lastname, customer_is_guest, base_currency_code, \
         order_currency_code, coupon_code, applied_rule_ids, shipping_method, \
         shipping_description, CAST(total_qty_ordered AS CHAR), quote_id \
         FROM {p}sales_order"
    );
    let row: Option<mysql::Row> = match ident {
        OrderIdent::Increment(inc) => c
            .exec_first(format!("{base} WHERE increment_id = :v"), params! { "v" => inc })
            .map_err(clean_err)?,
        OrderIdent::Id(id) => c
            .exec_first(format!("{base} WHERE entity_id = :v"), params! { "v" => id })
            .map_err(clean_err)?,
    };
    let Some(mut row) = row else { return Ok(None) };
    let s = |r: &mut mysql::Row, i: usize| r.take::<Option<String>, _>(i).flatten();
    let entity_id: u32 = row.take::<Option<u32>, _>(0).flatten().unwrap_or(0);
    let increment_id = s(&mut row, 1).unwrap_or_default();
    let state = s(&mut row, 2);
    let status = s(&mut row, 3);
    let store_id: Option<u32> = row.take::<Option<u32>, _>(4).flatten();
    let created_at = s(&mut row, 5);
    let updated_at = s(&mut row, 6);
    let customer_id: Option<u32> = row.take::<Option<u32>, _>(7).flatten();
    let customer_email = s(&mut row, 8);
    let customer_firstname = s(&mut row, 9);
    let customer_lastname = s(&mut row, 10);
    let guest = row.take::<Option<i64>, _>(11).flatten().unwrap_or(0) != 0;
    let base_currency = s(&mut row, 12);
    let order_currency = s(&mut row, 13);
    let coupon = s(&mut row, 14);
    let applied_rule_ids = s(&mut row, 15);
    let shipping_method = s(&mut row, 16);
    let shipping_description = s(&mut row, 17);
    let total_qty = s(&mut row, 18);
    let quote_id: Option<u64> = row.take::<Option<u64>, _>(19).flatten();

    let status_label: Option<String> = match &status {
        Some(st) => c
            .exec_first(
                format!("SELECT label FROM {p}sales_order_status WHERE status = :v"),
                params! { "v" => st.as_str() },
            )
            .ok()
            .flatten(),
        None => None,
    };
    let store: Option<String> = match store_id {
        Some(id) => c
            .exec_first(
                format!("SELECT code FROM {p}store WHERE store_id = :v"),
                params! { "v" => id },
            )
            .map_err(clean_err)?,
        None => None,
    };

    // Totals — 8 canonical pairs, (order currency, base currency) each.
    const TOTAL_KEYS: [&str; 8] =
        ["subtotal", "shipping", "tax", "discount", "grand_total", "paid", "refunded", "due"];
    let trow: Option<mysql::Row> = c
        .exec_first(
            format!(
                "SELECT CAST(subtotal AS CHAR), CAST(base_subtotal AS CHAR), \
                 CAST(shipping_amount AS CHAR), CAST(base_shipping_amount AS CHAR), \
                 CAST(tax_amount AS CHAR), CAST(base_tax_amount AS CHAR), \
                 CAST(discount_amount AS CHAR), CAST(base_discount_amount AS CHAR), \
                 CAST(grand_total AS CHAR), CAST(base_grand_total AS CHAR), \
                 CAST(total_paid AS CHAR), CAST(base_total_paid AS CHAR), \
                 CAST(total_refunded AS CHAR), CAST(base_total_refunded AS CHAR), \
                 CAST(total_due AS CHAR), CAST(base_total_due AS CHAR) \
                 FROM {p}sales_order WHERE entity_id = :v"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;
    let mut totals = Vec::with_capacity(8);
    if let Some(mut t) = trow {
        for (i, key) in TOTAL_KEYS.iter().enumerate() {
            totals.push((key.to_string(), s(&mut t, i * 2), s(&mut t, i * 2 + 1)));
        }
    }

    type ItemRow = mysql::Row;
    let item_rows: Vec<ItemRow> = c
        .exec(
            format!(
                "SELECT sku, name, product_type, parent_item_id, \
                 CAST(qty_ordered AS CHAR), CAST(qty_invoiced AS CHAR), \
                 CAST(qty_shipped AS CHAR), CAST(qty_refunded AS CHAR), \
                 CAST(qty_canceled AS CHAR), CAST(price AS CHAR), CAST(row_total AS CHAR) \
                 FROM {p}sales_order_item WHERE order_id = :v ORDER BY item_id"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;
    let items = item_rows
        .into_iter()
        .map(|mut r| {
            let q = |r: &mut mysql::Row, i: usize| {
                r.take::<Option<String>, _>(i).flatten().unwrap_or_else(|| "0".to_string())
            };
            (
                s(&mut r, 0).unwrap_or_default(),
                s(&mut r, 1),
                s(&mut r, 2).unwrap_or_default(),
                r.take::<Option<u64>, _>(3).flatten().is_some(),
                q(&mut r, 4),
                q(&mut r, 5),
                q(&mut r, 6),
                q(&mut r, 7),
                q(&mut r, 8),
                s(&mut r, 9),
                s(&mut r, 10),
            )
        })
        .collect();

    let addresses = c
        .exec(
            format!(
                "SELECT address_type, firstname, lastname, company, street, postcode, \
                 city, country_id, telephone FROM {p}sales_order_address \
                 WHERE parent_id = :v ORDER BY address_type"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;

    let payment: Option<(Option<String>, Option<String>, Option<String>)> = c
        .exec_first(
            format!(
                "SELECT method, last_trans_id, additional_information \
                 FROM {p}sales_order_payment WHERE parent_id = :v"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;

    let transactions: Vec<(String, String, bool, Option<String>)> = c
        .exec(
            format!(
                "SELECT txn_id, txn_type, is_closed, CAST(created_at AS CHAR) \
                 FROM {p}sales_payment_transaction WHERE order_id = :v ORDER BY transaction_id"
            ),
            params! { "v" => entity_id },
        )
        .map(|rows: Vec<(String, String, i64, Option<String>)>| {
            rows.into_iter().map(|(t, k, cl, d)| (t, k, cl != 0, d)).collect()
        })
        .unwrap_or_default();

    let invoices: Vec<(String, Option<i64>, Option<String>, Option<String>)> = c
        .exec(
            format!(
                "SELECT increment_id, state, CAST(grand_total AS CHAR), \
                 CAST(created_at AS CHAR) FROM {p}sales_invoice \
                 WHERE order_id = :v ORDER BY entity_id"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;
    let shipments: Vec<(u32, String, Option<String>, Option<String>)> = c
        .exec(
            format!(
                "SELECT entity_id, increment_id, CAST(total_qty AS CHAR), \
                 CAST(created_at AS CHAR) FROM {p}sales_shipment \
                 WHERE order_id = :v ORDER BY entity_id"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;
    let tracks: Vec<(u32, Option<String>, Option<String>, Option<String>)> = c
        .exec(
            format!(
                "SELECT parent_id, carrier_code, title, track_number \
                 FROM {p}sales_shipment_track WHERE order_id = :v ORDER BY entity_id"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;
    let creditmemos: Vec<(String, Option<i64>, Option<String>, Option<String>)> = c
        .exec(
            format!(
                "SELECT increment_id, state, CAST(grand_total AS CHAR), \
                 CAST(created_at AS CHAR) FROM {p}sales_creditmemo \
                 WHERE order_id = :v ORDER BY entity_id"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;

    let history: Vec<(Option<String>, Option<String>, Option<String>, bool)> = c
        .exec(
            format!(
                "SELECT status, comment, CAST(created_at AS CHAR), is_customer_notified \
                 FROM {p}sales_order_status_history WHERE parent_id = :v ORDER BY entity_id"
            ),
            params! { "v" => entity_id },
        )
        .map(
            |rows: Vec<(Option<String>, Option<String>, Option<String>, Option<i64>)>| {
                rows.into_iter()
                    .map(|(st, co, d, n)| (st, co, d, n.unwrap_or(0) != 0))
                    .collect()
            },
        )
        .unwrap_or_default();

    let in_grid: bool = c
        .exec_first::<u32, _, _>(
            format!("SELECT entity_id FROM {p}sales_order_grid WHERE entity_id = :v"),
            params! { "v" => entity_id },
        )
        .map(|r| r.is_some())
        .unwrap_or(false);

    Ok(Some(DbOrder {
        entity_id,
        increment_id,
        state,
        status,
        status_label,
        store,
        created_at,
        updated_at,
        customer_id,
        customer_email,
        customer_firstname,
        customer_lastname,
        guest,
        base_currency,
        order_currency,
        coupon,
        applied_rule_ids,
        shipping_method,
        shipping_description,
        total_qty,
        quote_id,
        totals,
        items,
        addresses,
        payment,
        transactions,
        invoices,
        shipments,
        tracks,
        creditmemos,
        history,
        in_grid,
    }))
}

/// Order search by increment_id, customer email, or **PSP transaction reference**
/// (`sales_order_payment.last_trans_id` / `sales_payment_transaction.txn_id` — the
/// workflow is "the PSP gives you a ref, find the order"), newest first;
/// `limit + 1` fetched to flag truncation.
#[allow(clippy::type_complexity)]
pub(crate) fn fetch_orders_like(
    conn: &DbConnection,
    table_prefix: &str,
    needle: &str,
    limit: usize,
) -> Result<
    (Vec<(u32, String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)>, bool),
    String,
> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let rows: Vec<(u32, String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)> =
        c.exec(
            format!(
                "SELECT DISTINCT o.entity_id, o.increment_id, o.status, \
                 CAST(o.grand_total AS CHAR), o.order_currency_code, o.customer_email, \
                 CAST(o.created_at AS CHAR) \
                 FROM {p}sales_order o \
                 LEFT JOIN {p}sales_order_payment pay ON pay.parent_id = o.entity_id \
                 LEFT JOIN {p}sales_payment_transaction t ON t.order_id = o.entity_id \
                 WHERE o.increment_id LIKE :pat OR o.customer_email LIKE :pat \
                 OR pay.last_trans_id LIKE :pat OR t.txn_id LIKE :pat \
                 ORDER BY o.entity_id DESC LIMIT {}",
                limit + 1
            ),
            params! { "pat" => format!("%{needle}%") },
        )
        .map_err(clean_err)?;
    let truncated = rows.len() > limit;
    Ok((rows.into_iter().take(limit).collect(), truncated))
}
