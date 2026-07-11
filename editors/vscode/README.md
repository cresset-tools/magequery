# magequery for VS Code

Magento 2 wiring answers inside the editor, powered by the
[magequery](https://github.com/cresset-tools/magequery) language server. Static analysis
of the checkout — no database, no `setup:di:compile`, no working PHP needed.

## What you get

- **Completions** — context-aware as you type: class and virtual-type names in
  `di.xml`/`events.xml`/`system.xml` attributes and text values, event names, config
  paths, ACL resource ids, module names in `<sequence>`, db_schema table references —
  and inside PHP strings behind `dispatch()`/`getValue()`/`isAllowed()`.
- **Diagnostics** — `magequery doctor` as squiggles: a `<preference>` pointing at a
  missing class, a di argument injecting a class that doesn't exist, a webapi
  `<resource>` no acl.xml declares, module-set drift, malformed config XML. Refreshed on
  save.
- **Go to definition** — a class name in `di.xml` / `events.xml` / `webapi.xml` /
  `system.xml` / `schema.graphqls` jumps to the PHP file; on an interface you also get
  the concrete class its `<preference>` resolves to.
- **Hover** — a class shows its DI card (preference resolution, plugin count, what wires
  it in, the webapi routes/cron jobs/events it serves); a config path shows its admin
  location and per-scope values; an event lists its observers.
- **Find references** — every di.xml injection, virtual type, observer registration,
  cron job, webapi route, GraphQL resolver and queue handler referencing a class.
- **Code lens** — on a PHP class declaration: `N plugin(s)` and
  `wired in N config place(s)`; on each intercepted method: `intercepted by N plugin
  method(s)`; on a plugin's interception methods: `intercepts Save::execute()` — all
  peeking straight into the other side.
- **Inlay hints** — the same plugin facts inline at the end of the signature line
  (`« 7 plugin(s) »` / `→ Save::execute()`), with the breakdown on hover.
- **Layout navigation** — jump from `template="Vendor_Module::…"` to the file (module
  original + theme overrides), from `<update handle=…>` to the defining files, from
  `<referenceBlock name=…>` to the block's declaration; `.phtml` files carry
  override/usage lenses; completions for templates, handles, and block names.
- **Outline & symbol search** — document symbols for every config XML, and
  workspace-wide search over classes, events, config paths, ACL ids, and tables.
- **Plugin-method jump, both directions** — go-to-definition on a
  `before*`/`around*`/`after*` declaration in a plugin class lands on the method it
  intercepts (resolved through the di.xml plugin declaration and the preference chain);
  go-to-definition on an intercepted method lands on the plugin methods wrapping it,
  including plugins declared on interfaces and parent classes.

Open buffers overlay the checkout, so diagnostics and answers follow your unsaved
edits as you type (debounced). It
complements a PHP language server (Intelephense, PHP Tools, Phpactor) rather than
replacing it — magequery answers the XML config layer PHP servers can't see.

## Server binary

The extension looks for the server in this order:

1. the `magequery.serverPath` setting,
2. `magequery` on `PATH` (install:
   `curl -LsSf https://github.com/cresset-tools/magequery/releases/latest/download/magequery-installer.sh | sh`),
3. a copy it downloads itself from GitHub releases (with your confirmation).

## Development

```sh
npm install
npm run build      # dist/extension.js
npm run package    # .vsix via vsce
```

Launch with F5 from VS Code (Extension Development Host), opening a Magento 2 checkout
as the workspace folder.
