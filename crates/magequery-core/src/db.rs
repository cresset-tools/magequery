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

/// How to look a sales rule up.
pub(crate) enum RuleIdent<'a> {
    Id(u32),
    Coupon(&'a str),
}

/// One cart price rule, raw.
pub(crate) struct DbSalesRule {
    pub rule_id: u32,
    pub name: String,
    pub description: Option<String>,
    pub active: bool,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    pub in_window: bool,
    pub coupon_type: i64,
    pub simple_action: Option<String>,
    pub discount_amount: Option<String>,
    pub discount_qty: Option<String>,
    pub discount_step: Option<u64>,
    pub apply_to_shipping: bool,
    pub free_shipping: bool,
    pub stop_rules_processing: bool,
    pub sort_order: u32,
    pub uses_per_customer: u64,
    pub uses_per_coupon: u64,
    pub times_used: u64,
    pub conditions: Option<String>,
    pub websites: Vec<String>,
    pub customer_groups: Vec<String>,
    pub coupon_count: u32,
    /// `(code, times_used, usage_limit, usage_per_customer, expiration, expired)`.
    #[allow(clippy::type_complexity)]
    pub coupons: Vec<(String, u64, Option<u64>, Option<u64>, Option<String>, bool)>,
    #[allow(clippy::type_complexity)]
    pub matched_coupon: Option<(String, u64, Option<u64>, Option<u64>, Option<String>, bool)>,
}

pub(crate) fn fetch_sales_rule(
    conn: &DbConnection,
    table_prefix: &str,
    ident: RuleIdent<'_>,
) -> Result<Option<DbSalesRule>, String> {
    use mysql::params;
    use mysql::prelude::Queryable;

    let mut c = connect(conn)?;
    let p = table_prefix;

    let coupon_select = format!(
        "SELECT code, times_used, usage_limit, usage_per_customer, \
         CAST(expiration_date AS CHAR), \
         (expiration_date IS NOT NULL AND expiration_date < NOW()) \
         FROM {p}salesrule_coupon"
    );
    type CouponRow = (String, u64, Option<u64>, Option<u64>, Option<String>, i64);
    let (rule_id, matched_coupon): (u32, Option<CouponRow>) = match ident {
        RuleIdent::Id(id) => (id, None),
        RuleIdent::Coupon(code) => {
            let hit: Option<(u32, String, u64, Option<u64>, Option<u64>, Option<String>, i64)> = c
                .exec_first(
                    format!(
                        "SELECT rule_id, code, times_used, usage_limit, usage_per_customer, \
                         CAST(expiration_date AS CHAR), \
                         (expiration_date IS NOT NULL AND expiration_date < NOW()) \
                         FROM {p}salesrule_coupon WHERE code = :v"
                    ),
                    params! { "v" => code },
                )
                .map_err(clean_err)?;
            match hit {
                Some((rid, code, used, limit, per_cust, exp, expired)) => {
                    (rid, Some((code, used, limit, per_cust, exp, expired)))
                }
                None => return Ok(None),
            }
        }
    };

    let row: Option<mysql::Row> = c
        .exec_first(
            format!(
                "SELECT rule_id, name, description, is_active, CAST(from_date AS CHAR), \
                 CAST(to_date AS CHAR), \
                 ((from_date IS NULL OR from_date <= CURDATE()) \
                  AND (to_date IS NULL OR to_date >= CURDATE())), \
                 coupon_type, simple_action, CAST(discount_amount AS CHAR), \
                 CAST(discount_qty AS CHAR), discount_step, apply_to_shipping, \
                 simple_free_shipping, stop_rules_processing, sort_order, \
                 uses_per_customer, uses_per_coupon, times_used, conditions_serialized \
                 FROM {p}salesrule WHERE rule_id = :v"
            ),
            params! { "v" => rule_id },
        )
        .map_err(clean_err)?;
    let Some(mut row) = row else { return Ok(None) };
    let s = |r: &mut mysql::Row, i: usize| r.take::<Option<String>, _>(i).flatten();
    let n = |r: &mut mysql::Row, i: usize| r.take::<Option<i64>, _>(i).flatten().unwrap_or(0);
    let rule_id = n(&mut row, 0) as u32;
    let name = s(&mut row, 1).unwrap_or_default();
    let description = s(&mut row, 2).filter(|d| !d.is_empty());
    let active = n(&mut row, 3) != 0;
    let from_date = s(&mut row, 4);
    let to_date = s(&mut row, 5);
    let in_window = n(&mut row, 6) != 0;
    let coupon_type = n(&mut row, 7);
    let simple_action = s(&mut row, 8);
    let discount_amount = s(&mut row, 9);
    let discount_qty = s(&mut row, 10);
    let discount_step: Option<u64> = row.take::<Option<u64>, _>(11).flatten();
    let apply_to_shipping = n(&mut row, 12) != 0;
    let free_shipping = n(&mut row, 13) != 0;
    let stop_rules_processing = n(&mut row, 14) != 0;
    let sort_order = n(&mut row, 15) as u32;
    let uses_per_customer = n(&mut row, 16) as u64;
    let uses_per_coupon = n(&mut row, 17) as u64;
    let times_used = n(&mut row, 18) as u64;
    let conditions = s(&mut row, 19);

    let websites: Vec<String> = c
        .exec(
            format!(
                "SELECT w.code FROM {p}salesrule_website rw \
                 JOIN {p}store_website w ON w.website_id = rw.website_id \
                 WHERE rw.rule_id = :v ORDER BY w.code"
            ),
            params! { "v" => rule_id },
        )
        .map_err(clean_err)?;
    let customer_groups: Vec<String> = c
        .exec(
            format!(
                "SELECT g.customer_group_code FROM {p}salesrule_customer_group rg \
                 JOIN {p}customer_group g ON g.customer_group_id = rg.customer_group_id \
                 WHERE rg.rule_id = :v ORDER BY g.customer_group_id"
            ),
            params! { "v" => rule_id },
        )
        .map_err(clean_err)?;

    let coupon_count: u64 = c
        .exec_first(
            format!("SELECT COUNT(*) FROM {p}salesrule_coupon WHERE rule_id = :v"),
            params! { "v" => rule_id },
        )
        .map_err(clean_err)?
        .unwrap_or(0);
    let coupons: Vec<CouponRow> = c
        .exec(
            format!("{coupon_select} WHERE rule_id = :v ORDER BY coupon_id LIMIT 10"),
            params! { "v" => rule_id },
        )
        .map_err(clean_err)?;

    Ok(Some(DbSalesRule {
        rule_id,
        name,
        description,
        active,
        from_date,
        to_date,
        in_window,
        coupon_type,
        simple_action,
        discount_amount,
        discount_qty,
        discount_step,
        apply_to_shipping,
        free_shipping,
        stop_rules_processing,
        sort_order,
        uses_per_customer,
        uses_per_coupon,
        times_used,
        conditions,
        websites,
        customer_groups,
        coupon_count: coupon_count as u32,
        coupons: coupons
            .into_iter()
            .map(|(code, used, limit, pc, exp, e)| (code, used, limit, pc, exp, e != 0))
            .collect(),
        matched_coupon: matched_coupon
            .map(|(code, used, limit, pc, exp, e)| (code, used, limit, pc, exp, e != 0)),
    }))
}

