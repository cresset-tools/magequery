//! Live database queries for the config domain.

use super::*;

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
