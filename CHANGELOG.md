# Changelog

## [0.12.4](https://github.com/cresset-tools/magequery/compare/magequery-v0.12.3...magequery-v0.12.4) (2026-07-22)


### Bug Fixes

* **engine:** fused interceptor gets the ObjectManager from the singleton, not the constructor ([#74](https://github.com/cresset-tools/magequery/issues/74)) ([3011df4](https://github.com/cresset-tools/magequery/commit/3011df46fc2890310386ac523aa34bf0b59a38e2))

## [0.12.3](https://github.com/cresset-tools/magequery/compare/magequery-v0.12.2...magequery-v0.12.3) (2026-07-21)


### Bug Fixes

* **engine:** fused interceptor must not `return` in `: void` methods ([#72](https://github.com/cresset-tools/magequery/issues/72)) ([d9d7928](https://github.com/cresset-tools/magequery/commit/d9d7928ee41afcabc98b7437b92c423337976a32)), closes [#71](https://github.com/cresset-tools/magequery/issues/71)

## [0.12.2](https://github.com/cresset-tools/magequery/compare/magequery-v0.12.1...magequery-v0.12.2) (2026-07-21)


### Bug Fixes

* **engine:** fold generated classes into the from-empty scan universe ([#69](https://github.com/cresset-tools/magequery/issues/69)) ([bb7bd01](https://github.com/cresset-tools/magequery/commit/bb7bd018f4ab8c2b40adf803d8b451c2d594bc68)), closes [#67](https://github.com/cresset-tools/magequery/issues/67)

## [0.12.1](https://github.com/cresset-tools/magequery/compare/magequery-v0.12.0...magequery-v0.12.1) (2026-07-21)


### Bug Fixes

* **magecommand:** static deploy — discover src/-bundled themes (Hyvä admin) ([#65](https://github.com/cresset-tools/magequery/issues/65)) ([b1ea416](https://github.com/cresset-tools/magequery/commit/b1ea416a4309cc014b5244e6e8ed7d04a01a5a86))

## [0.12.0](https://github.com/cresset-tools/magequery/compare/magequery-v0.11.0...magequery-v0.12.0) (2026-07-21)


### Features

* **magecommand:** adminhtml area support (--area) across the static commands ([#61](https://github.com/cresset-tools/magequery/issues/61)) ([9887802](https://github.com/cresset-tools/magequery/commit/988780210457030824045fda68d3c210c481e845))
* **magecommand:** static deploy — the no-PHP setup:static-content:deploy finale ([#62](https://github.com/cresset-tools/magequery/issues/62)) ([501b56f](https://github.com/cresset-tools/magequery/commit/501b56f69899bd138aac0423fdeb9fd049e8ea6a))
* **magecommand:** static files — full byte-exact static-file placement ([#60](https://github.com/cresset-tools/magequery/issues/60)) ([b890b5b](https://github.com/cresset-tools/magequery/commit/b890b5ba553658e5df9ea9b1f7c44ce116c1e455))
* **magecommand:** static minify — .min.css/.min.js via lightningcss + oxc ([#58](https://github.com/cresset-tools/magequery/issues/58)) ([3984279](https://github.com/cresset-tools/magequery/commit/398427951c33e97cc85632704e88a579df76753d))


### Performance Improvements

* **magecommand:** di compile ~24% faster — parallelize input walks, borrow-not-clone ([#64](https://github.com/cresset-tools/magequery/issues/64)) ([8708408](https://github.com/cresset-tools/magequery/commit/8708408aa821b8f27b62a4942ce93b0deff7c28a))
* **magecommand:** static deploy 2.1x faster — parallelize render/write, fix nested-rayon scaling ([#63](https://github.com/cresset-tools/magequery/issues/63)) ([48ad6a2](https://github.com/cresset-tools/magequery/commit/48ad6a224152f8a0f5244f73457ac48040cd3695))

## [0.11.0](https://github.com/cresset-tools/magequery/compare/magequery-v0.10.1...magequery-v0.11.0) (2026-07-20)


### Features

* **magecommand-less:** pure-Rust LESS compiler + Magento theme CSS deploy (static less) ([#50](https://github.com/cresset-tools/magequery/issues/50)) ([7c19716](https://github.com/cresset-tools/magequery/commit/7c19716c0d08473c7a24ca070bf85dc42a4a9d34))
* **magecommand:** static bundle — byte-exact SCD JS bundling from source ([#57](https://github.com/cresset-tools/magequery/issues/57)) ([ef8e3a8](https://github.com/cresset-tools/magequery/commit/ef8e3a8387a1d6e6066a38832b7e924ac77d8661))
* **magecommand:** static less --file/--compress + Cresset_MagecommandLess bridge module ([#53](https://github.com/cresset-tools/magequery/issues/53)) ([606b2eb](https://github.com/cresset-tools/magequery/commit/606b2eb597c78129b843da7d7cc6997ce0d588d2))
* **magecommand:** static requirejs — byte-exact requirejs-config.js aggregation ([#54](https://github.com/cresset-tools/magequery/issues/54)) ([a7e322d](https://github.com/cresset-tools/magequery/commit/a7e322d393943a6674f2b433b3e26fd001fe73a7))
* **magecommand:** static requirejs — min-resolver + mixins siblings, byte-exact ([#56](https://github.com/cresset-tools/magequery/issues/56)) ([f30dabc](https://github.com/cresset-tools/magequery/commit/f30dabc3fb69f5abd5f51d879fbc29adf7397c57))
* ship magecommand in releases; magequery stays the bgx default-bin ([#55](https://github.com/cresset-tools/magequery/issues/55)) ([0d1fed3](https://github.com/cresset-tools/magequery/commit/0d1fed3388a8d7964581de3e463f99aea8873013))


### Performance Improvements

* **magecommand-less:** mixin candidate index, frame variable cache, extend prescan — 2.2x faster ([#52](https://github.com/cresset-tools/magequery/issues/52)) ([7193f85](https://github.com/cresset-tools/magequery/commit/7193f857fe63ef13634aedba2cab0833f09e3c34))

## [0.10.1](https://github.com/cresset-tools/magequery/compare/magequery-v0.10.0...magequery-v0.10.1) (2026-07-19)


### Bug Fixes

* **magecommand:** compile arguments for vtypes over self-generated bases ([#48](https://github.com/cresset-tools/magequery/issues/48)) ([10a47a7](https://github.com/cresset-tools/magequery/commit/10a47a75546afff106c6c1cd72bd7f0f7217c7ef))

## [0.10.0](https://github.com/cresset-tools/magequery/compare/magequery-v0.9.0...magequery-v0.10.0) (2026-07-18)


### Features

* **magecommand:** byte-exact `setup:di:compile` reproduction ([#44](https://github.com/cresset-tools/magequery/issues/44)) ([82b623c](https://github.com/cresset-tools/magequery/commit/82b623c76f69294e5e700f18bf9e98d811c27199))
* **magecommand:** `di compile --fused` — fused interceptors ([#46](https://github.com/cresset-tools/magequery/issues/46)) ([96b9c2d](https://github.com/cresset-tools/magequery/commit/96b9c2d70c79a29fec7960c3a44d0f53ad774e7d))
* **vscode:** server auto-update prompt + 0.2.0 release ([#43](https://github.com/cresset-tools/magequery/issues/43)) ([e2dd390](https://github.com/cresset-tools/magequery/commit/e2dd39052eae052bc2a7ef67df787d95faa5ec8e))

## [0.9.0](https://github.com/cresset-tools/magequery/compare/magequery-v0.8.0...magequery-v0.9.0) (2026-07-15)


### Features

* **vscode:** prompt to update the managed server binary ([#41](https://github.com/cresset-tools/magequery/issues/41)) ([e0eea6f](https://github.com/cresset-tools/magequery/commit/e0eea6f21a3b6b7ce3b0fb7fcbc6db5912b61cbf))

## [0.8.0](https://github.com/cresset-tools/magequery/compare/magequery-v0.7.0...magequery-v0.8.0) (2026-07-13)


### Features

* add PHTML template lookup ([#29](https://github.com/cresset-tools/magequery/issues/29)) ([adb5502](https://github.com/cresset-tools/magequery/commit/adb55026a4465ff1235044d4f8a0364c4d0fcde1))
* **lsp:** parity nits — short templates, column/route nav, two lints ([#33](https://github.com/cresset-tools/magequery/issues/33)) ([ff5e1bf](https://github.com/cresset-tools/magequery/commit/ff5e1bf7a51510674e057ea33c0ad3ac25d727b6))
* **lsp:** quick fixes for doctor diagnostics ([#26](https://github.com/cresset-tools/magequery/issues/26)) ([1c50eae](https://github.com/cresset-tools/magequery/commit/1c50eaecfe109fd66398fde770811da3f1abfd76))
* **lsp:** rename ACL ids, event names, block names across config + PHP ([#39](https://github.com/cresset-tools/magequery/issues/39)) ([7e153ff](https://github.com/cresset-tools/magequery/commit/7e153ff76122e7a852cd68f3b12c1a85f7031a40))

## [0.7.0](https://github.com/cresset-tools/magequery/compare/magequery-v0.6.0...magequery-v0.7.0) (2026-07-11)


### Features

* **lsp:** layout navigation, symbols, observer lens, table nav ([#24](https://github.com/cresset-tools/magequery/issues/24)) ([e842115](https://github.com/cresset-tools/magequery/commit/e8421150e7d19cb18e6a1acf5045cd667baff9bd))

## [0.6.0](https://github.com/cresset-tools/magequery/compare/magequery-v0.5.0...magequery-v0.6.0) (2026-07-10)


### Features

* analyze unsaved buffers — the VFS overlay ([#21](https://github.com/cresset-tools/magequery/issues/21)) ([d26e04d](https://github.com/cresset-tools/magequery/commit/d26e04de5835e41d6ebdf567b6353b31d0b042f8))
* **lsp:** context-aware completions ([#23](https://github.com/cresset-tools/magequery/issues/23)) ([6ff3b8c](https://github.com/cresset-tools/magequery/commit/6ff3b8c6849604c77797fa626b14fa8214e7bfd4))

## [0.5.0](https://github.com/cresset-tools/magequery/compare/magequery-v0.4.0...magequery-v0.5.0) (2026-07-10)


### Features

* LSP server + VS Code and Zed extensions ([#18](https://github.com/cresset-tools/magequery/issues/18)) ([ad42edc](https://github.com/cresset-tools/magequery/commit/ad42edcbad1f0f2da0a0b2093aef1fe029c5c64f))

## [0.4.0](https://github.com/cresset-tools/magequery/compare/magequery-v0.3.0...magequery-v0.4.0) (2026-07-08)


### Features

* add product media gallery and customer-groups command ([#14](https://github.com/cresset-tools/magequery/issues/14)) ([1e6ac1d](https://github.com/cresset-tools/magequery/commit/1e6ac1d06e7a278faed3f59fbabd6b4266768949))
* add product-links command (related/up-sell/cross-sell) ([#17](https://github.com/cresset-tools/magequery/issues/17)) ([0f8f4b6](https://github.com/cresset-tools/magequery/commit/0f8f4b63e7a6763af60615de196f6784c6f07bf3))

## [0.3.0](https://github.com/cresset-tools/magequery/compare/magequery-v0.2.0...magequery-v0.3.0) (2026-07-06)


### Features

* **composer:** declare native-binary metadata for bougie prefetch ([#10](https://github.com/cresset-tools/magequery/issues/10)) ([1a642be](https://github.com/cresset-tools/magequery/commit/1a642be73027f8cf05e0f0c6b174f20617531be6))

## [0.2.0](https://github.com/cresset-tools/magequery/compare/magequery-v0.1.0...magequery-v0.2.0) (2026-07-06)


### Features

* add `magequery skill` command emitting the agent SKILL.md ([3333531](https://github.com/cresset-tools/magequery/commit/33335315558a8c93d65e2fbf285a47f8d7c7b092))
* add `magequery skill` command emitting the agent SKILL.md ([b7e9cb5](https://github.com/cresset-tools/magequery/commit/b7e9cb56dcee3a374c7023ba1b7e91032fbb580f))

## 0.1.0 (2026-07-05)


### Miscellaneous Chores

* release magequery 0.1.0 ([0267dd7](https://github.com/cresset-tools/magequery/commit/0267dd76bf6ca43c6d0e794ebe48f09fa217c257))
