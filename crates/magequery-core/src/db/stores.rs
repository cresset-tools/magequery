//! Live database queries for the stores domain.

use super::*;

/// The scope tree, raw: websites, groups, views, root-category names, currency rates.
#[allow(clippy::type_complexity)]
pub(crate) struct DbStoreTree {
    /// `(id, code, name, is_default, default_group_id)`.
    pub websites: Vec<(u32, String, String, bool, u32)>,
    /// `(id, website_id, name, root_category_id, default_store_id)`.
    pub groups: Vec<(u32, u32, String, u32, u32)>,
    /// `(id, code, name, website_id, group_id, active)`.
    pub views: Vec<(u32, String, String, u32, u32, bool)>,
    /// Root category names (store 0).
    pub category_names: std::collections::HashMap<u32, String>,
    pub currency_rates: Vec<(String, String, String)>,
}

pub(crate) fn fetch_store_tree(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<DbStoreTree, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;

    let websites: Vec<(u32, String, String, i64, u32)> = c
        .query(format!(
            "SELECT website_id, code, name, is_default, default_group_id \
             FROM {p}store_website WHERE website_id > 0 ORDER BY sort_order, website_id"
        ))
        .map_err(clean_err)?;
    let groups: Vec<(u32, u32, String, u32, u32)> = c
        .query(format!(
            "SELECT group_id, website_id, name, root_category_id, default_store_id \
             FROM {p}store_group WHERE group_id > 0 ORDER BY group_id"
        ))
        .map_err(clean_err)?;
    let views: Vec<(u32, String, String, u32, u32, i64)> = c
        .query(format!(
            "SELECT store_id, code, name, website_id, group_id, is_active \
             FROM {p}store WHERE store_id > 0 ORDER BY sort_order, store_id"
        ))
        .map_err(clean_err)?;

    let root_ids: std::collections::BTreeSet<u32> =
        groups.iter().map(|(_, _, _, root, _)| *root).collect();
    let category_names: std::collections::HashMap<u32, String> = if root_ids.is_empty() {
        Default::default()
    } else {
        let ids = root_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        c.query(format!(
            "SELECT entity_id, value FROM {p}catalog_category_entity_varchar \
             WHERE store_id = 0 AND entity_id IN ({ids}) AND attribute_id = {}",
            cat_attr(p, "name")
        ))
        .map(|rows: Vec<(u32, Option<String>)>| {
            rows.into_iter().filter_map(|(i, v)| v.map(|v| (i, v))).collect()
        })
        .unwrap_or_default()
    };

    let currency_rates: Vec<(String, String, String)> = c
        .query(format!(
            "SELECT currency_from, currency_to, CAST(rate AS CHAR) \
             FROM {p}directory_currency_rate ORDER BY currency_from, currency_to"
        ))
        .unwrap_or_default();

    Ok(DbStoreTree {
        websites: websites
            .into_iter()
            .map(|(id, code, name, d, g)| (id, code, name, d != 0, g))
            .collect(),
        groups,
        views: views
            .into_iter()
            .map(|(id, code, name, w, g, a)| (id, code, name, w, g, a != 0))
            .collect(),
        category_names,
        currency_rates,
    })
}

/// Order statuses with their state mappings: `(status, label)` +
/// `(status, state, is_default, visible_on_front)`.
#[allow(clippy::type_complexity)]
pub(crate) fn fetch_order_statuses(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<(Vec<(String, String)>, Vec<(String, String, bool, bool)>), String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let statuses: Vec<(String, String)> = c
        .query(format!("SELECT status, label FROM {p}sales_order_status ORDER BY status"))
        .map_err(clean_err)?;
    let states: Vec<(String, String, i64, i64)> = c
        .query(format!(
            "SELECT status, state, is_default, visible_on_front \
             FROM {p}sales_order_status_state ORDER BY state, status"
        ))
        .map_err(clean_err)?;
    Ok((
        statuses,
        states.into_iter().map(|(st, s, d, v)| (st, s, d != 0, v != 0)).collect(),
    ))
}

/// Customer groups joined with their tax class, plus a member count from
/// `customer_entity`. `(group_id, code, tax_class_id, tax_class_name, members)`.
#[allow(clippy::type_complexity)]
pub(crate) fn fetch_customer_groups(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<(i64, String, u32, Option<String>, u64)>, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    c.query(format!(
        "SELECT g.customer_group_id, g.customer_group_code, g.tax_class_id, t.class_name, \
                (SELECT COUNT(*) FROM {p}customer_entity e \
                 WHERE e.group_id = g.customer_group_id) \
         FROM {p}customer_group g \
         LEFT JOIN {p}tax_class t ON t.class_id = g.tax_class_id \
         ORDER BY g.customer_group_id"
    ))
    .map_err(clean_err)
}

/// One row per (entity type × store) sequence: profile + the sequence table's high-water
/// mark. `(entity_type, store_id, sequence_table, prefix, suffix, step, active,
/// max_value, warning_value, current)`.
#[allow(clippy::type_complexity)]
pub(crate) fn fetch_sales_sequences(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<
    Vec<(String, String, Option<String>, Option<String>, u64, bool, Option<u64>, Option<u64>, Option<u64>)>,
    String,
> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    type MetaRow = (String, u32, String, Option<String>, Option<String>, Option<u64>, Option<i64>, Option<u64>, Option<u64>);
    let rows: Vec<MetaRow> = c
        .query(format!(
            "SELECT m.entity_type, m.store_id, m.sequence_table, pr.prefix, pr.suffix, \
             pr.step, pr.is_active, pr.max_value, pr.warning_value \
             FROM {p}sales_sequence_meta m \
             LEFT JOIN {p}sales_sequence_profile pr ON pr.meta_id = m.meta_id \
             ORDER BY m.entity_type, m.store_id"
        ))
        .map_err(clean_err)?;
    let stores: std::collections::HashMap<u32, String> =
        c.query(format!("SELECT store_id, code FROM {p}store")).map_err(clean_err)?
            .into_iter()
            .collect();
    let mut out = Vec::with_capacity(rows.len());
    for (entity_type, store_id, table, prefix, suffix, step, active, max, warn) in rows {
        // The sequence table name comes from the meta row — identifier-sanitized.
        let current: Option<u64> =
            if table.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                c.query_first(format!("SELECT MAX(sequence_value) FROM {p}{table}"))
                    .ok()
                    .flatten()
            } else {
                None
            };
        let store =
            stores.get(&store_id).cloned().unwrap_or_else(|| format!("store/{store_id}"));
        out.push((
            entity_type,
            store,
            prefix,
            suffix,
            step.unwrap_or(1),
            active.unwrap_or(1) != 0,
            max,
            warn,
            current,
        ));
    }
    Ok(out)
}
