<?php
/**
 * Regenerate the module-order goldens (crates/magequery-core/tests/fixtures/module-order/)
 * from Magento's own code, so `sort_by_sequence` is tested against the real thing and not
 * against our reading of it.
 *
 *   php scripts/module-order-goldens.php <Loader.php> <out-dir> [<magento-root>]
 *
 * <Loader.php> is Framework/Module/ModuleList/Loader.php from a Magento or Mage-OS checkout.
 * Its three sorting methods are extracted verbatim and run as-is; nothing here reimplements
 * them. With <magento-root>, the modules registered under it (vendor + app/code, and
 * dev/tests/integration/_files the way the test bootstrap deploys them) are added as a
 * real-world case.
 *
 * Each case is {"name", "modules": {name: [sequence...]}, "expected": [names...] | null};
 * null means Magento throws (a <sequence> cycle).
 */

[$_, $loaderFile, $outDir] = $argv + [null, null, null];
$root = $argv[3] ?? null;
if (!$loaderFile || !$outDir) {
    fwrite(STDERR, "usage: php {$argv[0]} <Loader.php> <out-dir> [<magento-root>]\n");
    exit(2);
}

$src = file_get_contents($loaderFile);
preg_match_all('/    private function (sortBySequence|prearrangeModules|expandSequence)\(.*?\n    }\n/s', $src, $m);
if (count($m[0]) !== 3) {
    fwrite(STDERR, "expected 3 sorting methods in $loaderFile, found " . count($m[0]) . "\n");
    exit(1);
}
eval('final class GoldenLoader { ' . implode("\n", $m[0])
    . ' public function sort(array $l): array { return $this->sortBySequence($l); } }');

function golden(string $name, array $modules): array
{
    $in = [];
    foreach ($modules as $module => $sequence) {
        $in[$module] = ['name' => $module, 'sequence' => $sequence];
    }
    try {
        $expected = array_keys((new GoldenLoader())->sort($in));
    } catch (\LogicException $e) {
        $expected = null;
    }
    return ['name' => $name, 'modules' => (object)$modules, 'expected' => $expected];
}

/** A JSON array with one case per line: compact, and a regenerated case diffs as one line. */
function write_cases(string $file, array $cases): void
{
    $lines = array_map(fn($c) => json_encode($c, JSON_UNESCAPED_SLASHES), $cases);
    file_put_contents($file, "[\n" . implode(",\n", $lines) . "\n]\n");
    fwrite(STDERR, sprintf("%s: %d case(s)\n", $file, count($cases)));
}

@mkdir($outDir, 0777, true);

// ---- random graphs, aimed at the quirks -------------------------------------------------
// Vendors include names that contain "Magento_" without starting with it, and mixed case
// (ksort is bytewise: "Zed" < "acme"). Sequences may name modules that don't exist.
$vendors = ['Magento', 'Acme', 'acme', 'Zed', 'MageOS', 'Vendor_Magento', 'XMagento', 'Hyva', 'B2b'];
$cases = [];
mt_srand(20261104);
for ($case = 0; $case < 200; $case++) {
    $n = mt_rand(2, 30);
    $names = [];
    while (count($names) < $n) {
        $names[$vendors[mt_rand(0, count($vendors) - 1)] . '_' . chr(mt_rand(65, 90)) . mt_rand(0, 30)] = true;
    }
    $names = array_keys($names);
    shuffle($names);
    $cyclic = $case % 10 === 9;
    $modules = [];
    foreach ($names as $i => $name) {
        $seq = [];
        // Mostly acyclic: depend on modules earlier in the shuffled order.
        $deps = $i === 0 ? 0 : mt_rand(0, min(4, $i));
        for ($k = 0; $k < $deps; $k++) {
            $seq[] = $names[mt_rand(0, $i - 1)];
        }
        if (mt_rand(0, 9) === 0) {
            $seq[] = 'Missing_Module' . mt_rand(0, 3);
        }
        $modules[$name] = array_values(array_unique($seq));
    }
    if ($cyclic && $n >= 2) {
        $modules[$names[0]][] = $names[$n - 1];
        $modules[$names[$n - 1]][] = $names[0];
    }
    $cases[] = golden("random-$case", $modules);
}
write_cases("$outDir/random.json", $cases);

// ---- a real module set ------------------------------------------------------------------
if ($root) {
    $moduleXmls = array_merge(
        glob("$root/vendor/*/*/etc/module.xml") ?: [],
        glob("$root/vendor/*/*/src/etc/module.xml") ?: [],
        glob("$root/app/code/*/*/etc/module.xml") ?: [],
        glob("$root/dev/tests/integration/_files/*/*/etc/module.xml") ?: [],
    );
    $modules = [];
    foreach ($moduleXmls as $file) {
        $xml = simplexml_load_file($file);
        $name = (string)$xml->module['name'];
        $seq = [];
        foreach ($xml->module->sequence->module ?? [] as $dep) {
            $seq[] = (string)$dep['name'];
        }
        $modules[$name] = $seq;
    }
    ksort($modules);
    write_cases("$outDir/real.json", [golden(basename($root), $modules)]);
}
