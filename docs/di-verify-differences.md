# `magecommand di verify`: reading the report

`magecommand di verify --archive <php-output> --output <magecommand-output>` compares a
magecommand compile with a reference `generated/` tree, usually one produced by
`bin/magento setup:di:compile`. It sorts every difference into one of two buckets:

- **Unexplained.** The difference matches none of the patterns below. These are the ones
  to investigate, and the only ones `--fail-on-diff` fails on. The report lists them first,
  split by where they live:
  - **DI config** (`generated/metadata/`): the compiled object-manager config, i.e. the
    per-area `<area>.php` files, `interception.php` and the `*plugin-list.php` caches.
    A difference here changes how Magento wires objects.
  - **Generated code** (`generated/code/`): interceptors, proxies, factories and the
    other generated classes. A difference here changes the PHP that runs.

  Each changed file is shown with its **first divergence**, one line from each side,
  after the same normalization the classifiers apply. So that line is a real
  difference, not an entry that one of the patterns below already explains.
- **Explained.** The difference is known and expected. Each group links to its section
  below.

magecommand targets **Mage-OS 3.1.0 / Magento 2.4.9**. Most explained differences come from
comparing against an archive built by an older Magento (2.4.8 or earlier, see
[the disabled-module groups](#disabled-module-artifacts)), or on a different machine.

## Investigating an unexplained difference

- `--show-residual <path>` (e.g. `metadata/global.php`) prints the full context around
  the first divergence of one changed metadata file, with the same normalization applied.
- `--json` prints the raw compare report and the classification, including every file
  in every group.
- `--no-explain` turns classification off and lists every byte-level difference.
- Compile from empty, the way CI does. Building the reference into `generated/_code`
  first makes magecommand scan the reference as its class universe, which hides some
  bugs.

A divergence that stays unexplained on a store whose Magento matches magecommand's
target is a magecommand bug. Report it with the first-divergence lines, or the
`--show-residual` output.

## Plugin-list scope order

*DI config.* The plugin-list cache filename encodes the config scopes it was compiled
from. magecommand sorts the scope names alphabetically (`global|primary`), as Mage-OS
does. Older Magento lists them in module load order (`primary|global`). Sorted names map
the same scope set to one cache file whatever order the scopes are requested in. This is
the fix for Adobe issue #40408.

The report pairs each renamed file with its counterpart. When a pair also differs in
content, the difference is usually the plugin set of disabled modules (see
[disabled-module metadata](#disabled-module-metadata)).

## Disabled-module artifacts

*Generated code.* Interceptors, proxies and factories for classes in modules that are
disabled in `app/etc/config.php`. Since Magento 2.4.9, `setup:di:compile` compiles only
enabled modules, and magecommand does the same. An archive from 2.4.8 or earlier compiled
every module on disk, whether it was enabled or not. Leaving them out is the
improvement: no code gets generated that can never run.

## Disabled-module metadata

*DI config.* Metadata files (`<area>.php`, `interception.php`) that are byte-identical once
the top-level entries for classes in disabled modules are removed from both sides. This
is the same 2.4.9 behavior as above, for the compiled config.

## Disabled-module reachable types

*DI config.* Types named after an **enabled** module that nothing in the enabled
codebase pulls into the compile. Each one is declared by a disabled module, or is a
parent class or implemented interface of a class in one. A compile that includes every
module drags those interfaces into the interception walk; a 2.4.9 compile does not. The
classic case is an interface in a core module whose only implementations ship in a
disabled payment module. The disabled modules' own source verifies this, with one PHP
header parse per file.

## Disabled-module reachable artifacts

*Generated code.* The file-level twin of the previous section. Some generated artifacts,
usually factories, are named after an enabled module, but the only constructors that
type-hint them live in disabled modules. A compile that reads every module emits them;
since 2.4.9, nothing left in the codebase asks for them.

## Outside scan paths

*Both trees.* Classes in a composer package that registers itself as a module in
`registration.php` but ships no `etc/module.xml` and is not in `app/etc/config.php`. Such a
package is not an enabled module. Older compilers walk the ComponentRegistrar paths
directly and compile it anyway; magecommand discovers modules through `module.xml` plus
the enabled list.

This is matched by location, not by name: each class resolves to a real file that lies
under none of the compile's scan roots (enabled module directories, the framework
library paths, `setup/src`). A missing framework or module entry therefore still fails.

**Worth a look:** if a package listed here is supposed to be live, it isn't. It needs a
`module.xml` and an entry in `config.php`.

## Extra metadata

*DI config.* The output is a strict superset of the archive: every archive entry is
present unchanged and the output only adds entries. The largest addition is the
`nonLazyTypes` section (plus its `NonLazyTypes` step in `ModificationChain`). It comes from
the lazy-proxy compiler pass in 2.4.9 / PHP 8.4, which older archives predate. Extra
compiled entries are inert. A real regression would remove or change an archive entry,
and that keeps the file flagged.

## Class-scanner exclude regex

*DI config.* The ClassesScanner exclusion regex
(`#^(?:…/vendor/<pkg>…|…/app/code/<Vendor>/<Module>…)/Test#` and its `/tests#` sibling) lists
every enabled module directory. An archive compiled from a different module set lists
different members, in a different order. The file matches once that regex is
canonicalized on both sides and disabled-module entries are removed. A changed value
anywhere else survives canonicalization and keeps the file flagged.

## Filename casing

*Generated code.* Paths that differ only in letter case (`Tierprice` vs `TierPrice`). The
archive was generated on a case-insensitive filesystem (macOS), where the first directory
created sets the casing. magecommand emits the case the PHP class declares, which is the
correct form on the case-sensitive filesystems used in production.

## Generator version formatting

*Generated code.* The file is byte-identical after normalizing formatting that Magento's
code generator changed between versions. All of it preserves behavior:

- return-type spacing (`foo() : T` → `foo(): T`)
- explicit nullable defaults (`?T $x` → `?T $x = null`, the PHP 8.4-compatible form)
- the proxy template's laziness hardening: null guards in `__clone` and `_resetState`,
  so cloning or resetting an unused proxy no longer instantiates it, plus an added
  `__debugInfo`

These differences disappear on a 2.4.9 store.

## Obfuscated vendor source

*DI config.* The constructor chain of these classes runs through vendor source whose
class declaration exists only inside an `eval()` of encrypted code. The real compiler
executes that stub and reflects the class it produces. magecommand never executes PHP,
so it can't see the constructor, and the arguments entry degrades to `NULL` (a compile
finding records each one). At runtime the object manager falls back to reflection for a
`NULL` entry, so the store still works; the entry just isn't cached. This is the one
place where never executing PHP costs fidelity.

## Ordering only

*Both trees.* Same content in a different order. This is never counted as unexplained;
pass `--strict-ordering` to treat it as `changed`.

- **Interceptor method order.** PHP's `getMethods()` order, which the interceptor
  generator follows, differs between PHP versions (8.4 vs 8.5) for classes that use
  traits. Method order in a PHP class has no effect on behavior.
- **Plugin-list key order.** `PluginList` reads the `_data`, `_inherited` and `_processed`
  sections only by key (`isset`, `array_key_exists`, `[$type][$code]`) and never iterates
  them, so array key order can't be observed.
