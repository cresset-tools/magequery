<?php

declare(strict_types=1);

namespace Cresset\Magequery;

use RuntimeException;

require __DIR__ . '/version.php';

/**
 * Locates — downloading on first use — the prebuilt `magequery` binary that
 * matches this package's version, and execs it. The Rust source never ships
 * in the Composer package; only this launcher does. The binary is fetched
 * from the magequery GitHub release (cresset mirror first), SHA-256-verified
 * against the `.sha256` sidecar, cached per-version, and exec'd. Mirrors
 * `uv format` and Mago's `composer/bin/mago`.
 *
 * The same download layout (URL bases, tag prefix, targets, cache path)
 * is declared machine-readably in composer.json under
 * `extra.bougie.native-binary`, so bougie can prefetch the binary at
 * `bougie tool install` time instead of paying this download on first
 * run. Keep that block in sync with this class and with
 * dist-workspace.toml's target list.
 */
final class Launcher
{
    private const MIRROR_BASE = 'https://releases.bougie.tools/github/magequery/releases/download';
    private const GITHUB_BASE = 'https://github.com/cresset-tools/magequery/releases/download';
    private const USER_AGENT = 'cresset/magequery composer installer';

    /** @param list<string> $args */
    public static function main(array $args): int
    {
        try {
            $binary = self::ensureBinary();
        } catch (RuntimeException $e) {
            fwrite(STDERR, 'magequery: ' . $e->getMessage() . "\n");
            return 1;
        }

        return self::run($binary, $args);
    }

    private static function ensureBinary(): string
    {
        $target = self::targetTriple();
        $binName = self::binaryName();
        $cached = self::cacheDir() . DIRECTORY_SEPARATOR . $binName;
        if (is_file($cached)) {
            return $cached;
        }

        $tag = 'magequery-v' . MAGEQUERY_VERSION;
        $archive = 'magequery-' . $target . (self::isWindows() ? '.zip' : '.tar.gz');

        $tmp = self::makeTempDir();
        try {
            $archivePath = $tmp . DIRECTORY_SEPARATOR . $archive;
            $shaPath = $archivePath . '.sha256';

            self::fetch(self::urls($tag, $archive . '.sha256'), $shaPath);
            $expected = self::parseSidecar($shaPath);

            fwrite(STDERR, 'magequery: fetching magequery ' . MAGEQUERY_VERSION . " ($target)\n");
            self::fetch(self::urls($tag, $archive), $archivePath);
            self::verifySha256($archivePath, $expected);

            $extractDir = $tmp . DIRECTORY_SEPARATOR . 'extracted';
            self::mkdirp($extractDir);
            self::extract($archivePath, $extractDir);

            $staged = $extractDir . DIRECTORY_SEPARATOR . 'magequery-' . $target . DIRECTORY_SEPARATOR . $binName;
            if (!is_file($staged)) {
                throw new RuntimeException("extracted archive is missing $binName at $staged");
            }

            self::mkdirp(dirname($cached));
            // Atomic-ish: stage into the final directory, then rename over
            // the target so a concurrent run never sees a half-written file.
            $partial = $cached . '.partial';
            if (!@copy($staged, $partial)) {
                throw new RuntimeException('failed to stage magequery binary into the cache');
            }
            if (!self::isWindows()) {
                @chmod($partial, 0o755);
            }
            if (!@rename($partial, $cached)) {
                @unlink($partial);
                throw new RuntimeException('failed to install magequery binary into the cache');
            }
        } finally {
            self::rmrf($tmp);
        }

        return $cached;
    }

    private static function targetTriple(): string
    {
        $machine = strtolower(php_uname('m'));
        $isArm = in_array($machine, ['arm64', 'aarch64'], true);
        $isX64 = in_array($machine, ['x86_64', 'amd64', 'x64'], true);

        switch (PHP_OS_FAMILY) {
            case 'Linux':
                if ($isX64) {
                    return self::isMusl() ? 'x86_64-unknown-linux-musl' : 'x86_64-unknown-linux-gnu';
                }
                throw new RuntimeException("no prebuilt magequery binary for Linux/$machine (x86_64 only)");
            case 'Darwin':
                if ($isArm) {
                    return 'aarch64-apple-darwin';
                }
                throw new RuntimeException('no prebuilt magequery binary for Intel macOS (Apple Silicon only)');
            case 'Windows':
                if ($isX64) {
                    return 'x86_64-pc-windows-msvc';
                }
                throw new RuntimeException("no prebuilt magequery binary for Windows/$machine (x64 only)");
            default:
                throw new RuntimeException('unsupported OS: ' . PHP_OS_FAMILY);
        }
    }

    private static function isWindows(): bool
    {
        return PHP_OS_FAMILY === 'Windows';
    }

    /** Best-effort musl detection (Alpine and friends). */
    private static function isMusl(): bool
    {
        if (is_file('/etc/alpine-release')) {
            return true;
        }
        // `ldd --version` says "musl" on musl systems; glibc says "GLIBC"/"GNU".
        $out = @shell_exec('ldd --version 2>&1');
        return is_string($out) && stripos($out, 'musl') !== false;
    }

    private static function binaryName(): string
    {
        return self::isWindows() ? 'magequery.exe' : 'magequery';
    }

    private static function cacheDir(): string
    {
        if (self::isWindows()) {
            $base = getenv('LOCALAPPDATA') ?: sys_get_temp_dir();
        } else {
            $base = getenv('XDG_CACHE_HOME');
            if ($base === false || $base === '') {
                $home = getenv('HOME') ?: sys_get_temp_dir();
                $base = $home . '/.cache';
            }
        }

        return $base . DIRECTORY_SEPARATOR . 'magequery' . DIRECTORY_SEPARATOR . MAGEQUERY_VERSION;
    }