/// Rule search by name/description substring, newest first.
#[allow(clippy::type_complexity)]
pub(crate) fn fetch_sales_rules_like(
    conn: &DbConnection,
    table_prefix: &str,
    needle: &str,
    limit: usize,
) -> Result<(Vec<(u32, String, bool, Option<String>, Option<String>)>, bool), String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let rows: Vec<(u32, String, i64, Option<String>, Option<String>)> = c
        .exec(
            format!(
                "SELECT rule_id, name, is_active, CAST(from_date AS CHAR), \
                 CAST(to_date AS CHAR) FROM {p}salesrule \
                 WHERE name LIKE :pat OR description LIKE :pat \
                 ORDER BY rule_id DESC LIMIT {}",
                limit + 1
            ),
            params! { "pat" => format!("%{needle}%") },
        )
        .map_err(clean_err)?;
    let truncated = rows.len() > limit;
    Ok((
        rows.into_iter()
            .take(limit)
            .map(|(id, name, a, f, t)| (id, name, a != 0, f, t))
            .collect(),
        truncated,
    ))
}

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

/// Order search by increment_id or customer email substring, newest first;
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
                "SELECT entity_id, increment_id, status, CAST(grand_total AS CHAR), \
                 order_currency_code, customer_email, CAST(created_at AS CHAR) \
                 FROM {p}sales_order \
                 WHERE increment_id LIKE :pat OR customer_email LIKE :pat \
                 ORDER BY entity_id DESC LIMIT {}",
                limit + 1
            ),
            params! { "pat" => format!("%{needle}%") },
        )
        .map_err(clean_err)?;
    let truncated = rows.len() > limit;
    Ok((rows.into_iter().take(limit).collect(), truncated))
}

/// A helper for category attribute-id subqueries (`entity_type_code = catalog_category`).
fn cat_attr(p: &str, code: &str) -> String {
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
