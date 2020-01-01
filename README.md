# cresset/magequery

[magequery](https://github.com/cresset-tools/magequery) — a fast Magento 2
codebase inspector: DI resolution (preferences/plugins/virtual types),
events, cron, routes, config across scopes, and live DB introspection — from
the command line, in milliseconds, on a checkout that has never been set up.

```bash
composer require --dev cresset/magequery
vendor/bin/magequery di 'Magento\Catalog\Api\ProductRepositoryInterface'
vendor/bin/magequery plugins 'Magento\Checkout\Model\Cart' --chain
vendor/bin/magequery config web/secure/base_url --db
```

Or install it globally:

```bash
composer global require cresset/magequery
```

magequery is a single Rust binary. This package ships **only a thin PHP
launcher** — no Rust source. On first run it downloads the prebuilt
`magequery` binary matching this package's version for your platform, caches
it (`$XDG_CACHE_HOME/magequery/<version>/`), verifies its SHA-256, and execs
it. The package version maps 1:1 to the magequery release:
`cresset/magequery:0.1.0` runs `magequery-v0.1.0`.

Prebuilt targets: Linux x86_64 (gnu/musl), macOS arm64, Windows x64. Intel
macOS and Linux arm64 are not currently shipped. `ext-curl` is recommended;
`ext-zip` is required on Windows.

This is the Composer distribution branch of the magequery repo — it is
generated from `packaging/composer/` on `main` and contains no application
code of its own. EUPL-1.2.
