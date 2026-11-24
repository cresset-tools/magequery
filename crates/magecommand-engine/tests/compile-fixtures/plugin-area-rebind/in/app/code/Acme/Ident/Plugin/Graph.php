<?php

namespace Acme\Ident\Plugin;

use Acme\Ident\Model\Identifier;

class Graph
{
    public function afterGetValue(Identifier $subject, string $result): string
    {
        return $result;
    }
}
