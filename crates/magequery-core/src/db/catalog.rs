//! Live database queries for the catalog domain.

use super::*;

/// A helper for category attribute-id subqueries (`entity_type_code = catalog_category`).
pub(crate) fn cat_attr(p: &str, code: &str) -> String {
    format!(
        "(SELECT a.attribute_id FROM {p}eav_attribute a \
         JOIN {p}eav_entity_type t ON t.entity_type_id = a.entity_type_id \
         WHERE a.attribute_code = '{code}' AND t.entity_type_code = 'catalog_category')"
    )
}

/// One category with its tree fields and default-scope essentials — powers the tree view
/// and name/url_key search.
pub(crate) struct DbCategoryNode {
    pub id: u32,
    pub parent_id: u32,
    pub level: u32,
    pub position: u32,
    pub name: Option<String>,
    pub url_key: Option<String>,
    pub active: Option<bool>,
    pub in_menu: Option<bool>,
    pub anchor: Option<bool>,
    pub direct_products: u32,
}

/// Every category (excluding the global root, id 1) plus the store-group root map
/// `(root_category_id, group name)`.
pub(crate) fn fetch_category_nodes(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<(Vec<DbCategoryNode>, Vec<(u32, String)>), String> {
    use mysql::prelude::Queryable;
    use std::collections::HashMap;

    let mut c = connect(conn)?;
    let p = table_prefix;

    let rows: Vec<(u32, u32, u32, u32)> = c
        .query(format!(
            "SELECT entity_id, parent_id, level, position FROM {p}catalog_category_entity \
             WHERE entity_id <> 1"
        ))
        .map_err(clean_err)?;

    let mut strings: HashMap<(u32, &str), String> = HashMap::new();
    for code in ["name", "url_key"] {
        let vals: Vec<(u32, Option<String>)> = c
            .query(format!(
                "SELECT entity_id, value FROM {p}catalog_category_entity_varchar \
                 WHERE store_id = 0 AND attribute_id = {}",
                cat_attr(p, code)
            ))
            .map_err(clean_err)?;
        for (id, v) in vals {
            if let Some(v) = v {
                strings.insert((id, code), v);
            }
        }
    }
    let mut flags: HashMap<(u32, &str), bool> = HashMap::new();
    for code in ["is_active", "include_in_menu", "is_anchor"] {
        let vals: Vec<(u32, Option<i64>)> = c
            .query(format!(
                "SELECT entity_id, value FROM {p}catalog_category_entity_int \
                 WHERE store_id = 0 AND attribute_id = {}",
                cat_attr(p, code)
            ))
            .map_err(clean_err)?;
        for (id, v) in vals {
            if let Some(v) = v {
                flags.insert((id, code), v != 0);
            }
        }
    }
    let counts: HashMap<u32, u32> = c
        .query(format!(
            "SELECT category_id, COUNT(*) FROM {p}catalog_category_product GROUP BY category_id"
        ))
        .map(|rows: Vec<(u32, u64)>| rows.into_iter().map(|(i, n)| (i, n as u32)).collect())
        .unwrap_or_default();

    let nodes = rows
        .into_iter()
        .map(|(id, parent_id, level, position)| DbCategoryNode {
            id,
            parent_id,
            level,
            position,
            name: strings.get(&(id, "name")).cloned(),
            url_key: strings.get(&(id, "url_key")).cloned(),
            active: flags.get(&(id, "is_active")).copied(),
            in_menu: flags.get(&(id, "include_in_menu")).copied(),
            anchor: flags.get(&(id, "is_anchor")).copied(),
            direct_products: counts.get(&id).copied().unwrap_or(0),
        })
        .collect();

    let roots: Vec<(u32, String)> = c
        .query(format!(
            "SELECT root_category_id, name FROM {p}store_group WHERE group_id > 0"
        ))
        .map(|rows: Vec<(u32, String)>| rows)
        .unwrap_or_default();

    Ok((nodes, roots))
}

/// Everything the card needs for one category, raw.
pub(crate) struct DbCategoryCard {
    pub id: u32,
    pub path: String,
    pub level: u32,
    pub position: u32,
    pub parent_id: u32,
    pub children: u32,
    /// Per-scope attribute values (the fixed interesting set).
    pub values: Vec<DbProductValue>,
    pub stores: std::collections::HashMap<u32, String>,
    /// Ancestor path components (id, name) in path order (excluding the global root and
    /// the category itself).
    pub ancestors: Vec<(u32, String)>,
    /// `is_active` rows per entity on the path (incl. self): `(entity_id, store_id, value)`.
    pub active_rows: Vec<(u32, u32, Option<i64>)>,
    pub direct_products: u32,
    /// `(store code, indexed count)` per store view whose index table exists.
    pub indexed: Vec<(String, u32)>,
    /// `(request_path, store code, redirect_type)`.
    pub rewrites: Vec<(String, String, u16)>,
    pub root_of: Vec<String>,
    /// `(entity_id, sku, name, position)`, when requested.
    pub products: Vec<(u32, String, Option<String>, i64)>,
    /// The store code whose index was read; `None` = not requested.
    pub indexed_store: Option<String>,
    /// `(entity_id, sku, name, position, is_parent, visibility)`; `None` while
    /// `indexed_store` is set = the index table doesn't exist.
    #[allow(clippy::type_complexity)]
    pub indexed_products: Option<Vec<(u32, String, Option<String>, i64, bool, Option<i64>)>>,
}

pub(crate) fn fetch_category_card(
    conn: &DbConnection,
    table_prefix: &str,
    id: u32,
    include_products: bool,
    indexed_store: Option<Option<&str>>,
) -> Result<Option<DbCategoryCard>, String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    use std::collections::HashMap;

    let mut c = connect(conn)?;
    let p = table_prefix;

    let row: Option<(u32, String, u32, u32, u32, u32)> = c
        .exec_first(
            format!(
                "SELECT entity_id, path, level, position, parent_id, children_count \
                 FROM {p}catalog_category_entity WHERE entity_id = :v"
            ),
            params! { "v" => id },
        )
        .map_err(clean_err)?;
    let Some((id, path, level, position, parent_id, children)) = row else {
        return Ok(None);
    };

    let stores: HashMap<u32, String> =
        c.query(format!("SELECT store_id, code FROM {p}store")).map_err(clean_err)?
            .into_iter()
            .collect();

    const CAT_ATTRS: &str = "'name','is_active','include_in_menu','is_anchor','url_key',\
'url_path','display_mode','available_sort_by','default_sort_by','landing_page'";
    let mut values: Vec<DbProductValue> = Vec::new();
    for table in ["varchar", "int", "text", "decimal", "datetime"] {
        let rows: Vec<(String, String, Option<String>, Option<String>, u32, u32, Option<String>)> =
            c.exec(
                format!(
                    "SELECT a.attribute_code, a.backend_type, a.frontend_input, \
                     a.source_model, a.attribute_id, v.store_id, CAST(v.value AS CHAR) \
                     FROM {p}catalog_category_entity_{table} v \
                     JOIN {p}eav_attribute a ON a.attribute_id = v.attribute_id \
                     JOIN {p}eav_entity_type t ON t.entity_type_id = a.entity_type_id \
                     WHERE v.entity_id = :v AND t.entity_type_code = 'catalog_category' \
                     AND a.attribute_code IN ({CAT_ATTRS})"
                ),
                params! { "v" => id },
            )
            .unwrap_or_default();
        for (attribute, backend_type, input, source_model, attribute_id, store_id, value) in rows {
            values.push(DbProductValue {
                attribute,
                backend_type,
                input,
                source_model,
                attribute_id,
                store_id,
                value,
            });
        }
    }

    // Path components: names for the breadcrumb, is_active rows for the visibility walk.
    let path_ids: Vec<u32> = path.split('/').filter_map(|s| s.parse().ok()).collect();
    let ids_csv = path_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let names: HashMap<u32, String> = c
        .query(format!(
            "SELECT entity_id, value FROM {p}catalog_category_entity_varchar \
             WHERE store_id = 0 AND entity_id IN ({ids_csv}) AND attribute_id = {}",
            cat_attr(p, "name")
        ))
        .map(|rows: Vec<(u32, Option<String>)>| {
            rows.into_iter().filter_map(|(i, v)| v.map(|v| (i, v))).collect()
        })
        .unwrap_or_default();
    let ancestors: Vec<(u32, String)> = path_ids
        .iter()
        .filter(|&&c| c != 1 && c != id)
        .map(|&c| (c, names.get(&c).cloned().unwrap_or_else(|| format!("(category {c})"))))
        .collect();
    let active_rows: Vec<(u32, u32, Option<i64>)> = c
        .query(format!(
            "SELECT entity_id, store_id, value FROM {p}catalog_category_entity_int \
             WHERE entity_id IN ({ids_csv}) AND attribute_id = {}",
            cat_attr(p, "is_active")
        ))
        .map_err(clean_err)?;

    let direct_products: u64 = c
        .exec_first(
            format!("SELECT COUNT(*) FROM {p}catalog_category_product WHERE category_id = :v"),
            params! { "v" => id },
        )
        .map_err(clean_err)?
        .unwrap_or(0);

    // Per-store index counts — the per-store-view dimension tables are created by the
    // indexer (`..._index_store<store_id>`); probe tolerantly, absent = not indexed.
    let mut indexed: Vec<(String, u32)> = Vec::new();
    let mut store_ids: Vec<u32> = stores.keys().copied().filter(|&s| s > 0).collect();
    store_ids.sort();
    for store_id in store_ids {
        let count: Option<u64> = c
            .exec_first(
                format!(
                    "SELECT COUNT(*) FROM {p}catalog_category_product_index_store{store_id} \
                     WHERE category_id = :v AND store_id = {store_id}"
                ),
                params! { "v" => id },
            )
            .ok()
            .flatten();
        if let Some(n) = count {
            let code =
                stores.get(&store_id).cloned().unwrap_or_else(|| format!("store/{store_id}"));
            indexed.push((code, n as u32));
        }
    }

    let rewrites: Vec<(String, String, u16)> = c
        .exec(
            format!(
                "SELECT request_path, store_id, redirect_type FROM {p}url_rewrite \
                 WHERE entity_type = 'category' AND entity_id = :v \
                 ORDER BY store_id, request_path"
            ),
            params! { "v" => id },
        )
        .map_err(clean_err)?
        .into_iter()
        .map(|(path, store_id, redirect): (String, u32, u16)| {
            let store =
                stores.get(&store_id).cloned().unwrap_or_else(|| format!("store/{store_id}"));
            (path, store, redirect)
        })
        .collect();

    let root_of: Vec<String> = c
        .exec(
            format!(
                "SELECT name FROM {p}store_group WHERE root_category_id = :v AND group_id > 0"
            ),
            params! { "v" => id },
        )
        .unwrap_or_default();

    let products: Vec<(u32, String, Option<String>, i64)> = if include_products {
        c.exec(
            format!(
                "SELECT e.entity_id, e.sku, n.value, cp.position \
                 FROM {p}catalog_category_product cp \
                 JOIN {p}catalog_product_entity e ON e.entity_id = cp.product_id \
                 LEFT JOIN {p}catalog_product_entity_varchar n ON n.entity_id = e.entity_id \
                 AND n.store_id = 0 AND n.attribute_id = \
                 (SELECT a.attribute_id FROM {p}eav_attribute a \
                  JOIN {p}eav_entity_type t ON t.entity_type_id = a.entity_type_id \
                  WHERE a.attribute_code = 'name' AND t.entity_type_code = 'catalog_product') \
                 WHERE cp.category_id = :v ORDER BY cp.position, e.sku"
            ),
            params! { "v" => id },
        )
        .map_err(clean_err)?
    } else {
        Vec::new()
    };

    // `--indexed`: the actual product list from one store view's index table.
    let mut indexed_store_code: Option<String> = None;
    let mut indexed_products: Option<Vec<(u32, String, Option<String>, i64, bool, Option<i64>)>> =
        None;
    if let Some(store_choice) = indexed_store {
        let store_id = match store_choice {
            Some(code) => stores
                .iter()
                .find(|(_, c)| c.as_str() == code)
                .map(|(id, _)| *id)
                .ok_or_else(|| format!("no store view with code `{code}`"))?,
            None => *stores
                .keys()
                .filter(|&&s| s > 0)
                .min()
                .ok_or_else(|| "no store views exist".to_string())?,
        };
        indexed_store_code =
            Some(stores.get(&store_id).cloned().unwrap_or_else(|| format!("store/{store_id}")));
        indexed_products = c
            .exec(
                format!(
                    "SELECT i.product_id, e.sku, n.value, i.position, i.is_parent, i.visibility \
                     FROM {p}catalog_category_product_index_store{store_id} i \
                     JOIN {p}catalog_product_entity e ON e.entity_id = i.product_id \
                     LEFT JOIN {p}catalog_product_entity_varchar n ON n.entity_id = e.entity_id \
                     AND n.store_id = 0 AND n.attribute_id = \
                     (SELECT a.attribute_id FROM {p}eav_attribute a \
                      JOIN {p}eav_entity_type t ON t.entity_type_id = a.entity_type_id \
                      WHERE a.attribute_code = 'name' \
                      AND t.entity_type_code = 'catalog_product') \
                     WHERE i.category_id = :v AND i.store_id = {store_id} \
                     ORDER BY i.position, e.sku"
                ),
                params! { "v" => id },
            )
            .ok()
            .map(|rows: Vec<(u32, String, Option<String>, i64, i64, Option<i64>)>| {
                rows.into_iter()
                    .map(|(id, sku, name, pos, is_parent, vis)| {
                        (id, sku, name, pos, is_parent != 0, vis)
                    })
                    .collect()
            });
    }

    Ok(Some(DbCategoryCard {
        id,
        path,
        level,
        position,
        parent_id,
        children,
        values,
        stores,
        ancestors,
        active_rows,
        direct_products: direct_products as u32,
        indexed,
        rewrites,
        root_of,
        products,
        indexed_store: indexed_store_code,
        indexed_products,
    }))
}

