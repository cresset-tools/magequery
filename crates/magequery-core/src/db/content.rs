//! Live database queries for the content domain.

use super::*;

/// One CMS row, raw.
pub(crate) struct DbCmsEntry {
    pub id: u32,
    pub identifier: String,
    pub title: String,
    pub active: bool,
    pub created: Option<String>,
    pub updated: Option<String>,
    pub page_layout: Option<String>,
    pub meta_title: Option<String>,
    pub has_layout_update: bool,
    pub content: Option<String>,
    /// Store codes, `(all stores)` for store 0.
    pub stores: Vec<String>,
}

/// How to select CMS rows.
pub(crate) enum CmsSelector<'a> {
    All,
    Identifier(&'a str),
    Id(u32),
}

/// CMS pages/blocks: every row matching the selector (an identifier can match several
/// rows, scoped to different stores).
pub(crate) fn fetch_cms_entries(
    conn: &DbConnection,
    table_prefix: &str,
    kind: crate::model::CmsKind,
    sel: CmsSelector<'_>,
) -> Result<Vec<DbCmsEntry>, String> {
    use crate::model::CmsKind as K;
    use mysql::params;
    use mysql::prelude::Queryable;
    use std::collections::HashMap;

    let mut c = connect(conn)?;
    let p = table_prefix;
    let (select, table, id_col) = match kind {
        K::Page => (
            format!(
                "SELECT page_id, identifier, title, is_active, \
                 CAST(creation_time AS CHAR), CAST(update_time AS CHAR), page_layout, \
                 meta_title, (COALESCE(layout_update_xml, '') <> '' \
                 OR COALESCE(custom_layout_update_xml, '') <> ''), content \
                 FROM {p}cms_page"
            ),
            "cms_page_store",
            "page_id",
        ),
        K::Block => (
            format!(
                "SELECT block_id, identifier, title, is_active, \
                 CAST(creation_time AS CHAR), CAST(update_time AS CHAR), NULL, NULL, 0, \
                 content FROM {p}cms_block"
            ),
            "cms_block_store",
            "block_id",
        ),
    };
    let rows: Vec<mysql::Row> = match sel {
        CmsSelector::Identifier(i) => c
            .exec(format!("{select} WHERE identifier = :v ORDER BY 1"), params! { "v" => i })
            .map_err(clean_err)?,
        CmsSelector::Id(id) => c
            .exec(format!("{select} WHERE {id_col} = :v"), params! { "v" => id })
            .map_err(clean_err)?,
        CmsSelector::All => {
            c.query(format!("{select} ORDER BY identifier, 1")).map_err(clean_err)?
        }
    };
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let stores: HashMap<u32, String> =
        c.query(format!("SELECT store_id, code FROM {p}store")).map_err(clean_err)?
            .into_iter()
            .collect();
    let mut entries: Vec<DbCmsEntry> = Vec::with_capacity(rows.len());
    for mut r in rows {
        let s = |r: &mut mysql::Row, i: usize| r.take::<Option<String>, _>(i).flatten();
        let n = |r: &mut mysql::Row, i: usize| r.take::<Option<i64>, _>(i).flatten().unwrap_or(0);
        entries.push(DbCmsEntry {
            id: n(&mut r, 0) as u32,
            identifier: s(&mut r, 1).unwrap_or_default(),
            title: s(&mut r, 2).unwrap_or_default(),
            active: n(&mut r, 3) != 0,
            created: s(&mut r, 4),
            updated: s(&mut r, 5),
            page_layout: s(&mut r, 6),
            meta_title: s(&mut r, 7).filter(|m| !m.is_empty()),
            has_layout_update: n(&mut r, 8) != 0,
            content: s(&mut r, 9),
            stores: Vec::new(),
        });
    }
    let ids = entries.iter().map(|e| e.id.to_string()).collect::<Vec<_>>().join(",");
    let links: Vec<(u32, u32)> = c
        .query(format!("SELECT {id_col}, store_id FROM {p}{table} WHERE {id_col} IN ({ids})"))
        .map_err(clean_err)?;
    for e in &mut entries {
        e.stores = links
            .iter()
            .filter(|(id, _)| *id == e.id)
            .map(|(_, sid)| {
                if *sid == 0 {
                    "(all stores)".to_string()
                } else {
                    stores.get(sid).cloned().unwrap_or_else(|| format!("store/{sid}"))
                }
            })
            .collect();
    }
    Ok(entries)
}

/// CMS search by identifier/title substring: `(id, identifier, title, active)`.
pub(crate) fn fetch_cms_like(
    conn: &DbConnection,
    table_prefix: &str,
    kind: crate::model::CmsKind,
    needle: &str,
    limit: usize,
) -> Result<(Vec<(u32, String, String, bool)>, bool), String> {
    use crate::model::CmsKind as K;
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    let (table, id_col) = match kind {
        K::Page => ("cms_page", "page_id"),
        K::Block => ("cms_block", "block_id"),
    };
    let rows: Vec<(u32, String, String, i64)> = c
        .exec(
            format!(
                "SELECT {id_col}, identifier, title, is_active FROM {p}{table} \
                 WHERE identifier LIKE :pat OR title LIKE :pat \
                 ORDER BY identifier, {id_col} LIMIT {}",
                limit + 1
            ),
            params! { "pat" => format!("%{needle}%") },
        )
        .map_err(clean_err)?;
    let truncated = rows.len() > limit;
    Ok((
        rows.into_iter().take(limit).map(|(id, i, t, a)| (id, i, t, a != 0)).collect(),
        truncated,
    ))
}

