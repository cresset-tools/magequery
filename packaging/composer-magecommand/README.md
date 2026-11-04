# cresset/magecommand

[magecommand](https://github.com/cresset-tools/magequery) builds a Magento 2
codebase without PHP or node: `setup:di:compile` and
`setup:static-content:deploy`, reimplemented in Rust, with the same output as
a real `bin/magento` run.

```bash
composer require --dev cresset/magecommand
vendor/bin/magecommand di compile                  # generated/code + generated/metadata
vendor/bin/magecommand static deploy en_US nl_NL   # pub/static
```

This package ships **only a thin PHP launcher**. On first run it downloads the
prebuilt binary matching this package's version for your platform from the
magequery release, verifies its SHA-256, caches it
(`$XDG_CACHE_HOME/magecommand/<version>/`), and execs it. The package version
maps 1:1 to the magequery release: `cresset/magecommand:0.22.0` runs the
`magecommand` from `magequery-v0.22.0`.

[`cresset/magequery`](https://packagist.org/packages/cresset/magequery) ships
the same launcher plus the `magequery` inspector; install that one instead if
you want both tools.

Prebuilt targets: Linux x86_64 (gnu/musl), macOS arm64, Windows x64.
`ext-curl` is recommended; `ext-zip` is required on Windows.

`extra.bougie.native-binary` declares the same download layout, so
`bougie tool install cresset/magecommand` and `bgx cresset/magecommand`
prefetch and Sigstore-verify the binary at install time. `bin` names
`magecommand` inside the `magequery` release archive; it must stay in sync
with `src/Launcher.php` and the magequery repo's dist-workspace.toml targets.

This repository is generated from `packaging/composer-magecommand/` in the
magequery repo on each release and contains no application code of its own.
EUPL-1.2.