/// How to look a product up.
pub(crate) enum ProductIdent<'a> {
    Sku(&'a str),
    Id(u32),
}

/// The light identity lookup: `(entity_id, sku, type_id)`.
pub(crate) fn fetch_product_identity(
    conn: &DbConnection,
    table_prefix: &str,
    ident: &ProductIdent<'_>,
) -> Result<Option<(u32, String, String)>, String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let base = format!("SELECT entity_id, sku, type_id FROM {p}catalog_product_entity");
    match ident {
        ProductIdent::Sku(sku) => {
            c.exec_first(format!("{base} WHERE sku = :v"), params! { "v" => *sku })
        }
        ProductIdent::Id(id) => {
            c.exec_first(format!("{base} WHERE entity_id = :v"), params! { "v" => *id })
        }
    }
    .map_err(clean_err)
}

/// One link target, raw: position + sku + the enrichment attributes (decoded later).
pub(crate) struct DbLinkTarget {
    pub position: i32,
    pub sku: String,
    pub name: Option<String>,
    pub status: Option<i64>,
    pub visibility: Option<i64>,
    pub in_stock: Option<bool>,
}

pub(crate) struct DbProductLinks {
    pub entity_id: u32,
    pub sku: String,
    pub type_id: String,
    pub name: Option<String>,
    pub reverse: bool,
    pub related: Vec<DbLinkTarget>,
    pub up_sells: Vec<DbLinkTarget>,
    pub cross_sells: Vec<DbLinkTarget>,
}

