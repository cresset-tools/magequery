//! Live database connection testing (behind the `db` feature). Connects with the `env.php`
//! credentials and runs a trivial query, returning the server version. A short TCP pre-check
//! makes an unreachable host fail fast instead of hanging on the default connect timeout.

use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::model::{DbConnection, DbPing, UrlRewrite};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn ping(conn: &DbConnection) -> DbPing {
    let start = Instant::now();
    let result = try_ping(conn);
    let elapsed_ms = start.elapsed().as_millis();
    match result {
        Ok(version) => DbPing {
            connection: conn.name.clone(),
            ok: true,
            server_version: Some(version),
            error: None,
            elapsed_ms,
        },
        Err(error) => DbPing {
            connection: conn.name.clone(),
            ok: false,
            server_version: None,
            error: Some(error),
            elapsed_ms,
        },
    }
}

fn try_ping(conn: &DbConnection) -> Result<String, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let version: Option<String> = c.query_first("SELECT VERSION()").map_err(clean_err)?;
    Ok(version.unwrap_or_default())
}

/// Connect to a MySQL connection, with a fast reachability pre-check.
fn connect(conn: &DbConnection) -> Result<mysql::Conn, String> {
    let mut builder = mysql::OptsBuilder::new()
        .user(Some(conn.username.as_str()))
        .pass(Some(conn.password.as_str()))
        .db_name(Some(conn.dbname.as_str()));

    if let Some(socket) = &conn.unix_socket {
        if !std::path::Path::new(socket).exists() {
            return Err(format!("socket file not found: {socket}"));
        }
        builder = builder.socket(Some(socket.as_str()));
    } else {
        let port = conn.port.unwrap_or(3306);
        let addr = format!("{}:{}", conn.host, port);
        let sock = addr
            .to_socket_addrs()
            .map_err(|e| format!("cannot resolve {addr}: {e}"))?
            .next()
            .ok_or_else(|| format!("no address for {addr}"))?;
        TcpStream::connect_timeout(&sock, CONNECT_TIMEOUT)
            .map_err(|e| format!("cannot reach {addr}: {e}"))?;
        builder = builder.ip_or_hostname(Some(conn.host.as_str())).tcp_port(port);
    }
    mysql::Conn::new(builder).map_err(clean_err)
}

