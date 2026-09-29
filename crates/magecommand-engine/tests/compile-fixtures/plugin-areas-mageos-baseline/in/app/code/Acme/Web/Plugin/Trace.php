<?php

namespace Acme\Web\Plugin;

use Acme\Web\Model\Renderer;

class Trace
{
    public function beforeRender(Renderer $subject, string $template, array $vars = []): array
    {
        return [$template, $vars];
    }
}