/// Related / up-sell / cross-sell links for one product (`catalog_product_link`
/// types 1/4/5), each target enriched with name/status/visibility/stock. `reverse`
/// flips the direction to "products that link *to* this one".
pub(crate) fn fetch_product_links(
    conn: &DbConnection,
    table_prefix: &str,
    ident: ProductIdent<'_>,
    reverse: bool,
) -> Result<Option<DbProductLinks>, String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    use std::collections::HashMap;
    let mut c = connect(conn)?;
    let p = table_prefix;

    let base = format!("SELECT entity_id, sku, type_id FROM {p}catalog_product_entity");
    let row: Option<(u32, String, String)> = match ident {
        ProductIdent::Sku(sku) => {
            c.exec_first(format!("{base} WHERE sku = :v"), params! { "v" => sku })
        }
        ProductIdent::Id(id) => {
            c.exec_first(format!("{base} WHERE entity_id = :v"), params! { "v" => id })
        }
    }
    .map_err(clean_err)?;
    let Some((entity_id, sku, type_id)) = row else { return Ok(None) };

    // The product attribute ids we enrich targets with (name/status/visibility).
    let attr_ids: HashMap<String, u32> = c
        .query(format!(
            "SELECT a.attribute_code, a.attribute_id FROM {p}eav_attribute a \
             JOIN {p}eav_entity_type t ON t.entity_type_id = a.entity_type_id \
             WHERE t.entity_type_code = 'catalog_product' \
             AND a.attribute_code IN ('name','status','visibility')"
        ))
        .map_err(clean_err)?
        .into_iter()
        .collect();

    // Each link type -> its (target_id, sku, position) rows, position-ordered.
    let (own, other) = if reverse {
        ("linked_product_id", "product_id")
    } else {
        ("product_id", "linked_product_id")
    };
    let mut per_type: Vec<Vec<(u32, String, i32)>> = Vec::new();
    for t in [1u32, 4, 5] {
        let rows: Vec<(u32, String, Option<i32>)> = c
            .exec(
                format!(
                    "SELECT e.entity_id, e.sku, i.value \
                     FROM {p}catalog_product_link l \
                     JOIN {p}catalog_product_entity e ON e.entity_id = l.{other} \
                     LEFT JOIN {p}catalog_product_link_attribute_int i \
                       ON i.link_id = l.link_id AND i.product_link_attribute_id = ( \
                          SELECT product_link_attribute_id FROM {p}catalog_product_link_attribute \
                          WHERE link_type_id = {t} AND product_link_attribute_code = 'position' \
                          LIMIT 1) \
                     WHERE l.{own} = :v AND l.link_type_id = {t} \
                     ORDER BY i.value, e.sku"
                ),
                params! { "v" => entity_id },
            )
            .map_err(clean_err)?;
        per_type.push(rows.into_iter().map(|(id, sku, pos)| (id, sku, pos.unwrap_or(0))).collect());
    }

    // Batch the enrichment over every target id (+ the base, for its name).
    let mut ids: Vec<u32> = per_type.iter().flatten().map(|(id, _, _)| *id).collect();
    ids.push(entity_id);
    ids.sort_unstable();
    ids.dedup();
    let id_list = ids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");

    let names: HashMap<u32, String> = match attr_ids.get("name") {
        Some(nid) => c
            .query(format!(
                "SELECT entity_id, value FROM {p}catalog_product_entity_varchar \
                 WHERE store_id = 0 AND attribute_id = {nid} AND entity_id IN ({id_list})"
            ))
            .map(|r: Vec<(u32, Option<String>)>| {
                r.into_iter().filter_map(|(i, v)| v.map(|v| (i, v))).collect()
            })
            .unwrap_or_default(),
        None => HashMap::new(),
    };
    let int_attr = |c: &mut mysql::Conn, code: &str| -> HashMap<u32, i64> {
        match attr_ids.get(code) {
            Some(aid) => c
                .query(format!(
                    "SELECT entity_id, value FROM {p}catalog_product_entity_int \
                     WHERE store_id = 0 AND attribute_id = {aid} AND entity_id IN ({id_list})"
                ))
                .map(|r: Vec<(u32, Option<i64>)>| {
                    r.into_iter().filter_map(|(i, v)| v.map(|v| (i, v))).collect()
                })
                .unwrap_or_default(),
            None => HashMap::new(),
        }
    };
    let statuses = int_attr(&mut c, "status");
    let visibilities = int_attr(&mut c, "visibility");
    let stock: HashMap<u32, bool> = c
        .query(format!(
            "SELECT product_id, is_in_stock FROM {p}cataloginventory_stock_item \
             WHERE product_id IN ({id_list})"
        ))
        .map(|r: Vec<(u32, i64)>| r.into_iter().map(|(i, s)| (i, s != 0)).collect())
        .unwrap_or_default();

    let build = |rows: Vec<(u32, String, i32)>| -> Vec<DbLinkTarget> {
        rows.into_iter()
            .map(|(id, sku, position)| DbLinkTarget {
                position,
                sku,
                name: names.get(&id).cloned(),
                status: statuses.get(&id).copied(),
                visibility: visibilities.get(&id).copied(),
                in_stock: stock.get(&id).copied(),
            })
            .collect()
    };
    let mut built: Vec<Vec<DbLinkTarget>> = per_type.into_iter().map(build).collect();
    let cross_sells = built.pop().unwrap_or_default();
    let up_sells = built.pop().unwrap_or_default();
    let related = built.pop().unwrap_or_default();

    Ok(Some(DbProductLinks {
        name: names.get(&entity_id).cloned(),
        entity_id,
        sku,
        type_id,
        reverse,
        related,
        up_sells,
        cross_sells,
    }))
}

