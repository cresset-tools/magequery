# Cresset_MagecommandLess — magecommand-less inside `setup:static-content:deploy`

A minimal Magento 2 bridge module that swaps Magento's PHP LESS compiler
(`less.php` via `Magento\Framework\Css\PreProcessor\Adapter\Less\Processor`) for the
pure-Rust `magecommand static less` compiler, without touching any other part of the
static-content pipeline.

## How it works

Magento's SCD pipeline materializes every LESS entry point (theme fallback,
`//@magento_import`, `Vendor_Module::` notation) into `var/view_preprocessed/` *before*
the compile step; only then does `Processor::processContent()` hand the materialized file
to `Less_Parser`. This module replaces exactly that last step via a `di.xml` preference:

```
Magento\Framework\Css\PreProcessor\Adapter\Less\Processor
    → Cresset\MagecommandLess\Adapter\Processor
```

The replacement adapter resolves the materialized temp file exactly as stock does
(`Asset\Source` + `File\Temporary`), then shells out:

```
magecommand static less --file <tmpfile> --stdout [--compress]
```

`--compress` is passed when the app mode is not `developer` — the same expression stock
uses for `Less_Parser`'s `compress` option. stdout becomes the CSS; a non-zero exit
becomes a `ContentProcessorException` carrying magecommand's stderr verbatim (file, line,
column, source excerpt); compiler warnings are forwarded to the Magento logger.
Everything downstream (e.g. the `VariableNotation` post-step, minification, deploy copy)
runs unchanged on our output.

## Binary resolution

Three levels, later wins:

1. Constructor default: `magecommand` resolved on `PATH`.
2. `di.xml` argument (see the commented block in `etc/di.xml`):
   ```xml
   <type name="Cresset\MagecommandLess\Adapter\Processor">
       <arguments>
           <argument name="magecommandBin" xsi:type="string">/usr/local/bin/magecommand</argument>
       </arguments>
   </type>
   ```
3. Environment: `MAGECOMMAND_BIN=/path/to/magecommand bin/magento setup:static-content:deploy …`

## Install

```sh
mkdir -p app/code/Cresset
cp -r packaging/magento-module/Cresset_MagecommandLess app/code/Cresset/MagecommandLess
bin/magento module:enable Cresset_MagecommandLess
bin/magento setup:upgrade --keep-generated   # or setup:di:compile in production mode
```

In default/developer mode the `config.php` edit from `module:enable` is sufficient —
di.xml preferences are merged at runtime, so SCD picks the adapter up immediately (no DB
or DI compile required; useful on build boxes).

## Oracle-testing workflow

The point of this module: run **Magento's own SCD pipeline** on top of magecommand-less
and diff against a stock run — the strongest end-to-end oracle we have.

1. **Stock baseline** (module disabled, or on a pristine copy):
   ```sh
   rm -rf pub/static/frontend var/view_preprocessed var/cache
   bin/magento setup:static-content:deploy -f en_US --area frontend \
       --theme Magento/blank --theme Magento/luma
   ```
   Capture `pub/static/frontend/Magento/*/en_US/css/*.css`.
2. **Bridged run**: enable the module, point `MAGECOMMAND_BIN` at a freshly built binary
   (`cargo build --release -p magecommand`), clean the same dirs, re-run SCD.
3. **Prove it wasn't a silent fallback**: point `MAGECOMMAND_BIN` at a wrapper that logs
   each invocation before exec'ing the real binary:
   ```sh
   #!/bin/bash
   echo "$(date -u +%FT%T) magecommand $*" >> /path/to/invocations.log
   exec /path/to/magecommand "$@"
   ```
   Expect one `static less --file … --stdout --compress` line per compiled entry
   (13 outputs / 16 invocations for blank+luma: 6 standard entries per theme plus
   `mage/gallery/gallery.less` and PageBuilder's `hljs.less`; Luma's `critical.css` is a
   verbatim copy and never reaches the adapter).
4. **Diff per entry**: byte-compare, then `magecommand static cssdiff <stock> <bridged>`
   for any non-identical pair.

### Result on the mg-install-310 oracle copy (2026-07-20, default mode = compressed)

7/13 outputs byte-identical (incl. all `styles-l`, `print`, `email-fonts`, and
`critical.css`); the other 6 (`styles-m`, `email`, `email-inline` × 2 themes) differ by
exactly one known cosmetic string each — less.php prints `71.42857143000001%` where we
print `71.42857143%` (a PHP float-print artifact) — and are `cssdiff`-clean. Zero
unexplained divergences. Whole-SCD wall time dropped from 6.8s to 4.8s (Magento-reported
execution time 5.34s → 2.91s); the compiler itself does all 12 main entries in ~1s
including per-process startup.