/// Read `core_config_data` and resolve each row's scope to a `default`/`websites/<code>`/
/// `stores/<code>` string (via `store_website` / `store`). Returns `(scope, path, value)`.
pub(crate) fn fetch_config(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<(String, String, String)>, String> {
    use mysql::prelude::Queryable;
    use std::collections::HashMap;

    let mut c = connect(conn)?;
    let p = table_prefix;

    let websites: HashMap<u32, String> = c
        .query(format!("SELECT website_id, code FROM {p}store_website"))
        .map_err(clean_err)?
        .into_iter()
        .collect();
    let stores: HashMap<u32, String> = c
        .query(format!("SELECT store_id, code FROM {p}store"))
        .map_err(clean_err)?
        .into_iter()
        .collect();

    let rows: Vec<(String, u32, String, Option<String>)> = c
        .query(format!("SELECT scope, scope_id, path, value FROM {p}core_config_data"))
        .map_err(clean_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for (scope, scope_id, path, value) in rows {
        let scope = match scope.as_str() {
            "default" => "default".to_string(),
            "websites" => match websites.get(&scope_id) {
                Some(code) => format!("websites/{code}"),
                None => continue,
            },
            "stores" => match stores.get(&scope_id) {
                Some(code) => format!("stores/{code}"),
                None => continue,
            },
            _ => continue,
        };
        out.push((scope, path, value.unwrap_or_default()));
    }
    Ok(out)
}

/// Read the `theme` table: registered themes with their parent links, for resolving the
/// active `design/theme/theme_id` (a numeric id) to a theme path and its ancestry.
/// Returns `(theme_id, parent_id, theme_path, area)`; virtual themes have no path.
pub(crate) fn fetch_themes(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<(u32, Option<u32>, Option<String>, String)>, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    c.query(format!(
        "SELECT theme_id, parent_id, theme_path, area FROM {table_prefix}theme"
    ))
    .map_err(clean_err)
}

/// The live database's tables and columns from `information_schema`, keyed by table name
/// with the configured prefix stripped (tables not matching a non-empty prefix are
/// skipped — they're not Magento's).
pub(crate) fn fetch_live_schema(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<std::collections::HashMap<String, Vec<String>>, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let rows: Vec<(String, String)> = c
        .query(
            "SELECT TABLE_NAME, COLUMN_NAME FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME, ORDINAL_POSITION",
        )
        .map_err(clean_err)?;
    let mut out: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for (table, column) in rows {
        let name = if table_prefix.is_empty() {
            table
        } else {
            match table.strip_prefix(table_prefix) {
                Some(stripped) => stripped.to_string(),
                None => continue,
            }
        };
        out.entry(name).or_default().push(column);
    }
    Ok(out)
}

/// Rows from the `translation` table for one locale whose `string` contains `needle`
/// (case-insensitive): `(string, translate, store_id)`.
pub(crate) fn fetch_translations(
    conn: &DbConnection,
    table_prefix: &str,
    locale: &str,
    needle: &str,
) -> Result<Vec<(String, String, u32)>, String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    c.exec(
        format!(
            "SELECT string, translate, store_id FROM {table_prefix}translation \
             WHERE locale = :loc AND LOWER(string) LIKE :pat"
        ),
        params! { "loc" => locale, "pat" => format!("%{}%", needle.to_lowercase()) },
    )
    .map_err(clean_err)
}

/// Applied patch class names from `patch_list` (leading backslashes normalized away).
pub(crate) fn fetch_patch_list(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<String>, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let rows: Vec<String> =
        c.query(format!("SELECT patch_name FROM {table_prefix}patch_list")).map_err(clean_err)?;
    Ok(rows.into_iter().map(|r| r.trim_start_matches('\\').to_string()).collect())
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
    /// Configurable variants: `(entity_id, sku, enabled, price, special, final min, final max)`.
    #[allow(clippy::type_complexity)]
    pub children:
        Vec<(u32, String, Option<bool>, Option<String>, Option<String>, Option<String>, Option<String>)>,
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

    // A configurable's storefront price derives from its children — summarize each
    // variant's own default-scope price/special and index final range.
    let child_rows: Vec<(u32, String)> = c
        .exec(
            format!(
                "SELECT e.entity_id, e.sku FROM {p}catalog_product_super_link l \
                 JOIN {p}catalog_product_entity e ON e.entity_id = l.product_id \
                 WHERE l.parent_id = :v ORDER BY e.sku"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;
    let mut children = Vec::with_capacity(child_rows.len());
    if !child_rows.is_empty() {
        let ids: Vec<String> = child_rows.iter().map(|(id, _)| id.to_string()).collect();
        let ids = ids.join(",");
        let attr = |code: &str| {
            format!(
                "(SELECT a.attribute_id FROM {p}eav_attribute a \
                 JOIN {p}eav_entity_type t ON t.entity_type_id = a.entity_type_id \
                 WHERE a.attribute_code = '{code}' AND t.entity_type_code = 'catalog_product')"
            )
        };
        let decimals: Vec<(u32, String, Option<String>)> = c
            .query(format!(
                "SELECT v.entity_id, a.attribute_code, CAST(v.value AS CHAR) \
                 FROM {p}catalog_product_entity_decimal v \
                 JOIN {p}eav_attribute a ON a.attribute_id = v.attribute_id \
                 WHERE v.entity_id IN ({ids}) AND v.store_id = 0 \
                 AND a.attribute_code IN ('price','special_price')"
            ))
            .map_err(clean_err)?;
        let statuses: Vec<(u32, Option<i64>)> = c
            .query(format!(
                "SELECT entity_id, value FROM {p}catalog_product_entity_int \
                 WHERE entity_id IN ({ids}) AND store_id = 0 AND attribute_id = {}",
                attr("status")
            ))
            .map_err(clean_err)?;
        let finals: Vec<(u32, Option<String>, Option<String>)> = c
            .query(format!(
                "SELECT entity_id, CAST(MIN(final_price) AS CHAR), \
                 CAST(MAX(final_price) AS CHAR) FROM {p}catalog_product_index_price \
                 WHERE entity_id IN ({ids}) GROUP BY entity_id"
            ))
            .unwrap_or_default();
        for (id, child_sku) in child_rows {
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
            children.push((
                id,
                child_sku,
                enabled,
                value_of("price"),
                value_of("special_price"),
                final_min,
                final_max,
            ));
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
    pub parents: Vec<String>,
    /// Attribute codes a configurable is configured by, in position order.
    pub super_attributes: Vec<String>,
    /// Variant SKUs.
    pub children: Vec<String>,
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
    let super_attributes: Vec<String> = c
        .exec(
            format!(
                "SELECT a.attribute_code FROM {p}catalog_product_super_attribute sa \
                 JOIN {p}eav_attribute a ON a.attribute_id = sa.attribute_id \
                 WHERE sa.product_id = :v ORDER BY sa.position, a.attribute_code"
            ),
            params! { "v" => entity_id },
        )
        .unwrap_or_default();
    let children: Vec<String> = c
        .exec(
            format!(
                "SELECT e.sku FROM {p}catalog_product_super_link l \
                 JOIN {p}catalog_product_entity e ON e.entity_id = l.product_id \
                 WHERE l.parent_id = :v ORDER BY e.sku"
            ),
            params! { "v" => entity_id },
        )
        .map_err(clean_err)?;

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
        parents,
        super_attributes,
        children,
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

/// One queue's message counts from the MysqlMq driver tables.
pub(crate) struct DbQueueCounts {
    pub queue: String,
    pub new: u32,
    pub in_progress: u32,
    pub retry: u32,
    pub error: u32,
    pub done: u32,
    pub oldest_waiting_secs: Option<i64>,
}

/// Backlog per db-connection queue: `queue_message_status` counts grouped by status
/// (constants from `MysqlMq\Model\QueueManagement`: 2 new, 3 in progress, 4 complete,
/// 5 retry, 6 error, 7 to-be-deleted) plus the oldest waiting (new/retry) message's age
/// on the DB server's clock. Queues with no messages still appear (from `queue`).
pub(crate) fn fetch_queue_backlog(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<DbQueueCounts>, String> {
    use mysql::prelude::Queryable;
    use std::collections::HashMap;

    let mut c = connect(conn)?;
    let p = table_prefix;

    let names: Vec<String> =
        c.query(format!("SELECT name FROM {p}queue")).map_err(clean_err)?;
    let mut by_name: HashMap<String, DbQueueCounts> = names
        .into_iter()
        .map(|queue| {
            (
                queue.clone(),
                DbQueueCounts {
                    queue,
                    new: 0,
                    in_progress: 0,
                    retry: 0,
                    error: 0,
                    done: 0,
                    oldest_waiting_secs: None,
                },
            )
        })
        .collect();

    let counts: Vec<(String, u8, u64)> = c
        .query(format!(
            "SELECT q.name, s.status, COUNT(*) FROM {p}queue_message_status s \
             JOIN {p}queue q ON q.id = s.queue_id GROUP BY q.name, s.status"
        ))
        .map_err(clean_err)?;
    for (name, status, n) in counts {
        let Some(e) = by_name.get_mut(&name) else { continue };
        let n = n as u32;
        match status {
            2 => e.new = n,
            3 => e.in_progress = n,
            5 => e.retry = n,
            6 => e.error = n,
            4 | 7 => e.done += n,
            _ => {}
        }
    }

    let oldest: Vec<(String, Option<i64>)> = c
        .query(format!(
            "SELECT q.name, TIMESTAMPDIFF(SECOND, MIN(s.updated_at), NOW()) \
             FROM {p}queue_message_status s JOIN {p}queue q ON q.id = s.queue_id \
             WHERE s.status IN (2, 5) GROUP BY q.name"
        ))
        .map_err(clean_err)?;
    for (name, secs) in oldest {
        if let Some(e) = by_name.get_mut(&name) {
            e.oldest_waiting_secs = secs;
        }
    }

    Ok(by_name.into_values().collect())
}

/// One job's aggregated `cron_schedule` stats.
pub(crate) struct DbCronStat {
    pub job_code: String,
    pub pending: u32,
    pub running: u32,
    pub success: u32,
    pub error: u32,
    pub missed: u32,
    pub last_status: Option<String>,
    pub last_run: Option<String>,
    pub last_run_secs: Option<i64>,
    pub last_duration_secs: Option<i64>,
    pub last_error: Option<String>,
    pub next_scheduled: Option<String>,
}

/// Per-job `cron_schedule` summary: status counts, the most recently *started* run (its
/// status is the job's last outcome; duration = finished − executed), the most recent
/// retained error message, and the next pending run. All ages on the DB server's clock.
pub(crate) fn fetch_cron_stats(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<DbCronStat>, String> {
    use mysql::prelude::Queryable;
    use std::collections::HashMap;

    let mut c = connect(conn)?;
    let p = table_prefix;

    let mut by_code: HashMap<String, DbCronStat> = HashMap::new();
    fn stat<'a>(m: &'a mut HashMap<String, DbCronStat>, code: &str) -> &'a mut DbCronStat {
        m.entry(code.to_string()).or_insert_with(|| DbCronStat {
            job_code: code.to_string(),
            pending: 0,
            running: 0,
            success: 0,
            error: 0,
            missed: 0,
            last_status: None,
            last_run: None,
            last_run_secs: None,
            last_duration_secs: None,
            last_error: None,
            next_scheduled: None,
        })
    }

    let counts: Vec<(String, String, u64)> = c
        .query(format!(
            "SELECT job_code, status, COUNT(*) FROM {p}cron_schedule \
             GROUP BY job_code, status"
        ))
        .map_err(clean_err)?;
    for (code, status, n) in counts {
        let s = stat(&mut by_code, &code);
        let n = n as u32;
        match status.as_str() {
            "pending" => s.pending = n,
            "running" => s.running = n,
            "success" => s.success = n,
            "error" => s.error = n,
            "missed" => s.missed = n,
            _ => {}
        }
    }

    // The most recently started row per job = the last outcome.
    let last: Vec<(String, String, Option<String>, Option<i64>, Option<i64>)> = c
        .query(format!(
            "SELECT s.job_code, s.status, CAST(s.executed_at AS CHAR), \
             TIMESTAMPDIFF(SECOND, s.executed_at, NOW()), \
             TIMESTAMPDIFF(SECOND, s.executed_at, s.finished_at) \
             FROM {p}cron_schedule s \
             JOIN (SELECT job_code, MAX(executed_at) me FROM {p}cron_schedule \
                   WHERE executed_at IS NOT NULL GROUP BY job_code) m \
             ON m.job_code = s.job_code AND s.executed_at = m.me"
        ))
        .map_err(clean_err)?;
    for (code, status, run, secs, duration) in last {
        let s = stat(&mut by_code, &code);
        if s.last_status.is_none() {
            s.last_status = Some(status);
            s.last_run = run;
            s.last_run_secs = secs;
            s.last_duration_secs = duration;
        }
    }

    let errors: Vec<(String, Option<String>)> = c
        .query(format!(
            "SELECT s.job_code, s.messages FROM {p}cron_schedule s \
             JOIN (SELECT job_code, MAX(schedule_id) mi FROM {p}cron_schedule \
                   WHERE status = 'error' GROUP BY job_code) m \
             ON m.mi = s.schedule_id"
        ))
        .map_err(clean_err)?;
    for (code, msg) in errors {
        stat(&mut by_code, &code).last_error = msg.filter(|m| !m.is_empty());
    }

    let next: Vec<(String, Option<String>)> = c
        .query(format!(
            "SELECT job_code, CAST(MIN(scheduled_at) AS CHAR) FROM {p}cron_schedule \
             WHERE status = 'pending' GROUP BY job_code"
        ))
        .map_err(clean_err)?;
    for (code, at) in next {
        stat(&mut by_code, &code).next_scheduled = at;
    }

    Ok(by_code.into_values().collect())
}

/// A job's recent history rows — runs, errors, and misses (pending rows are excluded:
/// Magento schedules ahead, so dozens of future pendings would drown the log), newest
/// first. `(status, scheduled_at, executed_at, finished_at, duration, messages)`.
#[allow(clippy::type_complexity)]
pub(crate) fn fetch_cron_history(
    conn: &DbConnection,
    table_prefix: &str,
    job_code: &str,
    limit: usize,
) -> Result<
    Vec<(String, Option<String>, Option<String>, Option<String>, Option<i64>, Option<String>)>,
    String,
> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    c.exec(
        format!(
            "SELECT status, CAST(scheduled_at AS CHAR), CAST(executed_at AS CHAR), \
             CAST(finished_at AS CHAR), TIMESTAMPDIFF(SECOND, executed_at, finished_at), \
             messages FROM {table_prefix}cron_schedule \
             WHERE job_code = :code AND status <> 'pending' \
             ORDER BY COALESCE(executed_at, scheduled_at) DESC, schedule_id DESC \
             LIMIT {limit}"
        ),
        params! { "code" => job_code },
    )
    .map_err(clean_err)
}

/// Seconds since the last *successful* cron job finished, per the DB server's own clock
/// (`TIMESTAMPDIFF` — no client-side time needed). `None` = no successful runs recorded.
pub(crate) fn fetch_cron_last_success(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Option<i64>, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    c.query_first(format!(
        "SELECT TIMESTAMPDIFF(SECOND, MAX(finished_at), NOW()) FROM {table_prefix}cron_schedule \
         WHERE status = 'success'"
    ))
    .map_err(clean_err)
    .map(Option::flatten)
}

/// Count the store hierarchy — `(websites, store groups, store views)` — excluding the
/// synthetic admin scopes (id 0).
pub(crate) fn fetch_scope_counts(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<(usize, usize, usize), String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let count = |c: &mut mysql::Conn, sql: String| -> Result<usize, String> {
        Ok(c.query_first::<u64, _>(sql).map_err(clean_err)?.unwrap_or(0) as usize)
    };
    let websites = count(&mut c, format!("SELECT COUNT(*) FROM {p}store_website WHERE website_id > 0"))?;
    let groups = count(&mut c, format!("SELECT COUNT(*) FROM {p}store_group WHERE group_id > 0"))?;
    let stores = count(&mut c, format!("SELECT COUNT(*) FROM {p}store WHERE store_id > 0"))?;
    Ok((websites, groups, stores))
}

/// One `admin_user` row joined with its role name (wide row — columns by index).
pub(crate) struct DbAdminUser {
    pub id: u32,
    pub username: String,
    pub firstname: String,
    pub lastname: String,
    pub email: String,
    pub active: bool,
    pub created: Option<String>,
    pub last_login: Option<String>,
    pub last_login_secs: Option<i64>,
    pub logins: u32,
    pub failures: u32,
    pub locked: bool,
    pub lock_expires: Option<String>,
    pub locale: Option<String>,
    pub role: Option<String>,
}

/// Admin users with their group role: each user's `role_type='U'` row (user_type '2' =
/// admin, per `UserContextInterface`) points at the group via `parent_id`. Lock state and
/// login age are computed with the DB server's own clock.
pub(crate) fn fetch_admin_users(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<DbAdminUser>, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let rows: Vec<mysql::Row> = c
        .query(format!(
            "SELECT u.user_id, u.username, u.firstname, u.lastname, u.email, u.is_active, \
             CAST(u.created AS CHAR), CAST(u.logdate AS CHAR), \
             TIMESTAMPDIFF(SECOND, u.logdate, NOW()), u.lognum, u.failures_num, \
             (u.lock_expires IS NOT NULL AND u.lock_expires > NOW()), \
             CAST(u.lock_expires AS CHAR), u.interface_locale, r.role_name \
             FROM {p}admin_user u \
             LEFT JOIN {p}authorization_role ur ON ur.role_type = 'U' \
             AND ur.user_type = '2' AND ur.user_id = u.user_id \
             LEFT JOIN {p}authorization_role r ON r.role_id = ur.parent_id \
             ORDER BY u.username"
        ))
        .map_err(clean_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for mut row in rows {
        let s = |r: &mut mysql::Row, i: usize| r.take::<Option<String>, _>(i).flatten();
        let n = |r: &mut mysql::Row, i: usize| {
            r.take::<Option<i64>, _>(i).flatten().unwrap_or(0)
        };
        out.push(DbAdminUser {
            id: n(&mut row, 0) as u32,
            username: s(&mut row, 1).unwrap_or_default(),
            firstname: s(&mut row, 2).unwrap_or_default(),
            lastname: s(&mut row, 3).unwrap_or_default(),
            email: s(&mut row, 4).unwrap_or_default(),
            active: n(&mut row, 5) != 0,
            created: s(&mut row, 6),
            last_login: s(&mut row, 7),
            last_login_secs: row.take::<Option<i64>, _>(8).flatten(),
            logins: n(&mut row, 9) as u32,
            failures: n(&mut row, 10) as u32,
            locked: n(&mut row, 11) != 0,
            lock_expires: s(&mut row, 12),
            locale: s(&mut row, 13),
            role: s(&mut row, 14),
        });
    }
    Ok(out)
}

/// Admin roles (`role_type='G'`), their admin members, and their `authorization_rule`
/// permissions: `(role_id, name)` + `(role_id, username)` + `(role_id, resource, allow)`.
#[allow(clippy::type_complexity)]
pub(crate) fn fetch_admin_roles(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<(Vec<(u32, String)>, Vec<(u32, String)>, Vec<(u32, String, bool)>), String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let roles: Vec<(u32, String)> = c
        .query(format!(
            "SELECT role_id, role_name FROM {p}authorization_role \
             WHERE role_type = 'G' ORDER BY role_name"
        ))
        .map_err(clean_err)?;
    let members: Vec<(u32, String)> = c
        .query(format!(
            "SELECT ur.parent_id, u.username FROM {p}authorization_role ur \
             JOIN {p}admin_user u ON u.user_id = ur.user_id \
             WHERE ur.role_type = 'U' AND ur.user_type = '2' ORDER BY u.username"
        ))
        .map_err(clean_err)?;
    let rules: Vec<(u32, String, Option<String>)> = c
        .query(format!(
            "SELECT role_id, resource_id, permission FROM {p}authorization_rule \
             ORDER BY resource_id"
        ))
        .map_err(clean_err)?;
    let rules = rules
        .into_iter()
        .map(|(id, res, perm)| (id, res, perm.as_deref() == Some("allow")))
        .collect();
    Ok((roles, members, rules))
}

/// One `indexer_state` row.
pub(crate) struct DbIndexerState {
    pub indexer_id: String,
    /// `valid` / `invalid` / `working` / `suspended`.
    pub status: String,
    pub updated: Option<String>,
}

/// One `mview_state` row, with the pending-changelog count for scheduled views.
pub(crate) struct DbMviewState {
    pub view_id: String,
    /// `enabled` = update by schedule, `disabled` = update on save.
    pub mode: String,
    /// `idle` / `working` / `suspended`.
    pub status: String,
    /// Distinct entities in `<view_id>_cl` past the applied version — what
    /// `bin/magento indexer:status` reports as backlog. `None` = no changelog table.
    pub backlog: Option<u64>,
}

/// Live indexer + mview state. The backlog mirrors Magento's `IndexerStatusCommand`:
/// `COUNT(DISTINCT entity_id)` in the changelog beyond the view's applied `version_id`.
pub(crate) fn fetch_indexer_states(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<(Vec<DbIndexerState>, Vec<DbMviewState>), String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;

    let states: Vec<(String, String, Option<String>)> = c
        .query(format!(
            "SELECT indexer_id, status, CAST(updated AS CHAR) FROM {p}indexer_state"
        ))
        .map_err(clean_err)?;
    let states = states
        .into_iter()
        .map(|(indexer_id, status, updated)| DbIndexerState { indexer_id, status, updated })
        .collect();

    let views: Vec<(String, String, String, Option<u64>)> = c
        .query(format!("SELECT view_id, mode, status, version_id FROM {p}mview_state"))
        .map_err(clean_err)?;
    let mut out = Vec::with_capacity(views.len());
    for (view_id, mode, status, version_id) in views {
        // The changelog table name embeds the view id — identifier-sanitized, and the
        // query tolerated failing: the table is created lazily on first subscribe.
        let backlog = if mode == "enabled"
            && view_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            c.query_first::<u64, _>(format!(
                "SELECT COUNT(DISTINCT entity_id) FROM {p}{view_id}_cl \
                 WHERE version_id > {}",
                version_id.unwrap_or(0)
            ))
            .ok()
            .flatten()
        } else {
            None
        };
        out.push(DbMviewState { view_id, mode, status, backlog });
    }
    Ok((states, out))
}

/// One `eav_entity_type` row with its attribute count.
pub(crate) struct DbEavEntity {
    pub code: String,
    pub entity_table: Option<String>,
    pub value_table_prefix: Option<String>,
    pub attributes: usize,
}

pub(crate) fn fetch_eav_entities(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<DbEavEntity>, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let rows: Vec<(String, Option<String>, Option<String>, u64)> = c
        .query(format!(
            "SELECT t.entity_type_code, t.entity_table, t.value_table_prefix, \
             COUNT(a.attribute_id) \
             FROM {p}eav_entity_type t \
             LEFT JOIN {p}eav_attribute a ON a.entity_type_id = t.entity_type_id \
             GROUP BY t.entity_type_id, t.entity_type_code, t.entity_table, \
             t.value_table_prefix ORDER BY t.entity_type_code"
        ))
        .map_err(clean_err)?;
    Ok(rows
        .into_iter()
        .map(|(code, entity_table, value_table_prefix, n)| DbEavEntity {
            code,
            entity_table: entity_table.filter(|t| !t.is_empty()),
            value_table_prefix: value_table_prefix.filter(|t| !t.is_empty()),
            attributes: n as usize,
        })
        .collect())
}

/// Catalog-specific flags (`catalog_eav_attribute`), raw.
pub(crate) struct DbCatalogFlags {
    pub is_global: i64,
    pub searchable: bool,
    pub filterable: bool,
    pub filterable_in_search: bool,
    pub comparable: bool,
    pub used_in_listing: bool,
    pub used_for_sort_by: bool,
    pub visible_on_front: bool,
    pub apply_to: Option<String>,
}

/// One `eav_attribute` row, joined with its entity-type code and catalog flags.
pub(crate) struct DbEavAttribute {
    pub entity_code: String,
    pub attribute_id: u32,
    pub code: String,
    pub label: Option<String>,
    pub backend_type: String,
    pub frontend_input: Option<String>,
    pub required: bool,
    pub unique: bool,
    pub user_defined: bool,
    pub default_value: Option<String>,
    pub source_model: Option<String>,
    pub backend_model: Option<String>,
    pub frontend_model: Option<String>,
    pub catalog: Option<DbCatalogFlags>,
}

/// Every attribute across all entity types (a few hundred rows — filtering happens in
/// Rust). The row is too wide for a tuple, so columns are taken by index.
pub(crate) fn fetch_eav_attributes(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<Vec<DbEavAttribute>, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let rows: Vec<mysql::Row> = c
        .query(format!(
            "SELECT t.entity_type_code, a.attribute_id, a.attribute_code, a.frontend_label, \
             a.backend_type, a.frontend_input, a.is_required, a.is_unique, a.is_user_defined, \
             a.default_value, a.source_model, a.backend_model, a.frontend_model, \
             c.attribute_id, c.is_global, c.is_searchable, c.is_filterable, \
             c.is_filterable_in_search, c.is_comparable, c.used_in_product_listing, \
             c.used_for_sort_by, c.is_visible_on_front, c.apply_to \
             FROM {p}eav_attribute a \
             JOIN {p}eav_entity_type t ON t.entity_type_id = a.entity_type_id \
             LEFT JOIN {p}catalog_eav_attribute c ON c.attribute_id = a.attribute_id \
             ORDER BY t.entity_type_code, a.attribute_code"
        ))
        .map_err(clean_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for mut row in rows {
        let s = |r: &mut mysql::Row, i: usize| r.take::<Option<String>, _>(i).flatten();
        let n = |r: &mut mysql::Row, i: usize| {
            r.take::<Option<i64>, _>(i).flatten().unwrap_or(0)
        };
        let catalog = row
            .take::<Option<u32>, _>(13)
            .flatten()
            .is_some()
            .then(|| DbCatalogFlags {
                is_global: n(&mut row, 14),
                searchable: n(&mut row, 15) != 0,
                filterable: n(&mut row, 16) != 0,
                filterable_in_search: n(&mut row, 17) != 0,
                comparable: n(&mut row, 18) != 0,
                used_in_listing: n(&mut row, 19) != 0,
                used_for_sort_by: n(&mut row, 20) != 0,
                visible_on_front: n(&mut row, 21) != 0,
                apply_to: s(&mut row, 22).filter(|a| !a.is_empty()),
            });
        out.push(DbEavAttribute {
            entity_code: s(&mut row, 0).unwrap_or_default(),
            attribute_id: n(&mut row, 1) as u32,
            code: s(&mut row, 2).unwrap_or_default(),
            label: s(&mut row, 3).filter(|l| !l.is_empty()),
            backend_type: s(&mut row, 4).unwrap_or_default(),
            frontend_input: s(&mut row, 5).filter(|i| !i.is_empty()),
            required: n(&mut row, 6) != 0,
            unique: n(&mut row, 7) != 0,
            user_defined: n(&mut row, 8) != 0,
            default_value: s(&mut row, 9).filter(|d| !d.is_empty()),
            source_model: s(&mut row, 10).filter(|m| !m.is_empty()),
            backend_model: s(&mut row, 11).filter(|m| !m.is_empty()),
            frontend_model: s(&mut row, 12).filter(|m| !m.is_empty()),
            catalog,
        });
    }
    Ok(out)
}

/// An attribute's set memberships as `(set name, group name)`, plus the entity's total
/// set count (the "in 3 of 5 sets" denominator).
pub(crate) fn fetch_eav_sets(
    conn: &DbConnection,
    table_prefix: &str,
    attribute_id: u32,
    entity_code: &str,
) -> Result<(Vec<(String, String)>, usize), String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let sets: Vec<(String, String)> = c
        .exec(
            format!(
                "SELECT s.attribute_set_name, g.attribute_group_name \
                 FROM {p}eav_entity_attribute ea \
                 JOIN {p}eav_attribute_set s ON s.attribute_set_id = ea.attribute_set_id \
                 JOIN {p}eav_attribute_group g ON g.attribute_group_id = ea.attribute_group_id \
                 WHERE ea.attribute_id = :id ORDER BY s.attribute_set_name"
            ),
            params! { "id" => attribute_id },
        )
        .map_err(clean_err)?;
    let total: Option<u64> = c
        .exec_first(
            format!(
                "SELECT COUNT(*) FROM {p}eav_attribute_set s \
                 JOIN {p}eav_entity_type t ON t.entity_type_id = s.entity_type_id \
                 WHERE t.entity_type_code = :code"
            ),
            params! { "code" => entity_code },
        )
        .map_err(clean_err)?;
    Ok((sets, total.unwrap_or(0) as usize))
}

/// Admin-scope (store 0) option labels of a table-source attribute, in sort order.
pub(crate) fn fetch_eav_options(
    conn: &DbConnection,
    table_prefix: &str,
    attribute_id: u32,
) -> Result<Vec<String>, String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    c.exec(
        format!(
            "SELECT v.value FROM {p}eav_attribute_option o \
             JOIN {p}eav_attribute_option_value v \
             ON v.option_id = o.option_id AND v.store_id = 0 \
             WHERE o.attribute_id = :id ORDER BY o.sort_order, v.value"
        ),
        params! { "id" => attribute_id },
    )
    .map_err(clean_err)
}

/// Read the `url_rewrite` table, resolving each row's `store_id` to a store code. Filters
/// (path substring on request/target, store code, redirects-only) are applied **in SQL** —
/// the table is often huge. Fetches `limit + 1` rows to detect truncation; returns
/// `(rows, truncated)`.
pub(crate) fn fetch_url_rewrites(
    conn: &DbConnection,
    table_prefix: &str,
    path_filter: Option<&str>,
    store_filter: Option<&str>,
    redirects_only: bool,
    limit: usize,
) -> Result<(Vec<UrlRewrite>, bool), String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    use std::collections::HashMap;

    let mut c = connect(conn)?;
    let p = table_prefix;

    let stores: HashMap<u32, String> =
        c.query(format!("SELECT store_id, code FROM {p}store")).map_err(clean_err)?.into_iter().collect();

    let mut wheres: Vec<String> = Vec::new();
    if path_filter.is_some() {
        wheres.push("(request_path LIKE :pat OR target_path LIKE :pat)".into());
    }
    if let Some(code) = store_filter {
        let id = stores
            .iter()
            .find(|(_, c)| c.as_str() == code)
            .map(|(id, _)| *id)
            .ok_or_else(|| format!("no store with code `{code}`"))?;
        wheres.push(format!("store_id = {id}"));
    }
    if redirects_only {
        wheres.push("redirect_type <> 0".into());
    }
    let where_sql =
        if wheres.is_empty() { String::new() } else { format!("WHERE {}", wheres.join(" AND ")) };

    let sql = format!(
        "SELECT request_path, target_path, entity_type, entity_id, redirect_type, store_id, \
         description, is_autogenerated FROM {p}url_rewrite {where_sql} \
         ORDER BY request_path LIMIT {}",
        limit + 1
    );

    type Row = (String, String, String, i64, i64, i64, Option<String>, i64);
    let rows: Vec<Row> = match path_filter {
        Some(f) => c.exec(&sql, params! { "pat" => format!("%{f}%") }).map_err(clean_err)?,
        None => c.query(&sql).map_err(clean_err)?,
    };

    let truncated = rows.len() > limit;
    let out = rows
        .into_iter()
        .take(limit)
        .map(|(request_path, target_path, entity_type, entity_id, redirect_type, store_id, description, auto)| {
            let store_id = store_id as u32;
            let store = stores.get(&store_id).cloned().unwrap_or_else(|| format!("store/{store_id}"));
            UrlRewrite {
                request_path,
                target_path,
                entity_type,
                entity_id: entity_id as u32,
                redirect_type: redirect_type as u16,
                store,
                description: description.filter(|d| !d.is_empty()),
                autogenerated: auto != 0,
            }
        })
        .collect();
    Ok((out, truncated))
}

/// The `mysql` crate prints errors as `DriverError { … }` / `MySqlError { … }`; unwrap the
/// outer `Variant { … }` so the message reads cleanly.
fn clean_err(e: mysql::Error) -> String {
    let s = e.to_string();
    match (s.find("{ "), s.rfind(" }")) {
        (Some(open), Some(close)) if open + 2 <= close => s[open + 2..close].to_string(),
        _ => s,
    }
}