/// Everything price-shaped for one product, raw.
pub(crate) struct DbProductPrices {
    pub entity_id: u32,
    pub sku: String,
    pub type_id: String,
    pub price_scope_website: bool,
    pub values: Vec<DbProductValue>,
    pub stores: std::collections::HashMap<u32, String>,
    /// `(website_id, all_groups, customer_group_id, qty, value, percentage)`.
    pub tiers: Vec<(u32, bool, u32, String, Option<String>, Option<String>)>,
    /// `(rule_date, customer_group_id, website_id, rule_price)`.
    pub rules: Vec<(String, u32, u32, String)>,
    /// `(customer_group_id, website_id, price, final, min, max, tier)`.
    #[allow(clippy::type_complexity)]
    pub index: Vec<(u32, u32, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)>,
    pub customer_groups: std::collections::HashMap<u32, String>,
    pub websites: std::collections::HashMap<u32, String>,
    /// Component prices (configurable variants / grouped associates / bundle selections).
    pub children: Vec<DbChildPrice>,
    /// Bundles: `fixed` / `dynamic` from the `price_type` attribute.
    pub bundle_price_type: Option<String>,
}

pub(crate) struct DbChildPrice {
    pub entity_id: u32,
    pub sku: String,
    pub enabled: Option<bool>,
    pub price: Option<String>,
    pub special: Option<String>,
    pub final_min: Option<String>,
    pub final_max: Option<String>,
    pub selection_price: Option<String>,
    pub selection_percent: bool,
}

