# magequery

magequery reads a Magento 2 codebase and tells you how it is wired: module load order, DI
resolution (preferences, plugins, virtual types, constructor arguments), events, cron,
routes, config across scopes, and the admin permission tree, among others. Every result
cites the file and line it came from.

`bin/magento` and magerun boot the whole framework on every call, which takes one to three
seconds. magequery parses the source XML and PHP directly and reimplements Magento's
config-merge and DI rules, so it runs in milliseconds and works on a checkout that was never
set up. It needs no database, no `setup:di:compile`, and no working PHP install.

```console
$ magequery modules      # 88 modules, load order resolved
4 ms
$ magequery di Magento\Catalog\Api\ProductRepositoryInterface
19 ms
$ magequery doctor       # scan every config reference in the codebase
86 ms
```

Some commands can also read live data. Point them at the database from `env.php` (with
`--db`, or as a dedicated live command) to read products, orders, indexer status, the scope
tree, and similar runtime state. These commands report a clear error when the database is
unreachable. The static commands never touch it.

## Install

```console
# Linux and macOS
$ curl -LsSf https://bougie.tools/magequery.sh | sh

# Windows (PowerShell)
> irm https://bougie.tools/magequery.ps1 | iex
```

Prebuilt binaries (Linux gnu/musl, macOS arm64, Windows x64) are attached to every
[GitHub Release](https://github.com/cresset-tools/magequery/releases) and mirrored to cresset
infrastructure.

Or build from source. This needs a Rust toolchain, a C compiler, and CMake (the last two for
the bundled MySQL client used by the live commands):

```console
$ cargo install --git https://github.com/cresset-tools/magequery magequery
```

Point it at a store with `--root <path-to-magento>`, or run it from inside the Magento root
(the default is the current directory).

## Examples

`di` is the main command. It shows how the object manager assembles a type: the concrete
class, the merged constructor arguments, and the plugin chain, each with its source.

```console
$ magequery di Magento\Catalog\Api\ProductRepositoryInterface
Magento\Catalog\Api\ProductRepositoryInterface  (base)
  → Magento\Catalog\Model\ProductRepository   # vendor/.../module-catalog/etc/di.xml:11

arguments (1)
  collectionProcessor = \Magento\Catalog\Model\Api\SearchCriteria\ProductCollectionProcessor   # .../etc/di.xml:939

plugins (3)  run order
    0 remove_images_from_gallery_after_removing_product  [around delete]
    0 add_mutex_to_save_operation  [around save]
   10 configurableProductSaveOptions  [before save, after save]
```

`doctor` checks every merged config reference against what exists on disk. It reports broken
references, cycles, and wiring that looks forgotten, and exits non-zero only on errors, so
you can run it in CI.

```console
$ magequery doctor
error  di argument $defaultHandler of Magento\Indexer\Model\HandlerPool injects missing
       class Magento\Indexer\Model\Handler\DefaultHandler   # .../module-indexer/etc/di.xml:30
error  preference for ...\ProductRenderSearchResultsInterface points at missing class
       ...\ProductRenderSearchResults   # .../module-catalog/etc/di.xml:54
warn   ...\CouponUsagesDecrement implements ObserverInterface but no events.xml registers it
```

`info` prints a one-screen summary of the install. It reads the static config and fills in
live values from the database when it can reach one.

```console
$ magequery info
magento       2.4.7  (magento/product-community-edition)
mode          developer
maintenance   off

base url      https://shop.example/
admin         https://shop.example/admin_XXXXXX/

frontend      Luma  (theme Magento/luma)
search        opensearch @ 127.0.0.1:9200

db            shop @ /run/mysqld/mysqld.sock
cache         default: redis db5 · page_cache: redis db5 · fpc: built-in · 14/14 types on
queue         amqp @ 127.0.0.1:5672

stores        1 website(s), 1 store(s), 1 store view(s)
modules       88 enabled, 0 disabled  (88 vendor, 0 app/code)
```

## Notes

Every result ends with a `# file:line` comment. It is written as a trailing comment so a
copied line stays valid, and it points at the source that produced the fact.

The static commands parse source files only. They do not read `var/cache` or `generated/`,
so they work on a fresh checkout. Commands that can also use live data take `--db`; a few
commands, such as `product`, `order`, and `stores`, read the database directly.

DI, plugins, events, and routes vary by area. By default these commands show a collapsed
diff: the global base plus the per-area differences. `--area <name>` shows a single area, and
`--all-areas` expands every area in full.

`--json` works on every command and is never colored. The single-value commands (`mode`,
`base-url`, `maintenance`) print a bare value for use in scripts. Color is on when writing to
a terminal, respects `NO_COLOR`, and can be set with `--color auto|always|never`.

## Commands

Grouped as `magequery --help` shows them.

**Wiring** (the object manager): `di`, `preference`, `plugins`, `events`, `uses` (reverse DI).

**Entry points**: `routes`, `actions`, `webapi`, `cron`, `commands`, `graphql`.

**Data**: `schema`, `indexers`, `extension-attributes`, `catalog-attributes`, `eav`,
`product`, `price`, `category`, `order`, `customer`, `quote`, `invoice`, `shipment`,
`creditmemo`, `order-statuses`, `sequences`, `sales-rule`, `catalog-rule`, `tax`.

**Frontend**: `layout`, `widgets`, `email-templates`, `translations`, `ui-components`,
`cms-page`, `cms-block`.

**Config and admin**: `config`, `system-config`, `acl`, `menu`, `admin-users`,
`admin-roles`, `integrations`.

**Runtime** (from `env.php` and live connections): `db`, `redis`, `session`, `cache`,
`lock`, `queue`, `url-rewrites`, `stores`.

**Project**: `info`, `mode`, `maintenance`, `base-url`, `admin-url`, `modules`, `deps`,
`doctor`, `whatis`, `patches`.

The grammar is `magequery <command> [target] [flags]`. Run `magequery <command> --help` for
the details of each.

## Shell completions

The `completions` subcommand prints a completion script to stdout. Add one line to your shell
config:

```console
# bash (~/.bashrc)
source <(magequery completions bash)

# zsh (~/.zshrc, after 'autoload -Uz compinit && compinit')
source <(magequery completions zsh)

# fish (~/.config/fish/config.fish)
magequery completions fish | source
```

`magequery man` prints a man page.

## Agent skill

magequery ships a skill that teaches an AI coding agent when and how to use it on a Magento
codebase, so the agent reaches for magequery on wiring questions instead of grepping or booting
the framework. Install it into a project (or your home directory) with:

```console
$ mkdir -p .claude/skills/magequery
$ magequery skill > .claude/skills/magequery/SKILL.md
```

The `skill` subcommand emits the file from the installed binary, so it always matches your
version. It is plain markdown with YAML frontmatter, so it drops into a Cursor rule or your own
docs just as well.

## Editor integration (LSP)

The binary doubles as a language server: `magequery lsp` speaks LSP over stdio. Inside the
editor that gives you `doctor` findings as diagnostics on save, go-to-definition and hover on
class names in `di.xml`/`events.xml`/`webapi.xml`/`system.xml`/`schema.graphqls` (definition on
an interface also jumps to the class its `<preference>` resolves to), find-references over the
whole config graph (injections, virtual types, observers, cron, webapi, GraphQL, queues), and
code lenses on PHP classes (`N plugin(s)`, `wired in N config place(s)`).

- **VS Code** — the extension in [`editors/vscode`](editors/vscode); it finds `magequery` on
  PATH or downloads the release binary itself.
- **Zed** — the extension in [`editors/zed`](editors/zed), same binary resolution.
- **Anything else** — point your editor's LSP client at `magequery lsp` for PHP and XML files.

The server analyzes the checkout from disk, so answers refresh on save. It complements a PHP
language server rather than replacing one — magequery covers the XML config layer.

## Scope

The target is Magento 2.4 Open Source. Adobe Commerce, Mage-OS, OpenMage, and similar forks
often work but are not the supported target. Live commands need a readable `app/etc/env.php`,
and a reachable connection for the database.

## License

[European Union Public Licence v1.2](LICENSE) (`EUPL-1.2`).
