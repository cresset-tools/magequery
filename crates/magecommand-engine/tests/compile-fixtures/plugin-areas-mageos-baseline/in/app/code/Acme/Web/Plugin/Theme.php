<?php

namespace Acme\Web\Plugin;

use Acme\Web\Model\Renderer;

class Theme
{
    public function afterRender(Renderer $subject, string $result): string
    {
        return $result;
    }
}