/// Fetch the full price picture on one connection: EAV price attributes (decimal +
/// datetime value tables), tier prices, materialized catalog-rule prices, and the price
/// index. `catalogrule_product_price` / `catalog_product_index_price` queries tolerate
/// missing tables (never-reindexed or stripped installs) by returning empty.
pub(crate) fn fetch_product_prices(
    conn: &DbConnection,
    table_prefix: &str,
    ident: ProductIdent<'_>,
) -> Result<Option<DbProductPrices>, String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    use std::collections::HashMap;

    let Some((entity_id, sku, type_id)) = fetch_product_identity(conn, table_prefix, &ident)?
    else {
        return Ok(None);
    };
    let mut c = connect(conn)?;
    let p = table_prefix;

    // `catalog/price/scope` default-scope row; absent = 0 = global.
    let price_scope_website: bool = c
        .query_first::<Option<String>, _>(format!(
            "SELECT value FROM {p}core_config_data \
             WHERE path = 'catalog/price/scope' AND scope = 'default'"
        ))
        .ok()
        .flatten()
        .flatten()
        .map(|v| v.trim() == "1")
        .unwrap_or(false);

    let stores: HashMap<u32, String> =
        c.query(format!("SELECT store_id, code FROM {p}store")).map_err(clean_err)?
            .into_iter()
            .collect();
    let websites: HashMap<u32, String> = c
        .query(format!("SELECT website_id, code FROM {p}store_website"))
        .map_err(clean_err)?
        .into_iter()
        .collect();
    let customer_groups: HashMap<u32, String> = c
        .query(format!("SELECT customer_group_id, customer_group_code FROM {p}customer_group"))
        .map(|rows: Vec<(u32, String)>| rows.into_iter().collect())
        .unwrap_or_default();

    const PRICE_ATTRS: &str =
        "'price','special_price','special_from_date','special_to_date','cost','msrp','minimal_price'";
    let mut values: Vec<DbProductValue> = Vec::new();
    for table in ["decimal", "datetime"] {
        let rows: Vec<(String, String, Option<String>, Option<String>, u32, u32, Option<String>)> =
            c.exec(
                format!(
                    "SELECT a.attribute_code, a.backend_type, a.frontend_input, \
                     a.source_model, a.attribute_id, v.store_id, CAST(v.value AS CHAR) \
                     FROM {p}catalog_product_entity_{table} v \
                     JOIN {p}eav_attribute a ON a.attribute_id = v.attribute_id \
                     WHERE v.entity_id = :v AND a.attribute_code IN ({PRICE_ATTRS})"
                ),
                params! { "v" => entity_id },
            )
            .map_err(clean_err)?;
        for (attribute, backend_type, input, source_model, attribute_id, store_id, value) in rows {
            values.push(DbProductValue {
                attribute,
                backend_type,
                input,
                source_model,
                attribute_id,
                store_id,
                value,
            });
        }
    }

    let tiers: Vec<(u32, bool, u32, String, Option<String>, Option<String>)> = c
        .exec(
            format!(
                "SELECT website_id, all_groups, customer_group_id, CAST(qty AS CHAR), \
                 CAST(value AS CHAR), CAST(percentage_value AS CHAR) \
                 FROM {p}catalog_product_entity_tier_price WHERE entity_id = :v \
                 ORDER BY website_id, customer_group_id, qty"
            ),
            params! { "v" => entity_id },
        )
        .map(|rows: Vec<(u32, i64, u32, String, Option<String>, Option<String>)>| {
            rows.into_iter().map(|(w, a, g, q, v, pc)| (w, a != 0, g, q, v, pc)).collect()
        })
        .unwrap_or_default();

    let rules: Vec<(String, u32, u32, String)> = c
        .exec(
            format!(
                "SELECT CAST(rule_date AS CHAR), customer_group_id, website_id, \
                 CAST(rule_price AS CHAR) FROM {p}catalogrule_product_price \
                 WHERE product_id = :v ORDER BY rule_date, website_id, customer_group_id"
            ),
            params! { "v" => entity_id },
        )
        .unwrap_or_default();

    #[allow(clippy::type_complexity)]
    let index: Vec<(u32, u32, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)> = c
        .exec(
            format!(
                "SELECT customer_group_id, website_id, CAST(price AS CHAR), \
                 CAST(final_price AS CHAR), CAST(min_price AS CHAR), \
                 CAST(max_price AS CHAR), CAST(tier_price AS CHAR) \
                 FROM {p}catalog_product_index_price WHERE entity_id = :v \
                 ORDER BY website_id, customer_group_id"
            ),
            params! { "v" => entity_id },
        )
        .unwrap_or_default();

    // A composite product's storefront price derives from its components — summarize
    // each one's own default-scope price/special and index final range.
    let mut bundle_price_type: Option<String> = None;
    let component_rows: Vec<(u32, String, Option<String>, bool)> = match type_id.as_str() {
        "configurable" => c
            .exec(
                format!(
                    "SELECT e.entity_id, e.sku FROM {p}catalog_product_super_link l \
                     JOIN {p}catalog_product_entity e ON e.entity_id = l.product_id \
                     WHERE l.parent_id = :v ORDER BY e.sku"
                ),
                params! { "v" => entity_id },
            )
            .map_err(clean_err)?
            .into_iter()
            .map(|(id, sku): (u32, String)| (id, sku, None, false))
            .collect(),
        "grouped" => c
            .exec(
                format!(
                    "SELECT e.entity_id, e.sku FROM {p}catalog_product_link l \
                     JOIN {p}catalog_product_entity e ON e.entity_id = l.linked_product_id \
                     WHERE l.product_id = :v AND l.link_type_id = 3 ORDER BY e.sku"
                ),
                params! { "v" => entity_id },
            )
            .map_err(clean_err)?
            .into_iter()
            .map(|(id, sku): (u32, String)| (id, sku, None, false))
            .collect(),
        "bundle" => {
            let pt: Option<Option<i64>> = c
                .exec_first(
                    format!(
                        "SELECT value FROM {p}catalog_product_entity_int \
                         WHERE entity_id = :v AND store_id = 0 AND attribute_id = \
                         (SELECT a.attribute_id FROM {p}eav_attribute a \
                          JOIN {p}eav_entity_type t ON t.entity_type_id = a.entity_type_id \
                          WHERE a.attribute_code = 'price_type' \
                          AND t.entity_type_code = 'catalog_product')"
                    ),
                    params! { "v" => entity_id },
                )
                .unwrap_or(None);
            bundle_price_type = pt
                .flatten()
                .map(|v| if v == 1 { "fixed".to_string() } else { "dynamic".to_string() });
            c.exec(
                format!(
                    "SELECT e.entity_id, e.sku, s.selection_price_type, \
                     CAST(s.selection_price_value AS CHAR) \
                     FROM {p}catalog_product_bundle_selection s \
                     JOIN {p}catalog_product_entity e ON e.entity_id = s.product_id \
                     WHERE s.parent_product_id = :v ORDER BY s.position, e.sku"
                ),
                params! { "v" => entity_id },
            )
            .map_err(clean_err)?
            .into_iter()
            .map(|(id, sku, pt, pv): (u32, String, i64, Option<String>)| {
                (id, sku, pv, pt == 1)
            })
            .collect()
        }
        _ => Vec::new(),
    };
    let mut children = Vec::with_capacity(component_rows.len());
    if !component_rows.is_empty() {
        let ids: std::collections::BTreeSet<String> =
            component_rows.iter().map(|(id, ..)| id.to_string()).collect();
        let ids = ids.into_iter().collect::<Vec<_>>().join(",");
        let decimals: Vec<(u32, String, Option<String>)> = c
            .query(format!(
                "SELECT v.entity_id, a.attribute_code, CAST(v.value AS CHAR) \
                 FROM {p}catalog_product_entity_decimal v \
                 JOIN {p}eav_attribute a ON a.attribute_id = v.attribute_id \
                 WHERE v.entity_id IN ({ids}) AND v.store_id = 0 \
                 AND a.attribute_code IN ('price','special_price')"
            ))
            .map_err(clean_err)?;
        let statuses = fetch_child_statuses(&mut c, p, &ids)?;
        let finals: Vec<(u32, Option<String>, Option<String>)> = c
            .query(format!(
                "SELECT entity_id, CAST(MIN(final_price) AS CHAR), \
                 CAST(MAX(final_price) AS CHAR) FROM {p}catalog_product_index_price \
                 WHERE entity_id IN ({ids}) GROUP BY entity_id"
            ))
            .unwrap_or_default();
        for (id, child_sku, selection_price, selection_percent) in component_rows {
            let value_of = |code: &str| {
                decimals
                    .iter()
                    .find(|(e, c, _)| *e == id && c == code)
                    .and_then(|(_, _, v)| v.clone())
            };
            let enabled =
                statuses.iter().find(|(e, _)| *e == id).and_then(|(_, s)| *s).map(|s| s == 1);
            let (final_min, final_max) = finals
                .iter()
                .find(|(e, _, _)| *e == id)
                .map(|(_, min, max)| (min.clone(), max.clone()))
                .unwrap_or((None, None));
            children.push(DbChildPrice {
                entity_id: id,
                sku: child_sku,
                enabled,
                price: value_of("price"),
                special: value_of("special_price"),
                final_min,
                final_max,
                selection_price,
                selection_percent,
            });
        }
    }

    Ok(Some(DbProductPrices {
        entity_id,
        sku,
        type_id,
        price_scope_website,
        values,
        stores,
        tiers,
        rules,
        index,
        customer_groups,
        websites,
        children,
        bundle_price_type,
    }))
}

/// One `(attribute, store)` value row, with the attribute metadata needed to render it.
pub(crate) struct DbProductValue {
    pub attribute: String,
    pub backend_type: String,
    pub input: Option<String>,
    pub source_model: Option<String>,
    pub attribute_id: u32,
    pub store_id: u32,
    pub value: Option<String>,
}

/// Everything the database stores about one product, raw.
pub(crate) struct DbProduct {
    pub entity_id: u32,
    pub sku: String,
    pub type_id: String,
    pub attribute_set: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub websites: Vec<String>,
    pub values: Vec<DbProductValue>,
    /// Admin-scope option labels: `(attribute_id, option_id) -> label`.
    pub option_labels: std::collections::HashMap<(u32, u32), String>,
    /// `tax_class` rows for resolving `tax_class_id`.
    pub tax_classes: std::collections::HashMap<u32, String>,
    /// `store_id -> code` (0 stays "default" at assembly).
    pub stores: std::collections::HashMap<u32, String>,
    /// MSI `(source_code, quantity, in_stock)`; empty when the tables don't exist.
    pub stock: Vec<(String, String, bool)>,
    /// Legacy `(qty, is_in_stock, manage_stock)`.
    pub legacy_stock: Option<(String, bool, bool)>,
    /// `(category_id, breadcrumb)`.
    pub categories: Vec<(u32, String)>,
    /// `(request_path, store code, redirect_type)`.
    pub rewrites: Vec<(String, String, u16)>,
    /// Media gallery: `(file, media_type, store-0 label, position, disabled)`.
    pub media: Vec<(String, String, Option<String>, i32, bool)>,
    pub parents: Vec<String>,
    /// Attribute codes a configurable is configured by, in position order.
    pub super_attributes: Vec<String>,
    /// Variants/associates:
    /// `(entity_id, sku, enabled, option labels, stock qty, in_stock, default qty)`.
    #[allow(clippy::type_complexity)]
    pub children:
        Vec<(u32, String, Option<bool>, Vec<String>, Option<String>, Option<bool>, Option<String>)>,
    pub bundle_options: Vec<DbBundleOption>,
}

