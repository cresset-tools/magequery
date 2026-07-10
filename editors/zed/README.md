# magequery for Zed

Magento 2 wiring answers inside Zed, powered by the
[magequery](https://github.com/cresset-tools/magequery) language server: doctor
diagnostics as you save, go-to-definition and hover on class names in
`di.xml`/`events.xml`/`webapi.xml`, reverse-DI find-references.

The server attaches to the **PHP** and **XML** languages (install those extensions),
running alongside the language's primary server. It analyzes the checkout from disk —
answers refresh on save.

## Server binary

`magequery` from `PATH` when present; otherwise the extension downloads the current
GitHub release for your platform automatically.

## Development

Install as a dev extension: `zed: install dev extension` from the command palette,
pointing at this directory. Zed compiles the WASM shim with your local Rust toolchain,
targeting `wasm32-wasip2` — install that target first or the compile fails
("failed to compile Rust extension"):

```sh
rustup target add wasm32-wasip2
```

Until a magequery release with the `lsp` subcommand is on GitHub, the binary must also
be on PATH (`cargo install --path crates/magequery-cli --locked` from this repo) — the
extension's release-download fallback would fetch one without it.

## Publishing

Zed extensions are distributed via a PR to
[zed-industries/extensions](https://github.com/zed-industries/extensions): add this
repository as a submodule and an `extensions.toml` entry with
`path = "editors/zed"` (monorepo layout), then bump the version there for each release.
