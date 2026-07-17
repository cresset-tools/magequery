# Changelog

All notable changes to the magequery VS Code extension. The language features
themselves live in the `magequery` server binary (which the extension downloads and
keeps current); this log covers the extension client and the capabilities a current
server unlocks.

## 0.2.0

- **`.phtml` templates are now analyzed** — override/usage code lenses, hover, and
  rename reach template files, not only XML and PHP.
- **Automatic server-binary updates** — for a `magequery` binary the extension
  downloaded itself, it now checks GitHub on startup and prompts when a newer release
  is available. New `magequery.checkForUpdates` setting (on by default) and a
  `magequery: Check for Server Update` command; a binary supplied from `PATH` or
  `magequery.serverPath` is left for you to manage.
- With a current server (0.9.0+), this unlocks the full feature set added since the
  initial release: context-aware completions, doctor quick fixes,
  layout/template/route navigation, document & workspace symbols, and **rename** of
  ACL resource ids, event names, and layout block names across config XML and PHP
  string literals.

## 0.1.0

- Initial release: doctor diagnostics, go-to-definition and hover, reverse-DI
  find-references, and plugin code lenses for Magento 2 configuration.
