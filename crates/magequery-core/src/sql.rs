//! `magequery sql`: plain SQL over Magento-aware virtual tables.
//!
//! The agent writes ordinary MySQL. Each *virtual table* it names (`orders`,
//! `order_items`, …) is prepended as a `WITH` CTE over the real schema, carrying the
//! knowledge the live cards otherwise keep in Rust glue: the table prefix, which order
//! lines are real lines, the store and address joins. Nothing else in the statement is
//! rewritten — real tables pass through untouched.
//!
//! No SQL parser: a small tokenizer that respects strings, quoted identifiers and comments
//! is enough to (a) refuse anything but a single read statement, (b) find which virtual
//! tables and optional columns are referenced, and (c) know whether a top-level `LIMIT`
//! exists. Reference detection is by token, so it over-includes (an unused CTE or join is
//! harmless) but can never miss a use.

use std::collections::BTreeSet;

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------------------
// Virtual table definitions
// ---------------------------------------------------------------------------------------

/// A join an optional virtual column needs. `sql` uses `{p}` for the table prefix.
pub(crate) struct Join {
    pub alias: &'static str,
    pub sql: &'static str,
}

/// A column the virtual table adds on top of its base table. Its joins are only emitted
/// when the column name appears in the statement (or a `*` selects everything). `expr`
/// may use `{p}` for the table prefix.
pub(crate) struct VirtualColumn {
    pub name: &'static str,
    pub expr: &'static str,
    pub joins: &'static [&'static Join],
    pub note: &'static str,
}

pub(crate) struct VirtualTable {
    pub name: &'static str,
    /// What one row is, in words.
    pub grain: &'static str,
    /// The real table (unprefixed) every row comes from, and its alias in the CTE.
    pub base: &'static str,
    pub alias: &'static str,
    /// A `WHERE` over the base table (alias-qualified), shown in `--describe`.
    pub filter: Option<&'static str>,
    pub columns: &'static [VirtualColumn],
    /// Caveats on base-table and attribute columns, printed next to them in `--describe`.
    pub notes: &'static [(&'static str, &'static str)],
    /// Every EAV attribute of this entity type becomes a column (live metadata).
    pub eav: Option<&'static EavSpec>,
}

pub(crate) struct EavSpec {
    /// The `eav_entity_type` code whose attributes become columns.
    pub entity_type: &'static str,
    /// The attributes `*` selects. Any other attribute is selected by naming it; `*`
    /// cannot mean all of them (there are hundreds, each a subquery per row).
    pub common: &'static [&'static str],
}

/// One EAV attribute as a virtual column, from the live `eav_attribute` row.
#[derive(Debug, Clone)]
pub(crate) struct EavColumn {
    pub code: String,
    pub attribute_id: u32,
    /// `varchar`, `int`, `decimal`, `text`, `datetime`.
    pub backend_type: String,
    /// The value table, unprefixed (`catalog_product_entity_varchar`).
    pub value_table: String,
    pub label: Option<String>,
    pub options: OptionLabels,
}

/// Whether an attribute's values are option ids with labels in
/// `eav_attribute_option_value`, and so offer a label column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OptionLabels {
    None,
    /// A `select`: one option id; `<code>_label` is its label.
    Single,
    /// A `multiselect`: comma-separated option ids; `<code>_labels` is a JSON array.
    Multi,
}

impl OptionLabels {
    /// The label column's name for attribute `code`, if it has one.
    pub fn column(self, code: &str) -> Option<String> {
        match self {
            OptionLabels::None => None,
            OptionLabels::Single => Some(format!("{code}_label")),
            OptionLabels::Multi => Some(format!("{code}_labels")),
        }
    }
}

/// Live facts that EAV-backed virtual tables need, loaded by the caller only when
/// [`eav_entities`] says the statement uses one.
#[derive(Debug, Clone, Default)]
pub(crate) struct Context {
    /// Non-static attributes per entity type code.
    pub attributes: std::collections::BTreeMap<String, Vec<EavColumn>>,
    /// The store view attribute values resolve for (falling back to store 0).
    pub store_id: u32,
}

const J_STORE: Join = Join {
    alias: "s",
    sql: "LEFT JOIN {p}store s ON s.store_id = o.store_id",
};
const J_BILLING: Join = Join {
    alias: "ba",
    sql: "LEFT JOIN {p}sales_order_address ba ON ba.parent_id = o.entity_id AND ba.address_type = 'billing'",
};
const J_SHIPPING: Join = Join {
    alias: "sa",
    sql: "LEFT JOIN {p}sales_order_address sa ON sa.parent_id = o.entity_id AND sa.address_type = 'shipping'",
};
const J_STATUS: Join = Join {
    alias: "st",
    sql: "LEFT JOIN {p}sales_order_status st ON st.status = o.status",
};
const J_GROUP: Join = Join {
    alias: "cg",
    sql: "LEFT JOIN {p}customer_group cg ON cg.customer_group_id = o.customer_group_id",
};
// A configurable line has exactly one child (the chosen simple); bundles have many, so
// the join is restricted to configurables to keep one row per visible line.
const J_CHILD: Join = Join {
    alias: "c",
    sql: "LEFT JOIN {p}sales_order_item c ON c.parent_item_id = i.item_id AND i.product_type = 'configurable'",
};

