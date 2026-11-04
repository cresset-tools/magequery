# magequery

Fast Magento 2 tooling that reads the source directly: no PHP, no database, no booting the
framework. Two binaries:

- **magequery** answers how a codebase is wired (DI, plugins, events, routes, config, …),
  each result citing the file and line it came from.
- **magecommand** builds it: `setup:di:compile` and `setup:static-content:deploy`,
  reimplemented in Rust.

## Install

```console
$ curl -LsSf https://bougie.tools/magequery.sh | sh     # Linux, macOS
> irm https://bougie.tools/magequery.ps1 | iex          # Windows
```

Run from the Magento root, or pass `--root <path>`.

### Without installing (bgx)

[bougie](https://bougie.tools)'s `bgx` (like npx) runs either tool in an isolated,
globally-cached environment without adding it to your project. Everything after the package is
forwarded straight to the tool:

```console
$ bgx cresset/magequery doctor
$ bgx cresset/magequery di 'Magento\Catalog\Api\ProductRepositoryInterface'
$ bgx cresset/magecommand di compile
```

## magequery: for AI agents and CI

`magequery doctor` checks every config reference in the codebase against what is on disk:
missing classes, cycles, forgotten wiring. It runs in milliseconds and exits non-zero on
errors, so it works as a CI gate:

```yaml
- uses: cresset-tools/magequery-action@v1   # runs doctor by default
```

The agent skill teaches a coding agent to ask magequery wiring questions instead of grepping
or booting Magento:

```console
$ mkdir -p .claude/skills/magequery
$ magequery skill > .claude/skills/magequery/SKILL.md
```

Run `magequery --help` for the other commands (`di`, `plugins`, `events`, `config`, `sql`, …).
`magequery lsp` is a language server for the XML config layer; see [`editors/`](editors).

## magecommand: build without PHP

```console
$ magecommand di compile                   # generated/code + generated/metadata
$ magecommand static deploy en_US nl_NL    # pub/static
```

The output matches a real `bin/magento` run; `di verify` and `static verify` diff it against
one. `di digest` gives a CI cache key for `generated/`, and `di watch` keeps it compiled while
you edit. `magecommand skill` prints its agent skill.

In a PHP project, `composer require --dev cresset/magecommand` gives you
`vendor/bin/magecommand` (`cresset/magequery` ships both tools).

For a whole deploy build in one step (composer install, DI compile, and static deploy, run in
parallel), use [magebuild](https://github.com/cresset-tools/magebuild).

## License

[EUPL-1.2](LICENSE).
