---
name: magequery
description: >-
  Inspect and verify how a Magento 2 codebase is wired using the fast `magequery`
  CLI, before grepping, before reading XML by hand, and before booting
  `bin/magento` or magerun. Two modes, both first-choice. **Investigate**: what a
  class resolves to (preferences, plugins, virtual types, constructor arguments),
  why a plugin/observer/cron is not firing, where a config value comes from across
  scopes, how a URL routes, which file actually renders a block or template.
  **Verify a change landed**: did the column, extension attribute, EAV attribute,
  plugin, observer, route, module, template override or layout edit I just wrote
  actually register, and does the declaration match the live database. Reach for it
  whenever you are reading or editing `db_schema.xml`, `extension_attributes.xml`,
  `di.xml`, `events.xml`, `crontab.xml`, `webapi.xml`, `system.xml`, `config.xml`,
  `acl.xml`, `menu.xml`, `routes.xml`, `module.xml`, `config.php`, `indexer.xml`,
  `widget.xml`, `schema.graphqls`, a layout XML file, a `.phtml` template, a
  `ui_component` file, or a `Setup/Patch` class. Static by default, so it works on a
  checkout that was never set up; `--db` overlays and diffs the live database. It
  also inspects live orders, products, customers, and carts.
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
points you straight at the source line. magequery itself is read-only and never
modifies anything.

There are **two** moments to reach for it, not one:

1. **Before** you change anything — find what to change and where it is declared.
2. **After** you change something — confirm the declaration actually registered and
   matches the live database. magequery re-reads the merged config from source, so
   it is the cheapest possible proof that an edit landed. See *Verifying a change
   landed* below; do this instead of assuming, and instead of a manual grep for
   your own marker.

**Versus `bin/magento` and magerun:** magequery is static by default and `--db`
overlays the live database, so it answers what the code *declares*, what the
database *holds*, and the **difference between them** (`schema --db`,
`patches --pending`, `indexers --db`, `cron --db`, `eav --db`). Prefer it for
reading either side. Use `bin/magento`/magerun to *change* state — reindex, flush
cache, `setup:upgrade`.

## Before you start

- Check it is installed: `magequery --version`. If missing:
  `curl -LsSf https://bougie.tools/magequery.sh | sh`.
- Run it from the Magento root, or pass `--root /path/to/magento`.
- **Quote class names** so the shell keeps the backslashes:
  `magequery di 'Magento\Catalog\Api\ProductRepositoryInterface'`.

## Which command answers which question

**Editing a file? The command that reads it**

| file | command | file | command |
|---|---|---|---|
| `db_schema.xml` | `schema` | `module.xml` | `modules`, `deps` |
| `extension_attributes.xml` | `extension-attributes` | `config.php` | `modules --check`, `info` |
| `di.xml` | `di`, `plugins`, `preference`, `uses` | layout XML | `layout` |
| `events.xml` | `events` | `.phtml` | `templates` |
| `crontab.xml` | `cron` | `ui_component/*.xml` | `ui-components` |
| `webapi.xml` | `webapi` | `indexer.xml`, `mview.xml` | `indexers` |
| `routes.xml` | `routes`, `actions` | `widget.xml` | `widgets` |
| `system.xml` | `system-config` | `email_templates.xml` | `email-templates` |
| `config.xml` | `config` | `catalog_attributes.xml` | `catalog-attributes` |
| `acl.xml` | `acl` | `queue_*.xml`, `communication.xml` | `queue topology` |
| `menu.xml` | `menu` | `schema.graphqls` | `graphql` |
| `Setup/Patch/**` | `patches`, `eav` | `i18n/*.csv` | `translations` |
| `fieldset.xml` | `fieldset` | | |

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
- Who can get into the admin and what may they do? → `admin-users [<user>]`,
  `admin-roles [<role>]`. API access instead → `integrations [<name>]`.

**Data and schema (static)**
- What columns/indexes/foreign keys does a table have, and which module added
  each? → `schema <table> [--db]` (`--db` diffs against the live database).
- What is this EAV attribute (value table, sets, models, scope)? →
  `eav <attr|entity> [--db]`.
- Why is an index not updating? → `indexers [<id>] [--db]`.
- Who extends an API data interface? → `extension-attributes '<Type>'`.
- Which attributes load in a context group (`quote_item`, `catalog_product`, …)? →
  `catalog-attributes [<group>|<attr>]` — the "why isn't my attribute available on
  the quote item" surface.
- Is my field copied from the quote to the order (or order to credit memo)? →
  `fieldset [<id>|<field>]` — Magento's object-copy map from `etc/fieldset.xml`. An
  exact id (`quote_convert_item`) lists its fields and the aspects that carry them;
  anything else is a **field** search across every fieldset, so
  `fieldset my_custom_field` answers "is it copied, and who declared that". Renames
  show as `→ target_field`, and a field with no aspect is flagged: declared but never
  copied. This is the usual reason a custom quote-item field never reaches
  `sales_order_item`.
- Increment ids and statuses → `sequences [<entity>]`, `order-statuses [<filter>]`.

