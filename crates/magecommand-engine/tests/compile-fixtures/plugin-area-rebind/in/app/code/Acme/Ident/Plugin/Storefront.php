<?php

namespace Acme\Ident\Plugin;

use Acme\Ident\Model\Identifier;

class Storefront
{
    public function afterGetValue(Identifier $subject, string $result): string
    {
        return $result;
    }
}