pub(crate) struct DbBundleOption {
    pub title: String,
    pub required: bool,
    pub input_type: String,
    /// `(entity_id, sku, enabled, qty, is_default, price, price_percent, in_stock)`.
    #[allow(clippy::type_complexity)]
    pub selections:
        Vec<(u32, String, Option<bool>, String, bool, Option<String>, bool, Option<bool>)>,
}

/// Default-scope `status` per entity (batched over an id list).
fn fetch_child_statuses(
    c: &mut mysql::Conn,
    p: &str,
    ids: &str,
) -> Result<Vec<(u32, Option<i64>)>, String> {
    use mysql::prelude::Queryable;
    c.query(format!(
        "SELECT entity_id, value FROM {p}catalog_product_entity_int \
         WHERE entity_id IN ({ids}) AND store_id = 0 AND attribute_id = \
         (SELECT a.attribute_id FROM {p}eav_attribute a \
          JOIN {p}eav_entity_type t ON t.entity_type_id = a.entity_type_id \
          WHERE a.attribute_code = 'status' AND t.entity_type_code = 'catalog_product')"
    ))
    .map_err(clean_err)
}

/// Legacy stock `(product_id, qty, is_in_stock)` per entity (batched).
fn fetch_child_stock(
    c: &mut mysql::Conn,
    p: &str,
    ids: &str,
) -> Result<Vec<(u32, Option<String>, i64)>, String> {
    use mysql::prelude::Queryable;
    c.query(format!(
        "SELECT product_id, CAST(qty AS CHAR), is_in_stock \
         FROM {p}cataloginventory_stock_item WHERE product_id IN ({ids})"
    ))
    .map_err(clean_err)
}