**Frontend presentation**
- Which files contribute to a layout handle? → `layout [<handle>] [--area]`.
- Where does a PHTML template live, which themes override it, and which layout blocks
  use it? → `templates [<Vendor_Module::path.phtml>] [--area]`.
- Widget types, transactional emails, translations, and admin grids/forms →
  `widgets`, `email-templates`, `translations`, and `ui-components`.

**Live entities (need a database reachable from `env.php`)**
- `order <increment#>`, `product <sku>`, `customer <email>`, `quote <id|email>`,
  `price <sku>`, `category <id|name>`, `invoice|shipment|creditmemo <increment#>`,
  `sales-rule <coupon|id>`, `catalog-rule [<id|name>]`, `tax`, `stores`,
  `url-rewrites <path>`, `cms-page|cms-block [<identifier>]`.
  These read the database directly and report a clean error if it is unreachable.
- What customer groups exist, their tax class and member counts? →
  `customer-groups [<filter>]`. (`product <sku>` also lists the media gallery, with
  each image's base/thumbnail/swatch role.)
- What related / up-sell / cross-sell products does a SKU link to (and why isn't a
  link showing)? → `product-links <sku>`. Add `--reverse` for the products that link
  *to* it.

**The project itself**
- What am I looking at (version, deploy mode, URLs, theme and frontend stack, search
  engine, module counts, cron health)? → `info`. Script-friendly single facts:
  `mode`, `maintenance`, `base-url [--secure]`, `admin-url`.
- What modules exist and is the set consistent? →
  `modules [--check] [--enabled|--disabled] [--source app|vendor]`. `--check` catches
  a module on disk but missing from `config.php` (`setup:upgrade` never ran).
- What does a module depend on, both directions? → `deps <Module>` — resolves
  `<sequence>` *and* composer `require` edges, each flagged installed/enabled, with
  provenance. Often answers a "is X wired before Y" question in one call.

**Runtime (env.php and live connections)**
- `db info|ping`, `redis info|ping` — connections from `env.php`, plus a real
  connectivity test.
- `queue info|topology [<topic>]|backlog` — the queue wiring and whether anything is
  stuck. `session`, `cache`, `lock` — the configured backends (info-only).

**Health and whole-class views**
- Is anything in the merged config broken (missing classes, cycles, forgotten
  wiring)? → `magequery doctor` (exits non-zero on errors only, so it fits CI).
- Everything about one class in a single view → `whatis '<Class>'`.
- Which setup patches exist or are pending? → `patches [--db|--pending]`.

## Verifying a change landed

After editing config, run the command that reads that file. It re-parses the merged
config from source, so it proves the declaration registered — and with `--db`, that
it matches the database.

- Column in `db_schema.xml` → `schema <table>` lists it with the module that added
  it; `schema <table> --db` diffs the declaration against the live table (declared
  but missing live = `setup:upgrade` pending; whitelisted but no longer declared =
  `setup:upgrade` would DROP it).
- Extension attribute → `extension-attributes '<Type>'` (the added attribute is
  tagged with your module).
- EAV attribute added by a patch → `eav <code> [--db]`; the patch itself →
  `patches [--pending]`.
- Plugin, observer, preference, virtual type → `plugins '<Class>'`,
  `events <event_name>`, `preference '<Class>'`, then `di '<Class>'` for the whole
  assembled picture. Check the **area** and that it is not `disabled`.
- Template or theme override → `templates '<Vendor_Module::path.phtml>' --area <area>`
  reports the module file, every theme override candidate, and the layout handles
  that use it: the answer to *which file actually renders this*. Pair with
  `layout <handle> --area <area>` for the block tree.
- Route, endpoint, console command, GraphQL field → `routes`, `webapi <url>`,
  `commands <filter>`, `graphql '<Type.field>'`.
- New module → `modules --check`, then `deps <Module>`.
- Anything broken anywhere → `doctor` (exits non-zero on errors only).

Two honesty notes worth knowing before you read a result as a defect:

- `templates <ref>` reports **both** binding routes: a `used by (layout.xml):`
  section and a `bound in PHP:` section (a block's `protected $_template` or a
  `setTemplate()` call, with the class and line). So `0 layout.xml use(s)` is not
  dead code — check the PHP section, which says
  "rendered from PHP, not layout XML" when that is what is happening. Only
  "(not referenced by layout XML)" *plus* "(and no PHP binds it)" means unused.
- Area-aware commands default to **frontend** (`ui-components` to adminhtml). A miss
  in the wrong area names the right one — "Found in adminhtml — pass
  `--area adminhtml`" — so read the error rather than assuming the thing is absent.

## Reading the output

- The default text output is compact, greppable, and every fact ends with
  `# path:line`. Read it directly; open the cited line to see or edit the source.
- Add `--json` when you need to extract a specific field programmatically. JSON is
  never colored and its types are stable.
- Area-aware commands (`di`, `plugins`, `events`, `routes`, `webapi`, `uses`,
  `layout`, `templates`, `ui-components`, …) default to a **collapsed diff** where
  applicable. Use `--area <name>` for one area; commands that offer full expansion
  also accept `--all-areas`. Areas are
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
