# magequery for Zed

Magento 2 wiring answers inside Zed, powered by the
[magequery](https://github.com/cresset-tools/magequery) language server: doctor
diagnostics as you save, go-to-definition and hover on class names in
`di.xml`/`events.xml`/`webapi.xml`, reverse-DI find-references.

The server attaches to the **PHP** and **XML** languages (install those extensions),
running alongside the language's primary server. Open buffers overlay the checkout,
so diagnostics and answers follow your unsaved edits as you type.

Completions are context-aware: class/virtual-type names, event names, config paths,
ACL ids, module names, and db_schema tables, in both config XML and PHP strings.

Plugin indicators render as **inlay hints** (`« 7 plugin(s) »` at the end of an
intercepted method's signature, `→ Save::execute()` on a plugin's interception
methods — hover the hint for the breakdown). Enable them in Zed's settings:

```json
"inlay_hints": { "enabled": true }
```

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

## License

This extension shim is MIT-licensed (the Zed extension registry requires a license from
its allowlist, which excludes the repository's EUPL-1.2). The magequery binary the
extension downloads and runs remains EUPL-1.2.

## Publishing

Zed extensions are distributed via a PR to
[zed-industries/extensions](https://github.com/zed-industries/extensions): add this
repository as a submodule and an `extensions.toml` entry with
`path = "editors/zed"` (monorepo layout), then bump the version there for each release.
