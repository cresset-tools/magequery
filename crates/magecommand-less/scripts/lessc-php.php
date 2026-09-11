<?php
/**
 * Compile one LESS file with a STORE's own `wikimedia/less.php`, so its output
 * can be diffed against ours. The store supplies the library, which is the
 * whole point: `wikimedia/less.php` 3.x and 5.x are different compilers, and
 * Magento 2.4.7 and 2.4.8 ship them respectively.
 *
 *   php lessc-php.php <magento-root> <file.less> [--no-compress]
 */
if ($argc < 3) {
    fwrite(STDERR, "usage: lessc-php.php <magento-root> <file.less> [--no-compress]\n");
    exit(2);
}
[$root, $file] = [rtrim($argv[1], '/'), $argv[2]];
$autoload = $root . '/vendor/autoload.php';
if (!is_file($autoload)) {
    fwrite(STDERR, "no composer autoload at $autoload\n");
    exit(2);
}
require $autoload;
if (!class_exists('Less_Parser')) {
    fwrite(STDERR, "wikimedia/less.php is not installed in $root\n");
    exit(2);
}
// Magento's own adapter settings (Css\PreProcessor\Adapter\Less\Processor).
$parser = new Less_Parser([
    'compress' => !in_array('--no-compress', $argv, true),
    'relativeUrls' => false,
]);
try {
    $parser->parseFile($file, '');
    echo $parser->getCss(), "\n";
} catch (Exception $e) {
    fwrite(STDERR, 'ERR: ' . $e->getMessage() . "\n");
    exit(1);
}