const J_STOCK: Join = Join {
    alias: "si",
    sql: "LEFT JOIN {p}cataloginventory_stock_item si ON si.product_id = e.entity_id AND si.stock_id = 1",
};
const J_ATTR_SET: Join = Join {
    alias: "aset",
    sql: "LEFT JOIN {p}eav_attribute_set aset ON aset.attribute_set_id = e.attribute_set_id",
};

pub(crate) const VIRTUAL_TABLES: &[VirtualTable] = &[
    VirtualTable {
        name: "orders",
        grain: "one row per order",
        base: "sales_order",
        alias: "o",
        filter: None,
        columns: &[
            VirtualColumn {
                name: "is_revenue",
                expr: "o.state IN ('processing', 'complete')",
                joins: &[],
                note: "1 for processing/complete orders: paid (or invoiced) and not cancelled or \
                       fully refunded. Partial refunds are not subtracted (see base_total_refunded). \
                       Filter on `state` yourself for another definition",
            },
            VirtualColumn {
                name: "store_code",
                expr: "s.code",
                joins: &[&J_STORE],
                note: "the store view the order was placed in",
            },
            VirtualColumn {
                name: "website_id",
                expr: "s.website_id",
                joins: &[&J_STORE],
                note: "the website of store_code; base_* amounts are in its base currency when the \
                       price scope (catalog/price/scope) is Website, else in the global one",
            },
            VirtualColumn {
                name: "billing_country",
                expr: "ba.country_id",
                joins: &[&J_BILLING],
                note: "ISO-2 country of the billing address",
            },
            VirtualColumn {
                name: "shipping_country",
                expr: "sa.country_id",
                joins: &[&J_SHIPPING],
                note: "ISO-2 country of the shipping address; NULL for virtual-only orders",
            },
            VirtualColumn {
                name: "status_label",
                expr: "st.label",
                joins: &[&J_STATUS],
                note: "the admin label of `status`",
            },
            VirtualColumn {
                name: "customer_group_code",
                expr: "cg.customer_group_code",
                joins: &[&J_GROUP],
                note: "the customer group at order time",
            },
        ],
        notes: &[
            ("entity_id", "join order_items.order_id on this"),
            ("increment_id", "the order number customers see; unique per store, not globally"),
            ("state", "the lifecycle: new, pending_payment, processing, complete, closed, canceled, holded, payment_review"),
            ("status", "a label mapped onto a state; custom statuses exist, prefer `state` in filters"),
            ("created_at", "UTC. The store's timezone is config general/locale/timezone"),
            ("customer_id", "NULL for guest orders (customer_is_guest = 1); group by customer_email to include guests"),
            ("base_grand_total", "in the website base currency: sum this across stores"),
            ("grand_total", "in the order currency (order_currency_code): never sum across currencies"),
            ("base_subtotal", "excl. tax and shipping, before discount"),
            ("base_total_refunded", "credit-memoed so far, base currency"),
        ],
        eav: None,
    },
    VirtualTable {
        name: "order_items",
        grain: "one row per order line as the customer saw it",
        base: "sales_order_item",
        alias: "i",
        filter: Some("i.parent_item_id IS NULL"),
        columns: &[VirtualColumn {
            name: "simple_product_id",
            expr: "c.product_id",
            joins: &[&J_CHILD],
            note: "on a configurable line, the chosen variant's product id; NULL otherwise",
        }],
        notes: &[
            ("order_id", "join orders.entity_id on this"),
            ("parent_item_id", "always NULL here: the child rows of configurables and bundles are hidden, so summing qty never double-counts. Query sales_order_item for them"),
            ("sku", "on a configurable line this is the chosen variant's SKU"),
            ("product_id", "on a configurable line this is the configurable parent"),
            ("qty_ordered", "units on this line; see qty_invoiced / qty_shipped / qty_refunded"),
            ("base_row_total", "base currency, excl. tax, before discount; subtract base_discount_amount for net"),
            ("created_at", "UTC"),
        ],
        eav: None,
    },
    VirtualTable {
        name: "products",
        grain: "one row per product; every attribute is a column",
        base: "catalog_product_entity",
        alias: "e",
        filter: None,
        columns: &[
            VirtualColumn {
                name: "qty",
                expr: "si.qty",
                joins: &[&J_STOCK],
                note: "default-stock quantity (cataloginventory_stock_item). With MSI and several \
                       sources, per-source quantities are in inventory_source_item",
            },
            VirtualColumn {
                name: "is_in_stock",
                expr: "si.is_in_stock",
                joins: &[&J_STOCK],
                note: "the default-stock status flag: 1 in stock, 0 out of stock",
            },
            VirtualColumn {
                name: "attribute_set_name",
                expr: "aset.attribute_set_name",
                joins: &[&J_ATTR_SET],
                note: "the product's attribute set",
            },
            VirtualColumn {
                name: "website_ids",
                expr: "(SELECT GROUP_CONCAT(pw.website_id ORDER BY pw.website_id) \
                       FROM {p}catalog_product_website pw WHERE pw.product_id = e.entity_id)",
                joins: &[],
                note: "comma-separated websites the product is assigned to; \
                       filter with FIND_IN_SET('1', website_ids)",
            },
        ],
        notes: &[
            ("entity_id", "order_items.product_id joins on this (the parent, for configurable lines)"),
            ("sku", "unique; order lines keep their own copy in order_items.sku"),
            ("type_id", "simple, configurable, bundle, grouped, virtual, downloadable"),
            ("name", "resolved for --store (falling back to the default value)"),
            ("status", "1 enabled, 2 disabled; can differ per store view, so pass --store"),
            ("visibility", "1 not visible individually, 2 catalog, 3 search, 4 catalog and search"),
            ("price", "the base price attribute. Special, tier, and catalog-rule prices are \
                       separate: `magequery price <sku>` shows them all"),
            ("special_price", "applies only between special_from_date and special_to_date"),
            ("url_key", "resolved for --store"),
        ],
        eav: Some(&EavSpec {
            entity_type: "catalog_product",
            common: &["name", "status", "visibility", "price", "special_price", "url_key"],
        }),
    },
];

