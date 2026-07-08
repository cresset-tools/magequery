---
name: magequery
description: >-
  Inspect and debug how a Magento 2 codebase is wired using the fast `magequery`
  CLI. Use whenever you need to understand or debug a Magento store: what a class
  or interface resolves to (preferences, plugins, virtual types, constructor
  arguments), why a plugin/observer/cron is or is not firing, where a config
  value comes from across scopes, which module added a DB column/attribute/route/
  layout, how a URL routes, or to inspect live orders, products, and customers.
  Reach for it before grep or `bin/magento` for any "how is this wired" question:
  it answers in milliseconds and cites the exact file and line.
---

# Inspecting a Magento 2 codebase with magequery

`magequery` statically parses Magento's source (XML and PHP) and reimplements
its config-merge and dependency-injection rules. It does not boot the framework
and needs no database (except the explicit live commands below), so it answers in
milliseconds and works on a checkout that was never set up. **Every fact it prints
ends with a `# path:line` comment you can open.**

## The rule

For any question about how a Magento codebase is wired, run `magequery` **before**
grepping, reading files by hand, or booting `bin/magento`. It is faster and it
points you straight at the source line. Use it to *find* what to change, then edit
the cited file: magequery itself is read-only and never modifies anything.

## Before you start

- Check it is installed: `magequery --version`. If missing:
  `curl -LsSf https://bougie.tools/magequery.sh | sh`.
- Run it from the Magento root, or pass `--root /path/to/magento`.
- **Quote class names** so the shell keeps the backslashes:
  `magequery di 'Magento\Catalog\Api\ProductRepositoryInterface'`.

## Which command answers which question

**Wiring (the object manager)**
- What does an interface/abstract class resolve to, with its args and plugins?
  → `magequery di '<Class>'` (the flagship: concrete type + constructor arguments
  + plugin chain, all with provenance). Focused views: `preference '<Class>'`.
- What plugins/interceptors run on a class, and in what order?
  → `magequery plugins '<Class>'`; add `--chain` for the before/around/after
  execution onion (`--method <name>` to focus one method).
- Why is a plugin or observer not firing? → `plugins '<Class>'` /
  `events <event_name>` — check it is registered, not `disabled`, and in the
  right area.
- Who injects or uses a class (impact analysis)? → `magequery uses '<Class>'`.

**Entry points (how execution starts)**
- What route/controller handles a URL? → `routes`, `actions <url>`, `webapi <url>`.
- What cron jobs run and when? → `cron [<group>|<job>] [--db]`.
- What console commands does the codebase add? → `commands [<filter>]`.
- GraphQL type or field to resolver class? → `graphql '<Type>'` or `'<Type.field>'`.

**Config and admin**
- Where does a config path get its value, in every scope? →
  `config <path> [--db] [--decrypt]` (static by default; `--db` overlays
  `core_config_data`; `--decrypt` reveals encrypted values).
- Where does a setting live in the admin, and what path is it? →
  `system-config <filter>` (matches the path or the human label).
- What ACL resource guards something / where is a menu item? →
  `acl [<resource>]`, `menu [<item>]`.

**Data and schema (static)**
- What columns/indexes/foreign keys does a table have, and which module added
  each? → `schema <table> [--db]` (`--db` diffs against the live database).
- What is this EAV attribute (value table, sets, models, scope)? →
  `eav <attr|entity> [--db]`.
- Why is an index not updating? → `indexers [<id>] [--db]`.
- Who extends an API data interface? → `extension-attributes '<Type>'`.

**Live entities (need a database reachable from `env.php`)**
- `order <increment#>`, `product <sku>`, `customer <email>`, `quote <id|email>`,
  `price <sku>`, `category <id|name>`, `invoice|shipment|creditmemo <increment#>`,
  `sales-rule <coupon|id>`, `tax`, `stores`, `url-rewrites <path>`.
  These read the database directly and report a clean error if it is unreachable.
- What customer groups exist, their tax class and member counts? →
  `customer-groups [<filter>]`. (`product <sku>` also lists the media gallery, with
  each image's base/thumbnail/swatch role.)
- What related / up-sell / cross-sell products does a SKU link to (and why isn't a
  link showing)? → `product-links <sku>`. Add `--reverse` for the products that link
  *to* it.

**Health and whole-class views**
- Is anything in the merged config broken (missing classes, cycles, forgotten
  wiring)? → `magequery doctor` (exits non-zero on errors only, so it fits CI).
- Everything about one class in a single view → `whatis '<Class>'`.
- Which setup patches exist or are pending? → `patches [--db|--pending]`.

## Reading the output

- The default text output is compact, greppable, and every fact ends with
  `# path:line`. Read it directly; open the cited line to see or edit the source.
- Add `--json` when you need to extract a specific field programmatically. JSON is
  never colored and its types are stable.
- Area-aware commands (`di`, `plugins`, `events`, `routes`, `webapi`, `uses`, …)
  default to a **collapsed diff**: the global base plus per-area deltas. Use
  `--area <name>` for one area or `--all-areas` for the full expansion. Areas are
  `global, frontend, adminhtml, crontab, webapi_rest, webapi_soap, graphql`.
- `--db` opts a hybrid command into live data; the live-only commands above always
  use the database. All commands accept `--root <path>` and
  `--color auto|always|never`.

## Scope and limits

- Target is **Magento 2.4 Open Source**. Adobe Commerce, Mage-OS, and OpenMage
  often work but are not the supported target.
- It is a static engine: it never reads `var/cache` or `generated/`, so it is
  correct on a fresh checkout but reflects the **source**, not a stale compiled
  state.
- Discover the full command surface with `magequery --help`, and any command's
  flags with `magequery <command> --help`.
