<?php
// Regenerates expected.tsv with Magento's own Config\Dom and Indexer Converter:
//   php regenerate.php base.xml child.xml > expected.tsv
// run where /srv/store is a Magento install (for its autoloader). The crypt key is
// the one tests/devupgrade.rs writes to env.php: 32 times "k", base64-prefixed.
require '/srv/store/vendor/autoload.php';
$ids = ['/config/indexer' => 'id', '/config/indexer/handler' => 'name', '/config/indexer/source' => 'name',
    '/config/indexer/fieldset' => 'name', '/config/indexer/fieldset/field' => 'name', '/config/indexer/dependencies/indexer' => 'id'];
$vs = new class implements \Magento\Framework\Config\ValidationStateInterface { public function isValidationRequired() { return false; } };
$dom = new \Magento\Framework\Config\Dom(file_get_contents($argv[1]), $vs, $ids);
$dom->merge(file_get_contents($argv[2]));
$conv = new \Magento\Framework\Indexer\Config\Converter(new \Magento\Framework\Indexer\Config\Converter\SortingAdjustment());
$key = 'base64' . base64_encode(str_repeat("k", 32));
foreach ($conv->convert($dom->getDom()) as $id => $c) {
    $json = json_encode($c);
    echo $id, "\t", hash_hmac('md5', $json, str_repeat("k", 32)), "\t", $json, "\n";
}