pub(crate) fn virtual_table(name: &str) -> Option<&'static VirtualTable> {
    VIRTUAL_TABLES
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
}

// ---------------------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// An unquoted word: keyword, identifier, or function name (lowercased in `text`).
    Word,
    /// A `backtick` identifier (unescaped in `text`, case kept).
    Quoted,
    Str,
    Number,
    /// One punctuation/operator char, or `@`-variable.
    Punct,
}

#[derive(Debug, Clone)]
struct Token {
    kind: Kind,
    text: String,
    end: usize,
}

impl Token {
    fn is_word(&self, w: &str) -> bool {
        self.kind == Kind::Word && self.text == w
    }
    fn is_punct(&self, c: &str) -> bool {
        self.kind == Kind::Punct && self.text == c
    }
}

fn tokenize(src: &str) -> Result<Vec<Token>> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'#'
            || (c == b'-'
                && b.get(i + 1) == Some(&b'-')
                && b.get(i + 2).is_none_or(|n| n.is_ascii_whitespace()))
        {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            if matches!(b.get(i + 2), Some(b'!') | Some(b'+')) {
                // `/*! … */` is executed by MySQL and `/*+ … */` is an optimizer hint:
                // neither is a comment we can safely skip.
                return Err(sql_err(
                    "executable comments (/*! */, /*+ */) are not allowed",
                ));
            }
            match src[i + 2..].find("*/") {
                Some(off) => i += 2 + off + 2,
                None => return Err(sql_err("unterminated /* comment")),
            }
        } else if c == b'\'' || c == b'"' || c == b'`' {
            let (text, next) = quoted(src, i, c)?;
            let kind = if c == b'`' { Kind::Quoted } else { Kind::Str };
            out.push(Token {
                kind,
                text,
                end: next,
            });
            i = next;
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                i += 1;
            }
            out.push(Token {
                kind: Kind::Number,
                text: src[start..i].to_string(),
                end: i,
            });
        } else if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 {
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80)
            {
                i += 1;
            }
            let text = src[start..i].to_ascii_lowercase();
            out.push(Token {
                kind: Kind::Word,
                text,
                end: i,
            });
        } else {
            // One char per punct token; multi-char operators (<=, :=, ||) are irrelevant to
            // what we look for. Advance by the whole UTF-8 char.
            let ch = src[i..].chars().next().unwrap_or('?');
            i += ch.len_utf8();
            out.push(Token {
                kind: Kind::Punct,
                text: ch.to_string(),
                end: i,
            });
        }
    }
    Ok(out)
}

/// A quoted run starting at `i` with quote char `q`: doubled quotes and (for strings)
/// backslash escapes. Returns the unescaped text and the index after the closing quote.
fn quoted(src: &str, i: usize, q: u8) -> Result<(String, usize)> {
    let b = src.as_bytes();
    let mut j = i + 1;
    let mut text = Vec::new();
    while j < b.len() {
        let c = b[j];
        if c == b'\\' && q != b'`' && j + 1 < b.len() {
            text.push(b[j + 1]);
            j += 2;
        } else if c == q {
            if b.get(j + 1) == Some(&q) {
                text.push(q);
                j += 2;
            } else {
                return Ok((String::from_utf8_lossy(&text).into_owned(), j + 1));
            }
        } else {
            text.push(c);
            j += 1;
        }
    }
    Err(sql_err(&format!("unterminated {} quote", q as char)))
}

fn sql_err(msg: &str) -> Error {
    Error::Sql(msg.to_string())
}

// ---------------------------------------------------------------------------------------
// Guard + expansion
// ---------------------------------------------------------------------------------------