/// Integrations, raw: `(id, name, email, endpoint, status, setup_type, created,
/// updated, token state)` + per-integration `(resource, allow)` rules.
#[allow(clippy::type_complexity)]
pub(crate) fn fetch_integrations(
    conn: &DbConnection,
    table_prefix: &str,
) -> Result<
    Vec<(
        u32,
        String,
        Option<String>,
        Option<String>,
        i64,
        i64,
        Option<String>,
        Option<String>,
        String,
        Vec<(String, bool)>,
    )>,
    String,
> {
    use mysql::prelude::Queryable;
    use std::collections::HashMap;

    let mut c = connect(conn)?;
    let p = table_prefix;

    type IntRow = (u32, String, Option<String>, Option<String>, i64, i64, Option<String>, Option<String>, Option<u64>);
    let integrations: Vec<IntRow> = c
        .query(format!(
            "SELECT integration_id, name, email, endpoint, status, setup_type, \
             CAST(created_at AS CHAR), CAST(updated_at AS CHAR), consumer_id \
             FROM {p}integration ORDER BY name"
        ))
        .map_err(clean_err)?;
    if integrations.is_empty() {
        return Ok(Vec::new());
    }

    // Access-token state per consumer — presence and revocation only, never the secret.
    let tokens: HashMap<u64, i64> = c
        .query(format!(
            "SELECT consumer_id, revoked FROM {p}oauth_token WHERE type = 'access'"
        ))
        .map(|rows: Vec<(Option<u64>, i64)>| {
            rows.into_iter().filter_map(|(cid, r)| cid.map(|c| (c, r))).collect()
        })
        .unwrap_or_default();

    // Integration permissions: the U-role with user_type 1 (integration), then its rules.
    let roles: Vec<(u32, u32)> = c
        .query(format!(
            "SELECT role_id, user_id FROM {p}authorization_role WHERE user_type = '1'"
        ))
        .map(|rows: Vec<(u32, u32)>| rows)
        .unwrap_or_default();
    let rules: Vec<(u32, String, Option<String>)> = c
        .query(format!(
            "SELECT role_id, resource_id, permission FROM {p}authorization_rule \
             ORDER BY resource_id"
        ))
        .map(|rows: Vec<(u32, String, Option<String>)>| rows)
        .unwrap_or_default();

    Ok(integrations
        .into_iter()
        .map(|(id, name, email, endpoint, status, setup, created, updated, consumer)| {
            let token = match consumer.and_then(|cid| tokens.get(&cid)) {
                Some(0) => "active".to_string(),
                Some(_) => "revoked".to_string(),
                None => "none".to_string(),
            };
            let my_rules: Vec<(String, bool)> = roles
                .iter()
                .filter(|(_, uid)| *uid == id)
                .flat_map(|(rid, _)| {
                    rules.iter().filter(move |(r, ..)| r == rid).map(|(_, res, perm)| {
                        (res.clone(), perm.as_deref() == Some("allow"))
                    })
                })
                .collect();
            (id, name, email, endpoint, status, setup, created, updated, token, my_rules)
        })
        .collect())
}

/// All four OAuth credentials for one integration, by integration id. Returns
/// `(consumer_key, consumer_secret, access_token, access_secret, revoked)` — the access
/// pair is `None` until the integration is activated. `None` = no consumer at all. This
/// is the ONLY code path that selects the token secrets — kept separate so they can't
/// leak into the normal listing.
#[allow(clippy::type_complexity)]
pub(crate) fn fetch_integration_secrets(
    conn: &DbConnection,
    table_prefix: &str,
    integration_id: u32,
) -> Result<Option<(String, String, Option<String>, Option<String>, bool)>, String> {
    use mysql::params;
    use mysql::prelude::Queryable;
    let mut c = connect(conn)?;
    let p = table_prefix;
    // `key` is reserved — backtick it. oauth_token is LEFT-joined: consumer key/secret
    // exist at creation, the access token/secret only after activation.
    let row: Option<(String, String, Option<String>, Option<String>, Option<i64>)> = c
        .exec_first(
            format!(
                "SELECT c.`key`, c.secret, t.token, t.secret, t.revoked \
                 FROM {p}integration i \
                 JOIN {p}oauth_consumer c ON c.entity_id = i.consumer_id \
                 LEFT JOIN {p}oauth_token t \
                   ON t.consumer_id = i.consumer_id AND t.type = 'access' \
                 WHERE i.integration_id = :v"
            ),
            params! { "v" => integration_id },
        )
        .map_err(clean_err)?;
    Ok(row.map(|(ck, cs, tok, tsec, rev)| (ck, cs, tok, tsec, rev.unwrap_or(0) != 0)))
}
