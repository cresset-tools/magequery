# magequery sql — plain SQL over Magento-aware virtual tables

The live cards (`order`, `product`, `customer`, …) look up one record by identifier. They
cannot answer anything that spans many records: revenue by country over a period, low
stock, inactive carts, top customers. An agent can fall back to `magequery db info` +
`mysql`, but then it has to know everything the cards know implicitly — EAV value tables
and store fallback, the table prefix, configurable parent/child order lines, base vs
order currency — and that is where it gets answers subtly wrong.

`magequery sql` lets the agent write ordinary SQL against a small set of **virtual
tables** that encode that knowledge. magequery expands them into real SQL, runs it
read-only, and can print the expansion.

Acceptance bar: every read tool in elgentos/magento2-mcp (~39 tools over the REST API)
is answerable with one `magequery sql` query of about ten lines.

## Locked decisions

- **SQL, not a DSL.** Agents are already fluent in SQL; a new language is grammar to
  design and syntax for agents to get wrong. We remove the Magento traps from SQL, not
  SQL itself.
- **Not a dump of the cards' queries.** A card runs many queries glued by Rust (e.g.
  `fetch_customer` ≈ 8); the knowledge is in the glue. The virtual tables are that glue,
  made declarative.
- **Expansion is CTE substitution only.** Each referenced virtual table becomes a
  `WITH` CTE over the real schema; the agent's statement is otherwise passed through
  untouched. No rewriting of the agent's SQL, no optimizer. Real tables pass through,
  so any core or third-party table stays joinable.
- **Read-only, enforced by the tool.** `START TRANSACTION READ ONLY`, a statement
  timeout (`MAX_EXECUTION_TIME` on MySQL, `max_statement_time` on MariaDB), a default
  row limit (`--limit`, default 200, `--limit 0` to lift). Only a single `SELECT`/`WITH`
  statement is accepted.
- **Schema comes from what magequery already parses.** Real columns and foreign keys
  from `db_schema.xml` (the `schema` command's index); EAV columns from `eav_attribute`
  (live). `--describe` therefore includes custom modules' columns and custom attributes.
- **Target stays 2.4 OSS.** `entity_id` keys only; Commerce `row_id` staging is out of
  scope, as in `db/catalog.rs`. MySQL 8 and MariaDB 10.2+ (both have CTEs).

## Command surface

```console
magequery sql "<SELECT …>" [--store <code>] [--limit N] [--json]
magequery sql --print "<SELECT …>"     # the expanded SQL, not executed
magequery sql --describe               # list the virtual tables
magequery sql --describe <table>       # columns, types, source, caveats
magequery sql -                        # read the statement from stdin
```

`--store` sets the store view that store-scoped virtual columns resolve for (store row,
falling back to store 0). Default: admin/default values (store 0).

## Virtual tables (first slice)

| table | grain | notes |
|---|---|---|
| `products` | one row per product | static `catalog_product_entity` columns + **every EAV attribute as a column**, store fallback applied; `qty`, `is_in_stock` from `cataloginventory_stock_item` |
| `orders` | one row per order | `sales_order` + `store_code`, `website_id`, `billing_country`, `shipping_country`, `status_label`, `is_revenue` (see below) |
| `order_items` | one row per **visible** line | `parent_item_id IS NULL` only; the child SKU of a configurable is exposed as `simple_sku`, so lines are never double-counted |

Next, in order: `customers` (EAV, same machinery as products), `quotes` + `quote_items`,
`categories` (EAV + path), `stock` (MSI source items where present), `invoices`,
`creditmemos`, `shipments`, `reviews`, `search_terms`.

### Caveats live in `--describe`, not in hardcoded answers

The schema cannot decide what "revenue" means. Columns carry a one-line caveat that
`--describe` prints, e.g.:

- `base_grand_total` — in the website base currency; sum this across stores.
  `grand_total` is in the order currency and must not be summed across currencies.
- `is_revenue` — convenience: `state NOT IN ('canceled', 'closed')`. Partial
  refunds are not subtracted; they are in `creditmemos`. Override it by filtering `state` yourself.
- `products.name` etc. — resolved for `--store`; without it, admin values.

## Implementation

1. **Tokenize, don't parse.** Consistent with the rest of the codebase (hand-written
   focused parsers, no parser crates), a small MySQL tokenizer that respects strings,
   backtick identifiers and comments is enough:
   - reject anything that is not a single `SELECT`/`WITH` statement;
   - find identifiers following `FROM`/`JOIN` that name a virtual table (a real table of
     the same name wins only if written with the prefix or backticked — decide in M1);
   - collect every identifier token in the statement; an EAV attribute is joined iff its
     code appears. Over-inclusion is harmless (one extra LEFT JOIN), under-inclusion is
     impossible because any column use must appear as a token.
   `sqlparser-rs` is the fallback if tokenizing proves insufficient; it would be the
   heaviest dependency in core, so it needs a reason.
