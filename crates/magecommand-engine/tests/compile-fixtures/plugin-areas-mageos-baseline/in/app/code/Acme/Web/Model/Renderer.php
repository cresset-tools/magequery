<?php

namespace Acme\Web\Model;

class Renderer
{
    public function render(string $template, array $vars = []): string
    {
        return $template;
    }

    /** Plugged in di.xml, but final: interceptors may not override it. */
    final public function version(): int
    {
        return 1;
    }

    /** Ordinary public method that no plugin targets. */
    public function describe(): string
    {
        return 'renderer';
    }

    /** Plugged, but static: not interceptable either. */
    public static function reset(): void
    {
    }
}