/// The statement magequery will run, and what it learned about it.
#[derive(Debug, Clone)]
pub(crate) struct Expanded {
    pub sql: String,
    /// Virtual tables that were expanded, in definition order.
    pub tables: Vec<&'static str>,
    /// The row cap appended as `LIMIT cap + 1` (to detect truncation); `None` when the
    /// statement has its own top-level `LIMIT` or the caller lifted the cap.
    pub cap: Option<u64>,
}

/// What the guard and reference scan learned about a statement.
struct Analysis<'s> {
    toks: Vec<Token>,
    /// The statement without trailing semicolons and comments.
    body: &'s str,
    starts_with_with: bool,
    used: Vec<&'static VirtualTable>,
    columns: BTreeSet<String>,
    star: bool,
    has_limit: bool,
}

/// The EAV entity types the virtual tables in `stmt` need attribute metadata for. Also
/// runs the guard, so a refused statement fails here before any database lookup.
pub(crate) fn eav_entities(stmt: &str) -> Result<Vec<&'static str>> {
    Ok(analyze(stmt)?
        .used
        .iter()
        .filter_map(|vt| vt.eav.map(|e| e.entity_type))
        .collect())
}

/// Validate `stmt` as a single read statement and expand the virtual tables it names.
/// `prefix` is `env.php`'s table prefix; `limit` caps the rows unless the statement has
/// its own top-level `LIMIT` (`None` = no cap); `ctx` carries the live attribute
/// metadata for EAV tables (see [`eav_entities`]).
pub(crate) fn expand(
    stmt: &str,
    prefix: &str,
    limit: Option<u64>,
    ctx: &Context,
) -> Result<Expanded> {
    let a = analyze(stmt)?;
    let cap = if a.has_limit { None } else { limit };
    let ctes = a
        .used
        .iter()
        .map(|vt| {
            Ok(format!(
                "{} AS (\n{}\n)",
                vt.name,
                cte_body(vt, prefix, &a, ctx)?
            ))
        })
        .collect::<Result<Vec<String>>>()?;
    let (toks, body) = (&a.toks, a.body);
    let mut sql = if ctes.is_empty() {
        body.to_string()
    } else if a.starts_with_with {
        // Splice ours in front of the statement's own CTEs, keeping RECURSIVE (which
        // applies to the whole list) where the author put it.
        let with = toks.iter().position(|t| t.is_word("with")).unwrap_or(0);
        let mut after = toks[with].end;
        if toks.get(with + 1).is_some_and(|t| t.is_word("recursive")) {
            after = toks[with + 1].end;
        }
        format!(
            "{} {},\n{}",
            &body[..after],
            ctes.join(",\n"),
            body[after..].trim_start()
        )
    } else {
        format!("WITH {}\n{}", ctes.join(",\n"), body)
    };
    if let Some(n) = cap {
        sql.push_str(&format!("\nLIMIT {}", n.saturating_add(1)));
    }
    Ok(Expanded {
        sql,
        tables: a.used.iter().map(|vt| vt.name).collect(),
        cap,
    })
}

fn analyze(stmt: &str) -> Result<Analysis<'_>> {
    let mut toks = tokenize(stmt)?;
    // Trailing semicolons are fine; any other one means a second statement.
    while toks.last().is_some_and(|t| t.is_punct(";")) {
        toks.pop();
    }
    if toks.iter().any(|t| t.is_punct(";")) {
        return Err(sql_err("one statement only"));
    }
    let body = &stmt[..toks.last().map_or(0, |t| t.end)];
    let first = toks
        .iter()
        .find(|t| !t.is_punct("("))
        .ok_or_else(|| sql_err("empty statement"))?;
    if !(first.is_word("select") || first.is_word("with")) {
        return Err(sql_err(&format!(
            "only SELECT (or WITH … SELECT) is allowed, not {}",
            first.text.to_uppercase()
        )));
    }
    for (k, t) in toks.iter().enumerate() {
        let next = toks.get(k + 1);
        let bad = match t.text.as_str() {
            _ if t.kind != Kind::Word => None,
            "into" => Some("SELECT … INTO"),
            "update" | "delete" | "insert" | "replace" | "lock"
                if !next.is_some_and(|n| n.is_punct("(")) =>
            {
                Some("writes and locking reads")
            }
            "for" if next.is_some_and(|n| n.is_word("share")) => Some("locking reads"),
            _ => None,
        };
        if let Some(what) = bad {
            return Err(sql_err(&format!(
                "{what} not allowed: magequery sql is read-only"
            )));
        }
    }

    // A virtual table is referenced by a bare, unqualified word (`shop.orders` and
    // `` `orders` `` are real tables). Columns are referenced by any word or quoted name,
    // qualified or not (`o.is_revenue`). `*` (other than `COUNT(*)`) selects every column.
    let mut tables = BTreeSet::new();
    let mut columns = BTreeSet::new();
    let mut star = false;
    for (k, t) in toks.iter().enumerate() {
        let prev = k.checked_sub(1).map(|p| &toks[p]);
        match t.kind {
            Kind::Word => {
                if !prev.is_some_and(|p| p.is_punct(".")) {
                    tables.insert(t.text.clone());
                }
                columns.insert(t.text.clone());
            }
            Kind::Quoted => {
                columns.insert(t.text.to_ascii_lowercase());
            }
            Kind::Punct if t.text == "*" && !prev.is_some_and(|p| p.is_punct("(")) => star = true,
            _ => {}
        }
    }

    let own_ctes = if first.is_word("with") {
        cte_names(&toks)
    } else {
        BTreeSet::new()
    };
    let used: Vec<&'static VirtualTable> = VIRTUAL_TABLES
        .iter()
        .filter(|vt| tables.contains(vt.name) && !own_ctes.contains(vt.name))
        .collect();

    let has_limit = {
        let mut depth = 0i32;
        toks.iter().any(|t| {
            if t.is_punct("(") {
                depth += 1;
            } else if t.is_punct(")") {
                depth -= 1;
            }
            depth == 0 && t.is_word("limit")
        })
    };
    let starts_with_with = first.is_word("with");
    Ok(Analysis {
        toks,
        body,
        starts_with_with,
        used,
        columns,
        star,
        has_limit,
    })
}