    /**
     * Mirror first (low latency, no GitHub anonymous rate limits), GitHub
     * release as fallback.
     *
     * @return list<string>
     */
    private static function urls(string $tag, string $file): array
    {
        return [
            self::MIRROR_BASE . "/$tag/$file",
            self::GITHUB_BASE . "/$tag/$file",
        ];
    }

    /** @param list<string> $urls */
    private static function fetch(array $urls, string $dest): void
    {
        $errors = [];
        foreach ($urls as $url) {
            $data = self::httpGet($url, $errors);
            if ($data !== null) {
                if (@file_put_contents($dest, $data) === false) {
                    throw new RuntimeException("failed to write $dest");
                }

                return;
            }
        }

        throw new RuntimeException("download failed:\n  " . implode("\n  ", $errors));
    }

    /** @param list<string> $errors */
    private static function httpGet(string $url, array &$errors): ?string
    {
        if (function_exists('curl_init')) {
            $ch = curl_init($url);
            curl_setopt_array($ch, [
                CURLOPT_RETURNTRANSFER => true,
                CURLOPT_FOLLOWLOCATION => true,
                CURLOPT_FAILONERROR => true,
                CURLOPT_CONNECTTIMEOUT => 20,
                CURLOPT_TIMEOUT => 120,
                CURLOPT_USERAGENT => self::USER_AGENT,
            ]);
            $data = curl_exec($ch);
            if ($data === false) {
                $errors[] = "$url: " . curl_error($ch);

                return null;
            }
            // No curl_close(): the handle is freed when $ch goes out of
            // scope (curl handles are objects since PHP 8.0), and the
            // function is deprecated as a no-op in 8.5+.

            return is_string($data) ? $data : null;
        }

        $ctx = stream_context_create(['http' => [
            'follow_location' => 1,
            'timeout' => 120,
            'user_agent' => self::USER_AGENT,
        ]]);
        $data = @file_get_contents($url, false, $ctx);
        if ($data === false) {
            $err = error_get_last();
            $errors[] = "$url: " . ($err['message'] ?? 'request failed');

            return null;
        }

        return $data;
    }

    private static function parseSidecar(string $path): string
    {
        $body = (string) @file_get_contents($path);
        $hex = strtok(trim($body), " \t\n");
        if (!is_string($hex) || strlen($hex) !== 64 || !ctype_xdigit($hex)) {
            throw new RuntimeException('malformed sha256 sidecar');
        }

        return strtolower($hex);
    }

    private static function verifySha256(string $file, string $expected): void
    {
        $actual = hash_file('sha256', $file);
        if ($actual === false || !hash_equals($expected, $actual)) {
            throw new RuntimeException("sha256 mismatch for $file: expected $expected, got " . (string) $actual);
        }
    }

    private static function extract(string $archive, string $into): void
    {
        if (self::isWindows()) {
            if (!class_exists(\ZipArchive::class)) {
                throw new RuntimeException('ext-zip is required to install magequery on Windows');
            }
            $zip = new \ZipArchive();
            if ($zip->open($archive) !== true) {
                throw new RuntimeException('failed to open zip archive');
            }
            $zip->extractTo($into);
            $zip->close();

            return;
        }

        // .tar.gz via PharData (ext-phar is enabled by default).
        try {
            $phar = new \PharData($archive);
            $phar->extractTo($into, null, true);
        } catch (\Throwable $e) {
            // Fall back to the system tar if Phar can't read it.
            if (self::runQuiet(['tar', '-xzf', $archive, '-C', $into]) !== 0) {
                throw new RuntimeException('failed to extract archive: ' . $e->getMessage());
            }
        }
    }

    /** @param list<string> $args */
    private static function run(string $binary, array $args): int
    {
        if (function_exists('pcntl_exec')) {
            // Replace this process so signals and TTY pass through cleanly.
            // Only returns if exec failed.
            pcntl_exec($binary, $args);
        }

        $proc = @proc_open(
            array_merge([$binary], $args),
            [0 => STDIN, 1 => STDOUT, 2 => STDERR],
            $pipes
        );
        if (!is_resource($proc)) {
            fwrite(STDERR, "magequery: failed to execute $binary\n");

            return 1;
        }

        return proc_close($proc);
    }

    /** @param list<string> $cmd */
    private static function runQuiet(array $cmd): int
    {
        $proc = @proc_open($cmd, [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
        if (!is_resource($proc)) {
            return 1;
        }
        foreach ($pipes as $pipe) {
            if (is_resource($pipe)) {
                stream_get_contents($pipe);
                fclose($pipe);
            }
        }

        return proc_close($proc);
    }

    private static function makeTempDir(): string
    {
        $dir = sys_get_temp_dir() . DIRECTORY_SEPARATOR . 'magequery-dl-' . bin2hex(random_bytes(6));
        if (!@mkdir($dir, 0o777, true) && !is_dir($dir)) {
            throw new RuntimeException('failed to create temp dir');
        }

        return $dir;
    }

    private static function mkdirp(string $dir): void
    {
        if (!is_dir($dir) && !@mkdir($dir, 0o777, true) && !is_dir($dir)) {
            throw new RuntimeException("failed to create directory $dir");
        }
    }

    private static function rmrf(string $dir): void
    {
        if (!is_dir($dir)) {
            return;
        }
        $items = new \RecursiveIteratorIterator(
            new \RecursiveDirectoryIterator($dir, \FilesystemIterator::SKIP_DOTS),
            \RecursiveIteratorIterator::CHILD_FIRST
        );
        foreach ($items as $item) {
            if ($item->isDir()) {
                @rmdir($item->getPathname());
            } else {
                @unlink($item->getPathname());
            }
        }
        @rmdir($dir);
    }
}
