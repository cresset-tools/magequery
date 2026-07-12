# M2 study notes — compiled metadata, verified against real generator source

Oracle: mg-install-310 (Mage-OS 3.1.0 / Magento 2.4.9 base, compiled on PHP 8.5.8).
Every algorithm below was read from the actual setup sources in that checkout
(`vendor/modulargento/magento2-base/setup/src/Magento/Setup/Module/Di/...`), not
from memory. File format = `var_export` byte-exact (implemented + locked by tests
in `magecommand-engine/src/phpexport.rs`).

## File inventory (generated/metadata/) and status

| file | status | generator |
|---|---|---|
| app_action_list.php | **byte-identical** | Module\Dir\Reader::getActionFiles + ksort (path convention, enabled modules, blind 4-char strip) |
| global.php + 6 area files | todo | Operation\Area → Compiler\Config\Reader → ModificationChain |
| 6 `…\|plugin-list.php` | todo | Operation\PluginListGenerator (filename = sorted scope names joined by \|; content = PluginList triple `[_data, _inherited, _processed]`) |
| interception.php | todo | Operation\InterceptionCache → Interception\Config::initialize (true/false per class: has plugins incl. via ancestors) |

## Compile paths (DiCompileCommand)

- `application` = ComponentRegistrar MODULE paths filtered to **enabled** (deployment config), registrar order
- `library` = ComponentRegistrar LIBRARY paths (framework etc.)
- `setup` = setup dir; `generated_helpers` = generated/code
- Excludes: `getExcludedModulePaths` / `getExcludedLibraryPaths` / `getExcludedSetupPaths`
  (Test/tests dir regexes — read them when building the scanner)
- Per operation: AREA_CONFIG_GENERATOR gets app+lib+setup+generated; INTERCEPTION_CACHE
  gets app+lib+generated (NO setup); APP_ACTION_LIST none (module reader)

## Area operation (Operation\Area::doOperation)

1. DefinitionsCollection over the paths: class → constructor definition
   (ClassReaderDecorator = reflection; ours = magecommand-php parser). `sortDefinitions`.
2. areas = [global] + areaList codes; per area: `Reader::generateCachePerScope`
3. ModificationChain: **BackslashTrim → PreferencesResolving → InterceptorSubstitution →
   InterceptionPreferencesResolving (= PreferencesResolving again) → NonLazyTypes**
4. `ksort` on arguments, preferences, instanceTypes (NOT nonLazyTypes — insertion order)
5. write `<area>.php`

## Reader::generateCachePerScope

- areaConfig = global di config; for non-global extend with area's.
- `fillThirdPartyInterfaces`: collection = (all preference KEYS with `[]` ctor) merged-under scanned definitions — how 3rd-party interfaces (Aws\CacheInterface) enter the universe.
- `arguments` = for each collection entry where `Type::isConcrete` (skips interfaces/abstract):
  ArgumentsResolver::getResolvedConstructorArguments(class, ctor) — `NULL` when ctor empty/none.
  PLUS every virtual type: ctor = base type's definition (from collection, else reflect if
  concrete), resolved under the vtype's name.
- `preferences` = for each collection instance name: pref = areaConfig->getPreference
  (fixpoint); emit `name => pref` when different. (Validation errors can't fire — the
  oracle compile succeeded.)
- `instanceTypes` = vtype → its base (one hop).

## ArgumentsResolver encodings (per ctor param, in order, key = param name)

1. base: param optional → `getNonObjectArgument(default)`; required with CLASS type →
   `getInstanceArgument(type)`; required non-class → `['_vn_' => true]`.
2. di.xml-configured override by param name:
   - param has class type → `getConfiguredInstanceArgument`: `_i_/_ins_` of
     `config['instance']` per **type-level isShared(instance)**; explicit
     `config['shared']` overrides the pattern.
   - config is `['argument' => name]` → `['_a_' => name, '_d_' => default]`.
   - else → `getNonObjectArgument(configured value)`.
3. `getInstanceArgument(t)`: `['_i_' => t]` if isShared(t) (type-level di shared, default
   true) else `['_ins_' => t]`. NO preference resolution here (done later by the chain).
4. `getNonObjectArgument(v)`: null → `['_vn_' => true]`; array containing any nested
   `['instance'=>…]`/`['argument'=>…]` (recursive check) → `['_vac_' => transformed]`
   where instances→_i_/_ins_ maps, arguments→`['_a_'=>…,'_d_'=>null]`, recursing;
   else `['_v_' => v]`.
5. Configured instance names are `ltrim($i, '\\')`-ed before all this.
- Defaults are EVALUATED values (`Foo::class` → FQCN string, consts resolved) — needs the
  const-expression evaluator. Scalar-typed required params (`int $x`) have type=null in
  ClassReader terms → `_vn_`.

## Modification chain details

- BackslashTrim / PreferencesResolving / InterceptorSubstitution: READ THESE before
  implementing arguments (they rewrite `_i_/_ins_` targets: preference resolution and
  Interceptor substitution happen HERE, post-resolver).
- NonLazyTypes (PHP ≥ 8.4 only — active in the oracle): candidates = arguments keys +
  instanceTypes values + preferences values (insertion order, deduped); nonLazy when NOT
  lazy-eligible. Ineligible = name ends `\Proxy`, class_exists fails (⇒ **virtual types
  and generated-not-present classes land here**), interface/abstract/trait, final/enum/
  readonly, any INTERNAL ancestor (⇒ needs the PHP-builtin stub table), or has
  `#[\Magento\Framework\ObjectManager\Attribute\NonLazy]` (⇒ parser must capture class
  attribute names).

## Universe numbers (oracle)

- global.php arguments: 14,639 top-level entries; interfaces excluded; NULL for
  no-ctor-config concrete classes.
- interception.php: 19,467 entries incl. interfaces + generated Interceptors, 2,856 true.

## New capabilities this demands

1. magequery-core: parse + export `shared` attr on type/virtualType (isShared).
2. magecommand-php: const-expression evaluator (defaults → values; `::class`, class
   consts transitive, literals, arrays, core constants) + class-attribute names capture.
3. magecommand-engine: path scanner with Magento's exclude regexes; definitions
   collection; Reader equivalent; the chain; PHP-builtin stub table (internal-ancestor
   checks); section-level diff loop against the archive until whole files go clean.