/// The CTE names a `WITH` statement defines itself: `WITH [RECURSIVE] a [(cols)] AS (…), b …`.
fn cte_names(toks: &[Token]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let Some(mut k) = toks.iter().position(|t| t.is_word("with")) else {
        return names;
    };
    k += 1;
    if toks.get(k).is_some_and(|t| t.is_word("recursive")) {
        k += 1;
    }
    while let Some(name) = toks.get(k) {
        names.insert(name.text.to_ascii_lowercase());
        k += 1;
        // Skip an optional column list, then `AS`, then the parenthesized body.
        if toks.get(k).is_some_and(|t| t.is_punct("(")) {
            k = skip_parens(toks, k);
        }
        if toks.get(k).is_some_and(|t| t.is_word("as")) {
            k += 1;
        }
        if toks.get(k).is_some_and(|t| t.is_punct("(")) {
            k = skip_parens(toks, k);
        }
        if toks.get(k).is_some_and(|t| t.is_punct(",")) {
            k += 1;
        } else {
            break;
        }
    }
    names
}

/// Index just past the parenthesized group opening at `k`.
fn skip_parens(toks: &[Token], mut k: usize) -> usize {
    let mut depth = 0;
    while let Some(t) = toks.get(k) {
        if t.is_punct("(") {
            depth += 1;
        } else if t.is_punct(")") {
            depth -= 1;
            if depth == 0 {
                return k + 1;
            }
        }
        k += 1;
    }
    k
}

fn cte_body(vt: &VirtualTable, prefix: &str, a: &Analysis, ctx: &Context) -> Result<String> {
    let wanted: Vec<&VirtualColumn> = vt
        .columns
        .iter()
        .filter(|c| a.star || a.columns.contains(c.name))
        .collect();
    let mut select = vec![format!("{}.*", vt.alias)];
    select.extend(
        wanted
            .iter()
            .map(|c| format!("{} AS {}", c.expr.replace("{p}", prefix), c.name)),
    );
    if let Some(spec) = vt.eav {
        let attrs = ctx.attributes.get(spec.entity_type).ok_or_else(|| {
            sql_err(&format!(
                "`{}` reads its attribute list from the database, which is not reachable",
                vt.name
            ))
        })?;
        for attr in attrs {
            // A fixed column of the same name wins; it is what --describe documents.
            if vt.columns.iter().any(|c| c.name == attr.code) {
                continue;
            }
            let label = attr
                .options
                .column(&attr.code)
                .filter(|l| a.columns.contains(l));
            let want_value = a.columns.contains(&attr.code)
                || (a.star && spec.common.contains(&attr.code.as_str()));
            if !want_value && label.is_none() {
                continue;
            }
            let value = attr_value(attr, vt.alias, prefix, ctx.store_id);
            if want_value {
                select.push(format!("{value} AS {}", quote_ident(&attr.code)));
            }
            if let Some(label) = label {
                let expr = option_labels(attr, &value, prefix, ctx.store_id);
                select.push(format!("{expr} AS {}", quote_ident(&label)));
            }
        }
    }
    let mut joins: Vec<&Join> = Vec::new();
    for c in &wanted {
        for j in c.joins {
            if !joins.iter().any(|seen| seen.alias == j.alias) {
                joins.push(j);
            }
        }
    }
    let mut out = format!(
        "  SELECT {}\n  FROM {prefix}{} {}",
        select.join(",\n    "),
        vt.base,
        vt.alias
    );
    for j in joins {
        out.push_str("\n  ");
        out.push_str(&j.sql.replace("{p}", prefix));
    }
    if let Some(f) = vt.filter {
        out.push_str(&format!("\n  WHERE {f}"));
    }
    Ok(out)
}