2. **Virtual table definitions** in core (`queries/sql/`), one per table: base table,
   fixed joins, static columns with caveats, and an EAV entity type if any. The same
   definitions render `--describe`.
3. **Expansion**: emit `WITH <vt> AS (…)` for each referenced table, prefixing real table
   names with the `env.php` prefix, pushing `--store` into the EAV joins
   (`COALESCE(store_row.value, default_row.value)`).
4. **Execution** behind the `db` feature, reusing `db::connect` and `clean_err`. Output
   through the existing table/`--json` renderers; columns in select order.
5. **Later: rebuild cards on the same definitions** where it simplifies them, so
   `order <id>` and `sql … FROM orders` cannot disagree. Not a goal of M1.

## The agent skill (required, part of M1)

`assets/skill/SKILL.md` is embedded in the binary and is how agents learn the tool, so
the feature is not done until the skill teaches it. Add a section **after** "Live
entities", roughly:

````markdown
**Questions across many records (reports, lists, totals)**
The live entity commands look up one record. For anything across many — revenue by
period or country, best sellers, low stock, inactive carts — write SQL with
`magequery sql`. It adds Magento-aware virtual tables (`products`, `orders`,
`order_items`, …) that already handle EAV store fallback, the table prefix, and
configurable parent/child lines. Real tables can be joined too. It is read-only and
limited to 200 rows unless you pass `--limit`.

Always look at the virtual schema first; the column notes say which amounts to sum:

    magequery sql --describe              # the virtual tables
    magequery sql --describe orders       # columns, sources, caveats

Examples:

    # revenue by shipping country, last 90 days
    magequery sql "SELECT shipping_country, COUNT(*) orders, SUM(base_grand_total) revenue
                   FROM orders WHERE is_revenue AND created_at >= NOW() - INTERVAL 90 DAY
                   GROUP BY 1 ORDER BY revenue DESC"

    # enabled products low on stock, names as the Dutch store view shows them
    magequery sql --store nl "SELECT sku, name, qty FROM products
                              WHERE status = 1 AND qty < 5 ORDER BY qty"

    # best sellers this year (order_items never double-counts configurables)
    magequery sql "SELECT i.sku, SUM(i.qty_ordered) qty FROM order_items i
                   JOIN orders o ON o.entity_id = i.order_id
                   WHERE o.is_revenue AND o.created_at >= '2026-01-01'
                   GROUP BY 1 ORDER BY qty DESC"

Add `--print` to see the real SQL a query expands to, and `--json` to parse results.
````

The examples in the skill are executed by a test against the fixture DB, so they
cannot rot.

## Milestones

- **M1 — orders + order_items.** Tokenizer, statement guard, read-only execution,
  `--describe`, `--print`, `--limit`, `--json`, skill section. No EAV yet.
- **M2 — products (EAV).** Attribute-token detection, store fallback, stock columns.
  Benchmark on the largest catalog we have; pushing `--store` into the join is the
  main lever.
- **M3 — customers, quotes, categories, sales documents.** Mostly definitions, reusing
  M2's EAV path.
- **M4 — acceptance.** Walk the magento2-mcp read-tool list; each answered by one
  query, recorded as a test. Gaps become new virtual columns or tables.

## Open questions

- Name collisions between virtual and real tables (`orders` vs a third-party
  `orders` table): prefix-wins rule, or a `v.` schema-style qualifier?
- Should a production-looking `env.php` (non-local host, `MAGE_MODE=production`) warn,
  or require `--yes`? Aggregates over `sales_order` on a busy primary are not free.
