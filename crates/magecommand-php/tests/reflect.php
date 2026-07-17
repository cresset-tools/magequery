<?php
/**
 * Ground-truth dumper for the differential harness. Runs inside a real
 * Magento checkout (composer autoloader); reads "FQCN\tfile" lines on stdin,
 * reflects each class, and emits one canonical JSON record per line.
 *
 * This is TEST INFRASTRUCTURE ONLY: the shipped magecommand never executes
 * PHP — this script is the oracle its parser is proven against.
 *
 * Usage: php reflect.php /path/to/magento-root < classlist.tsv
 */

declare(strict_types=1);

error_reporting(E_ALL & ~E_DEPRECATED & ~E_NOTICE & ~E_WARNING);
ini_set('memory_limit', '4G');
ini_set('display_errors', 'stderr');

$root = $argv[1] ?? null;
if ($root === null || !is_file($root . '/vendor/autoload.php')) {
    fwrite(STDERR, "usage: php reflect.php <magento-root> < classlist\n");
    exit(2);
}
require $root . '/vendor/autoload.php';

/** Canonical, order- and case-insensitive type string. */
function type_str(?ReflectionType $t): ?string
{
    if ($t === null) {
        return null;
    }
    if ($t instanceof ReflectionNamedType) {
        $n = strtolower(ltrim($t->getName(), '\\'));
        $parts = [$n];
        if ($t->allowsNull() && $n !== 'null' && $n !== 'mixed') {
            $parts[] = 'null';
        }
        $parts = array_unique($parts);
        sort($parts);
        return implode('|', $parts);
    }
    if ($t instanceof ReflectionIntersectionType) {
        $parts = array_map(
            fn($x) => strtolower(ltrim($x->getName(), '\\')),
            $t->getTypes()
        );
        sort($parts);
        return implode('&', $parts);
    }
    if ($t instanceof ReflectionUnionType) {
        $parts = [];
        foreach ($t->getTypes() as $x) {
            $s = type_str($x);
            // Intersection members contain '&' but never '|'.
            foreach (explode('|', $s) as $p) {
                $parts[] = $p;
            }
        }
        $parts = array_unique($parts);
        sort($parts);
        return implode('|', $parts);
    }
    return 'unknown';
}

while (($line = fgets(STDIN)) !== false) {
    $line = rtrim($line, "\n");
    if ($line === '') {
        continue;
    }
    [$fqcn, $file] = explode("\t", $line, 2);
    $rec = ['fqcn' => $fqcn];
    try {
        $exists = class_exists($fqcn) || interface_exists($fqcn)
            || trait_exists($fqcn) || enum_exists($fqcn);
        if (!$exists) {
            $rec['status'] = 'unloadable';
            echo json_encode($rec), "\n";
            continue;
        }
        $r = new ReflectionClass($fqcn);
        $loaded = $r->getFileName();
        if ($loaded === false || realpath($loaded) !== realpath($file)) {
            // The autoloader served a different file than the one we parsed
            // (duplicate declarations, test fixtures): not comparable.
            $rec['status'] = 'shadowed';
            echo json_encode($rec), "\n";
            continue;
        }

        $isEnum = $r->isEnum();
        $rec['status'] = 'ok';
        $rec['kind'] = $isEnum ? 'enum'
            : ($r->isInterface() ? 'interface' : ($r->isTrait() ? 'trait' : 'class'));
        // Interfaces reflect abstract=true, enums final=true — implicit
        // flags the source never wrote; only classes carry them honestly.
        $rec['abstract'] = $rec['kind'] === 'class' && $r->isAbstract();
        $rec['final'] = $rec['kind'] === 'class' && $r->isFinal();
        $p = $r->getParentClass();
        $rec['parent'] = $p ? strtolower($p->getName()) : null;
        $rec['interfaces'] = array_map('strtolower', array_values($r->getInterfaceNames()));

        $methods = [];
        foreach ($r->getMethods() as $m) {
            if ($m->getDeclaringClass()->getName() !== $r->getName()) {
                continue; // inherited
            }
            $mf = $m->getFileName();
            if ($mf === false || realpath($mf) !== realpath($file)) {
                continue; // engine-provided (enum cases()/from()) or trait-imported
            }
            $methods[strtolower($m->getName())] = [
                'v' => $m->isPublic() ? 'public' : ($m->isProtected() ? 'protected' : 'private'),
                'static' => $m->isStatic(),
                'abstract' => !$r->isInterface() && $m->isAbstract(),
                'ref' => $m->returnsReference(),
                'ret' => type_str($m->getReturnType()),
                'params' => array_map(fn($pp) => [
                    'name' => $pp->getName(),
                    'type' => type_str($pp->getType()),
                    'ref' => $pp->isPassedByReference(),
                    'variadic' => $pp->isVariadic(),
                    'hasDefault' => $pp->isDefaultValueAvailable(),
                    'promoted' => $pp->isPromoted(),
                ], $m->getParameters()),
            ];
        }
        $rec['methods'] = $methods;

        $caseNames = [];
        if ($isEnum) {
            $re = new ReflectionEnum($fqcn);
            foreach ($re->getCases() as $case) {
                $caseNames[$case->getName()] = true;
            }
            $rec['cases'] = array_keys($caseNames);
        }
        // Trait-provided constants (8.2+) reflect with declaring class = the
        // using class; exclude them by name — the source file doesn't write
        // them.
        $traitConsts = [];
        $traitQueue = $r->getTraits();
        while ($traitQueue) {
            $t = array_pop($traitQueue);
            foreach ($t->getReflectionConstants() as $tc) {
                $traitConsts[$tc->getName()] = true;
            }
            foreach ($t->getTraits() as $tt) {
                $traitQueue[] = $tt;
            }
        }
        $consts = [];
        foreach ($r->getReflectionConstants() as $c) {
            if ($c->getDeclaringClass()->getName() !== $r->getName()) {
                continue;
            }
            if (isset($caseNames[$c->getName()]) || isset($traitConsts[$c->getName()])) {
                continue; // enum cases and trait constants reflect as own
            }
            $consts[] = $c->getName();
        }
        sort($consts);
        $rec['constants'] = $consts;
    } catch (Throwable $e) {
        $rec = [
            'fqcn' => $fqcn,
            'status' => 'error',
            'error' => get_class($e) . ': ' . $e->getMessage(),
        ];
    }
    echo json_encode($rec), "\n";
}