/// One attribute's value for the row, as a scalar subquery on the value table's unique
/// `(entity_id, attribute_id, store_id)` key: the store view's own row when it has one,
/// else the default (store 0). Subqueries rather than joins: a product has hundreds of
/// attributes and MySQL allows 61 tables per join.
fn attr_value(attr: &EavColumn, alias: &str, prefix: &str, store_id: u32) -> String {
    format!(
        "(SELECT v.value FROM {prefix}{} v WHERE v.entity_id = {alias}.entity_id \
         AND v.attribute_id = {} AND {})",
        attr.value_table,
        attr.attribute_id,
        store_scope("v", store_id)
    )
}

/// The label column of an option attribute whose raw value is `value`. A `select` gives
/// the one option's label; a `multiselect` gives a JSON array of labels in the admin's
/// option order (not selection order), built with `JSON_QUOTE` + `GROUP_CONCAT` because
/// `JSON_ARRAYAGG` is missing from MariaDB 10.4. Labels fall back per option, like
/// values. NULL when no option is set.
fn option_labels(attr: &EavColumn, value: &str, prefix: &str, store_id: u32) -> String {
    let label = |option: &str| {
        format!(
            "(SELECT ov.value FROM {prefix}eav_attribute_option_value ov \
             WHERE ov.option_id = {option} AND {})",
            store_scope("ov", store_id)
        )
    };
    match attr.options {
        OptionLabels::Multi => format!(
            "(SELECT CONCAT('[', GROUP_CONCAT(JSON_QUOTE({}) ORDER BY o.sort_order, o.option_id \
             SEPARATOR ','), ']') FROM {prefix}eav_attribute_option o \
             WHERE o.attribute_id = {} AND FIND_IN_SET(o.option_id, {value}))",
            label("o.option_id"),
            attr.attribute_id
        ),
        _ => label(value),
    }
}

/// The store condition of a store-scoped value lookup (one row per store): the default
/// row only, or the store's row preferred over the default.
fn store_scope(alias: &str, store_id: u32) -> String {
    if store_id == 0 {
        format!("{alias}.store_id = 0")
    } else {
        format!("{alias}.store_id IN (0, {store_id}) ORDER BY {alias}.store_id DESC LIMIT 1")
    }
}

fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(stmt: &str) -> Expanded {
        expand(stmt, "", Some(200), &catalog(0)).unwrap_or_else(|e| panic!("{stmt}: {e}"))
    }

    /// A context with a few product attributes, as the live `eav_attribute` rows give.
    fn catalog(store_id: u32) -> Context {
        let attr = |code: &str, id: u32, backend: &str, labels: OptionLabels| EavColumn {
            code: code.to_string(),
            attribute_id: id,
            backend_type: backend.to_string(),
            value_table: format!("catalog_product_entity_{backend}"),
            label: None,
            options: labels,
        };
        let mut ctx = Context {
            store_id,
            ..Default::default()
        };
        ctx.attributes.insert(
            "catalog_product".to_string(),
            vec![
                attr("name", 73, "varchar", OptionLabels::None),
                attr("status", 97, "int", OptionLabels::None),
                attr("price", 77, "decimal", OptionLabels::None),
                attr("color", 93, "int", OptionLabels::Single),
                attr("material", 140, "varchar", OptionLabels::Multi),
                attr("qty", 999, "decimal", OptionLabels::None),
                attr("group", 500, "varchar", OptionLabels::None),
            ],
        );
        ctx
    }

    fn refused(stmt: &str) -> String {
        match expand(stmt, "", Some(200), &catalog(0)) {
            Err(e) => e.to_string(),
            Ok(x) => panic!("accepted: {}", x.sql),
        }
    }

    #[test]
    fn plain_sql_passes_through_with_a_limit() {
        let x = ok("SELECT 1;");
        assert_eq!(x.sql, "SELECT 1\nLIMIT 201");
        assert!(x.tables.is_empty());
        assert_eq!(x.cap, Some(200));
    }

    #[test]
    fn own_top_level_limit_is_respected() {
        let x = ok("SELECT * FROM orders ORDER BY entity_id DESC LIMIT 5");
        assert!(x.sql.ends_with("LIMIT 5"));
        assert_eq!(x.cap, None);
        // A LIMIT inside a subquery is not the statement's.
        let x = ok("SELECT * FROM (SELECT * FROM sales_order LIMIT 5) t");
        assert_eq!(x.cap, Some(200));
        // No cap requested.
        assert!(!expand("SELECT 1", "", None, &Context::default())
            .unwrap()
            .sql
            .contains("LIMIT"));
    }

    #[test]
    fn virtual_tables_become_prefixed_ctes() {
        let x = expand(
            "SELECT COUNT(*) FROM orders",
            "m2_",
            Some(10),
            &Context::default(),
        )
        .unwrap();
        assert_eq!(x.tables, vec!["orders"]);
        assert!(x
            .sql
            .starts_with("WITH orders AS (\n  SELECT o.*\n  FROM m2_sales_order o\n)"));
        assert!(x.sql.ends_with("SELECT COUNT(*) FROM orders\nLIMIT 11"));
    }

    #[test]
    fn optional_joins_only_when_their_column_is_named() {
        let x = ok("SELECT shipping_country, SUM(base_grand_total) FROM orders GROUP BY 1");
        assert!(x.sql.contains("sa.country_id AS shipping_country"));
        assert!(x.sql.contains("address_type = 'shipping'"));
        assert!(!x.sql.contains("address_type = 'billing'"));
        assert!(!x.sql.contains("sales_order_status"));
        // store_code and website_id share one join.
        let x = ok("SELECT store_code, website_id FROM orders");
        assert_eq!(x.sql.matches("LEFT JOIN store s").count(), 1);
    }

    #[test]
    fn qualified_and_quoted_column_names_count() {
        let x =
            ok("SELECT o.increment_id FROM orders o WHERE o.is_revenue AND o.`store_code` = 'nl'");
        assert!(x.sql.contains("AS is_revenue") && x.sql.contains("AS store_code"));
        assert!(!x.sql.contains("AS status_label"));
    }

    #[test]
    fn star_selects_every_virtual_column_but_count_star_does_not() {
        let x = ok("SELECT * FROM orders");
        assert!(x.sql.contains("AS status_label") && x.sql.contains("AS customer_group_code"));
        let x = ok("SELECT o.* FROM orders o");
        assert!(x.sql.contains("AS status_label"));
        let x = ok("SELECT COUNT(*) FROM orders");
        assert!(!x.sql.contains("AS status_label"));
    }

    #[test]
    fn order_items_hide_child_lines() {
        let x = ok("SELECT sku, SUM(qty_ordered) FROM order_items GROUP BY sku");
        assert!(x
            .sql
            .contains("FROM sales_order_item i\n  WHERE i.parent_item_id IS NULL"));
        assert!(!x.sql.contains("LEFT JOIN"));
        let x = ok("SELECT simple_product_id FROM order_items");
        assert!(x.sql.contains("i.product_type = 'configurable'"));
    }

    #[test]
    fn words_in_strings_comments_and_qualified_tables_do_not_count() {
        let x = ok("SELECT 'orders' AS x -- orders\n FROM sales_order /* order_items */");
        assert!(x.tables.is_empty());
        assert!(ok("SELECT * FROM shop.orders").tables.is_empty());
        // A backticked name is the real table, never the virtual one.
        assert!(ok("SELECT * FROM `orders`").tables.is_empty());
    }

    #[test]
    fn with_statements_get_ours_spliced_in_front() {
        let x = ok(
            "WITH recent AS (SELECT * FROM orders WHERE created_at > NOW() - INTERVAL 1 DAY) \
                    SELECT COUNT(*) FROM recent",
        );
        assert!(x.sql.starts_with("WITH orders AS (\n"));
        assert!(x.sql.contains("\n),\nrecent AS (SELECT"));
        let x = ok(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 3) \
                    SELECT * FROM n JOIN orders ON orders.entity_id = n.i",
        );
        assert!(x.sql.starts_with("WITH RECURSIVE orders AS (\n"));
        // The author's own `orders` CTE wins over the virtual one.
        let x = ok("WITH orders AS (SELECT 1 AS a) SELECT * FROM orders");
        assert!(x.tables.is_empty());
    }

    #[test]
    fn writes_and_multiple_statements_are_refused() {
        assert!(refused("UPDATE sales_order SET state = 'x'").contains("only SELECT"));
        assert!(refused("SELECT 1; DROP TABLE sales_order").contains("one statement"));
        assert!(refused("SELECT * FROM sales_order INTO OUTFILE '/tmp/x'").contains("INTO"));
        assert!(refused("SELECT * FROM sales_order FOR UPDATE").contains("read-only"));
        assert!(refused("SELECT * FROM sales_order FOR SHARE").contains("read-only"));
        assert!(refused("SELECT * FROM sales_order LOCK IN SHARE MODE").contains("read-only"));
        assert!(refused("WITH x AS (SELECT 1) DELETE FROM sales_order").contains("read-only"));
        assert!(refused("SELECT /*! SLEEP(1) */ 1").contains("executable"));
        assert!(refused("SELECT 'unterminated").contains("unterminated"));
        // String functions that share a name with a statement are fine.
        ok("SELECT REPLACE(sku, '-', ''), INSERT(sku, 1, 0, 'x') FROM sales_order_item");
    }

    #[test]
    fn product_attributes_are_subqueries_on_the_default_row() {
        let x = ok("SELECT sku, name, price FROM products WHERE status = 1");
        assert_eq!(x.tables, vec!["products"]);
        assert!(x.sql.contains(
            "(SELECT v.value FROM catalog_product_entity_varchar v WHERE v.entity_id = e.entity_id \
             AND v.attribute_id = 73 AND v.store_id = 0) AS `name`"
        ));
        assert!(x
            .sql
            .contains("AND v.attribute_id = 97 AND v.store_id = 0) AS `status`"));
        assert!(!x.sql.contains("AS `color`"));
        assert!(!x.sql.contains("LEFT JOIN"));
    }

    #[test]
    fn store_view_values_fall_back_to_the_default() {
        let x = expand("SELECT name FROM products", "", None, &catalog(2)).unwrap();
        assert!(x
            .sql
            .contains("AND v.store_id IN (0, 2) ORDER BY v.store_id DESC LIMIT 1) AS `name`"));
    }

    #[test]
    fn option_labels_resolve_through_the_option_table() {
        let x = expand(
            "SELECT sku, color_label FROM products",
            "m_",
            None,
            &catalog(1),
        )
        .unwrap();
        assert!(x
            .sql
            .contains("FROM m_eav_attribute_option_value ov WHERE ov.option_id = (SELECT"));
        assert!(x
            .sql
            .contains("ov.store_id IN (0, 1) ORDER BY ov.store_id DESC LIMIT 1) AS `color_label`"));
        // The label alone does not select the raw option id.
        assert!(!x.sql.contains("AS `color`,"));
        assert!(!x.sql.ends_with("AS `color`"));
    }

    #[test]
    fn multiselect_labels_are_a_json_array_in_option_order() {
        let x = expand(
            "SELECT sku, material_labels FROM products",
            "",
            None,
            &catalog(2),
        )
        .unwrap();
        assert!(x.sql.contains(
            "(SELECT CONCAT('[', GROUP_CONCAT(JSON_QUOTE((SELECT ov.value FROM \
             eav_attribute_option_value ov WHERE ov.option_id = o.option_id AND ov.store_id IN \
             (0, 2) ORDER BY ov.store_id DESC LIMIT 1)) ORDER BY o.sort_order, o.option_id \
             SEPARATOR ','), ']') FROM eav_attribute_option o WHERE o.attribute_id = 140 AND \
             FIND_IN_SET(o.option_id, (SELECT v.value FROM catalog_product_entity_varchar v"
        ));
        assert!(x.sql.ends_with("AS `material_labels`\n  FROM catalog_product_entity e\n)\nSELECT sku, material_labels FROM products"));
        // The single-select spelling is not a column of a multiselect, and vice versa.
        assert!(!ok("SELECT material_label FROM products")
            .sql
            .contains("AS `material_label`"));
        assert!(!ok("SELECT color_labels FROM products")
            .sql
            .contains("AS `color_labels`"));
    }

    #[test]
    fn product_fixed_columns_and_star() {
        let x = ok("SELECT sku, qty, is_in_stock FROM products WHERE qty < 5");
        // A fixed column beats an attribute of the same name.
        assert!(x.sql.contains("si.qty AS qty"));
        assert!(!x.sql.contains("attribute_id = 999"));
        assert_eq!(x.sql.matches("cataloginventory_stock_item si").count(), 1);
        let x = ok("SELECT * FROM products");
        for col in [
            "AS `name`",
            "AS `status`",
            "AS `price`",
            "AS qty",
            "AS website_ids",
        ] {
            assert!(x.sql.contains(col), "{col}");
        }
        // `*` is the common set, not every attribute.
        assert!(!x.sql.contains("AS `color`"));
        // Codes are quoted, so a reserved word is a fine attribute.
        assert!(ok("SELECT `group` FROM products")
            .sql
            .contains("AS `group`"));
    }

    #[test]
    fn eav_tables_need_the_attribute_list() {
        let e = expand("SELECT name FROM products", "", None, &Context::default()).unwrap_err();
        assert!(e.to_string().contains("attribute list"));
        assert_eq!(
            eav_entities("SELECT * FROM products p JOIN orders o").unwrap(),
            vec!["catalog_product"]
        );
        assert!(eav_entities("SELECT * FROM orders").unwrap().is_empty());
        assert!(eav_entities("DELETE FROM products").is_err());
    }

    /// The skill teaches `magequery sql` by example; every example it shows must at least
    /// pass the guard and expand. (Running them needs a database: see the plan.)
    #[test]
    fn skill_examples_expand() {
        let skill = include_str!("../../../assets/skill/SKILL.md");
        let examples = skill_sql_examples(skill);
        assert!(
            examples.len() >= 3,
            "expected the skill to show sql examples"
        );
        for stmt in examples {
            let x = ok(&stmt);
            assert!(
                !x.tables.is_empty(),
                "example uses no virtual table: {stmt}"
            );
        }
    }

    /// The double-quoted statement of every `magequery sql … "…"` line in `md`.
    fn skill_sql_examples(md: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = md;
        while let Some(at) = rest.find("magequery sql") {
            rest = &rest[at..];
            let line_end = rest.find('\n').unwrap_or(rest.len());
            match rest[..line_end].find('"') {
                Some(q) => {
                    let body = &rest[q + 1..];
                    let close = body.find('"').expect("unterminated example");
                    out.push(body[..close].to_string());
                    rest = &body[close + 1..];
                }
                None => rest = &rest[line_end..],
            }
        }
        out
    }
}
