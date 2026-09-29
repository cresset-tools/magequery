<?php

namespace Acme\Web\Plugin;

use Acme\Web\Model\Renderer;

class Cache
{
    public function aroundRender(
        Renderer $subject,
        callable $proceed,
        string $template,
        array $vars = []
    ): string {
        return $proceed($template, $vars);
    }

    /** Targets a FINAL method: no interceptor override may be emitted. */
    public function afterVersion(Renderer $subject, int $result): int
    {
        return $result;
    }

    /** Targets a STATIC method: likewise not interceptable. */
    public function afterReset(Renderer $subject): void
    {
    }
}
