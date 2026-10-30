//! Live database queries for the admin domain.

use super::*;

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
        ))?;

    let mut out = Vec::with_capacity(rows.len());
    for mut row in rows {
        let s = |r: &mut mysql::Row, i: usize| r.col::<String>(i);
        let n = |r: &mut mysql::Row, i: usize| {
            r.col::<i64>(i).unwrap_or(0)
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
            last_login_secs: row.col::<i64>(8),
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
    let mut c = connect(conn)?;
    let p = table_prefix;
    // `role_name`, `username` and `resource_id` are nullable in the schema.
    let roles: Vec<(u32, Option<String>)> = c
        .query(format!(
            "SELECT role_id, role_name FROM {p}authorization_role \
             WHERE role_type = 'G' ORDER BY role_name"
        ))?;
    let members: Vec<(u32, Option<String>)> = c
        .query(format!(
            "SELECT ur.parent_id, u.username FROM {p}authorization_role ur \
             JOIN {p}admin_user u ON u.user_id = ur.user_id \
             WHERE ur.role_type = 'U' AND ur.user_type = '2' ORDER BY u.username"
        ))?;
    let rules: Vec<(u32, Option<String>, Option<String>)> = c
        .query(format!(
            "SELECT role_id, resource_id, permission FROM {p}authorization_rule \
             ORDER BY resource_id"
        ))?;
    let rules = rules
        .into_iter()
        .filter_map(|(id, res, perm)| Some((id, res?, perm.as_deref() == Some("allow"))))
        .collect();
    Ok((
        roles.into_iter().map(|(id, name)| (id, name.unwrap_or_default())).collect(),
        members.into_iter().filter_map(|(id, user)| Some((id, user?))).collect(),
        rules,
    ))
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
    let mut c = connect(conn)?;
    let p = table_prefix;

    // Every column of both state tables is nullable in the schema; a row without its id
    // can't be joined to anything and is dropped.
    let states: Vec<(Option<String>, Option<String>, Option<String>)> = c
        .query(format!(
            "SELECT indexer_id, status, CAST(updated AS CHAR) FROM {p}indexer_state"
        ))?;
    let states = states
        .into_iter()
        .filter_map(|(indexer_id, status, updated)| {
            let status = status.unwrap_or_default();
            Some(DbIndexerState { indexer_id: indexer_id?, status, updated })
        })
        .collect();

    type ViewRow = (Option<String>, Option<String>, Option<String>, Option<u64>);
    let views: Vec<ViewRow> = c
        .query(format!("SELECT view_id, mode, status, version_id FROM {p}mview_state"))?;
    let mut out = Vec::with_capacity(views.len());
    for (view_id, mode, status, version_id) in views {
        let Some(view_id) = view_id else { continue };
        let (mode, status) = (mode.unwrap_or_default(), status.unwrap_or_default());
        // The changelog table name embeds the view id — identifier-sanitized, and the
        // query tolerated failing: the table is created lazily on first subscribe.
        let backlog = if mode == "enabled"
            && view_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            c.query_first::<u64>(format!(
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
        ))?;
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
        ))?;

    let mut out = Vec::with_capacity(rows.len());
    for mut row in rows {
        let s = |r: &mut mysql::Row, i: usize| r.col::<String>(i);
        let n = |r: &mut mysql::Row, i: usize| {
            r.col::<i64>(i).unwrap_or(0)
        };
        let catalog = row
            .col::<u32>(13)
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
    let mut c = connect(conn)?;
    let p = table_prefix;
    let sets: Vec<(Option<String>, Option<String>)> = c
        .exec(
            format!(
                "SELECT s.attribute_set_name, g.attribute_group_name \
                 FROM {p}eav_entity_attribute ea \
                 JOIN {p}eav_attribute_set s ON s.attribute_set_id = ea.attribute_set_id \
                 JOIN {p}eav_attribute_group g ON g.attribute_group_id = ea.attribute_group_id \
                 WHERE ea.attribute_id = :id ORDER BY s.attribute_set_name"
            ),
            params! { "id" => attribute_id },
        )?;
    let total: Option<u64> = c
        .exec_first(
            format!(
                "SELECT COUNT(*) FROM {p}eav_attribute_set s \
                 JOIN {p}eav_entity_type t ON t.entity_type_id = s.entity_type_id \
                 WHERE t.entity_type_code = :code"
            ),
            params! { "code" => entity_code },
        )?;
    let sets = sets
        .into_iter()
        .map(|(set, group)| (set.unwrap_or_default(), group.unwrap_or_default()))
        .collect();
    Ok((sets, total.unwrap_or(0) as usize))
}

/// Admin-scope (store 0) option labels of a table-source attribute, in sort order.
pub(crate) fn fetch_eav_options(
    conn: &DbConnection,
    table_prefix: &str,
    attribute_id: u32,
) -> Result<Vec<String>, String> {
    use mysql::params;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let labels: Vec<Option<String>> = c.exec(
        format!(
            "SELECT v.value FROM {p}eav_attribute_option o \
             JOIN {p}eav_attribute_option_value v \
             ON v.option_id = o.option_id AND v.store_id = 0 \
             WHERE o.attribute_id = :id ORDER BY o.sort_order, v.value"
        ),
        params! { "id" => attribute_id },
    )?;
    Ok(labels.into_iter().flatten().collect())
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
    use std::collections::HashMap;

    let mut c = connect(conn)?;
    let p = table_prefix;

    let stores: HashMap<u32, String> = store_codes(&mut c, p)?;

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

    // `request_path`/`target_path` are nullable in the schema.
    type Row = (Option<String>, Option<String>, String, i64, i64, i64, Option<String>, i64);
    let rows: Vec<Row> = match path_filter {
        Some(f) => c.exec(&sql, params! { "pat" => format!("%{f}%") })?,
        None => c.query(&sql)?,
    };

    let truncated = rows.len() > limit;
    let out = rows
        .into_iter()
        .take(limit)
        .map(|(request_path, target_path, entity_type, entity_id, redirect_type, store_id, description, auto)| {
            let store_id = store_id as u32;
            let store = stores.get(&store_id).cloned().unwrap_or_else(|| format!("store/{store_id}"));
            UrlRewrite {
                request_path: request_path.unwrap_or_default(),
                target_path: target_path.unwrap_or_default(),
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