/// Fetch one product wholesale (identity + per-scope EAV values + option labels + stock
/// + categories + rewrites + configurable links) on a single connection. OSS schema
/// (`entity_id` keys; Adobe Commerce's `row_id` staging is out of scope).
pub(crate) fn fetch_product(
    conn: &DbConnection,
    table_prefix: &str,
    ident: ProductIdent<'_>,
) -> Result<Option<DbProduct>, String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    use std::collections::HashMap;

    let mut c = connect(conn)?;
    let p = table_prefix;

    type EntityRow = (u32, String, String, u32, Option<String>, Option<String>);
    let base = format!(
        "SELECT entity_id, sku, type_id, attribute_set_id, CAST(created_at AS CHAR), \
         CAST(updated_at AS CHAR) FROM {p}catalog_product_entity"
    );
    let row: Option<EntityRow> = match ident {
        ProductIdent::Sku(sku) => c
            .exec_first(format!("{base} WHERE sku = :v"), params! { "v" => sku })
            .map_err(clean_err)?,
        ProductIdent::Id(id) => c
            .exec_first(format!("{base} WHERE entity_id = :v"), params! { "v" => id })
            .map_err(clean_err)?,
    };
    let Some((entity_id, sku, type_id, set_id, created_at, updated_at)) = row else {
        return Ok(None);
    };

    let attribute_set: Option<String> = c
        .exec_first(
            format!("SELECT attribute_set_name FROM {p}eav_attribute_set WHERE attribute_set_id = :v"),
            params! { "v" => set_id },
        )
        .map_err(clean_err)?;

    let stores: HashMap<u32, String> =
        c.query(format!("SELECT store_id, code FROM {p}store")).map_err(clean_err)?
            .into_iter()
            .collect();

    let websites: Vec<String> = c
        .exec(
            format!(
                "SELECT w.code FROM {p}catalog_product_website pw \
                 JOIN {p}store_website w ON w.website_id = pw.website_id \
                 WHERE pw.product_id = :v ORDER BY w.code"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;

    let mut values: Vec<DbProductValue> = Vec::new();
    for table in ["varchar", "int", "decimal", "text", "datetime"] {
        let rows: Vec<(String, String, Option<String>, Option<String>, u32, u32, Option<String>)> =
            c.exec(
                format!(
                    "SELECT a.attribute_code, a.backend_type, a.frontend_input, \
                     a.source_model, a.attribute_id, v.store_id, CAST(v.value AS CHAR) \
                     FROM {p}catalog_product_entity_{table} v \
                     JOIN {p}eav_attribute a ON a.attribute_id = v.attribute_id \
                     WHERE v.entity_id = :v"
                ),
                params! { "v" => entity_id },
            )
            .map_err(clean_err)?;
        for (attribute, backend_type, input, source_model, attribute_id, store_id, value) in rows {
            values.push(DbProductValue {
                attribute,
                backend_type,
                input,
                source_model,
                attribute_id,
                store_id,
                value,
            });
        }
    }

    // Admin labels for the select/multiselect attributes that appear on this product.
    let option_attr_ids: Vec<String> = values
        .iter()
        .filter(|v| matches!(v.input.as_deref(), Some("select" | "multiselect")))
        .map(|v| v.attribute_id.to_string())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut option_labels: HashMap<(u32, u32), String> = HashMap::new();
    if !option_attr_ids.is_empty() {
        let rows: Vec<(u32, u32, String)> = c
            .query(format!(
                "SELECT o.attribute_id, o.option_id, v.value FROM {p}eav_attribute_option o \
                 JOIN {p}eav_attribute_option_value v \
                 ON v.option_id = o.option_id AND v.store_id = 0 \
                 WHERE o.attribute_id IN ({})",
                option_attr_ids.join(",")
            ))
            .map_err(clean_err)?;
        option_labels = rows.into_iter().map(|(a, o, l)| ((a, o), l)).collect();
    }

    let tax_classes: HashMap<u32, String> = c
        .query(format!("SELECT class_id, class_name FROM {p}tax_class"))
        .map(|rows: Vec<(u32, String)>| rows.into_iter().collect())
        .unwrap_or_default();

    // MSI is keyed by SKU; the tables may be absent (modules removed) — degrade quietly.
    let stock: Vec<(String, String, bool)> = c
        .exec(
            format!(
                "SELECT source_code, CAST(quantity AS CHAR), status \
                 FROM {p}inventory_source_item WHERE sku = :v ORDER BY source_code"
            ),
            params! { "v" => sku.as_str() },
        )
        .map(|rows: Vec<(String, String, i64)>| {
            rows.into_iter().map(|(s, q, st)| (s, q, st != 0)).collect()
        })
        .unwrap_or_default();

    let legacy_stock: Option<(String, bool, bool)> = c
        .exec_first(
            format!(
                "SELECT CAST(qty AS CHAR), is_in_stock, manage_stock \
                 FROM {p}cataloginventory_stock_item WHERE product_id = :v"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?
        .map(|(qty, in_stock, manage): (Option<String>, i64, i64)| {
            (qty.unwrap_or_else(|| "0".to_string()), in_stock != 0, manage != 0)
        });

    // Categories with admin-style breadcrumbs: names of every path component past the
    // two roots (global root + the store group's tree root).
    let cat_rows: Vec<(u32, String)> = c
        .exec(
            format!(
                "SELECT cp.category_id, ce.path FROM {p}catalog_category_product cp \
                 JOIN {p}catalog_category_entity ce ON ce.entity_id = cp.category_id \
                 WHERE cp.product_id = :v ORDER BY cp.category_id"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;
    let mut categories: Vec<(u32, String)> = Vec::new();
    if !cat_rows.is_empty() {
        let path_ids: std::collections::BTreeSet<u32> = cat_rows
            .iter()
            .flat_map(|(_, path)| path.split('/').filter_map(|s| s.parse::<u32>().ok()))
            .collect();
        let ids: Vec<String> = path_ids.iter().map(|i| i.to_string()).collect();
        let names: HashMap<u32, String> = c
            .query(format!(
                "SELECT v.entity_id, v.value FROM {p}catalog_category_entity_varchar v \
                 JOIN {p}eav_attribute a ON a.attribute_id = v.attribute_id \
                 AND a.attribute_code = 'name' \
                 JOIN {p}eav_entity_type t ON t.entity_type_id = a.entity_type_id \
                 AND t.entity_type_code = 'catalog_category' \
                 WHERE v.store_id = 0 AND v.entity_id IN ({})",
                ids.join(",")
            ))
            .map_err(clean_err)?
            .into_iter()
            .collect();
        for (id, path) in cat_rows {
            let components: Vec<u32> =
                path.split('/').filter_map(|s| s.parse().ok()).collect();
            let named: Vec<&str> = components
                .iter()
                .skip(2)
                .filter_map(|c| names.get(c).map(String::as_str))
                .collect();
            let breadcrumb = if named.is_empty() {
                names.get(&id).cloned().unwrap_or_else(|| format!("(category {id})"))
            } else {
                named.join(" > ")
            };
            categories.push((id, breadcrumb));
        }
    }

    let rewrites: Vec<(String, String, u16)> = c
        .exec(
            format!(
                "SELECT request_path, store_id, redirect_type FROM {p}url_rewrite \
                 WHERE entity_type = 'product' AND entity_id = :v \
                 ORDER BY store_id, request_path"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?
        .into_iter()
        .map(|(path, store_id, redirect): (String, u32, u16)| {
            let store =
                stores.get(&store_id).cloned().unwrap_or_else(|| format!("store/{store_id}"));
            (path, store, redirect)
        })
        .collect();

    let parents: Vec<String> = c
        .exec(
            format!(
                "SELECT e.sku FROM {p}catalog_product_super_link l \
                 JOIN {p}catalog_product_entity e ON e.entity_id = l.parent_id \
                 WHERE l.product_id = :v ORDER BY e.sku"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;
    let super_attrs: Vec<(u32, String)> = c
        .exec(
            format!(
                "SELECT sa.attribute_id, a.attribute_code \
                 FROM {p}catalog_product_super_attribute sa \
                 JOIN {p}eav_attribute a ON a.attribute_id = sa.attribute_id \
                 WHERE sa.product_id = :v ORDER BY sa.position, a.attribute_code"
            ),
            params! { "v" => entity_id },
        )
        .unwrap_or_default();
    let super_attributes: Vec<String> = super_attrs.iter().map(|(_, c)| c.clone()).collect();

    // Component children: configurable variants (super_link) or grouped associates
    // (catalog_product_link type 3, with the default add-to-cart qty link attribute).
    let mut default_qtys: HashMap<u32, String> = HashMap::new();
    let child_rows: Vec<(u32, String)> = if type_id == "grouped" {
        let links: Vec<(u32, u32, String)> = c
            .exec(
                format!(
                    "SELECT l.link_id, e.entity_id, e.sku FROM {p}catalog_product_link l \
                     JOIN {p}catalog_product_entity e ON e.entity_id = l.linked_product_id \
                     WHERE l.product_id = :v AND l.link_type_id = 3 ORDER BY e.sku"
                ),
                params! { "v" => entity_id },
            )
            .map_err(clean_err)?;
        if !links.is_empty() {
            let link_ids =
                links.iter().map(|(l, _, _)| l.to_string()).collect::<Vec<_>>().join(",");
            let qtys: HashMap<u32, String> = c
                .query(format!(
                    "SELECT d.link_id, CAST(d.value AS CHAR) \
                     FROM {p}catalog_product_link_attribute_decimal d \
                     JOIN {p}catalog_product_link_attribute a \
                     ON a.product_link_attribute_id = d.product_link_attribute_id \
                     WHERE a.link_type_id = 3 AND a.product_link_attribute_code = 'qty' \
                     AND d.link_id IN ({link_ids})"
                ))
                .map(|rows: Vec<(u32, Option<String>)>| {
                    rows.into_iter().filter_map(|(l, q)| q.map(|q| (l, q))).collect()
                })
                .unwrap_or_default();
            for (link_id, child, _) in &links {
                if let Some(q) = qtys.get(link_id) {
                    default_qtys.insert(*child, q.clone());
                }
            }
        }
        links.into_iter().map(|(_, id, sku)| (id, sku)).collect()
    } else {
        c.exec(
            format!(
                "SELECT e.entity_id, e.sku FROM {p}catalog_product_super_link l \
                 JOIN {p}catalog_product_entity e ON e.entity_id = l.product_id \
                 WHERE l.parent_id = :v ORDER BY e.sku"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?
    };
    let mut children = Vec::with_capacity(child_rows.len());
    if !child_rows.is_empty() {
        let ids = child_rows.iter().map(|(i, _)| i.to_string()).collect::<Vec<_>>().join(",");
        let statuses = fetch_child_statuses(&mut c, p, &ids)?;
        // Each child's value per super attribute, with the admin option label.
        let mut super_values: HashMap<(u32, u32), String> = HashMap::new();
        if !super_attrs.is_empty() {
            let attr_ids =
                super_attrs.iter().map(|(i, _)| i.to_string()).collect::<Vec<_>>().join(",");
            let rows: Vec<(u32, u32, Option<u32>)> = c
                .query(format!(
                    "SELECT entity_id, attribute_id, value FROM {p}catalog_product_entity_int \
                     WHERE entity_id IN ({ids}) AND store_id = 0 \
                     AND attribute_id IN ({attr_ids})"
                ))
                .map_err(clean_err)?;
            let labels: HashMap<u32, String> = c
                .query(format!(
                    "SELECT o.option_id, v.value FROM {p}eav_attribute_option o \
                     JOIN {p}eav_attribute_option_value v \
                     ON v.option_id = o.option_id AND v.store_id = 0 \
                     WHERE o.attribute_id IN ({attr_ids})"
                ))
                .map(|rows: Vec<(u32, String)>| rows.into_iter().collect())
                .unwrap_or_default();
            for (child, attr, value) in rows {
                if let Some(option) = value {
                    let label =
                        labels.get(&option).cloned().unwrap_or_else(|| option.to_string());
                    super_values.insert((child, attr), label);
                }
            }
        }
        let stock_rows = fetch_child_stock(&mut c, p, &ids)?;
        for (id, child_sku) in child_rows {
            let enabled =
                statuses.iter().find(|(e, _)| *e == id).and_then(|(_, s)| *s).map(|s| s == 1);
            let options: Vec<String> = super_attrs
                .iter()
                .map(|(attr_id, _)| {
                    super_values.get(&(id, *attr_id)).cloned().unwrap_or_else(|| "-".to_string())
                })
                .collect();
            let stock = stock_rows.iter().find(|(e, _, _)| *e == id);
            children.push((
                id,
                child_sku.clone(),
                enabled,
                options,
                stock.and_then(|(_, qty, _)| qty.clone()),
                stock.map(|(_, _, s)| *s != 0),
                default_qtys.get(&id).cloned(),
            ));
        }
    }

    // Bundle options with their selections.
    let mut bundle_options: Vec<DbBundleOption> = Vec::new();
    if type_id == "bundle" {
        let options: Vec<(u32, i64, String, Option<String>)> = c
            .exec(
                format!(
                    "SELECT o.option_id, o.required, o.type, v.title \
                     FROM {p}catalog_product_bundle_option o \
                     LEFT JOIN {p}catalog_product_bundle_option_value v \
                     ON v.option_id = o.option_id AND v.store_id = 0 \
                     WHERE o.parent_id = :v ORDER BY o.position, o.option_id"
                ),
                params! { "v" => entity_id },
            )
            .map_err(clean_err)?;
        type SelRow = (u32, u32, String, String, i64, i64, Option<String>);
        let selections: Vec<SelRow> = c
            .exec(
                format!(
                    "SELECT s.option_id, e.entity_id, e.sku, CAST(s.selection_qty AS CHAR), \
                     s.is_default, s.selection_price_type, CAST(s.selection_price_value AS CHAR) \
                     FROM {p}catalog_product_bundle_selection s \
                     JOIN {p}catalog_product_entity e ON e.entity_id = s.product_id \
                     WHERE s.parent_product_id = :v ORDER BY s.position, e.sku"
                ),
                params! { "v" => entity_id },
            )
            .map_err(clean_err)?;
        let (statuses, stock_rows) = if selections.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            let ids: std::collections::BTreeSet<u32> =
                selections.iter().map(|(_, id, ..)| *id).collect();
            let ids = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            (fetch_child_statuses(&mut c, p, &ids)?, fetch_child_stock(&mut c, p, &ids)?)
        };
        for (option_id, required, input_type, title) in options {
            let sels = selections
                .iter()
                .filter(|(o, ..)| *o == option_id)
                .map(|(_, id, sku, qty, is_default, price_type, price)| {
                    let enabled = statuses
                        .iter()
                        .find(|(e, _)| e == id)
                        .and_then(|(_, s)| *s)
                        .map(|s| s == 1);
                    let in_stock =
                        stock_rows.iter().find(|(e, _, _)| e == id).map(|(_, _, s)| *s != 0);
                    (
                        *id,
                        sku.clone(),
                        enabled,
                        qty.clone(),
                        *is_default != 0,
                        price.clone(),
                        *price_type == 1,
                        in_stock,
                    )
                })
                .collect();
            bundle_options.push(DbBundleOption {
                title: title.unwrap_or_else(|| format!("(option {option_id})")),
                required: required != 0,
                input_type,
                selections: sels,
            });
        }
    }

    // Media gallery: the gallery rows linked to this entity, with the store-0
    // label/position. The tables are core catalog but may be absent on a stripped
    // synthetic DB — degrade quietly, like MSI stock above.
    let media: Vec<(String, String, Option<String>, i32, bool)> = c
        .exec(
            format!(
                "SELECT mg.value, mg.media_type, mgv.label, mgv.position, \
                        mg.disabled, mgv.disabled \
                 FROM {p}catalog_product_entity_media_gallery mg \
                 JOIN {p}catalog_product_entity_media_gallery_value_to_entity mgvte \
                   ON mgvte.value_id = mg.value_id \
                 LEFT JOIN {p}catalog_product_entity_media_gallery_value mgv \
                   ON mgv.value_id = mg.value_id AND mgv.store_id = 0 \
                 WHERE mgvte.entity_id = :v \
                 ORDER BY mgv.position, mg.value_id"
            ),
            params! { "v" => entity_id },
        )
        .map(|rows: Vec<(String, String, Option<String>, Option<i32>, i64, Option<i64>)>| {
            rows.into_iter()
                .map(|(file, media_type, label, pos, mg_dis, mgv_dis)| {
                    let disabled = mg_dis != 0 || mgv_dis.unwrap_or(0) != 0;
                    (file, media_type, label, pos.unwrap_or(0), disabled)
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(Some(DbProduct {
        entity_id,
        sku,
        type_id,
        attribute_set,
        created_at,
        updated_at,
        websites,
        values,
        option_labels,
        tax_classes,
        stores,
        stock,
        legacy_stock,
        categories,
        rewrites,
        media,
        parents,
        super_attributes,
        children,
        bundle_options,
    }))
}

/// SKU-substring search: `(entity_id, sku, type_id, default-scope name, status)`,
/// fetching `limit + 1` to detect truncation.
#[allow(clippy::type_complexity)]
pub(crate) fn fetch_products_like(
    conn: &DbConnection,
    table_prefix: &str,
    needle: &str,
    limit: usize,
) -> Result<(Vec<(u32, String, String, Option<String>, Option<i64>)>, bool), String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let attr = |code: &str| {
        format!(
            "(SELECT a.attribute_id FROM {p}eav_attribute a \
             JOIN {p}eav_entity_type t ON t.entity_type_id = a.entity_type_id \
             WHERE a.attribute_code = '{code}' AND t.entity_type_code = 'catalog_product')"
        )
    };
    let rows: Vec<(u32, String, String, Option<String>, Option<i64>)> = c
        .exec(
            format!(
                "SELECT e.entity_id, e.sku, e.type_id, n.value, s.value \
                 FROM {p}catalog_product_entity e \
                 LEFT JOIN {p}catalog_product_entity_varchar n ON n.entity_id = e.entity_id \
                 AND n.store_id = 0 AND n.attribute_id = {} \
                 LEFT JOIN {p}catalog_product_entity_int s ON s.entity_id = e.entity_id \
                 AND s.store_id = 0 AND s.attribute_id = {} \
                 WHERE e.sku LIKE :pat ORDER BY e.sku LIMIT {}",
                attr("name"),
                attr("status"),
                limit + 1
            ),
            params! { "pat" => format!("%{needle}%") },
        )
        .map_err(clean_err)?;
    let truncated = rows.len() > limit;
    Ok((rows.into_iter().take(limit).collect(), truncated))
}
