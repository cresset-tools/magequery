//! Live database queries for the rules domain.

use super::*;

/// The tax configuration, raw.
#[allow(clippy::type_complexity)]
pub(crate) struct DbTaxInfo {
    /// `(class_id, name, type)`.
    pub classes: Vec<(u32, String, String)>,
    /// `(rate_id, code, country, region code or *, postcode display, rate)`.
    pub rates: Vec<(u32, String, String, String, String, String)>,
    /// `(rule_id, code, priority, calculate_subtotal)`.
    pub rules: Vec<(u32, String, u32, bool)>,
    /// `(rule_id, rate_id, customer_class_id, product_class_id)`.
    pub links: Vec<(u32, u32, u32, u32)>,
}

pub(crate) fn fetch_tax_info(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<DbTaxInfo, String> {
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;

    let classes: Vec<(u32, String, String)> = c
        .query(format!(
            "SELECT class_id, class_name, class_type FROM {p}tax_class \
             ORDER BY class_type, class_id"
        ))
        .map_err(clean_err)?;
    type RateRow = (u32, String, String, Option<String>, Option<String>, i64, Option<String>, Option<String>, String);
    let rates: Vec<RateRow> = c
        .query(format!(
            "SELECT r.tax_calculation_rate_id, r.code, r.tax_country_id, reg.code, \
             r.tax_postcode, r.zip_is_range, CAST(r.zip_from AS CHAR), \
             CAST(r.zip_to AS CHAR), CAST(r.rate AS CHAR) \
             FROM {p}tax_calculation_rate r \
             LEFT JOIN {p}directory_country_region reg ON reg.region_id = r.tax_region_id \
             ORDER BY r.tax_country_id, r.code"
        ))
        .map_err(clean_err)?;
    let rates = rates
        .into_iter()
        .map(|(id, code, country, region, postcode, zip_is_range, from, to, rate)| {
            let postcode = if zip_is_range != 0 {
                format!("{}–{}", from.as_deref().unwrap_or("?"), to.as_deref().unwrap_or("?"))
            } else {
                postcode.unwrap_or_else(|| "*".to_string())
            };
            (id, code, country, region.unwrap_or_else(|| "*".to_string()), postcode, rate)
        })
        .collect();
    let rules: Vec<(u32, String, u32, i64)> = c
        .query(format!(
            "SELECT tax_calculation_rule_id, code, priority, calculate_subtotal \
             FROM {p}tax_calculation_rule ORDER BY priority, position, tax_calculation_rule_id"
        ))
        .map_err(clean_err)?;
    let links: Vec<(u32, u32, u32, u32)> = c
        .query(format!(
            "SELECT tax_calculation_rule_id, tax_calculation_rate_id, \
             customer_tax_class_id, product_tax_class_id FROM {p}tax_calculation"
        ))
        .map_err(clean_err)?;

    Ok(DbTaxInfo {
        classes,
        rates,
        rules: rules.into_iter().map(|(i, c, pr, cs)| (i, c, pr, cs != 0)).collect(),
        links,
    })
}

/// One catalog rule, raw.
pub(crate) struct DbCatalogRule {
    pub rule_id: u32,
    pub name: String,
    pub description: Option<String>,
    pub active: bool,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    pub in_window: bool,
    pub simple_action: Option<String>,
    pub discount_amount: Option<String>,
    pub sort_order: u32,
    pub stop_rules_processing: bool,
    pub conditions: Option<String>,
    pub websites: Vec<String>,
    pub customer_groups: Vec<String>,
    pub matched_products: u32,
}

pub(crate) fn fetch_catalog_rule(
    conn: &DbConnection,
    table_prefix: &str,
    id: u32,
) -> Result<Option<DbCatalogRule>, String> {
    use mysql::params;
    use mysql::prelude::Queryable;

    let mut c = connect(conn)?;
    let p = table_prefix;
    let row: Option<mysql::Row> = c
        .exec_first(
            format!(
                "SELECT rule_id, name, description, is_active, CAST(from_date AS CHAR), \
                 CAST(to_date AS CHAR), \
                 ((from_date IS NULL OR from_date <= CURDATE()) \
                  AND (to_date IS NULL OR to_date >= CURDATE())), \
                 simple_action, CAST(discount_amount AS CHAR), sort_order, \
                 stop_rules_processing, conditions_serialized \
                 FROM {p}catalogrule WHERE rule_id = :v"
            ),
            params! { "v" => id },
        )
        .map_err(clean_err)?;
    let Some(mut row) = row else { return Ok(None) };
    let s = |r: &mut mysql::Row, i: usize| r.take::<Option<String>, _>(i).flatten();
    let n = |r: &mut mysql::Row, i: usize| r.take::<Option<i64>, _>(i).flatten().unwrap_or(0);
    let rule_id = n(&mut row, 0) as u32;

    let websites: Vec<String> = c
        .exec(
            format!(
                "SELECT w.code FROM {p}catalogrule_website rw \
                 JOIN {p}store_website w ON w.website_id = rw.website_id \
                 WHERE rw.rule_id = :v ORDER BY w.code"
            ),
            params! { "v" => rule_id },
        )
        .map_err(clean_err)?;
    let customer_groups: Vec<String> = c
        .exec(
            format!(
                "SELECT g.customer_group_code FROM {p}catalogrule_customer_group rg \
                 JOIN {p}customer_group g ON g.customer_group_id = rg.customer_group_id \
                 WHERE rg.rule_id = :v ORDER BY g.customer_group_id"
            ),
            params! { "v" => rule_id },
        )
        .map_err(clean_err)?;
    let matched: u64 = c
        .exec_first(
            format!(
                "SELECT COUNT(DISTINCT product_id) FROM {p}catalogrule_product \
                 WHERE rule_id = :v"
            ),
            params! { "v" => rule_id },
        )
        .ok()
        .flatten()
        .unwrap_or(0);

    Ok(Some(DbCatalogRule {
        rule_id,
        name: s(&mut row, 1).unwrap_or_default(),
        description: s(&mut row, 2).filter(|d| !d.is_empty()),
        active: n(&mut row, 3) != 0,
        from_date: s(&mut row, 4),
        to_date: s(&mut row, 5),
        in_window: n(&mut row, 6) != 0,
        simple_action: s(&mut row, 7),
        discount_amount: s(&mut row, 8),
        sort_order: n(&mut row, 9) as u32,
        stop_rules_processing: n(&mut row, 10) != 0,
        conditions: s(&mut row, 11),
        websites,
        customer_groups,
        matched_products: matched as u32,
    }))
}

/// Catalog rules by name/description substring (or all with an empty needle), with
/// per-rule materialized product counts.
#[allow(clippy::type_complexity)]
pub(crate) fn fetch_catalog_rules(
    conn: &DbConnection,
    table_prefix: &str,
    needle: &str,
    limit: usize,
) -> Result<(Vec<(u32, String, bool, Option<String>, Option<String>, u32)>, bool), String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let rows: Vec<(u32, String, i64, Option<String>, Option<String>, u64)> = c
        .exec(
            format!(
                "SELECT r.rule_id, r.name, r.is_active, CAST(r.from_date AS CHAR), \
                 CAST(r.to_date AS CHAR), COUNT(DISTINCT cp.product_id) \
                 FROM {p}catalogrule r \
                 LEFT JOIN {p}catalogrule_product cp ON cp.rule_id = r.rule_id \
                 WHERE r.name LIKE :pat OR r.description LIKE :pat \
                 GROUP BY r.rule_id, r.name, r.is_active, r.from_date, r.to_date \
                 ORDER BY r.rule_id LIMIT {}",
                limit + 1
            ),
            params! { "pat" => format!("%{needle}%") },
        )
        .map_err(clean_err)?;
    let truncated = rows.len() > limit;
    Ok((
        rows.into_iter()
            .take(limit)
            .map(|(id, name, a, f, t, m)| (id, name, a != 0, f, t, m as u32))
            .collect(),
        truncated,
    ))
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
